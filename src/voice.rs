// voice.rs — WebRTC SFU logic for voice channels.
//
// IMPORTANT, read this before touching anything else in this file: unlike
// every other file touched in this project, this one has NOT been
// compile-checked. The `webrtc` crate's current dependency tree requires a
// newer Rust edition than was available in the sandbox this was written
// in, and there was no way to reach a newer toolchain given the network
// constraints there. Every method name and callback signature here is
// grounded in real references rather than memory — either confirmed
// directly via docs.rs (on_track's three-argument closure and the
// Box::pin(async move {...}) return shape in particular, which would have
// been easy to get subtly wrong from memory alone) or cross-referenced
// against pion/webrtc, the Go library this Rust crate is an explicit,
// close port of — which WAS compiled and actually run, including a real
// two-party offer/answer exchange, in the same session this was written.
// That said: treat this file as a first draft. If `cargo build` doesn't
// succeed, paste the exact error back and it'll get fixed from there —
// that's expected, not a sign anything went wrong in how it was written.
//
// Design — the "simple" version, deliberately built so it can evolve into
// seamless mid-call renegotiation later without a rewrite:
//   - Every voice_offer creates a BRAND NEW PeerConnection for that
//     (board, user) pair, closing any previous one for the same pair
//     first. A "someone joined or left, please refresh" re-offer from an
//     existing participant is handled by exactly the same code path as a
//     first-time join — there's no separate "renegotiate" branch.
//   - RTP forwarding: each participant's OWN incoming audio, once
//     negotiated, gets read from their PeerConnection and written into a
//     single shared TrackLocalStaticRTP unique to them. That same shared
//     track object gets added as an outgoing track to every OTHER
//     participant's PeerConnection. Writing to it once fans out to
//     everyone who has added it — this is the standard SFU forwarding
//     pattern, and it's what keeps the server from ever needing to decode
//     or re-encode audio: it only ever moves RTP packets around.
//   - What a future seamless version changes: instead of everyone
//     tearing down and re-offering on every roster change, the server
//     would instead call add_track on each EXISTING participant's
//     connection for the new participant's source, and trigger
//     renegotiation just for those — reusing this exact same
//     PeerConnection-per-participant and shared-source-track machinery,
//     just orchestrated differently.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::{APIBuilder, API};
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::RTCRtpTransceiver;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};
use webrtc::track::track_remote::TrackRemote;

use crate::state::AppState;

/// (board_id, username) — a participant's identity within one voice board.
type ParticipantKey = (String, String);

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
        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)
            .expect("register default interceptors");
        let api = APIBuilder::new()
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

impl Default for VoiceRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// Handles an incoming SDP offer for a (board, user) pair: tears down any
/// existing connection for that exact pair, builds a fresh PeerConnection
/// wired to hear every other current participant, and returns the SDP
/// answer to send back over the signaling WebSocket. `others` is the
/// current username roster of the board, excluding `username` itself —
/// the caller (ws.rs) already has this from the existing voice-presence
/// tracking, so it doesn't need to be recomputed here.
pub async fn handle_offer(
    state: &Arc<AppState>,
    board_id: &str,
    username: &str,
    offer_sdp: &str,
    others: &[String],
) -> Result<String, String> {
    tracing::info!(
        "voice: offer from {username} for board {board_id} ({} other(s) known: {:?})",
        others.len(),
        others,
    );
    let key: ParticipantKey = (board_id.to_string(), username.to_string());

    // Tear down any previous connection for this exact participant first —
    // this is what makes a "someone joined/left, please refresh" re-offer
    // work correctly: it's handled identically to a first-time join,
    // nothing about this path needs to know which case it is.
    if let Some(old_pc) = state.voice_runtime.connections.lock().await.remove(&key) {
        let _ = old_pc.close().await;
    }
    state.voice_runtime.sources.lock().await.remove(&key);

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
    // arrives below. Created now so it can be added to other connections
    // and registered under `key` before negotiation even completes.
    let my_source = Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: "audio/opus".to_owned(),
            clock_rate: 48000,
            channels: 1,
            ..Default::default()
        },
        format!("audio-{username}"),
        format!("chriscord-{username}"),
    ));

    // Always explicitly register how to receive this offering client's own
    // microphone — this need exists on every offer regardless of roster
    // size. It's easy to assume this only matters when nobody else has
    // audio to send back, but that's wrong: sending other participants'
    // audio and receiving this participant's own mic are two separate,
    // unrelated needs. Making this conditional on the other one having
    // NOT happened (the previous version of this code) meant it silently
    // stopped running the moment a second participant existed — exactly
    // the case that actually needs it. This is the known pion/webrtc-rs
    // bug class where an incoming track on an unregistered media section
    // never fires on_track at all.
    if let Err(e) = pc.add_transceiver_from_kind(
        RTPCodecType::Audio,
        Some(RTCRtpTransceiverInit { direction: RTCRtpTransceiverDirection::Recvonly, send_encodings: vec![] }),
    ).await {
        tracing::warn!("voice: failed to add recvonly audio transceiver for {username}: {e}");
    }

    // Hear every other current participant who already has forwarded
    // audio available. Someone who hasn't finished negotiating yet simply
    // won't have a source in the map — they'll be picked up once they
    // negotiate and everyone else's next refresh includes them.
    {
        let sources = state.voice_runtime.sources.lock().await;
        for other in others {
            let other_key = (board_id.to_string(), other.clone());
            if let Some(track) = sources.get(&other_key) {
                let track_dyn: Arc<dyn TrackLocal + Send + Sync> = Arc::clone(track) as _;
                if let Err(e) = pc.add_track(track_dyn).await {
                    tracing::warn!("voice: failed to add track for {other} to {username}'s connection: {e}");
                }
            }
        }
    }

    // When this participant's own microphone track arrives, forward every
    // RTP packet into their shared outgoing source — this is the actual
    // "selective forwarding" step; every other participant's connection
    // that has added this same track object receives these packets too.
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

    let answer = pc
        .create_answer(None)
        .await
        .map_err(|e| format!("create_answer failed: {e}"))?;
    pc.set_local_description(answer.clone())
        .await
        .map_err(|e| format!("set_local_description failed: {e}"))?;

    state.voice_runtime.connections.lock().await.insert(key.clone(), Arc::clone(&pc));
    state.voice_runtime.sources.lock().await.insert(key, my_source);

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
