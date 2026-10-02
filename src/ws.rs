use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use sqlx::Row;
use std::sync::Arc;

use crate::{db, messages::{row_to_msg, Attachment, HISTORY_PAGE}, state::AppState, voice};

#[derive(Deserialize)]
pub struct WsQuery {
    pub token: String,
    /// Presence to connect with, so someone who's invisible never flashes
    /// online for the moment before their first set_status arrives.
    #[serde(default)]
    pub status: Option<String>,
}

/// The presence values a client may choose.
fn valid_status(s: &str) -> bool { matches!(s, "online" | "idle" | "invisible") }

/// Whether someone shows as offline to others despite being connected.
fn is_invisible(state: &AppState, username: &str) -> bool {
    state.presence.lock().unwrap().get(username).map(String::as_str) == Some("invisible")
}

#[derive(Deserialize)]
struct ClientMsg {
    #[serde(rename = "type")]
    msg_type:        String,
    board_id:        Option<String>,
    content:         Option<String>,
    attachments:     Option<Vec<Attachment>>,
    sdp:             Option<String>,
    candidate:       Option<String>,
    sdp_mid:         Option<String>,
    sdp_mline_index: Option<u16>,
    expected_others: Option<Vec<String>>,
    speaking:        Option<bool>,
    muted:           Option<bool>,
    deafened:        Option<bool>,
    pfp_updated_at:  Option<i64>,
    pfp_data:        Option<String>,
    profile_updated_at: Option<i64>,
    bio:             Option<String>,
    /// Base64 banner image; absent or empty means "no banner".
    banner_data:     Option<String>,
    /// set_status: "online" | "idle" | "invisible"
    status:          Option<String>,
    /// typing: whether they're typing right now
    typing:          Option<bool>,
}

pub async fn ws_handler(
    ws:           WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, query.token, query.status, state))
}

async fn handle_socket(socket: WebSocket, token: String, initial_status: Option<String>, state: Arc<AppState>) {
    let username = match db::verify_token(&state.pool, &token).await {
        Ok(Some(u)) => u,
        _ => return,
    };
    // Where this account's picture/banner/bio are stored (by public key).
    let user_key = crate::pfp::storage_key_for(&state.pool, &username).await.unwrap_or_default();
    if let Some(st) = initial_status.filter(|s| valid_status(s)) {
        state.presence.lock().unwrap().insert(username.clone(), st);
    }

    { let mut o = state.online.lock().unwrap(); *o.entry(username.clone()).or_insert(0) += 1; }
    let mut rx = state.tx.subscribe();
    broadcast_users(&state);
    broadcast_voice_state(&state);

    let mut subscribed_board: Option<String> = None;
    let conn_id = NEXT_CONN_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (mut sink, mut stream) = socket.split();

    // Heartbeat: ping every WS_PING_EVERY; anything received (a message, a
    // pong, the client's own ping) counts as alive. Nothing for
    // WS_DEAD_AFTER means the client is gone without saying so — network
    // dropped, machine asleep, app killed — and the connection is ended
    // here rather than lingering until TCP gives up minutes later.
    let mut heartbeat = tokio::time::interval(WS_PING_EVERY);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_seen = std::time::Instant::now();
    // A close frame means the app ended the connection on purpose (quit,
    // left the server) — then there's no "reconnecting" grace period.
    let mut closed_on_purpose = false;

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if last_seen.elapsed() > WS_DEAD_AFTER {
                    tracing::info!("ws: {username} stopped responding — dropping the connection");
                    break;
                }
                if sink.send(Message::Ping(Vec::new())).await.is_err() { break; }
            }
            msg = stream.next() => {
                if matches!(msg, Some(Ok(_))) { last_seen = std::time::Instant::now(); }
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(cm) = serde_json::from_str::<ClientMsg>(&text) else { continue };
                        match cm.msg_type.as_str() {
                            "subscribe" => {
                                if let Some(bid) = cm.board_id {
                                    let history = load_history(&state, &bid).await;
                                    let payload = serde_json::json!({
                                        "type": "history", "board_id": &bid, "messages": history,
                                    });
                                    if sink.send(Message::Text(payload.to_string())).await.is_err() { break; }
                                    subscribed_board = Some(bid);
                                }
                            }
                            "message" => {
                                if let Some(bid) = cm.board_id {
                                    let content     = cm.content.as_deref().unwrap_or("").trim().to_string();
                                    let attachments = cm.attachments.unwrap_or_default();
                                    if !content.is_empty() || !attachments.is_empty() {
                                        save_and_broadcast(&state, &bid, &username, &content, attachments).await;
                                    }
                                }
                            }
                            "join_voice" => {
                                if let Some(bid) = cm.board_id {
                                    // Validate server-side that this board actually belongs to a
                                    // voice-type room, rather than trusting client UI gating alone.
                                    let room_type: Option<String> = sqlx::query(
                                        "SELECT rooms.room_type AS room_type
                                         FROM boards JOIN rooms ON boards.room_id = rooms.id
                                         WHERE boards.id = ?"
                                    ).bind(&bid).fetch_optional(&state.pool).await.ok().flatten()
                                     .map(|r| r.get("room_type"));
                                    if room_type.as_deref() == Some("voice") {
                                        // Already in a call on another device? Move it here: that
                                        // device is told to hang up, and its audio is closed.
                                        let prev = voice_owner(&username).filter(|o| *o != conn_id);
                                        let prev_board = { state.voice.lock().unwrap().get(&username).cloned() };
                                        if let (Some(old_conn), Some(old_bid)) = (prev, prev_board) {
                                            voice::close_participant(&state, &old_bid, &username).await;
                                            let _ = state.tx.send(serde_json::json!({
                                                "type": "voice_moved", "target": username, "target_conn": old_conn, "board_id": old_bid,
                                            }).to_string());
                                            tracing::info!("voice: {username}'s call moved to another device");
                                        }
                                        { voice_owners().lock().unwrap().insert(username.clone(), conn_id); }
                                        // A user can only be in one voice channel at a time —
                                        // inserting simply overwrites any previous entry, so
                                        // moving between channels needs no separate leave step.
                                        { state.voice.lock().unwrap().insert(username.clone(), bid.clone()); }
                                        { state.voice_reconnecting.lock().unwrap().remove(&username); } // back properly
                                        // Mute/deafen start as whatever the app says it is right
                                        // now — never carried over from an earlier session, which
                                        // is how someone could show as muted while talking.
                                        { state.voice_status.lock().unwrap().insert(username.clone(),
                                            (cm.muted.unwrap_or(false), cm.deafened.unwrap_or(false))); }
                                        broadcast_voice_state(&state);
                                        let statuses: Vec<serde_json::Value> = {
                                            let voice = state.voice.lock().unwrap();
                                            let status = state.voice_status.lock().unwrap();
                                            voice.iter()
                                                .filter(|(u, b)| **b == bid && **u != username)
                                                .map(|(u, _)| {
                                                    let (muted, deafened) = status.get(u).copied().unwrap_or((false, false));
                                                    serde_json::json!({ "username": u, "muted": muted, "deafened": deafened })
                                                })
                                                .collect()
                                        };
                                        send_to_user(&state, &username, serde_json::json!({
                                            "type": "voice_status_snapshot", "board_id": bid, "statuses": statuses,
                                        }));
                                    }
                                }
                            }
                            "leave_voice" if !may_control_voice(&username, conn_id) => {
                                // Another device of this account is the one in the call —
                                // this one can't hang it up (e.g. a second device starting
                                // up and tidying what looks like a leftover call).
                            }
                            "leave_voice" => {
                                { voice_owners().lock().unwrap().remove(&username); }
                                // Also ends a "reconnecting" placeholder — a client that
                                // dropped mid-call and then chose to hang up sends this
                                // once it's back, so nobody waits for it to return.
                                let was_reconnecting = { state.voice_reconnecting.lock().unwrap().remove(&username).is_some() };
                                let left_board = { state.voice.lock().unwrap().remove(&username) };
                                if let Some(bid) = left_board {
                                    { state.voice_status.lock().unwrap().remove(&username); }
                                    voice::close_participant(&state, &bid, &username).await;
                                    broadcast_voice_state(&state);
                                } else if was_reconnecting {
                                    broadcast_voice_state(&state);
                                }
                            }
                            "voice_offer" if !may_control_voice(&username, conn_id) => {}
                            "voice_offer" => {
                                if let (Some(bid), Some(sdp)) = (cm.board_id, cm.sdp) {
                                    let others: Vec<String> = {
                                        let voice = state.voice.lock().unwrap();
                                        voice.iter()
                                            .filter(|(u, b)| **b == bid && **u != username)
                                            .map(|(u, _)| u.clone())
                                            .collect()
                                    };
                                    let expected_others = cm.expected_others.unwrap_or_default();
                                    match voice::handle_offer(&state, &bid, &username, &sdp, &others, &expected_others).await {
                                        Ok(answer_sdp) => {
                                            send_to_user(&state, &username, serde_json::json!({
                                                "type": "voice_answer", "sdp": answer_sdp,
                                            }));
                                        }
                                        Err(e) => tracing::warn!("voice_offer failed for {username}: {e}"),
                                    }
                                }
                            }
                            "voice_ice" if !may_control_voice(&username, conn_id) => {}
                            "voice_ice" => {
                                if let (Some(bid), Some(candidate)) = (cm.board_id, cm.candidate) {
                                    let init = webrtc::ice_transport::ice_candidate::RTCIceCandidateInit {
                                        candidate,
                                        sdp_mid: cm.sdp_mid,
                                        sdp_mline_index: cm.sdp_mline_index,
                                        ..Default::default()
                                    };
                                    voice::handle_ice_candidate(&state, &bid, &username, init).await;
                                }
                            }
                            "voice_renegotiate_answer" if !may_control_voice(&username, conn_id) => {}
                            "voice_renegotiate_answer" => {
                                if let (Some(bid), Some(sdp)) = (cm.board_id, cm.sdp) {
                                    voice::handle_renegotiate_answer(&state, &bid, &username, &sdp).await;
                                }
                            }
                            "speaking" if !may_control_voice(&username, conn_id) => {}
                            "speaking" => {
                                if let (Some(bid), Some(speaking)) = (cm.board_id, cm.speaking) {
                                    // Only meaningful if they're actually still in this voice
                                    // channel — ignore anything else as stale/stray.
                                    let in_channel = { state.voice.lock().unwrap().get(&username) == Some(&bid) };
                                    if in_channel {
                                        let _ = state.tx.send(serde_json::json!({
                                            "type": "voice_speaking", "board_id": bid,
                                            "username": username, "speaking": speaking,
                                        }).to_string());
                                    } else {
                                        // Dropped as stale — worth seeing if someone's ring
                                        // ever fails to show for everyone else.
                                        tracing::warn!("voice: ignoring speaking={speaking} from {username} for board {bid}: not in that voice channel");
                                    }
                                }
                            }
                            "voice_mute_state" if !may_control_voice(&username, conn_id) => {}
                            "voice_mute_state" => {
                                if let Some(bid) = cm.board_id {
                                    let in_channel = { state.voice.lock().unwrap().get(&username) == Some(&bid) };
                                    if in_channel {
                                        let muted = cm.muted.unwrap_or(false);
                                        let deafened = cm.deafened.unwrap_or(false);
                                        { state.voice_status.lock().unwrap().insert(username.clone(), (muted, deafened)); }
                                        let _ = state.tx.send(serde_json::json!({
                                            "type": "voice_mute_state", "board_id": bid, "username": username,
                                            "muted": muted, "deafened": deafened,
                                        }).to_string());
                                    }
                                }
                            }
                            "pfp_info" => {
                                // The client reports the Unix timestamp of when its own
                                // picture last changed (0 means it has none at all) —
                                // ask it to upload only if what we have cached, if
                                // anything, doesn't match. A matching timestamp means
                                // our cache is already current, so there's nothing to do.
                                if let Some(updated_at) = cm.pfp_updated_at {
                                    if updated_at > 0 && !user_key.is_empty() && crate::pfp::cached_timestamp(&user_key) != Some(updated_at) {
                                        send_to_user(&state, &username, serde_json::json!({
                                            "type": "pfp_request",
                                        }));
                                    }
                                }
                            }
                            "pfp_upload" => {
                                if let (Some(data), Some(updated_at)) = (cm.pfp_data, cm.pfp_updated_at) {
                                    use base64::Engine as _;
                                    tracing::info!("voice: pfp_upload from {username}: {} base64 chars", data.len());
                                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&data) {
                                        tracing::info!("voice: pfp_upload from {username}: decoded to {} bytes", bytes.len());
                                        let saved = crate::pfp::save_cached(&user_key, &bytes, updated_at);
                                        if let Err(e) = &saved {
                                            tracing::warn!("voice: rejected pfp from {username}: {e}");
                                        }
                                        if saved.is_ok() {
                                            tracing::info!("voice: cached {} bytes for {username}", bytes.len());
                                            // Broadcast to everyone, not just a targeted
                                            // send — anyone currently displaying this
                                            // user's avatar (voice occupant list, member
                                            // list, and so on) needs to know to refetch it.
                                            let _ = state.tx.send(serde_json::json!({
                                                "type": "pfp_updated", "username": username, "updated_at": updated_at,
                                            }).to_string());
                                        } else {
                                            tracing::warn!("voice: failed to cache pfp for {username}");
                                        }
                                    } else {
                                        tracing::warn!("voice: invalid base64 pfp data from {username}");
                                    }
                                }
                            }
                            // Presence: Online / Away (manual or automatic) / Invisible.
                            "set_status" => {
                                if let Some(st) = cm.status.filter(|s| valid_status(s)) {
                                    let changed = state.presence.lock().unwrap().insert(username.clone(), st.clone()) != Some(st);
                                    if changed { broadcast_users(&state); }
                                }
                            }
                            // Typing indicator. Never sent for invisible users —
                            // typing would give away that they're actually on.
                            // Clients drop a stale "typing" on their own after
                            // 10 s, so a lost stop message can't leave it stuck.
                            "typing" => {
                                if !is_invisible(&state, &username) {
                                    let _ = state.tx.send(serde_json::json!({
                                        "type": "typing", "username": username, "typing": cm.typing.unwrap_or(false),
                                    }).to_string());
                                }
                            }
                            // Profile (bio + banner) sync — same handshake as the pfp above.
                            "profile_info" => {
                                if let Some(updated_at) = cm.profile_updated_at {
                                    if updated_at > 0 && !user_key.is_empty() && crate::profile::cached_timestamp(&user_key) != Some(updated_at) {
                                        send_to_user(&state, &username, serde_json::json!({ "type": "profile_request" }));
                                    }
                                }
                            }
                            "profile_upload" => {
                                if let Some(updated_at) = cm.profile_updated_at {
                                    use base64::Engine as _;
                                    let banner = match cm.banner_data.as_deref() {
                                        Some(b) if !b.is_empty() => match base64::engine::general_purpose::STANDARD.decode(b) {
                                            Ok(bytes) => Some(bytes),
                                            Err(_) => { tracing::warn!("profile: invalid base64 banner from {username}"); None }
                                        },
                                        _ => None,
                                    };
                                    let bio = cm.bio.unwrap_or_default();
                                    match crate::profile::save_cached(&user_key, &bio, banner.as_deref(), updated_at) {
                                        Ok(()) => {
                                            let _ = state.tx.send(serde_json::json!({
                                                "type": "profile_updated", "username": username, "updated_at": updated_at,
                                            }).to_string());
                                        }
                                        Err(e) => tracing::warn!("profile: rejected profile from {username}: {e}"),
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Message::Close(_))) => { closed_on_purpose = true; break; }
                    None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            result = rx.recv() => {
                match result {
                    Ok(bcast) => {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&bcast) {
                            let fwd = match v["type"].as_str() {
                                Some("users") | Some("server_updated") => true,
                                Some("typing") => v["username"].as_str() != Some(username.as_str()),
                                Some("rooms_updated") => true,
                                Some("voice_state") => true,
                                // Speaking rings are only for people in that same call —
                                // someone just browsing the server still gets voice_state
                                // (who's in which channel) but no activity.
                                Some("voice_speaking") => {
                                    let my_board = state.voice.lock().unwrap().get(&username).cloned();
                                    my_board.as_deref().is_some() && v["board_id"].as_str() == my_board.as_deref()
                                        && voice_owner(&username) == Some(conn_id) // not your other devices
                                }
                                Some("voice_moved") => v["target"].as_str() == Some(username.as_str()) && v["target_conn"].as_u64() == Some(conn_id),
                                Some("voice_mute_state") => true,
                                Some("pfp_updated") | Some("profile_updated") => true,
                                Some("message") => true,
                                Some("message_edit") | Some("message_delete") | Some("message_pin") | Some("message_reaction") => subscribed_board.as_deref()
                                    .map(|bid| v["board_id"].as_str() == Some(bid))
                                    .unwrap_or(false),
                                Some("voice_answer") | Some("voice_ice") | Some("voice_status_snapshot") | Some("voice_renegotiate") =>
                                    v["target"].as_str() == Some(username.as_str()) && may_control_voice(&username, conn_id),
                                Some("pfp_request") | Some("profile_request") =>
                                    v["target"].as_str() == Some(username.as_str()),
                                _ => false,
                            };
                            // Kicked/banned from the admin panel: pass the notice on,
                            // then end this connection.
                            if v["type"].as_str() == Some("kicked") && v["target"].as_str() == Some(username.as_str()) {
                                let _ = sink.send(Message::Text(bcast)).await;
                                break;
                            }
                            if fwd { if sink.send(Message::Text(bcast)).await.is_err() { break; } }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
    }

    {
        let mut o = state.online.lock().unwrap();
        let c = o.entry(username.clone()).or_insert(0);
        if *c <= 1 {
            o.remove(&username);
            state.presence.lock().unwrap().remove(&username);
            // Stop any typing indicator left showing for them.
            let _ = state.tx.send(serde_json::json!({ "type": "typing", "username": username, "typing": false }).to_string());
        } else { *c -= 1; }
    }
    broadcast_users(&state);

    // A dropped connection ends their audio — but rather than vanishing
    // from the channel (which looks like they left), they stay listed as
    // "reconnecting" for VOICE_REJOIN_GRACE, since the app rejoins on its
    // own when it gets back. If it doesn't come back in time, they're
    // removed then. A client that closed on purpose (quit, or left the
    // server) sends a close frame first, and is removed straight away.
    // Only if this connection is the device in the call — closing the
    // laptop mustn't hang up the PC.
    let owned = voice_owner(&username) == Some(conn_id);
    if owned { voice_owners().lock().unwrap().remove(&username); }
    let left_board = if owned { state.voice.lock().unwrap().remove(&username) } else { None };
    if let Some(bid) = left_board.clone().filter(|_| closed_on_purpose) {
        // Left on purpose (the app sent a close frame): out of the call now.
        { state.voice_status.lock().unwrap().remove(&username); }
        voice::close_participant(&state, &bid, &username).await;
        broadcast_voice_state(&state);
    } else if let Some(bid) = left_board {
        { state.voice_status.lock().unwrap().remove(&username); }
        voice::close_participant(&state, &bid, &username).await;
        let drop_id = NEXT_DROP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        { state.voice_reconnecting.lock().unwrap().insert(username.clone(), (bid, drop_id)); }
        broadcast_voice_state(&state);
        let st = Arc::clone(&state);
        let user = username.clone();
        tokio::spawn(async move {
            tokio::time::sleep(VOICE_REJOIN_GRACE).await;
            let expired = {
                let mut r = st.voice_reconnecting.lock().unwrap();
                if r.get(&user).map(|(_, id)| *id) == Some(drop_id) { r.remove(&user); true } else { false }
            };
            if expired { broadcast_voice_state(&st); }
        });
    }
}

/// Delivers a message to exactly one connected user, by piggybacking on the
/// same broadcast channel everything else uses — every connection already
/// receives every broadcast and decides whether to act on it, so a
/// `target` field plus a matching check in the forwarding whitelist above
/// is enough to make this "targeted" without needing a separate
/// per-connection registry.
pub fn send_to_user(state: &Arc<AppState>, target_username: &str, mut payload: serde_json::Value) {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("target".to_string(), serde_json::Value::String(target_username.to_string()));
    }
    let _ = state.tx.send(payload.to_string());
}

/// How long someone whose connection dropped mid-call stays listed as
/// "reconnecting" before they're taken out of the channel.
const VOICE_REJOIN_GRACE: std::time::Duration = std::time::Duration::from_secs(60);
/// Heartbeat timing (see handle_socket). The app pings every 10 s too, so
/// a live client is never quiet for anywhere near this long.
const WS_PING_EVERY: std::time::Duration = std::time::Duration::from_secs(20);
const WS_DEAD_AFTER: std::time::Duration = std::time::Duration::from_secs(50);
static NEXT_DROP_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

// ── One device per call ──────────────────────────────────────────────────────
//
// The same account can be signed in on several devices at once (PC and
// laptop), each with its own connection. Voice is per account, so exactly
// one of those connections — the "voice owner" — is the one actually in
// the call. Only it gets the call's signalling and speaking activity, only
// it can change the call (mute, leave…), and only its disconnecting takes
// the account out. Joining from another device moves the call there: the
// old device is told ("voice_moved") and hangs up.
static NEXT_CONN_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
fn voice_owners() -> &'static std::sync::Mutex<std::collections::HashMap<String, u64>> {
    static OWNERS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, u64>>> = std::sync::OnceLock::new();
    OWNERS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}
fn voice_owner(user: &str) -> Option<u64> { voice_owners().lock().unwrap().get(user).copied() }
/// True if this connection may act on the account's call: it's the owner,
/// or nobody owns it right now.
fn may_control_voice(user: &str, conn: u64) -> bool { voice_owner(user).map_or(true, |o| o == conn) }

fn broadcast_voice_state(state: &Arc<AppState>) {
    let group = |pairs: Vec<(String, String)>| {
        let mut m: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        for (user, board) in pairs { m.entry(board).or_default().push(user); }
        m
    };
    let channels = group(state.voice.lock().unwrap().iter().map(|(u, b)| (u.clone(), b.clone())).collect());
    // Separate from channels so older clients (which don't know about it)
    // simply don't show the placeholder.
    let reconnecting = group(state.voice_reconnecting.lock().unwrap().iter().map(|(u, (b, _))| (u.clone(), b.clone())).collect());
    // Everyone's current mute/deafen, sent with every update so every app —
    // including ones that just connected and aren't in a call — always has
    // the real state, and anything stale is replaced rather than lingering.
    let mute_states: serde_json::Map<String, serde_json::Value> = {
        let voice = state.voice.lock().unwrap();
        let status = state.voice_status.lock().unwrap();
        voice.keys().map(|u| {
            let (muted, deafened) = status.get(u).copied().unwrap_or((false, false));
            (u.clone(), serde_json::json!({ "muted": muted, "deafened": deafened }))
        }).collect()
    };
    let _ = state.tx.send(serde_json::json!({
        "type": "voice_state", "channels": channels, "reconnecting": reconnecting, "mute_states": mute_states,
    }).to_string());
}

/// Sends everyone the member list: who's online (invisible people are
/// left out, so they look offline) and, under "presence", each online
/// person's status ("online" or "idle"). Not "statuses" — that key already
/// means the voice mute/deafen list on voice_status_snapshot, and the Go
/// client decodes every server message into one struct.
pub fn broadcast_users(state: &Arc<AppState>) {
    let (online, statuses) = {
        let o = state.online.lock().unwrap();
        let p = state.presence.lock().unwrap();
        let mut online = Vec::new();
        let mut statuses = serde_json::Map::new();
        for user in o.keys() {
            let st = p.get(user).map(String::as_str).unwrap_or("online");
            if st == "invisible" { continue; }
            online.push(user.clone());
            statuses.insert(user.clone(), serde_json::Value::String(st.to_string()));
        }
        (online, statuses)
    };
    let state = Arc::clone(state);
    tokio::spawn(async move {
        let all = crate::db::all_known_users(&state.pool).await.unwrap_or_default();
        let owner = crate::admin::owner_username(&state.pool).await;
        let _ = state.tx.send(serde_json::json!({ "type": "users", "online": online, "presence": statuses, "all": all, "owner": owner }).to_string());
    });
}

async fn load_history(state: &Arc<AppState>, board_id: &str) -> Vec<serde_json::Value> {
    let mut rows = match sqlx::query(
        "SELECT id, board_id, username, content,
                attachment_url, attachment_name, attachment_mime,
                attachments, edited, created_at, pinned_at
         FROM messages WHERE board_id = ? ORDER BY id DESC LIMIT ?",
    ).bind(board_id).bind(HISTORY_PAGE).fetch_all(&state.pool).await {
        Ok(r) => r,
        Err(e) => { tracing::error!("load_history: {}", e); return vec![]; }
    };
    rows.reverse();
    let mut msgs: Vec<crate::messages::ChatMessage> = rows.iter().map(row_to_msg).collect();
    crate::messages::attach_reactions(&state.pool, &mut msgs).await;
    msgs.iter().filter_map(|m| serde_json::to_value(m).ok()).collect()
}

async fn save_and_broadcast(
    state:       &Arc<AppState>,
    board_id:    &str,
    username:    &str,
    content:     &str,
    attachments: Vec<Attachment>,
) {
    let id              = uuid::Uuid::now_v7().to_string();
    let now             = chrono::Utc::now().to_rfc3339();
    let attach_json     = serde_json::to_string(&attachments).unwrap_or_default();

    let _ = sqlx::query(
        "INSERT INTO messages (id, board_id, username, content, attachments, edited, created_at)
         VALUES (?, ?, ?, ?, ?, 0, ?)",
    )
    .bind(&id).bind(board_id).bind(username).bind(content)
    .bind(&attach_json).bind(&now)
    .execute(&state.pool).await;

    let msg = crate::messages::ChatMessage {
        id, board_id: board_id.to_string(), username: username.to_string(),
        content: content.to_string(), attachments, edited: false, created_at: now, pinned: false, reactions: Vec::new(),
    };
    let _ = state.tx.send(serde_json::json!({ "type": "message", "data": msg }).to_string());
}
