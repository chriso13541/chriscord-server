use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;

use crate::{db, messages::row_to_msg, state::AppState};

#[derive(Deserialize)]
pub struct WsQuery {
    pub token: String,
}

#[derive(Deserialize)]
struct ClientMsg {
    #[serde(rename = "type")]
    msg_type: String,
    board_id: Option<String>,
    content:  Option<String>,
    // attachment fields (sent together with content)
    attachment_url:  Option<String>,
    attachment_name: Option<String>,
    attachment_mime: Option<String>,
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

    // Register as online
    {
        let mut online = state.online.lock().unwrap();
        *online.entry(username.clone()).or_insert(0) += 1;
    }
    broadcast_users(&state);

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
                                        "type": "history",
                                        "board_id": &bid,
                                        "messages": history,
                                    });
                                    if sink.send(Message::Text(payload.to_string())).await.is_err() { break; }
                                    subscribed_board = Some(bid);
                                }
                            }
                            "message" => {
                                if let Some(bid) = cm.board_id {
                                    let content = cm.content.as_deref().unwrap_or("").trim().to_string();
                                    let has_attachment = cm.attachment_url.is_some();
                                    if !content.is_empty() || has_attachment {
                                        save_and_broadcast(
                                            &state, &bid, &username, &content,
                                            cm.attachment_url,
                                            cm.attachment_name,
                                            cm.attachment_mime,
                                        ).await;
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
                            // Always forward user-list updates; filter messages by board
                            let fwd = match v["type"].as_str() {
                                Some("users") => true,
                                Some("message") => {
                                    subscribed_board.as_deref()
                                        .map(|bid| v["data"]["board_id"].as_str() == Some(bid))
                                        .unwrap_or(false)
                                }
                                _ => false,
                            };
                            if fwd {
                                if sink.send(Message::Text(bcast)).await.is_err() { break; }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
    }

    // Unregister — decrement and remove if last connection
    {
        let mut online = state.online.lock().unwrap();
        let count = online.entry(username.clone()).or_insert(0);
        if *count <= 1 { online.remove(&username); } else { *count -= 1; }
    }
    broadcast_users(&state);
}

fn broadcast_users(state: &Arc<AppState>) {
    let online: Vec<String> = {
        let online = state.online.lock().unwrap();
        online.keys().cloned().collect()
    };

    // Spawn a task to fetch all known users from DB and broadcast.
    // We can't block here (sync fn), so fire-and-forget.
    let state = Arc::clone(state);
    tokio::spawn(async move {
        let all = crate::db::all_known_users(&state.pool)
            .await
            .unwrap_or_default();

        let payload = serde_json::json!({
            "type":   "users",
            "online": online,
            "all":    all,
        });
        let _ = state.tx.send(payload.to_string());
    });
}

async fn load_history(state: &Arc<AppState>, board_id: &str) -> Vec<serde_json::Value> {
    let rows = match sqlx::query(
        "SELECT id, board_id, username, content,
                attachment_url, attachment_name, attachment_mime, created_at
         FROM messages WHERE board_id = ?
         ORDER BY created_at ASC LIMIT 100",
    )
    .bind(board_id)
    .fetch_all(&state.pool)
    .await {
        Ok(r)  => r,
        Err(e) => {
            tracing::error!("load_history query failed: {}", e);
            return vec![];
        }
    };

    rows.iter()
        .filter_map(|r| serde_json::to_value(row_to_msg(r)).ok())
        .collect()
}

async fn save_and_broadcast(
    state:           &Arc<AppState>,
    board_id:        &str,
    username:        &str,
    content:         &str,
    attachment_url:  Option<String>,
    attachment_name: Option<String>,
    attachment_mime: Option<String>,
) {
    let id  = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    let _ = sqlx::query(
        "INSERT INTO messages (id, board_id, username, content,
            attachment_url, attachment_name, attachment_mime, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id).bind(board_id).bind(username).bind(content)
    .bind(&attachment_url).bind(&attachment_name).bind(&attachment_mime)
    .bind(&now)
    .execute(&state.pool)
    .await;

    let msg = crate::messages::ChatMessage {
        id, board_id: board_id.to_string(), username: username.to_string(),
        content: content.to_string(),
        attachment_url, attachment_name, attachment_mime,
        created_at: now,
    };
    let payload = serde_json::json!({ "type": "message", "data": msg });
    let _ = state.tx.send(payload.to_string());
}
