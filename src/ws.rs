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

use crate::{db, state::AppState};

// ── Protocol types ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct WsQuery {
    pub token: String,
}

/// Messages sent from the client to the server over WS.
#[derive(Deserialize)]
struct ClientMsg {
    #[serde(rename = "type")]
    msg_type: String,
    board_id: Option<String>,
    content:  Option<String>,
}

// ── Handler ───────────────────────────────────────────────────────────────────

pub async fn ws_handler(
    ws:                WebSocketUpgrade,
    Query(query):      Query<WsQuery>,
    State(state):      State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, query.token, state))
}

async fn handle_socket(socket: WebSocket, token: String, state: Arc<AppState>) {
    // Verify the session token before doing anything
    let username = match db::verify_token(&state.pool, &token).await {
        Ok(Some(u)) => u,
        _ => return, // Invalid token — just close
    };

    let mut rx = state.tx.subscribe();
    let mut subscribed_board: Option<String> = None;

    // Split the socket so we can independently receive from the client and
    // send broadcasts without fighting the borrow checker.
    let (mut sink, mut stream) = socket.split();

    loop {
        tokio::select! {
            // ── Inbound: message from this client ─────────────────────────
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(cm) = serde_json::from_str::<ClientMsg>(&text) else {
                            continue;
                        };
                        match cm.msg_type.as_str() {
                            "subscribe" => {
                                if let Some(bid) = cm.board_id {
                                    // Send message history immediately on subscribe
                                    let history = load_history(&state, &bid).await;
                                    let payload = serde_json::json!({
                                        "type":     "history",
                                        "messages": history,
                                    });
                                    if sink.send(Message::Text(payload.to_string())).await.is_err() {
                                        break;
                                    }
                                    subscribed_board = Some(bid);
                                }
                            }
                            "message" => {
                                if let (Some(bid), Some(content)) = (cm.board_id, cm.content) {
                                    let content = content.trim().to_string();
                                    if !content.is_empty() && content.len() <= 2000 {
                                        save_and_broadcast(&state, &bid, &username, &content).await;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    // Client disconnected or sent a close frame
                    None | Some(Ok(Message::Close(_))) => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }

            // ── Outbound: message from broadcast channel ───────────────────
            result = rx.recv() => {
                match result {
                    Ok(bcast) => {
                        // Only forward if the client is subscribed to this board
                        if let Some(ref bid) = subscribed_board {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&bcast) {
                                let matches = v["data"]["board_id"]
                                    .as_str()
                                    .map(|id| id == bid)
                                    .unwrap_or(false);
                                if matches {
                                    if sink.send(Message::Text(bcast)).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    // Lagged — receiver fell behind; skip and continue
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    // Channel closed (server shutting down)
                    Err(_) => break,
                }
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn load_history(state: &Arc<AppState>, board_id: &str) -> Vec<serde_json::Value> {
    let rows = sqlx::query(
        "SELECT id, board_id, username, content, created_at
         FROM messages
         WHERE board_id = ?
         ORDER BY created_at ASC
         LIMIT 100",
    )
    .bind(board_id)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    rows.iter()
        .map(|r| {
            serde_json::json!({
                "id":         r.get::<String, _>("id"),
                "board_id":   r.get::<String, _>("board_id"),
                "username":   r.get::<String, _>("username"),
                "content":    r.get::<String, _>("content"),
                "created_at": r.get::<String, _>("created_at"),
            })
        })
        .collect()
}

async fn save_and_broadcast(
    state:    &Arc<AppState>,
    board_id: &str,
    username: &str,
    content:  &str,
) {
    let id  = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    let _ = sqlx::query(
        "INSERT INTO messages (id, board_id, username, content, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(board_id)
    .bind(username)
    .bind(content)
    .bind(&now)
    .execute(&state.pool)
    .await;

    let payload = serde_json::json!({
        "type": "message",
        "data": {
            "id":         id,
            "board_id":   board_id,
            "username":   username,
            "content":    content,
            "created_at": now,
        }
    });
    let _ = state.tx.send(payload.to_string());
}
