use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::sync::Arc;

use crate::{db, state::AppState};

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    pub id:         String,
    pub board_id:   String,
    pub username:   String,
    pub content:    String,
    pub created_at: String,
}

#[derive(Deserialize)]
pub struct SendMsgReq {
    pub content: String,
}

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn db_err() -> ApiErr {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "DB error" })))
}

fn token_from(h: &HeaderMap) -> &str {
    h.get("X-Session-Token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /api/boards/:id/messages — last 100 messages, oldest first.
pub async fn get_messages(
    headers: HeaderMap,
    Path(board_id): Path<String>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<Vec<ChatMessage>>, ApiErr> {
    db::verify_token(&s.pool, token_from(&headers))
        .await
        .map_err(|_| db_err())?
        .ok_or_else(|| {
            (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "Unauthorized" })))
        })?;

    let rows = sqlx::query(
        "SELECT id, board_id, username, content, created_at
         FROM messages
         WHERE board_id = ?
         ORDER BY created_at ASC
         LIMIT 100",
    )
    .bind(&board_id)
    .fetch_all(&s.pool)
    .await
    .map_err(|_| db_err())?;

    Ok(Json(
        rows.iter()
            .map(|r| ChatMessage {
                id:         r.get("id"),
                board_id:   r.get("board_id"),
                username:   r.get("username"),
                content:    r.get("content"),
                created_at: r.get("created_at"),
            })
            .collect(),
    ))
}

/// POST /api/boards/:id/messages — send a message via REST (also broadcasts over WS).
/// The WS handler also writes messages; this endpoint is for REST clients or fallback.
pub async fn post_message(
    headers: HeaderMap,
    Path(board_id): Path<String>,
    State(s): State<Arc<AppState>>,
    Json(body): Json<SendMsgReq>,
) -> Result<Json<ChatMessage>, ApiErr> {
    let username = db::verify_token(&s.pool, token_from(&headers))
        .await
        .map_err(|_| db_err())?
        .ok_or_else(|| {
            (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "Unauthorized" })))
        })?;

    let content = body.content.trim().to_string();
    if content.is_empty() || content.len() > 2000 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Content must be 1–2000 characters" })),
        ));
    }

    let id  = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO messages (id, board_id, username, content, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&board_id)
    .bind(&username)
    .bind(&content)
    .bind(&now)
    .execute(&s.pool)
    .await
    .map_err(|_| db_err())?;

    let msg = ChatMessage { id, board_id, username, content, created_at: now };

    // Broadcast to all WS connections
    let payload = serde_json::json!({ "type": "message", "data": msg });
    let _ = s.tx.send(payload.to_string());

    Ok(Json(msg))
}
