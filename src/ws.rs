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
pub struct WsQuery { pub token: String }

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
}

pub async fn ws_handler(
    ws:           WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, query.token, state))
}

async fn handle_socket(socket: WebSocket, token: String, state: Arc<AppState>) {
    let username = match db::verify_token(&state.pool, &token).await {
        Ok(Some(u)) => u,
        _ => return,
    };

    { let mut o = state.online.lock().unwrap(); *o.entry(username.clone()).or_insert(0) += 1; }
    broadcast_users(&state);
    broadcast_voice_state(&state);

    let mut rx = state.tx.subscribe();
    let mut subscribed_board: Option<String> = None;
    let (mut sink, mut stream) = socket.split();

    loop {
        tokio::select! {
            msg = stream.next() => {
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
                                        // A user can only be in one voice channel at a time —
                                        // inserting simply overwrites any previous entry, so
                                        // moving between channels needs no separate leave step.
                                        { state.voice.lock().unwrap().insert(username.clone(), bid.clone()); }
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
                            "leave_voice" => {
                                let left_board = { state.voice.lock().unwrap().remove(&username) };
                                if let Some(bid) = left_board {
                                    { state.voice_status.lock().unwrap().remove(&username); }
                                    voice::close_participant(&state, &bid, &username).await;
                                    broadcast_voice_state(&state);
                                }
                            }
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
                                    }
                                }
                            }
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
                            _ => {}
                        }
                    }
                    None | Some(Ok(Message::Close(_))) => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            result = rx.recv() => {
                match result {
                    Ok(bcast) => {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&bcast) {
                            let fwd = match v["type"].as_str() {
                                Some("users") => true,
                                Some("rooms_updated") => true,
                                Some("voice_state") => true,
                                Some("voice_speaking") => true,
                                Some("voice_mute_state") => true,
                                Some("message") => true,
                                Some("message_edit") | Some("message_delete") => subscribed_board.as_deref()
                                    .map(|bid| v["board_id"].as_str() == Some(bid))
                                    .unwrap_or(false),
                                Some("voice_answer") | Some("voice_ice") | Some("voice_status_snapshot") =>
                                    v["target"].as_str() == Some(username.as_str()),
                                _ => false,
                            };
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
        if *c <= 1 { o.remove(&username); } else { *c -= 1; }
    }
    broadcast_users(&state);

    // A dropped connection is an implicit leave from voice too — otherwise a
    // crashed or closed client leaves a ghost entry showing them still "in"
    // the channel forever.
    let left_board = { state.voice.lock().unwrap().remove(&username) };
    if let Some(bid) = left_board {
        { state.voice_status.lock().unwrap().remove(&username); }
        voice::close_participant(&state, &bid, &username).await;
        broadcast_voice_state(&state);
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

fn broadcast_voice_state(state: &Arc<AppState>) {
    let channels: std::collections::HashMap<String, Vec<String>> = {
        let voice = state.voice.lock().unwrap();
        let mut m: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        for (user, board) in voice.iter() {
            m.entry(board.clone()).or_default().push(user.clone());
        }
        m
    };
    let _ = state.tx.send(serde_json::json!({ "type": "voice_state", "channels": channels }).to_string());
}

fn broadcast_users(state: &Arc<AppState>) {
    let online: Vec<String> = { state.online.lock().unwrap().keys().cloned().collect() };
    let state = Arc::clone(state);
    tokio::spawn(async move {
        let all = crate::db::all_known_users(&state.pool).await.unwrap_or_default();
        let _ = state.tx.send(serde_json::json!({ "type": "users", "online": online, "all": all }).to_string());
    });
}

async fn load_history(state: &Arc<AppState>, board_id: &str) -> Vec<serde_json::Value> {
    let mut rows = match sqlx::query(
        "SELECT id, board_id, username, content,
                attachment_url, attachment_name, attachment_mime,
                attachments, edited, created_at
         FROM messages WHERE board_id = ? ORDER BY id DESC LIMIT ?",
    ).bind(board_id).bind(HISTORY_PAGE).fetch_all(&state.pool).await {
        Ok(r) => r,
        Err(e) => { tracing::error!("load_history: {}", e); return vec![]; }
    };
    rows.reverse();
    rows.iter().filter_map(|r| serde_json::to_value(row_to_msg(r)).ok()).collect()
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
        content: content.to_string(), attachments, edited: false, created_at: now,
    };
    let _ = state.tx.send(serde_json::json!({ "type": "message", "data": msg }).to_string());
}
