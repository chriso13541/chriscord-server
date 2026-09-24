// voice.rs — WebRTC SFU logic for voice channels.
//
// This file has been built and run successfully multiple times over the
// course of development, with mute, deafen, and real two-way audio
// between separate machines all confirmed working — it's no longer a
// first draft. Worth knowing about the UDP networking setup specifically
// (see ICE_UDP_PORT below): it's been through three iterations. First a
// 100-port ephemeral range; then a single muxed port, which caused
// constant, audible roughness and was reverted back to per-connection
// ports on a smaller 20-port range; then, after real open-internet
// testing with multiple external participants showed that fixed range
// exhausting fast (each connection was gathering 5+ redundant host
// candidates — one per virtual/VPN/Docker interface on the server
// machine — multiplying how many ports one participant alone could
// consume), back to the single muxed port a third time, this time paired
// with an interface filter to cut that redundant candidate count down,
// on the theory that the earlier roughness was from excess ICE traffic
// sharing the socket rather than the single socket being inherently
// unusable. If roughness shows up again after this change, that theory
// was wrong and is the next thing to revisit — see ICE_UDP_PORT's own
// comment for more on this reasoning.
//
// EphemeralUDP/UDPNetwork::Ephemeral (used in the second iteration above)
// was confirmed via an actual successful `cargo build`. UDPMuxDefault
// (used here) matches a real, executed test from webrtc-ice's own test
// suite plus a real third-party production crate using the identical
// pattern — not just a method signature, but code known to compile and
// run elsewhere. set_interface_filter is confirmed against pion's own
// official documentation, including its true=keep/false=exclude polarity,
// for the identical API this Rust crate is a direct port of.
//
// Design — each participant's offer declares its own real shape upfront:
// their own mic (transceiver 0) plus one receive-only placeholder per
// other participant they already know about at join time (transceivers
// 1..), sent alongside the offer as an ordered `expected_others` list. The
// server attaches each known participant's audio to its matching
// placeholder by position — never adding media sections the offer didn't
// already reserve, since an SDP answer can't validly declare more
// sections than its offer did.
//   - RTP forwarding: each participant's OWN incoming audio, once
//     negotiated, gets read from their PeerConnection and written into a
//     single shared TrackLocalStaticRTP unique to them. That same shared
//     track object gets attached to every OTHER participant's matching
//     placeholder transceiver. Writing to it once fans out to everyone
//     who has it attached — the standard SFU forwarding pattern, and
//     what keeps the server from ever needing to decode or re-encode
//     audio: it only ever moves RTP packets around.
//   - Seamless mid-call updates: when someone joins AFTER others are
//     already connected, those existing participants' connections are
//     never torn down or rebuilt. Instead the server renegotiates each of
//     them in place — adds a new transceiver to their already-established
//     PeerConnection, attaches the new participant's source to it, and
//     sends them a fresh offer for just that one connection (see
//     renegotiate_one/renegotiate_others_for_new_source below). Their own
//     already-flowing audio, ICE state, and everyone else they can
//     already hear are completely unaffected — this is what replaced the
//     earlier "everyone re-offers from scratch on every roster change"
//     design, which caused a brief audible drop for every other
//     participant whenever anyone joined or left. Leaving still doesn't
//     trigger a renegotiation to remove that participant's section — their
//     track just stops producing audio, which is harmless and keeps this
//     simpler; only joining requires proactively updating everyone else.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::Mutex as AsyncMutex;

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::{APIBuilder, API};
use webrtc::ice::udp_mux::{UDPMuxDefault, UDPMuxParams};
use webrtc::ice::udp_network::UDPNetwork;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpHeaderExtensionCapability, RTPCodecType};
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::RTCRtpTransceiver;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::sdp::extmap::SDES_MID_URI;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};
use webrtc::track::track_remote::TrackRemote;

use crate::state::AppState;

/// (board_id, username) — a participant's identity within one voice board.
type ParticipantKey = (String, String);

/// The single UDP port every voice connection is muxed over, instead of
/// each connection allocating its own from a range. All ICE traffic for
/// every participant, in every call, shares this one bound socket — the
/// library demuxes incoming packets to the right connection using each
/// ICE agent's ufrag (present in the first STUN packet from a given
/// address, part of the standard ICE/STUN handshake), then by source
/// address after that — this is the same underlying idea as "tag traffic
/// by its source port," just handled by the library rather than hand-
/// rolled, and it's what makes exactly one small, fixed firewall/port-
/// forwarding rule enough regardless of how many participants are on a
/// call or how many are behind NATs of their own on the open internet.
///
/// This project tried this once before and reverted it after seeing
/// constant, audible roughness with it — but that test ran with every
/// connection also gathering 5+ redundant host candidates per participant
/// (one per virtual/VPN/Docker interface on the server machine, on top of
/// the one that actually mattered), all sharing this same socket. The
/// interface filter below exists specifically to cut that down; if
/// roughness shows up again after this change, that's the next thing to
/// suspect, but the two problems (too many ports needed, and too much
/// redundant ICE traffic sharing one socket) share the same root cause,
/// so it's worth trying together rather than assuming the single port
/// alone was ever the actual problem.
const ICE_UDP_PORT: u16 = 50000;

pub struct VoiceRuntime {
    api: API,
    /// Each participant's live PeerConnection to the server.
    connections: AsyncMutex<HashMap<ParticipantKey, Arc<RTCPeerConnection>>>,
    /// Each participant's own forwarded audio — written to by their
    /// on_track handler, added as an outgoing track to everyone else's
    /// connection so a single write fans out to the whole room.
    sources: AsyncMutex<HashMap<ParticipantKey, Arc<TrackLocalStaticRTP>>>,
    /// For each participant's connection, the set of other usernames it
    /// already has a transceiver attached for — whether from their own
    /// initial offer's expected_others, or from a later seamless
    /// renegotiation when someone new joined after them. This is what lets
    /// renegotiation add only what's actually missing, and lets it safely
    /// run more than once for the same participant without duplicating
    /// attachments.
    attached_peers: AsyncMutex<HashMap<ParticipantKey, HashSet<String>>>,
}

impl VoiceRuntime {
    pub async fn new() -> Self {
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .expect("register default codecs");
        // Every offer here has multiple audio m= sections bundled together
        // (this participant's own mic, plus one receive-only placeholder
        // per other participant — see handle_offer's own comment on
        // expected_others). Without this, the server has no negotiated way
        // to tell incoming packets on different sections apart, and falls
        // back to a probing path that only works reliably when every
        // packet's `mid` RTP header extension gets through — occasionally
        // it doesn't, and that participant's audio silently stops for the
        // rest of that connection ("Incoming unhandled RTP ssrc(...), on_
        // track will not be fired. mid RTP Extensions required for
        // Simulcast" in the server log). Registering it here is what makes
        // the server actually negotiate and use it. Confirmed against this
        // project's exact pinned v0.11 source, where this identical
        // register_header_extension pattern is already used elsewhere
        // (interceptor_registry, for a different extension) — not just a
        // method signature, but code known to compile and run in this
        // exact version.
        media_engine
            .register_header_extension(
                RTCRtpHeaderExtensionCapability { uri: SDES_MID_URI.to_owned() },
                RTPCodecType::Audio,
                None,
            )
            .expect("register mid header extension for audio");
        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)
            .expect("register default interceptors");
        let socket = UdpSocket::bind(("0.0.0.0", ICE_UDP_PORT))
            .await
            .expect("failed to bind voice UDP mux port — is something else already using it?");
        let udp_mux = UDPMuxDefault::new(UDPMuxParams::new(socket));
        let mut setting_engine = SettingEngine::default();
        setting_engine.set_udp_network(UDPNetwork::Muxed(udp_mux));
        // Excludes common virtual/VPN/container interface name patterns
        // from ICE candidate gathering — confirmed true=keep/false=exclude
        // against pion's own documentation for this identical API (this
        // Rust crate is an explicit port of it). Logged at info level (the
        // default visible level, no RUST_LOG needed) specifically so it's
        // easy to confirm which interfaces actually got excluded on a
        // given machine, since this project has no visibility into what
        // any particular server host's real interface list looks like.
        setting_engine.set_interface_filter(Box::new(|interface_name: &str| {
            let excluded_prefixes = [
                "docker", "veth", "br-", "tun", "tap", "wg", "vbox", "vmnet", "virbr",
            ];
            let keep = !excluded_prefixes.iter().any(|p| interface_name.starts_with(p));
            tracing::info!(
                "voice: ICE interface {interface_name}: {}",
                if keep { "included" } else { "excluded" }
            );
            keep
        }));
        let api = APIBuilder::new()
            .with_setting_engine(setting_engine)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();
        Self {
            api,
            connections: AsyncMutex::new(HashMap::new()),
            sources: AsyncMutex::new(HashMap::new()),
            attached_peers: AsyncMutex::new(HashMap::new()),
        }
    }
}

/// Handles an incoming SDP offer for a (board, user) pair: tears down any
/// existing connection for that exact pair, and returns the SDP answer to
/// send back over the signaling WebSocket.
///
/// `expected_others` is the ordered list of usernames the client declared
/// placeholder receive-only transceivers for when it built this offer —
/// transceiver index 0 is always this participant's own mic (from
/// AddTrack on their side), and transceiver index i+1 corresponds to
/// expected_others[i]. This is what makes attaching each other
/// participant's audio valid: an answer can't declare more media sections
/// than the offer did, so the client has to reserve the slots upfront
/// rather than the server trying to invent extra ones. `others` is the
/// server's own, independently-tracked roster — kept only for logging, so
/// a mismatch between what the client expected and what the server
/// actually knows is visible rather than silently papered over.
pub async fn handle_offer(
    state: &Arc<AppState>,
    board_id: &str,
    username: &str,
    offer_sdp: &str,
    others: &[String],
    expected_others: &[String],
) -> Result<String, String> {
    tracing::info!(
        "voice: offer from {username} for board {board_id} — server knows of {:?}, client expected {:?}",
        others, expected_others,
    );
    let key: ParticipantKey = (board_id.to_string(), username.to_string());

    // Tear down any previous connection for this exact participant first —
    // this is what makes a "someone joined/left, please refresh" re-offer
    // work correctly: it's handled identically to a first-time join,
    // nothing about this path needs to know which case it is. Note this
    // only removes the PeerConnection, not the source track below — see
    // its own comment for why that has to persist across a refresh.
    if let Some(old_pc) = state.voice_runtime.connections.lock().await.remove(&key) {
        let _ = old_pc.close().await;
    }

    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };
    let pc = Arc::new(
        state
            .voice_runtime
            .api
            .new_peer_connection(config)
            .await
            .map_err(|e| format!("failed to create peer connection: {e}"))?,
    );

    // This participant's own outgoing audio, once their microphone track
    // arrives below. Reuses the existing source object for this
    // (board, username) if a previous connection already registered one,
    // rather than creating a fresh one on every refresh: other
    // participants' connections may already have this exact object
    // attached to one of their transceivers, and replacing it out from
    // under them would silently orphan their attachment — their
    // transceiver would keep pointing at a track nobody writes to
    // anymore, with nothing to tell them to reattach to a new one. The
    // source is this participant's stable identity across reconnects;
    // only the PeerConnection writing into it actually needs rebuilding.
    let mut is_new_participant = false;
    let my_source = {
        let mut sources = state.voice_runtime.sources.lock().await;
        match sources.get(&key) {
            Some(existing) => Arc::clone(existing),
            None => {
                is_new_participant = true;
                let fresh = Arc::new(TrackLocalStaticRTP::new(
                    RTCRtpCodecCapability {
                        mime_type: "audio/opus".to_owned(),
                        clock_rate: 48000,
                        channels: 1,
                        ..Default::default()
                    },
                    format!("audio-{username}"),
                    format!("chriscord-{username}"),
                ));
                sources.insert(key.clone(), Arc::clone(&fresh));
                fresh
            }
        }
    };

    // When this participant's own microphone track arrives, forward every
    // RTP packet into their shared outgoing source — this is the actual
    // "selective forwarding" step; every other participant's connection
    // that has attached this same track object receives these packets too.
    let forward_target = Arc::clone(&my_source);
    let username_for_track = username.to_string();
    pc.on_track(Box::new(
        move |remote: Arc<TrackRemote>, _receiver: Arc<RTCRtpReceiver>, _transceiver: Arc<RTCRtpTransceiver>| {
            let forward_target = Arc::clone(&forward_target);
            let username_for_track = username_for_track.clone();
            Box::pin(async move {
                loop {
                    match remote.read_rtp().await {
                        Ok((packet, _attrs)) => {
                            if forward_target.write_rtp(&packet).await.is_err() {
                                break; // no one listening anymore — connection likely closing
                            }
                        }
                        Err(_) => {
                            tracing::debug!("voice: remote track ended for {username_for_track}");
                            break;
                        }
                    }
                }
            })
        },
    ));

    // Trickle ICE candidates back to this participant as they're
    // gathered, over the same WebSocket they're already connected on.
    // Delivery itself (finding their connection and sending) lives in
    // ws.rs, which owns the actual socket — this just hands candidates
    // off as they're found.
    let state_for_ice = Arc::clone(state);
    let username_for_ice = username.to_string();
    pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
        let state_for_ice = Arc::clone(&state_for_ice);
        let username_for_ice = username_for_ice.clone();
        Box::pin(async move {
            let candidate = match candidate {
                Some(c) => c,
                None => return, // gathering finished — nothing further to send
            };
            let init = match candidate.to_json() {
                Ok(init) => init,
                Err(e) => {
                    tracing::warn!("voice: failed to serialize ICE candidate: {e}");
                    return;
                }
            };
            crate::ws::send_to_user(
                &state_for_ice,
                &username_for_ice,
                serde_json::json!({
                    "type": "voice_ice",
                    "candidate": init.candidate,
                    "sdp_mid": init.sdp_mid,
                    "sdp_mline_index": init.sdp_mline_index,
                }),
            );
        })
    }));

    let remote_desc = RTCSessionDescription::offer(offer_sdp.to_string())
        .map_err(|e| format!("invalid offer SDP: {e}"))?;
    pc.set_remote_description(remote_desc)
        .await
        .map_err(|e| format!("set_remote_description failed: {e}"))?;

    // The offer's own transceivers now exist, created from its media
    // sections by set_remote_description above: index 0 is this
    // participant's own mic (already covered by on_track), and indices
    // 1.. are the placeholder receive-only slots the client declared, one
    // per expected_others entry, in the same order. Attach each known
    // participant's already-forwarded audio to its matching placeholder
    // by position — this is what a plain add_track before
    // set_remote_description couldn't validly do, since an answer can't
    // declare more media sections than the offer did. A placeholder with
    // nothing to attach yet (someone who hasn't finished negotiating), or
    // an offer that declared more placeholders than expected_others names
    // (always at least one, even solo, to avoid a known pion/webrtc-rs bug
    // where a single-media-section offer's on_track never fires at all),
    // simply stays empty and unused — harmless.
    let transceivers = pc.get_transceivers().await;
    let mut newly_attached: Vec<String> = Vec::new();
    {
        let sources = state.voice_runtime.sources.lock().await;
        for (i, other) in expected_others.iter().enumerate() {
            let transceiver_index = i + 1; // index 0 is this participant's own mic
            let Some(transceiver) = transceivers.get(transceiver_index) else {
                tracing::warn!(
                    "voice: {username}'s offer didn't declare a placeholder for {other} at index {transceiver_index}"
                );
                continue;
            };
            let other_key = (board_id.to_string(), other.clone());
            let Some(track) = sources.get(&other_key) else {
                continue; // other participant hasn't finished negotiating yet
            };
            let track_dyn: Arc<dyn TrackLocal + Send + Sync> = Arc::clone(track) as _;
            let sender = transceiver.sender().await;
            if let Err(e) = transceiver.set_sender_track(sender, Some(track_dyn)).await {
                tracing::warn!("voice: failed to attach {other}'s audio to {username}'s connection: {e}");
                continue;
            }
            transceiver.set_direction(RTCRtpTransceiverDirection::Sendonly).await;
            newly_attached.push(other.clone());
        }
    }
    if !newly_attached.is_empty() {
        let mut attached = state.voice_runtime.attached_peers.lock().await;
        attached.entry(key.clone()).or_default().extend(newly_attached);
    }

    let answer = pc
        .create_answer(None)
        .await
        .map_err(|e| format!("create_answer failed: {e}"))?;
    pc.set_local_description(answer.clone())
        .await
        .map_err(|e| format!("set_local_description failed: {e}"))?;

    state.voice_runtime.connections.lock().await.insert(key, Arc::clone(&pc));

    // A genuinely new participant (not a refresh/reconnect of an existing
    // one) — trigger seamless renegotiation on everyone else's EXISTING
    // connections in the background, so they hear this new participant
    // without their own connection ever being touched. Spawned rather than
    // awaited here so this new participant's own answer isn't held up
    // waiting on however many other renegotiations there are to do.
    if is_new_participant {
        let state_for_renegotiate = Arc::clone(state);
        let board_id_owned = board_id.to_string();
        let username_owned = username.to_string();
        tokio::spawn(async move {
            renegotiate_others_for_new_source(&state_for_renegotiate, &board_id_owned, &username_owned).await;
        });
    }

    Ok(answer.sdp)
}

/// Renegotiates every other current participant's EXISTING connection on
/// this board, in sequence, to add the newly-joined participant's audio —
/// this is the actual "seamless" mechanism: their own established
/// PeerConnection, ICE state, and already-flowing audio to and from them
/// are never touched. Processed one at a time (not concurrently) so two
/// people joining moments apart can't both try to renegotiate the same
/// existing connection at once, which the WebRTC signaling state machine
/// doesn't allow — a connection has to return to "stable" before it can
/// process another offer.
async fn renegotiate_others_for_new_source(state: &Arc<AppState>, board_id: &str, new_username: &str) {
    let others: Vec<(ParticipantKey, Arc<RTCPeerConnection>)> = {
        let connections = state.voice_runtime.connections.lock().await;
        connections
            .iter()
            .filter(|((b, u), _)| b == board_id && u != new_username)
            .map(|(k, pc)| (k.clone(), Arc::clone(pc)))
            .collect()
    };
    for (key, pc) in others {
        if let Err(e) = renegotiate_one(state, &key, &pc, board_id).await {
            tracing::warn!("voice: renegotiation failed for {}: {e}", key.1);
        }
    }
}

/// Adds a transceiver (and attaches its audio) for every current board
/// member this one connection doesn't already have one for, then
/// renegotiates — create_offer/set_local_description on an ALREADY-
/// established PeerConnection is exactly how WebRTC renegotiation works;
/// nothing about the connection's existing transceivers, ICE state, or
/// already-flowing audio is affected by adding more. Checking against
/// attached_peers (rather than just adding one transceiver for the one
/// participant that triggered this call) makes this self-healing: if a
/// renegotiation for one new participant ever gets missed or races with
/// another, the very next renegotiation for this same connection picks up
/// anything still missing, rather than requiring perfect ordering.
async fn renegotiate_one(
    state: &Arc<AppState>,
    key: &ParticipantKey,
    pc: &Arc<RTCPeerConnection>,
    board_id: &str,
) -> Result<(), String> {
    let username = &key.1;
    let board_members: Vec<String> = {
        let voice = state.voice.lock().unwrap();
        voice
            .iter()
            .filter(|(u, b)| **b == *board_id && *u != username)
            .map(|(u, _)| u.clone())
            .collect()
        // voice's std::sync::MutexGuard is dropped here, at the end of this
        // block — deliberately, before the .await just below. Holding a
        // std::sync::Mutex guard across an await point isn't valid for a
        // future a tokio::spawn'd task might run (it requires Send, and
        // MutexGuard isn't), unlike the AsyncMutex guards used elsewhere in
        // this file which are fine to hold across an await.
    };
    let missing: Vec<String> = {
        let attached = state.voice_runtime.attached_peers.lock().await;
        let already = attached.get(key).cloned().unwrap_or_default();
        board_members.into_iter().filter(|u| !already.contains(u)).collect()
    };
    if missing.is_empty() {
        return Ok(());
    }

    let mut newly_attached = Vec::new();
    {
        let sources = state.voice_runtime.sources.lock().await;
        for other in &missing {
            let other_key = (board_id.to_string(), other.clone());
            let Some(track) = sources.get(&other_key) else {
                continue; // that other participant hasn't finished negotiating yet
            };
            let transceiver = pc
                .add_transceiver_from_kind(
                    RTPCodecType::Audio,
                    Some(RTCRtpTransceiverInit { direction: RTCRtpTransceiverDirection::Recvonly, send_encodings: vec![] }),
                )
                .await
                .map_err(|e| format!("add_transceiver_from_kind for {other}: {e}"))?;
            let track_dyn: Arc<dyn TrackLocal + Send + Sync> = Arc::clone(track) as _;
            let sender = transceiver.sender().await;
            transceiver
                .set_sender_track(sender, Some(track_dyn))
                .await
                .map_err(|e| format!("set_sender_track for {other}: {e}"))?;
            transceiver.set_direction(RTCRtpTransceiverDirection::Sendonly).await;
            newly_attached.push(other.clone());
        }
    }
    if newly_attached.is_empty() {
        return Ok(()); // everyone missing was still mid-negotiation — nothing to renegotiate yet
    }

    let offer = pc.create_offer(None).await.map_err(|e| format!("create_offer: {e}"))?;
    pc.set_local_description(offer.clone())
        .await
        .map_err(|e| format!("set_local_description: {e}"))?;

    {
        let mut attached = state.voice_runtime.attached_peers.lock().await;
        attached.entry(key.clone()).or_default().extend(newly_attached);
    }

    crate::ws::send_to_user(
        state,
        username,
        serde_json::json!({ "type": "voice_renegotiate", "board_id": board_id, "sdp": offer.sdp }),
    );
    Ok(())
}

/// Applies the client's answer to a server-initiated renegotiation (see
/// renegotiate_one above) — set_remote_description on the SAME, already-
/// established PeerConnection, completing that offer/answer round without
/// ever having torn anything down. If there's no matching connection
/// (e.g. it raced with the participant leaving), this is silently a
/// no-op rather than an error, same reasoning as handle_ice_candidate
/// below.
pub async fn handle_renegotiate_answer(
    state: &Arc<AppState>,
    board_id: &str,
    username: &str,
    answer_sdp: &str,
) {
    let key: ParticipantKey = (board_id.to_string(), username.to_string());
    let Some(pc) = state.voice_runtime.connections.lock().await.get(&key).cloned() else {
        return;
    };
    let Ok(remote_desc) = RTCSessionDescription::answer(answer_sdp.to_string()) else {
        tracing::warn!("voice: invalid renegotiation answer SDP from {username}");
        return;
    };
    if let Err(e) = pc.set_remote_description(remote_desc).await {
        tracing::warn!("voice: set_remote_description failed for {username}'s renegotiation answer: {e}");
    }
}

/// Applies an ICE candidate the client sent for its own (board, user)
/// connection. If there's no matching connection (e.g. it raced with a
/// refresh and got torn down), this is silently a no-op rather than an
/// error — a late-arriving candidate for a connection that no longer
/// exists is a normal, expected race in ICE trickling, not a bug.
pub async fn handle_ice_candidate(
    state: &Arc<AppState>,
    board_id: &str,
    username: &str,
    candidate: RTCIceCandidateInit,
) {
    let key: ParticipantKey = (board_id.to_string(), username.to_string());
    if let Some(pc) = state.voice_runtime.connections.lock().await.get(&key) {
        if let Err(e) = pc.add_ice_candidate(candidate).await {
            tracing::debug!("voice: add_ice_candidate failed for {username}: {e}");
        }
    }
}

/// Tears down a participant's voice connection entirely — called on an
/// explicit leave_voice, and on disconnect so a dropped connection doesn't
/// leave a dangling PeerConnection and audio source behind.
pub async fn close_participant(state: &Arc<AppState>, board_id: &str, username: &str) {
    let key: ParticipantKey = (board_id.to_string(), username.to_string());
    if let Some(pc) = state.voice_runtime.connections.lock().await.remove(&key) {
        let _ = pc.close().await;
    }
    state.voice_runtime.sources.lock().await.remove(&key);
    // Also drop any references to this participant from everyone else's
    // attached_peers sets — otherwise, if they rejoin later (with a fresh
    // source object), renegotiation would wrongly think everyone already
    // has them attached and skip re-adding them.
    {
        let mut attached = state.voice_runtime.attached_peers.lock().await;
        attached.remove(&key);
        for set in attached.values_mut() {
            set.remove(username);
        }
    }
}
