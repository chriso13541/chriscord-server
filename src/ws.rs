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
    reply_to:        Option<String>,
    sdp:             Option<String>,
    candidate:       Option<String>,
    sdp_mid:         Option<String>,
    sdp_mline_index: Option<u16>,
    expected_others: Option<Vec<String>>,
    speaking:        Option<bool>,
    muted:           Option<bool>,
    deafened:        Option<bool>,
    /// voice_video: whether their camera is now on
    video:           Option<bool>,
    /// voice_keyframe / voice_watch: whose camera or screen
    target:          Option<String>,
    /// voice_screen: whether their screen share is now on
    screen:          Option<bool>,
    /// voice_screen_window: where the window they're sharing is —
    /// "focused", "background" (behind other windows) or "minimized"
    window:          Option<String>,
    /// voice_watch: start (true) or stop watching target's screen
    watch:           Option<bool>,
    /// voice_keyframe: "screen" for a screen share, else their camera
    kind:            Option<String>,
    /// voice_watch: the most they want to receive (both set, or neither:
    /// as shared); voice_screen_native: what they're sharing at
    height:          Option<u32>,
    fps:             Option<u32>,
    pfp_updated_at:  Option<i64>,
    pfp_data:        Option<String>,
    profile_updated_at: Option<i64>,
    bio:             Option<String>,
    /// Base64 banner image; absent or empty means "no banner".
    banner_data:     Option<String>,
    /// profile_upload: their card colour ("#rrggbb", "" for the default)
    tint:            Option<String>,
    /// profile_upload: their global nickname; set_nickname: their nickname
    /// on this server. "" (or absent) means none.
    nickname:        Option<String>,
    /// set_status: "online" | "idle" | "invisible"
    status:          Option<String>,
    /// set_activity: the game they're playing ("" or absent: none)
    game:            Option<String>,
    /// presence: their status, encrypted (see "presence" below)
    blob:            Option<String>,
    /// typing: whether they're typing right now
    typing:          Option<bool>,
    /// voice_e2ee: who it's for (absent: everyone in the call)
    to:              Option<String>,
    /// voice_e2ee: the key-exchange message itself — opaque to the server
    data:            Option<serde_json::Value>,
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
    // Which channels this member can see (None = all). Worked out again
    // whenever channels or roles change (see the forwarding below), and used
    // to keep private channels' messages and voice activity from them.
    let mut visible = crate::access::visible_for(&state.pool, &username).await;
    let sees = |vis: &Option<crate::access::Visible>, board: &str| vis.as_ref().map_or(true, |v| v.boards.contains(board));
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
                            "subscribe" if cm.board_id.as_deref().map_or(false, |b| !sees(&visible, b)) => {}
                            "subscribe" => {
                                if let Some(bid) = cm.board_id {
                                    // Without Read Message History: an empty history (new
                                    // messages still arrive live).
                                    let history = if crate::roles::has(&state.pool, &username, crate::roles::READ_HISTORY).await {
                                        load_history(&state, &bid).await
                                    } else { Vec::new() };
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
                                    let perms = crate::roles::permissions_of(&state.pool, &username).await;
                                    let denied = if !sees(&visible, &bid) { Some("post in that channel") }
                                        else if perms & crate::roles::SEND_MESSAGES == 0 { Some("send messages") }
                                        else if !attachments.is_empty() && perms & crate::roles::ATTACH_FILES == 0 { Some("attach files") }
                                        else { None };
                                    if let Some(what) = denied {
                                        send_to_user(&state, &username, serde_json::json!({
                                            "type": "action_denied", "message": format!("You don't have permission to {what} on this server"),
                                        }));
                                    } else if !content.is_empty() || !attachments.is_empty() {
                                        save_and_broadcast(&state, &bid, &username, &content, attachments, cm.reply_to.clone()).await;
                                    }
                                }
                            }
                            "join_voice" if cm.board_id.as_deref().map_or(false, |b| !sees(&visible, b)) => {
                                send_to_user(&state, &username, serde_json::json!({
                                    "type": "action_denied", "message": "You don't have access to that voice channel",
                                }));
                            }
                            "join_voice" if !crate::roles::has(&state.pool, &username, crate::roles::CONNECT).await => {
                                send_to_user(&state, &username, serde_json::json!({
                                    "type": "action_denied", "message": "You don't have permission to join voice channels on this server",
                                }));
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
                                        // Cameras start off; the app says when it turns one on.
                                        { video_on().lock().unwrap().remove(&username); }
                                        { screen_on().lock().unwrap().remove(&username); }
                                        { screen_window().lock().unwrap().remove(&username); }
                                        // Mute/deafen start as whatever the app says it is right
                                        // now — never carried over from an earlier session, which
                                        // is how someone could show as muted while talking.
                                        { state.voice_status.lock().unwrap().insert(username.clone(),
                                            (cm.muted.unwrap_or(false), cm.deafened.unwrap_or(false))); }
                                        // What the server won't pass on from them (Speak, Video,
                                        // a server mute) — before their media starts flowing.
                                        update_voice_blocks(&state, &username).await;
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
                            "voice_video" if !may_control_voice(&username, conn_id) => {}
                            "voice_video" if cm.video == Some(true) && (voice::blocked(&username) & voice::BLOCK_VIDEO) != 0 => {
                                send_to_user(&state, &username, serde_json::json!({
                                    "type": "action_denied", "message": "You don't have permission to turn on your camera here",
                                }));
                            }
                            "voice_video" => {
                                if let Some(bid) = cm.board_id {
                                    let in_channel = { state.voice.lock().unwrap().get(&username) == Some(&bid) };
                                    if in_channel {
                                        let on = cm.video.unwrap_or(false);
                                        {
                                            let mut set = video_on().lock().unwrap();
                                            if on { set.insert(username.clone()); } else { set.remove(&username); }
                                        }
                                        broadcast_voice_state(&state);
                                    }
                                }
                            }
                            "voice_screen" if !may_control_voice(&username, conn_id) => {}
                            "voice_screen" if cm.screen == Some(true) && (voice::blocked(&username) & voice::BLOCK_VIDEO) != 0 => {
                                send_to_user(&state, &username, serde_json::json!({
                                    "type": "action_denied", "message": "You don't have permission to share your screen here",
                                }));
                            }
                            "voice_screen" => {
                                if let Some(bid) = cm.board_id {
                                    let in_channel = { state.voice.lock().unwrap().get(&username) == Some(&bid) };
                                    if in_channel {
                                        let on = cm.screen.unwrap_or(false);
                                        {
                                            let mut set = screen_on().lock().unwrap();
                                            if on { set.insert(username.clone()); } else { set.remove(&username); }
                                        }
                                        if !on { screen_window().lock().unwrap().remove(&username); }
                                        broadcast_voice_state(&state);
                                        if !on {
                                            voice::screen_stopped(&state, &bid, &username).await;
                                        } else {
                                            // A share (re)starting: who's watching it so far.
                                            voice::notify_screen_viewers(&state, &bid, &username).await;
                                        }
                                    }
                                }
                            }
                            // Where the window being shared is, so the people watching
                            // can be told why the picture stopped (a minimised window
                            // isn't drawn by Windows at all). Kept even if it arrives
                            // just before voice_screen; only sent out while sharing.
                            "voice_screen_window" if !may_control_voice(&username, conn_id) => {}
                            "voice_screen_window" => {
                                if let (Some(bid), Some(w)) = (cm.board_id, cm.window.as_deref()) {
                                    let in_channel = { state.voice.lock().unwrap().get(&username) == Some(&bid) };
                                    if in_channel {
                                        let changed = {
                                            let mut m = screen_window().lock().unwrap();
                                            match w {
                                                "background" | "minimized" => m.insert(username.clone(), w.to_string()).as_deref() != Some(w),
                                                _ => m.remove(&username).is_some(),
                                            }
                                        };
                                        if changed { broadcast_voice_state(&state); }
                                    }
                                }
                            }
                            "voice_watch" if !may_control_voice(&username, conn_id) => {}
                            "voice_watch" => {
                                if let (Some(bid), Some(target)) = (cm.board_id, cm.target) {
                                    let in_channel = { state.voice.lock().unwrap().get(&username) == Some(&bid) };
                                    if in_channel {
                                        let limit = match (cm.height, cm.fps) {
                                            (Some(h), Some(f)) if h > 0 && f > 0 => Some(voice::ScreenSize { height: h, fps: f }),
                                            _ => None,
                                        };
                                        voice::set_watch(&state, &bid, &username, &target, cm.watch.unwrap_or(false), limit).await;
                                    }
                                }
                            }
                            "voice_screen_native" if !may_control_voice(&username, conn_id) => {}
                            "voice_screen_native" => {
                                if let (Some(bid), Some(h), Some(f)) = (cm.board_id, cm.height, cm.fps) {
                                    let in_channel = { state.voice.lock().unwrap().get(&username) == Some(&bid) };
                                    if in_channel && h > 0 && f > 0 {
                                        voice::set_screen_native(&state, &bid, &username, voice::ScreenSize { height: h, fps: f }).await;
                                    }
                                }
                            }
                            "voice_keyframe" if !may_control_voice(&username, conn_id) => {}
                            "voice_keyframe" => {
                                if let (Some(bid), Some(target)) = (cm.board_id, cm.target) {
                                    let in_channel = { state.voice.lock().unwrap().get(&username) == Some(&bid) };
                                    if in_channel {
                                        if cm.kind.as_deref() == Some("screen") {
                                            voice::request_screen_keyframe(&state, &bid, &target).await;
                                        } else {
                                            voice::request_keyframe(&state, &bid, &target).await;
                                        }
                                    }
                                }
                            }
                            // Call encryption's key exchange (the app's e2ee.go): call keys
                            // and wrapped sending keys, passed between the people in one
                            // call. The server can't read or forge them — it only relays,
                            // adding who sent it and their identity key (which the apps
                            // check the sender's signature against).
                            "voice_e2ee" if !may_control_voice(&username, conn_id) => {}
                            "voice_e2ee" => {
                                if let (Some(bid), Some(data)) = (cm.board_id, cm.data) {
                                    let in_channel = { state.voice.lock().unwrap().get(&username) == Some(&bid) };
                                    if in_channel && data.is_object() && data.to_string().len() <= VOICE_E2EE_MAX {
                                        let from_key: String = sqlx::query("SELECT public_key FROM users WHERE username = ?")
                                            .bind(&username).fetch_optional(&state.pool).await.ok().flatten()
                                            .map(|r| r.get("public_key")).unwrap_or_default();
                                        let _ = state.tx.send(serde_json::json!({
                                            "type": "voice_e2ee", "board_id": bid, "from": username,
                                            "from_key": from_key, "to": cm.to, "data": data,
                                        }).to_string());
                                    }
                                }
                            }
                            // Voice moderation (Mute / Deafen / Move Members): on members
                            // with a lower role. Enforced by the server's forwarding
                            // (voice.rs), not just shown.
                            "voice_server_mute" | "voice_server_deafen" => {
                                let Some(target) = cm.target.clone() else { continue };
                                let deafen = cm.msg_type == "voice_server_deafen";
                                let perm = if deafen { crate::roles::DEAFEN_MEMBERS } else { crate::roles::MUTE_MEMBERS };
                                let allowed = crate::roles::has(&state.pool, &username, perm).await
                                    && (target == username || crate::roles::outranks(&state.pool, &username, &target).await);
                                if !allowed {
                                    send_to_user(&state, &username, serde_json::json!({
                                        "type": "action_denied",
                                        "message": format!("You can't server-{} {target}", if deafen { "deafen" } else { "mute" }),
                                    }));
                                    continue;
                                }
                                if deafen {
                                    let on = cm.deafened.unwrap_or(false);
                                    voice::set_server_deafened(&state, &target, on).await;
                                    tracing::info!("voice: {username} {} {target}", if on { "server-deafened" } else { "undeafened" });
                                } else {
                                    let on = cm.muted.unwrap_or(false);
                                    { let mut m = server_muted().lock().unwrap(); if on { m.insert(target.clone()); } else { m.remove(&target); } }
                                    update_voice_blocks(&state, &target).await;
                                    tracing::info!("voice: {username} {} {target}", if on { "server-muted" } else { "unmuted" });
                                }
                                broadcast_voice_state(&state);
                            }
                            // Move someone to another voice channel (board_id), or
                            // disconnect them (no board_id). They're taken out of the
                            // call they're in here and now, so it holds whatever their
                            // app does; for a move, their app is told to join the other.
                            "voice_move" => {
                                let Some(target) = cm.target.clone() else { continue };
                                let allowed = crate::roles::has(&state.pool, &username, crate::roles::MOVE_MEMBERS).await
                                    && crate::roles::outranks(&state.pool, &username, &target).await;
                                if !allowed {
                                    send_to_user(&state, &username, serde_json::json!({
                                        "type": "action_denied", "message": format!("You can't move {target}"),
                                    }));
                                    continue;
                                }
                                let from = { state.voice.lock().unwrap().get(&target).cloned() };
                                let Some(from) = from else { continue }; // not in a call
                                let dest = cm.board_id.clone().filter(|b| !b.is_empty());
                                if let Some(dest) = &dest {
                                    if *dest == from { continue; }
                                    let room_type: Option<String> = sqlx::query(
                                        "SELECT rooms.room_type AS room_type FROM boards JOIN rooms ON boards.room_id = rooms.id WHERE boards.id = ?"
                                    ).bind(dest).fetch_optional(&state.pool).await.ok().flatten().map(|r| r.get("room_type"));
                                    if room_type.as_deref() != Some("voice") || !crate::access::can_view_board(&state.pool, &target, dest).await {
                                        send_to_user(&state, &username, serde_json::json!({
                                            "type": "action_denied", "message": format!("{target} can't be moved there"),
                                        }));
                                        continue;
                                    }
                                }
                                let target_conn = voice_owner(&target);
                                { voice_owners().lock().unwrap().remove(&target); }
                                { state.voice.lock().unwrap().remove(&target); }
                                { state.voice_status.lock().unwrap().remove(&target); }
                                voice::close_participant(&state, &from, &target).await;
                                broadcast_voice_state(&state);
                                match dest {
                                    Some(dest) => {
                                        tracing::info!("voice: {username} moved {target} to {dest}");
                                        let _ = state.tx.send(serde_json::json!({
                                            "type": "voice_move", "target": target, "target_conn": target_conn,
                                            "board_id": dest, "by": username,
                                        }).to_string());
                                    }
                                    None => {
                                        tracing::info!("voice: {username} disconnected {target}");
                                        let _ = state.tx.send(serde_json::json!({
                                            "type": "voice_removed", "target": target, "target_conn": target_conn,
                                            "message": format!("{username} disconnected you from the call"),
                                        }).to_string());
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
                            // Their status — online / away / invisible, and what they're
                            // playing — sealed with the server-wide presence key
                            // (channel_keys.rs), so all the server ever sees is a blob
                            // of the same size, sent about once a minute whether
                            // anything changed or not. It's passed on as it is; each
                            // app opens it and decides how to show them (an invisible
                            // person's app says so inside, and everyone's app hides
                            // them). Nothing about it is logged.
                            "presence" => {
                                let Some(blob) = cm.blob.filter(|b| b.len() <= MAX_PRESENCE_BLOB && b.starts_with("e2ee:1:")) else { continue };
                                // A readable status from before (or from the address it
                                // connected with) no longer applies.
                                state.presence.lock().unwrap().remove(&username);
                                presence_blobs().lock().unwrap().insert(username.clone(), blob.clone());
                                let _ = state.tx.send(serde_json::json!({
                                    "type": "presence", "username": username, "blob": blob,
                                }).to_string());
                            }
                            // What game they're playing (their app works it out, if they
                            // let it), shown as "Playing …" by their name. One per
                            // account: the device that set it, until it clears it or
                            // disconnects.
                            "set_activity" => {
                                let game: String = cm.game.as_deref().unwrap_or("").chars()
                                    .filter(|c| !c.is_control()).take(128).collect::<String>().trim().to_string();
                                let changed = {
                                    let mut a = activities().lock().unwrap();
                                    if game.is_empty() {
                                        let mine = a.get(&username).map_or(false, |cur| cur.conn == conn_id);
                                        if mine { a.remove(&username); }
                                        mine
                                    } else {
                                        let since = match a.get(&username) {
                                            Some(cur) if cur.name == game => cur.since.clone(),
                                            _ => chrono::Utc::now().to_rfc3339(),
                                        };
                                        let same = a.get(&username).map_or(false, |cur| cur.name == game && cur.conn == conn_id);
                                        a.insert(username.clone(), Activity { conn: conn_id, name: game, since });
                                        !same
                                    }
                                };
                                if changed { broadcast_users(&state); }
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
                            // Their nickname on this server only ("" clears it, so their
                            // global nickname or username shows again).
                            "set_nickname" if !crate::roles::has(&state.pool, &username, crate::roles::CHANGE_NICKNAME).await => {
                                send_to_user(&state, &username, serde_json::json!({
                                    "type": "action_denied", "message": "You don't have permission to change your nickname on this server",
                                }));
                            }
                            // Someone else's nickname on this server (Manage Nicknames,
                            // members with a lower role only).
                            "set_member_nickname" => {
                                let Some(target) = cm.target.clone() else { continue };
                                let allowed = crate::roles::has(&state.pool, &username, crate::roles::MANAGE_NICKNAMES).await
                                    && (target == username || crate::roles::outranks(&state.pool, &username, &target).await);
                                if !allowed {
                                    send_to_user(&state, &username, serde_json::json!({
                                        "type": "action_denied", "message": format!("You can't change {target}'s nickname"),
                                    }));
                                    continue;
                                }
                                match crate::profile::clean_nickname(cm.nickname.as_deref().unwrap_or("")) {
                                    Ok(nick) => {
                                        let saved = sqlx::query("UPDATE users SET nickname = ? WHERE username = ?")
                                            .bind(&nick).bind(&target).execute(&state.pool).await;
                                        if saved.map(|r| r.rows_affected()).unwrap_or(0) > 0 {
                                            tracing::info!("nickname: {username} set {target}'s to {:?}", nick);
                                            broadcast_users(&state);
                                        }
                                    }
                                    Err(why) => send_to_user(&state, &username, serde_json::json!({
                                        "type": "action_denied", "message": why,
                                    })),
                                }
                            }
                            "set_nickname" => {
                                match crate::profile::clean_nickname(cm.nickname.as_deref().unwrap_or("")) {
                                    Ok(nick) => {
                                        let saved = sqlx::query("UPDATE users SET nickname = ? WHERE username = ?")
                                            .bind(&nick).bind(&username).execute(&state.pool).await;
                                        match saved {
                                            Ok(_) => {
                                                tracing::info!("nickname: {username} is now {:?} on this server", nick);
                                                broadcast_users(&state);
                                            }
                                            Err(e) => {
                                                tracing::warn!("nickname: couldn't save {username}'s nickname: {e}");
                                                send_to_user(&state, &username, serde_json::json!({
                                                    "type": "action_denied", "message": "Couldn't save your nickname — try again",
                                                }));
                                            }
                                        }
                                    }
                                    Err(why) => send_to_user(&state, &username, serde_json::json!({
                                        "type": "action_denied", "message": why,
                                    })),
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
                                    let tint = cm.tint.unwrap_or_default();
                                    let nickname = cm.nickname.unwrap_or_default();
                                    let old_nick = crate::profile::global_nickname(&user_key);
                                    match crate::profile::save_cached(&user_key, &bio, &tint, &nickname, banner.as_deref(), updated_at) {
                                        Ok(()) => {
                                            let _ = state.tx.send(serde_json::json!({
                                                "type": "profile_updated", "username": username, "updated_at": updated_at,
                                            }).to_string());
                                            // Their global nickname goes out with the member
                                            // list, which is what every client names people by.
                                            if crate::profile::global_nickname(&user_key) != old_nick {
                                                broadcast_users(&state);
                                            }
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
                        if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&bcast) {
                            // Channels or roles changed: work out again what this member can see.
                            if matches!(v["type"].as_str(), Some("rooms_updated") | Some("roles_updated")) {
                                visible = crate::access::visible_for(&state.pool, &username).await;
                                // In a call: Speak / Video may have changed.
                                let in_call = { state.voice.lock().unwrap().contains_key(&username) };
                                if in_call && voice_owner(&username) == Some(conn_id) {
                                    update_voice_blocks(&state, &username).await;
                                }
                                // In a voice channel they can no longer see? Take them out of it.
                                let in_board = { state.voice.lock().unwrap().get(&username).cloned() };
                                if let Some(bid) = in_board.filter(|b| !sees(&visible, b) && voice_owner(&username) == Some(conn_id)) {
                                    { voice_owners().lock().unwrap().remove(&username); }
                                    { state.voice.lock().unwrap().remove(&username); }
                                    { state.voice_status.lock().unwrap().remove(&username); }
                                    voice::close_participant(&state, &bid, &username).await;
                                    broadcast_voice_state(&state);
                                    let notice = serde_json::json!({ "type": "voice_removed", "message": "You no longer have access to that voice channel" });
                                    if sink.send(Message::Text(notice.to_string())).await.is_err() { break; }
                                }
                            }
                            let mut bcast = bcast;
                            // Who's in which call: leave out channels they can't see.
                            if v["type"].as_str() == Some("voice_state") && visible.is_some() {
                                for key in ["channels", "reconnecting"] {
                                    if let Some(map) = v[key].as_object_mut() {
                                        map.retain(|board, _| sees(&visible, board));
                                    }
                                }
                                bcast = v.to_string();
                            }
                            let fwd = match v["type"].as_str() {
                                Some("users") | Some("server_updated") | Some("server_restarting") => true,
                                Some("typing") => v["username"].as_str() != Some(username.as_str()),
                                Some("rooms_updated") => true,
                                Some("voice_state") => true,
                                Some("roles_updated") => true,
                                Some("emojis_updated") | Some("stickers_updated") => true,
                                // Message encryption keys moved (channel_keys.rs): the app shares
                                // keys others need and retries messages it couldn't open. Only for
                                // channels this member can see (or none named).
                                Some("e2ee_changed") => v["board_id"].as_str().map_or(true, |b| b == crate::channel_keys::PRESENCE_KEY_ID || sees(&visible, b)),
                                Some("presence") => true,
                                // Imported history added or claimed (imports.rs): the app loads
                                // the channel again if it's showing it.
                                Some("history_changed") => v["board_id"].as_str().map_or(true, |b| sees(&visible, b)),
                                Some("action_denied") => v["target"].as_str() == Some(username.as_str()),
                                // Speaking rings are only for people in that same call —
                                // someone just browsing the server still gets voice_state
                                // (who's in which channel) but no activity.
                                Some("voice_speaking") => {
                                    let my_board = state.voice.lock().unwrap().get(&username).cloned();
                                    my_board.as_deref().is_some() && v["board_id"].as_str() == my_board.as_deref()
                                        && voice_owner(&username) == Some(conn_id) // not your other devices
                                }
                                Some("voice_moved") => v["target"].as_str() == Some(username.as_str()) && v["target_conn"].as_u64() == Some(conn_id),
                                // Moved or disconnected by a moderator: the device that was in the call.
                                Some("voice_move") | Some("voice_removed") => v["target"].as_str() == Some(username.as_str())
                                    && (v["target_conn"].is_null() || v["target_conn"].as_u64() == Some(conn_id)),
                                // Key exchange: only to the others in that same call (just the
                                // one it's addressed to, if it is), on the device in the call.
                                Some("voice_e2ee") => {
                                    let my_board = state.voice.lock().unwrap().get(&username).cloned();
                                    my_board.as_deref().is_some() && v["board_id"].as_str() == my_board.as_deref()
                                        && voice_owner(&username) == Some(conn_id)
                                        && v["from"].as_str() != Some(username.as_str())
                                        && (v["to"].is_null() || v["to"].as_str() == Some(username.as_str()))
                                }
                                Some("voice_mute_state") => true,
                                Some("pfp_updated") | Some("profile_updated") => true,
                                Some("message") => v["data"]["board_id"].as_str().map_or(true, |b| sees(&visible, b)),
                                Some("message_edit") | Some("message_delete") | Some("message_pin") | Some("message_reaction") => subscribed_board.as_deref()
                                    .map(|bid| v["board_id"].as_str() == Some(bid))
                                    .unwrap_or(false),
                                Some("voice_answer") | Some("voice_ice") | Some("voice_status_snapshot") | Some("voice_renegotiate") | Some("voice_screen_low") | Some("voice_screen_viewers") =>
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
            activities().lock().unwrap().remove(&username);
            presence_blobs().lock().unwrap().remove(&username);
            // Stop any typing indicator left showing for them.
            let _ = state.tx.send(serde_json::json!({ "type": "typing", "username": username, "typing": false }).to_string());
        } else {
            *c -= 1;
            // Their other device stays: only the game this one reported goes.
            let mut a = activities().lock().unwrap();
            if a.get(&username).map_or(false, |cur| cur.conn == conn_id) { a.remove(&username); }
        }
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
/// Who has their camera on (only meaningful while they're in a call —
/// broadcast_voice_state lists just those still in one).
/// Who's sharing their screen right now (like video_on).
fn screen_on() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static ON: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    ON.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// username → where the window they're sharing is, when it isn't in front:
/// "background" or "minimized" (see voice_screen_window).
fn screen_window() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    static W: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> = std::sync::OnceLock::new();
    W.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn video_on() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static ON: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    ON.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}
fn voice_owner(user: &str) -> Option<u64> { voice_owners().lock().unwrap().get(user).copied() }

/// Who a moderator has server-muted (until they're unmuted; kept while the
/// server runs, across leaving and rejoining calls).
fn server_muted() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Works out what the server won't pass on from someone in a call (see
/// voice.rs): their mic without Speak or while server-muted, their camera
/// and screen without Video. Losing Video switches a camera or share off.
async fn update_voice_blocks(state: &Arc<AppState>, username: &str) {
    let perms = crate::roles::permissions_of(&state.pool, username).await;
    let muted = { server_muted().lock().unwrap().contains(username) };
    let mut bits = 0u8;
    if perms & crate::roles::SPEAK == 0 || muted { bits |= voice::BLOCK_AUDIO; }
    if perms & crate::roles::VIDEO == 0 { bits |= voice::BLOCK_VIDEO; }
    // Listen: whether anyone's voice is sent to them.
    let listen_changed = voice::set_can_listen(state, username, perms & crate::roles::LISTEN != 0).await;
    let before = voice::blocked(username);
    voice::set_blocked(username, bits);
    if bits & voice::BLOCK_VIDEO != 0 {
        let had_camera = { video_on().lock().unwrap().remove(username) };
        let had_screen = { screen_on().lock().unwrap().remove(username) };
        if had_screen {
            { screen_window().lock().unwrap().remove(username); }
            let board = { state.voice.lock().unwrap().get(username).cloned() };
            if let Some(board) = board {
                voice::screen_stopped(state, &board, username).await;
            }
        }
        if had_camera || had_screen {
            send_to_user(state, username, serde_json::json!({
                "type": "action_denied", "message": "You don't have permission to use video here any more",
            }));
            broadcast_voice_state(state);
        }
    }
    if before != bits || listen_changed { broadcast_voice_state(state); }
}
/// True if this connection may act on the account's call: it's the owner,
/// or nobody owns it right now.
/// The most a voice_e2ee message may be (bytes of JSON): a call key and
/// its signature, or one wrapped key, are a few hundred.
const VOICE_E2EE_MAX: usize = 4096;

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
    // Who's sharing a camera — only people actually in a call.
    let video: Vec<String> = {
        let voice = state.voice.lock().unwrap();
        let on = video_on().lock().unwrap();
        on.iter().filter(|u| voice.contains_key(*u)).cloned().collect()
    };
    let screens: Vec<String> = {
        let voice = state.voice.lock().unwrap();
        let on = screen_on().lock().unwrap();
        on.iter().filter(|u| voice.contains_key(*u)).cloned().collect()
    };
    // For the people watching: a sharer's window that's out of focus or
    // minimised (absent = in front, or sharing a whole screen).
    let screen_window: serde_json::Map<String, serde_json::Value> = {
        let w = screen_window().lock().unwrap();
        screens.iter().filter_map(|u| w.get(u).map(|s| (u.clone(), serde_json::Value::String(s.clone())))).collect()
    };
    // Who the server isn't letting be heard (no Speak, or server-muted) and
    // who's server-deafened — only people in a call.
    let (server_muted, server_deafened): (Vec<String>, Vec<String>) = {
        let voice = state.voice.lock().unwrap();
        let mut deaf = voice::server_deafened();
        let no_listen: Vec<String> = voice.keys().filter(|u| voice::cannot_listen(u.as_str()) && !deaf.contains(*u)).cloned().collect();
        deaf.extend(no_listen);
        (
            voice.keys().filter(|u| voice::blocked(u) & voice::BLOCK_AUDIO != 0).cloned().collect(),
            deaf.into_iter().filter(|u| voice.contains_key(u)).collect(),
        )
    };
    let _ = state.tx.send(serde_json::json!({
        "type": "voice_state", "channels": channels, "reconnecting": reconnecting, "mute_states": mute_states,
        "video_on": video, "screen_on": screens, "screen_window": screen_window,
        "server_muted": server_muted, "server_deafened": server_deafened,
    }).to_string());
}

/// Sends everyone the member list: who's online (invisible people are
/// left out, so they look offline) and, under "presence", each online
/// person's status ("online" or "idle"). Not "statuses" — that key already
/// means the voice mute/deafen list on voice_status_snapshot, and the Go
/// client decodes every server message into one struct.
/// The game someone's playing (set_activity), and which of their
/// connections said so.
struct Activity { conn: u64, name: String, since: String }

/// The most a status blob may be (it's padded to one size well under this).
const MAX_PRESENCE_BLOB: usize = 4096;

/// Each connected member's latest status blob ("presence"), for people
/// who connect later.
fn presence_blobs() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    static P: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> = std::sync::OnceLock::new();
    P.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn activities() -> &'static std::sync::Mutex<std::collections::HashMap<String, Activity>> {
    static A: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Activity>>> = std::sync::OnceLock::new();
    A.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub fn broadcast_users(state: &Arc<AppState>) {
    let (online, statuses, playing, blobs) = {
        let o = state.online.lock().unwrap();
        let p = state.presence.lock().unwrap();
        let acts = activities().lock().unwrap();
        let pb = presence_blobs().lock().unwrap();
        let mut online = Vec::new();
        let mut statuses = serde_json::Map::new();
        let mut playing = serde_json::Map::new();
        let mut blobs = serde_json::Map::new();
        for user in o.keys() {
            if let Some(b) = pb.get(user) {
                blobs.insert(user.clone(), serde_json::Value::String(b.clone()));
            }
            let st = p.get(user).map(String::as_str).unwrap_or("online");
            if st == "invisible" { continue; }
            online.push(user.clone());
            statuses.insert(user.clone(), serde_json::Value::String(st.to_string()));
            if let Some(a) = acts.get(user) {
                playing.insert(user.clone(), serde_json::json!({ "name": a.name, "since": a.since }));
            }
        }
        (online, statuses, playing, blobs)
    };
    let state = Arc::clone(state);
    tokio::spawn(async move {
        let all = crate::db::all_known_users(&state.pool).await.unwrap_or_default();
        let owner = crate::admin::owner_username(&state.pool).await;
        // What to call people: "nicknames" are this server's own (set_nickname),
        // "global_nicknames" the ones from their profiles. A client shows
        // the server nickname, else the global one, else the username.
        let mut nicknames = serde_json::Map::new();
        let mut global_nicknames = serde_json::Map::new();
        if let Ok(rows) = sqlx::query("SELECT username, public_key, nickname FROM users").fetch_all(&state.pool).await {
            for r in rows {
                let user: String = r.get("username");
                let nick: String = r.get("nickname");
                if !nick.is_empty() {
                    nicknames.insert(user.clone(), serde_json::Value::String(nick));
                }
                let global = crate::profile::global_nickname(&r.get::<String, _>("public_key").to_lowercase());
                if !global.is_empty() {
                    global_nicknames.insert(user, serde_json::Value::String(global));
                }
            }
        }
        let _ = state.tx.send(serde_json::json!({
            "type": "users", "online": online, "presence": statuses, "all": all, "owner": owner,
            "activities": playing, "presence_blobs": blobs,
            "nicknames": nicknames, "global_nicknames": global_nicknames,
        }).to_string());
    });
}

async fn load_history(state: &Arc<AppState>, board_id: &str) -> Vec<serde_json::Value> {
    let mut rows = match sqlx::query(
        "SELECT id, board_id, username, content,
                attachment_url, attachment_name, attachment_mime,
                attachments, edited, created_at, pinned_at, reply_to, kind, imported_from
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
    reply_to:    Option<String>,
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

    let reply_to = crate::messages::valid_reply_target(&state.pool, board_id, reply_to.as_deref()).await;
    if reply_to.is_some() {
        let _ = sqlx::query("UPDATE messages SET reply_to = ? WHERE id = ?").bind(&reply_to).bind(&id).execute(&state.pool).await;
    }
    let mut msg = crate::messages::ChatMessage {
        id, board_id: board_id.to_string(), username: username.to_string(),
        content: content.to_string(), attachments, edited: false, created_at: now, pinned: false, reactions: Vec::new(),
        reply_to, reply: None, kind: None, imported_from: None,
    };
    crate::messages::attach_replies(&state.pool, std::slice::from_mut(&mut msg)).await;
    let _ = state.tx.send(serde_json::json!({ "type": "message", "data": msg }).to_string());
}
