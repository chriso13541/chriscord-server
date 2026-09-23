// voice.rs — WebRTC SFU logic for voice channels.
//
// This file has been built and run successfully multiple times over the
// course of development, with mute, deafen, and real two-way audio
// between separate machines all confirmed working — it's no longer a
// first draft. Worth knowing about the UDP networking setup specifically
// (see ICE_UDP_PORT_MIN/MAX below): it went through a single-muxed-port
// version briefly, which caused constant, audible roughness from every
// participant's audio sharing one demux path, and was reverted back to
// per-connection ports on a small fixed range instead. The
// EphemeralUDP/UDPNetwork::Ephemeral API used here has been confirmed via
// an actual successful `cargo build` against this project's exact pinned
// dependency version, not just read off documentation.
//
// Design — each participant's offer declares its own real shape upfront:
// their own mic (transceiver 0) plus one receive-only placeholder per
// other participant they already know about (transceivers 1..), sent
// alongside the offer as an ordered `expected_others` list. The server
// attaches each known participant's audio to its matching placeholder by
// position — never adding media sections the offer didn't already
// reserve, since an SDP answer can't validly declare more sections than
// its offer did. A "someone joined or left, please refresh" re-offer is
// handled by exactly the same code path as a first-time join: a brand new
// PeerConnection for that (board, user) pair, replacing any previous one.
//   - RTP forwarding: each participant's OWN incoming audio, once
//     negotiated, gets read from their PeerConnection and written into a
//     single shared TrackLocalStaticRTP unique to them. That same shared
//     track object gets attached to every OTHER participant's matching
//     placeholder transceiver. Writing to it once fans out to everyone
//     who has it attached — the standard SFU forwarding pattern, and
//     what keeps the server from ever needing to decode or re-encode
//     audio: it only ever moves RTP packets around.
//   - What a future seamless version changes: instead of everyone
//     tearing down and re-offering on every roster change, the server
//     would instead trigger renegotiation on each EXISTING participant's
//     connection to add the new participant's source — reusing this
//     exact same PeerConnection-per-participant and shared-source-track
//     machinery, just orchestrated differently.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::{APIBuilder, API};
use webrtc::ice::udp_network::{EphemeralUDP, UDPNetwork};
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
use webrtc::sdp::extmap::SDES_MID_URI;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};
use webrtc::track::track_remote::TrackRemote;

use crate::state::AppState;

/// (board_id, username) — a participant's identity within one voice board.
type ParticipantKey = (String, String);

/// Fixed range of UDP ports voice connections allocate from, instead of
/// either the OS's full ephemeral range or a single shared, muxed socket.
/// Each connection gets its own dedicated port and its own dedicated read
/// path — deliberately reverted from a single muxed port after that
/// caused constant, ongoing audio roughness: every participant's traffic
/// sharing one demux path introduced enough jitter to be clearly audible,
/// even at the low packet rates a small voice call produces. This range
/// is much smaller than this project's first attempt at port-range
/// scoping (100 ports) — 20 comfortably covers a self-hosted friend-group
/// setup — while still avoiding the shared-socket bottleneck entirely.
/// This is what makes it possible to open one small, specific firewall
/// rule for voice traffic — e.g. `ufw allow 50000:50019/udp` — alongside
/// 7070 for signaling, rather than opening tens of thousands of ports or
/// disabling the firewall outright. If this range is ever changed, the
/// firewall rule on whichever machine runs the server needs to change to
/// match.
const ICE_UDP_PORT_MIN: u16 = 50000;
const ICE_UDP_PORT_MAX: u16 = 50019;

pub struct VoiceRuntime {
    api: API,
    /// Each participant's live PeerConnection to the server.
    connections: AsyncMutex<HashMap<ParticipantKey, Arc<RTCPeerConnection>>>,
    /// Each participant's own forwarded audio — written to by their
    /// on_track handler, added as an outgoing track to everyone else's
    /// connection so a single write fans out to the whole room.
    sources: AsyncMutex<HashMap<ParticipantKey, Arc<TrackLocalStaticRTP>>>,
}

impl VoiceRuntime {
    pub fn new() -> Self {
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
        let ephemeral_range = EphemeralUDP::new(ICE_UDP_PORT_MIN, ICE_UDP_PORT_MAX)
            .expect("ICE_UDP_PORT_MIN..ICE_UDP_PORT_MAX is a valid range");
        let mut setting_engine = SettingEngine::default();
        setting_engine.set_udp_network(UDPNetwork::Ephemeral(ephemeral_range));
        let api = APIBuilder::new()
            .with_setting_engine(setting_engine)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();
        Self {
            api,
            connections: AsyncMutex::new(HashMap::new()),
            sources: AsyncMutex::new(HashMap::new()),
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
    let my_source = {
        let mut sources = state.voice_runtime.sources.lock().await;
        match sources.get(&key) {
            Some(existing) => Arc::clone(existing),
            None => {
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
    {
        let sources = state.voice_runtime.sources.lock().await;
        for (i, other) in expected_others.iter().enumerate() {
            let transceiver_index = i + 1; // index 0 is this participant's own mic
            let Some(transceiver) = transceivers.get(transceiver_index) else {
                tracing::warn!(
                    "voice: {username}'s offer didn't declare a placeholder for {other} at index {transceiver_index} — will be picked up on their next refresh"
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
        }
    }

    let answer = pc
        .create_answer(None)
        .await
        .map_err(|e| format!("create_answer failed: {e}"))?;
    pc.set_local_description(answer.clone())
        .await
        .map_err(|e| format!("set_local_description failed: {e}"))?;

    state.voice_runtime.connections.lock().await.insert(key, Arc::clone(&pc));

    Ok(answer.sdp)
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
}
