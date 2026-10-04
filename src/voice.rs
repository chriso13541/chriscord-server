// voice.rs — WebRTC SFU logic for voice channels.
//
// This file has been built and run successfully multiple times over the
// course of development, with mute, deafen, and real two-way audio
// between separate machines all confirmed working — it's no longer a
// first draft. Worth knowing about the UDP networking setup specifically:
// all voice traffic now runs over ONE muxed UDP port (ICE_UDP_PORT, the
// same number as the HTTP/WebSocket port, so the router only needs 7070
// forwarded as TCP+UDP). See ICE_UDP_PORT's own comment for why earlier
// single-port attempts failed and what makes this one work.
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
//
// Video (webcams) rides the same connections. A client's offer may carry
// one extra send-only video section, AFTER all the audio placeholders (so
// the by-position audio mapping above is untouched). Its packets only
// start once that person turns their camera on — that's when on_track
// fires for it, and the server makes a per-person video source just like
// the audio one and renegotiates everyone else's connection to add it
// (renegotiate_one handles audio and video together). Someone who joins
// later gets everyone's existing video added by a renegotiation shortly
// after their own answer. Turning a camera off just stops the packets;
// whether a camera is on is signalled separately (voice_video in ws.rs),
// which is what the apps go by. Video is only ever forwarded, never
// decoded — a viewer whose picture needs a fresh start asks for a
// keyframe (voice_keyframe), which reaches the sender as an RTCP PLI.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::Mutex as AsyncMutex;

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::{APIBuilder, API};
use webrtc::ice::network_type::NetworkType;
use webrtc::ice::udp_mux::{UDPMuxDefault, UDPMuxParams};
use webrtc::ice::udp_network::UDPNetwork;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::signaling_state::RTCSignalingState;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
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

/// The single UDP port all voice connections share (muxed by ICE ufrag,
/// then by remote address). Deliberately the same number as the TCP
/// HTTP/WebSocket port so the router only needs 7070 forwarded as TCP+UDP.
///
/// Why the earlier single-port attempts failed (roughness the first time,
/// no audio at all the second, with "Discarded message, not a valid remote
/// candidate" in the log): in webrtc-ice 0.11, gather_candidates_local_
/// udp_mux creates one host candidate per local IP that passes the
/// filters, all sharing the muxed socket, and start_candidate spawns a
/// separate recv_loop per candidate on that same socket. Those loops race
/// for every packet, and each validates a packet against its OWN
/// candidate's network type — so e.g. an IPv6 candidate's loop reading a
/// packet from an IPv4 client drops it. The interface filter didn't help
/// because it filters interfaces, not address families: the real
/// interface still contributed its IPv6 addresses.
///
/// The fix is to guarantee exactly ONE host candidate per connection:
/// IPv4-only gathering (set_network_types) plus, optionally, pinning one
/// exact IP (CHRISCORD_ICE_IP via set_ip_filter). With one candidate there
/// is one reader, and nothing to misroute.
///
/// Loopback is always excluded too: 127.0.0.1 is IPv4, so Udp4-only
/// doesn't stop it, and it's a second candidate (second reader) on the
/// same socket that no client can ever reach anyway.
///
/// Reaching the server from outside the LAN: muxed mode skips STUN
/// (server-reflexive) gathering entirely, and in webrtc-ice 0.11 that
/// includes the nat_1to1 "srflx" mapping (agent_gather.rs), so the only
/// built-in option is nat_1to1 "host", which REPLACES the LAN address with
/// the public one — LAN clients then depend on router NAT loopback. So
/// instead, when CHRISCORD_PUBLIC_IP is set, every private-address host
/// candidate is sent to the client twice: once as-is (LAN clients use it)
/// and once with the public IP swapped in (see public_candidate_copy).
/// Checks a remote client sends to publicIP:7070 are port-forwarded to
/// this socket, routed to the right connection by ICE ufrag, and answered
/// from the same socket — so the router's port-forward mapping carries
/// the replies back out. Without CHRISCORD_PUBLIC_IP, only LAN clients can
/// connect: a remote client is only ever told the private LAN address.
const ICE_UDP_PORT: u16 = 7070;

/// The server's public IPv4 address, from CHRISCORD_PUBLIC_IP (read once).
fn public_ip() -> Option<&'static str> {
    static PUBLIC_IP: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    PUBLIC_IP
        .get_or_init(|| {
            std::env::var("CHRISCORD_PUBLIC_IP").ok()
                .map(|v| v.trim().to_string())
                .filter(|v| v.parse::<std::net::Ipv4Addr>().is_ok())
        })
        .as_deref()
}

/// Given one of this server's ICE candidate lines, returns a copy
/// advertising `public_ip` in place of a private (RFC 1918) host address —
/// same port, a distinct foundation, and a slightly lower priority so a
/// client that can reach the LAN address still prefers it. Anything that
/// isn't a private IPv4 host candidate is left alone (None).
fn public_candidate_copy(candidate: &str, public_ip: &str) -> Option<String> {
    let mut parts: Vec<String> = candidate.split_whitespace().map(str::to_string).collect();
    // candidate:<foundation> <component> <transport> <priority> <address> <port> typ <type> ...
    if parts.len() < 8 || parts[6] != "typ" || parts[7] != "host" {
        return None;
    }
    let addr: std::net::Ipv4Addr = parts[4].parse().ok()?;
    if !addr.is_private() || parts[4] == public_ip {
        return None;
    }
    let priority: u32 = parts[3].parse().ok()?;
    parts[0] = format!("{}p", parts[0]);
    parts[3] = priority.saturating_sub(1).to_string();
    parts[4] = public_ip.to_string();
    Some(parts.join(" "))
}

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
    /// Each participant's forwarded webcam video, made the first time their
    /// camera's packets arrive (see the design note at the top).
    video_sources: AsyncMutex<HashMap<ParticipantKey, Arc<TrackLocalStaticRTP>>>,
    /// The SSRC of each participant's incoming camera stream on their
    /// current connection — where a keyframe request (PLI) has to point.
    video_ssrc: AsyncMutex<HashMap<ParticipantKey, u32>>,
    /// Like attached_peers, for video: whose camera each connection
    /// already has a send-only video section for.
    video_attached: AsyncMutex<HashMap<ParticipantKey, HashSet<String>>>,
    /// One renegotiation at a time per connection — a connection has to be
    /// back to "stable" (its last offer answered) before the next offer.
    reneg_locks: AsyncMutex<HashMap<ParticipantKey, Arc<AsyncMutex<()>>>>,
    /// When each camera was last asked for a keyframe, so a burst of
    /// requests (several viewers at once) becomes one.
    keyframe_last: AsyncMutex<HashMap<ParticipantKey, Instant>>,
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
        // Same for video, which shares the bundle once cameras are on.
        media_engine
            .register_header_extension(
                RTCRtpHeaderExtensionCapability { uri: SDES_MID_URI.to_owned() },
                RTPCodecType::Video,
                None,
            )
            .expect("register mid header extension for video");
        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)
            .expect("register default interceptors");
        let socket = UdpSocket::bind(("0.0.0.0", ICE_UDP_PORT))
            .await
            .expect("failed to bind voice UDP port — is something else already using it?");
        tracing::info!("voice: all voice traffic muxed over UDP port {ICE_UDP_PORT}");
        let udp_mux = UDPMuxDefault::new(UDPMuxParams::new(socket));
        let mut setting_engine = SettingEngine::default();
        setting_engine.set_udp_network(UDPNetwork::Muxed(udp_mux));
        // IPv4 only — this is what stops IPv6 addresses on the real
        // interface from adding extra candidates (and extra readers) on the
        // shared socket. See ICE_UDP_PORT's comment.
        setting_engine.set_network_types(vec![NetworkType::Udp4]);
        // Never gather loopback (see ICE_UDP_PORT). Optionally pin the
        // single exact IP to gather from, e.g. CHRISCORD_ICE_IP=192.168.1.50
        // — useful if the chosen interface has more than one IPv4 address.
        let wanted: Option<std::net::IpAddr> = std::env::var("CHRISCORD_ICE_IP").ok().map(|ip| {
            ip.trim().parse().expect("CHRISCORD_ICE_IP must be a valid IP address")
        });
        if let Some(w) = wanted {
            tracing::info!("voice: CHRISCORD_ICE_IP set — only gathering from {w}");
        }
        setting_engine.set_ip_filter(Box::new(move |addr: std::net::IpAddr| {
            !addr.is_loopback() && wanted.map_or(true, |w| addr == w)
        }));
        // Clients outside the LAN get the public IP as an extra candidate
        // (see ICE_UDP_PORT). e.g. CHRISCORD_PUBLIC_IP=203.0.113.7
        match (public_ip(), std::env::var("CHRISCORD_PUBLIC_IP")) {
            (Some(ip), _) => tracing::info!(
                "voice: CHRISCORD_PUBLIC_IP={ip} — advertising it alongside the LAN address (forward UDP {ICE_UDP_PORT} to this machine)"
            ),
            (None, Ok(bad)) if !bad.trim().is_empty() => tracing::warn!(
                "voice: CHRISCORD_PUBLIC_IP={bad:?} is not an IPv4 address — ignoring it; only LAN clients can connect to voice"
            ),
            _ => tracing::warn!(
                "voice: CHRISCORD_PUBLIC_IP not set — only LAN clients can connect to voice"
            ),
        }
        // Filters which network interfaces ICE gathers candidates from —
        // confirmed true=keep/false=exclude against pion's own
        // documentation for this identical API (this Rust crate is an
        // explicit port of it). Logged at info level (the default visible
        // level, no RUST_LOG needed) so it's easy to confirm which
        // interfaces actually got included or excluded on a given machine.
        // The interface(s) ICE should actually gather candidates from can
        // be set explicitly via CHRISCORD_ICE_INTERFACES — a comma-
        // separated list of exact interface names, e.g. "eth0" or
        // "eth0,wlan0". This takes priority whenever set, and is the
        // precise, no-guessing alternative to the exclusion heuristic
        // below: find the right value with `ip route get 8.8.8.8` (the
        // name shown after "dev" is the interface actually used to reach
        // the internet) or `ip addr` (match against whichever IP is
        // already known to be the real one). Without it set, this falls
        // back to excluding common virtual/VPN/container interface name
        // patterns — a reasonable default, but a guess at naming
        // conventions rather than a confirmed fit for any specific
        // machine, since this project has no way to know a given server
        // host's actual interface list ahead of time.
        let explicit_interfaces: Vec<String> = std::env::var("CHRISCORD_ICE_INTERFACES")
            .ok()
            .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        if explicit_interfaces.is_empty() {
            tracing::info!(
                "voice: CHRISCORD_ICE_INTERFACES not set — falling back to excluding common virtual/VPN/container interface name patterns"
            );
        } else {
            tracing::info!("voice: CHRISCORD_ICE_INTERFACES set — only gathering candidates from: {explicit_interfaces:?}");
        }
        setting_engine.set_interface_filter(Box::new(move |interface_name: &str| {
            let keep = if explicit_interfaces.is_empty() {
                let excluded_prefixes = [
                    "docker", "veth", "br-", "tun", "tap", "wg", "vbox", "vmnet", "virbr",
                ];
                !excluded_prefixes.iter().any(|p| interface_name.starts_with(p))
            } else {
                explicit_interfaces.iter().any(|i| i == interface_name)
            };
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
            video_sources: AsyncMutex::new(HashMap::new()),
            video_ssrc: AsyncMutex::new(HashMap::new()),
            video_attached: AsyncMutex::new(HashMap::new()),
            reneg_locks: AsyncMutex::new(HashMap::new()),
            keyframe_last: AsyncMutex::new(HashMap::new()),
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
    // A fresh connection starts with none of anyone's video attached.
    state.voice_runtime.video_attached.lock().await.remove(&key);

    let config = RTCConfiguration {
        // Note: with the muxed UDP port, webrtc-ice 0.11 skips STUN
        // gathering entirely, so these servers are currently unused — the
        // public address comes from CHRISCORD_PUBLIC_IP instead (see
        // ICE_UDP_PORT). Kept so switching back to an ephemeral port range
        // wouldn't also need this restored.
        ice_servers: vec![
            RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                ..Default::default()
            },
            RTCIceServer {
                urls: vec!["stun:stun.cloudflare.com:3478".to_owned()],
                ..Default::default()
            },
        ],
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
    let state_for_track = Arc::clone(state);
    let key_for_track = key.clone();
    pc.on_track(Box::new(
        move |remote: Arc<TrackRemote>, _receiver: Arc<RTCRtpReceiver>, _transceiver: Arc<RTCRtpTransceiver>| {
            let audio_target = Arc::clone(&forward_target);
            let username_for_track = username_for_track.clone();
            let state = Arc::clone(&state_for_track);
            let key = key_for_track.clone();
            Box::pin(async move {
                // Their camera (it only arrives once it's turned on), or their mic.
                let forward_target = if remote.kind() == RTPCodecType::Video {
                    tracing::info!("voice: {username_for_track}'s camera stream arrived");
                    video_source_for(&state, &key, remote.ssrc()).await
                } else {
                    audio_target
                };
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
            // The public-IP twin of a LAN candidate, for clients outside the
            // LAN (see ICE_UDP_PORT) — sent right after the original.
            let public_copy = public_ip().and_then(|ip| public_candidate_copy(&init.candidate, ip));
            for candidate in std::iter::once(init.candidate.clone()).chain(public_copy) {
                crate::ws::send_to_user(
                    &state_for_ice,
                    &username_for_ice,
                    serde_json::json!({
                        "type": "voice_ice",
                        "candidate": candidate,
                        "sdp_mid": init.sdp_mid,
                        "sdp_mline_index": init.sdp_mline_index,
                    }),
                );
            }
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

    state.voice_runtime.connections.lock().await.insert(key.clone(), Arc::clone(&pc));

    // Anyone on this board already sharing a camera: add their video to
    // this connection with a renegotiation once this answer has landed.
    let others_have_video = {
        let videos = state.voice_runtime.video_sources.lock().await;
        videos.keys().any(|(b, u)| b == board_id && u != username)
    };
    if others_have_video {
        let state_for_video = Arc::clone(state);
        let board_for_video = board_id.to_string();
        let key_for_video = key.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let pc = state_for_video.voice_runtime.connections.lock().await.get(&key_for_video).cloned();
            if let Some(pc) = pc {
                if let Err(e) = renegotiate_one(&state_for_video, &key_for_video, &pc, &board_for_video).await {
                    tracing::warn!("voice: adding existing cameras for {} failed: {e}", key_for_video.1);
                }
            }
        });
    }

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
    tracing::info!(
        "voice: {new_username} joined board {board_id}, renegotiating {} existing connection(s): {:?}",
        others.len(),
        others.iter().map(|(k, _)| &k.1).collect::<Vec<_>>()
    );
    for (key, pc) in others {
        if let Err(e) = renegotiate_one(state, &key, &pc, board_id).await {
            tracing::warn!("voice: renegotiation failed for {}: {e}", key.1);
        }
    }
}

/// Adds a transceiver (and attaches its audio) for every current board
/// member this one connection doesn't already have one for — and likewise
/// a video transceiver for every member sharing a camera it doesn't have
/// yet — then renegotiates: create_offer/set_local_description on an
/// ALREADY-established PeerConnection is exactly how WebRTC renegotiation
/// works; nothing about the connection's existing transceivers, ICE state,
/// or already-flowing media is affected by adding more. Checking against
/// attached_peers / video_attached (rather than just adding the one thing
/// that triggered this call) makes this self-healing: if a renegotiation
/// ever gets missed or races with another, the very next one for this
/// same connection picks up anything still missing.
///
/// One at a time per connection (reneg_locks), and only once its last
/// offer has been answered (signaling state back to stable) — WebRTC
/// doesn't allow a new offer while one is still outstanding.
async fn renegotiate_one(
    state: &Arc<AppState>,
    key: &ParticipantKey,
    pc: &Arc<RTCPeerConnection>,
    board_id: &str,
) -> Result<(), String> {
    let username = &key.1;
    let lock = {
        let mut locks = state.voice_runtime.reneg_locks.lock().await;
        Arc::clone(locks.entry(key.clone()).or_default())
    };
    let _one_at_a_time = lock.lock().await;
    for _ in 0..40 {
        if pc.signaling_state() == RTCSignalingState::Stable {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if pc.signaling_state() != RTCSignalingState::Stable {
        return Err("still waiting on the answer to an earlier renegotiation".to_string());
    }

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
        board_members.iter().filter(|u| !already.contains(*u)).cloned().collect()
    };
    let missing_video: Vec<String> = {
        let attached = state.voice_runtime.video_attached.lock().await;
        let already = attached.get(key).cloned().unwrap_or_default();
        let videos = state.voice_runtime.video_sources.lock().await;
        board_members
            .iter()
            .filter(|u| !already.contains(*u) && videos.contains_key(&(board_id.to_string(), (*u).clone())))
            .cloned()
            .collect()
    };
    if missing.is_empty() && missing_video.is_empty() {
        tracing::info!("voice: renegotiate_one for {username}: nothing missing, no-op");
        return Ok(());
    }
    tracing::info!("voice: renegotiate_one for {username}: missing audio={missing:?} video={missing_video:?}");

    let mut newly_attached = Vec::new();
    {
        let sources = state.voice_runtime.sources.lock().await;
        for other in &missing {
            let other_key = (board_id.to_string(), other.clone());
            let Some(track) = sources.get(&other_key) else {
                tracing::info!("voice: renegotiate_one for {username}: {other}'s source not ready yet, skipping this round");
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
    let mut newly_video = Vec::new();
    {
        let videos = state.voice_runtime.video_sources.lock().await;
        for other in &missing_video {
            let Some(track) = videos.get(&(board_id.to_string(), other.clone())) else { continue };
            let transceiver = pc
                .add_transceiver_from_kind(
                    RTPCodecType::Video,
                    Some(RTCRtpTransceiverInit { direction: RTCRtpTransceiverDirection::Recvonly, send_encodings: vec![] }),
                )
                .await
                .map_err(|e| format!("add video transceiver for {other}: {e}"))?;
            let track_dyn: Arc<dyn TrackLocal + Send + Sync> = Arc::clone(track) as _;
            let sender = transceiver.sender().await;
            transceiver
                .set_sender_track(sender, Some(track_dyn))
                .await
                .map_err(|e| format!("set video sender track for {other}: {e}"))?;
            transceiver.set_direction(RTCRtpTransceiverDirection::Sendonly).await;
            newly_video.push(other.clone());
        }
    }
    if newly_attached.is_empty() && newly_video.is_empty() {
        tracing::info!("voice: renegotiate_one for {username}: everyone missing was still mid-negotiation, no-op this round");
        return Ok(()); // everyone missing was still mid-negotiation — nothing to renegotiate yet
    }

    let offer = pc.create_offer(None).await.map_err(|e| format!("create_offer: {e}"))?;
    pc.set_local_description(offer.clone())
        .await
        .map_err(|e| format!("set_local_description: {e}"))?;

    if !newly_attached.is_empty() {
        let mut attached = state.voice_runtime.attached_peers.lock().await;
        attached.entry(key.clone()).or_default().extend(newly_attached.clone());
    }
    if !newly_video.is_empty() {
        let mut attached = state.voice_runtime.video_attached.lock().await;
        attached.entry(key.clone()).or_default().extend(newly_video.clone());
    }

    tracing::info!("voice: renegotiate_one sending offer to {username}, newly attached: audio={newly_attached:?} video={newly_video:?}");
    crate::ws::send_to_user(
        state,
        username,
        serde_json::json!({ "type": "voice_renegotiate", "board_id": board_id, "sdp": offer.sdp }),
    );

    // The new viewer needs a keyframe to start each picture: ask those
    // cameras for one once the renegotiation has had a moment to land.
    if !newly_video.is_empty() {
        let state_for_kf = Arc::clone(state);
        let board_for_kf = board_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1200)).await;
            for other in newly_video {
                request_keyframe(&state_for_kf, &board_for_kf, &other).await;
            }
        });
    }
    Ok(())
}

/// The shared outgoing video source for one participant's camera — made
/// the first time it's needed, after which everyone else on the board is
/// renegotiated to receive it. Also records the camera stream's SSRC on
/// their current connection, for keyframe requests.
async fn video_source_for(state: &Arc<AppState>, key: &ParticipantKey, ssrc: u32) -> Arc<TrackLocalStaticRTP> {
    state.voice_runtime.video_ssrc.lock().await.insert(key.clone(), ssrc);
    let (source, fresh) = {
        let mut videos = state.voice_runtime.video_sources.lock().await;
        match videos.get(key) {
            Some(existing) => (Arc::clone(existing), false),
            None => {
                let username = &key.1;
                let fresh = Arc::new(TrackLocalStaticRTP::new(
                    RTCRtpCodecCapability {
                        mime_type: "video/VP8".to_owned(),
                        clock_rate: 90000,
                        ..Default::default()
                    },
                    format!("video-{username}"),
                    format!("chriscord-{username}"),
                ));
                videos.insert(key.clone(), Arc::clone(&fresh));
                (fresh, true)
            }
        }
    };
    if fresh {
        let state = Arc::clone(state);
        let (board_id, username) = key.clone();
        tokio::spawn(async move {
            renegotiate_others_for_new_source(&state, &board_id, &username).await;
        });
    }
    source
}

/// Asks a participant's camera for a fresh keyframe (RTCP PLI) — what a
/// viewer needs to start or recover its picture. At most one request per
/// camera every half second; extra ones are dropped.
pub async fn request_keyframe(state: &Arc<AppState>, board_id: &str, username: &str) {
    let key: ParticipantKey = (board_id.to_string(), username.to_string());
    {
        let mut last = state.voice_runtime.keyframe_last.lock().await;
        let now = Instant::now();
        if last.get(&key).map_or(false, |t| now.duration_since(*t) < Duration::from_millis(500)) {
            return;
        }
        last.insert(key.clone(), now);
    }
    let Some(ssrc) = state.voice_runtime.video_ssrc.lock().await.get(&key).copied() else { return };
    let Some(pc) = state.voice_runtime.connections.lock().await.get(&key).cloned() else { return };
    let pli: Box<dyn webrtc::rtcp::packet::Packet + Send + Sync> =
        Box::new(PictureLossIndication { sender_ssrc: 0, media_ssrc: ssrc });
    if let Err(e) = pc.write_rtcp(&[pli]).await {
        tracing::debug!("voice: keyframe request to {username} failed: {e}");
    }
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
        tracing::warn!("voice: got a renegotiation answer from {username} but no matching connection exists");
        return;
    };
    let Ok(remote_desc) = RTCSessionDescription::answer(answer_sdp.to_string()) else {
        tracing::warn!("voice: invalid renegotiation answer SDP from {username}");
        return;
    };
    match pc.set_remote_description(remote_desc).await {
        Ok(()) => tracing::info!("voice: applied {username}'s renegotiation answer successfully"),
        Err(e) => tracing::warn!("voice: set_remote_description failed for {username}'s renegotiation answer: {e}"),
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
    state.voice_runtime.video_sources.lock().await.remove(&key);
    state.voice_runtime.video_ssrc.lock().await.remove(&key);
    state.voice_runtime.reneg_locks.lock().await.remove(&key);
    state.voice_runtime.keyframe_last.lock().await.remove(&key);
    {
        let mut attached = state.voice_runtime.video_attached.lock().await;
        attached.remove(&key);
        for set in attached.values_mut() {
            set.remove(username);
        }
    }
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
