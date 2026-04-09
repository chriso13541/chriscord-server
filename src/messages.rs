use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::sync::Arc;

use crate::{db, state::AppState};

#[derive(Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    pub id:              String,
    pub board_id:        String,
    pub username:        String,
    pub content:         String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_url:  Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_mime: Option<String>,
    pub created_at:      String,
}

#[derive(Deserialize)]
pub struct SendMsgReq {
    pub content:         Option<String>,
    pub attachment_url:  Option<String>,
    pub attachment_name: Option<String>,
    pub attachment_mime: Option<String>,
}

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn db_err() -> ApiErr {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "DB error" })))
}

fn token_from(h: &HeaderMap) -> &str {
    h.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("")
}

pub fn row_to_msg(r: &sqlx::sqlite::SqliteRow) -> ChatMessage {
    ChatMessage {
        id:              r.get("id"),
        board_id:        r.get("board_id"),
        username:        r.get("username"),
        content:         r.get("content"),
        attachment_url:  r.get("attachment_url"),
        attachment_name: r.get("attachment_name"),
        attachment_mime: r.get("attachment_mime"),
        created_at:      r.get("created_at"),
    }
}

pub async fn get_messages(
    headers:        HeaderMap,
    Path(board_id): Path<String>,
    State(s):       State<Arc<AppState>>,
) -> Result<Json<Vec<ChatMessage>>, ApiErr> {
    db::verify_token(&s.pool, token_from(&headers))
        .await.map_err(|_| db_err())?
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "Unauthorized" }))))?;

    let rows = sqlx::query(
        "SELECT id, board_id, username, content,
                attachment_url, attachment_name, attachment_mime, created_at
         FROM messages WHERE board_id = ? ORDER BY created_at ASC LIMIT 100",
    ).bind(&board_id).fetch_all(&s.pool).await.map_err(|_| db_err())?;

    Ok(Json(rows.iter().map(row_to_msg).collect()))
}

pub async fn post_message(
    headers:        HeaderMap,
    Path(board_id): Path<String>,
    State(s):       State<Arc<AppState>>,
    Json(body):     Json<SendMsgReq>,
) -> Result<Json<ChatMessage>, ApiErr> {
    let username = db::verify_token(&s.pool, token_from(&headers))
        .await.map_err(|_| db_err())?
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "Unauthorized" }))))?;

    let content = body.content.as_deref().unwrap_or("").trim().to_string();
    if content.is_empty() && body.attachment_url.is_none() {
        return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "Must have content or attachment" }))));
    }

    let id  = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO messages (id, board_id, username, content,
            attachment_url, attachment_name, attachment_mime, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id).bind(&board_id).bind(&username).bind(&content)
    .bind(&body.attachment_url).bind(&body.attachment_name).bind(&body.attachment_mime)
    .bind(&now)
    .execute(&s.pool).await.map_err(|_| db_err())?;

    let msg = ChatMessage {
        id, board_id, username, content,
        attachment_url:  body.attachment_url,
        attachment_name: body.attachment_name,
        attachment_mime: body.attachment_mime,
        created_at: now,
    };
    let _ = s.tx.send(serde_json::json!({ "type": "message", "data": msg }).to_string());
    Ok(Json(msg))
}
