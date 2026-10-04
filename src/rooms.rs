use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Serialize;
use sqlx::Row;
use std::sync::Arc;

use crate::{db, state::AppState};

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct Room {
    pub id:         String,
    pub name:       String,
    pub is_private: bool,
    pub room_type:  String, // "text" or "voice"
}

#[derive(Serialize)]
pub struct Board {
    pub id:      String,
    pub room_id: String,
    pub name:    String,
    /// Limited to certain roles (see access.rs) — the app shows a lock.
    pub is_private: bool,
}

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn db_err() -> ApiErr {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": "DB error" })),
    )
}

fn unauth() -> ApiErr {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "Unauthorized" })),
    )
}

fn token_from(h: &HeaderMap) -> &str {
    h.get("X-Session-Token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /api/rooms — requires session token.
pub async fn list_rooms(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
) -> Result<Json<Vec<Room>>, ApiErr> {
    let username = db::verify_token(&s.pool, token_from(&headers))
        .await
        .map_err(|_| db_err())?
        .ok_or_else(unauth)?;
    // Categories you can't see aren't listed at all.
    let visible = crate::access::visible_for(&s.pool, &username).await;

    let rows = sqlx::query(
        "SELECT id, name, is_private, room_type FROM rooms ORDER BY position, created_at",
    )
    .fetch_all(&s.pool)
    .await
    .map_err(|_| db_err())?;

    Ok(Json(
        rows.iter()
            .filter(|r| visible.as_ref().map_or(true, |v| v.rooms.contains(&r.get::<String, _>("id"))))
            .map(|r| Room {
                id:         r.get("id"),
                name:       r.get("name"),
                is_private: r.get::<i64, _>("is_private") != 0,
                room_type:  r.get("room_type"),
            })
            .collect(),
    ))
}

/// GET /api/rooms/:id/boards — boards readable by any authenticated user or owner.
pub async fn list_boards(
    headers: HeaderMap,
    Path(room_id): Path<String>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<Vec<Board>>, ApiErr> {
    // Accept either a valid session token OR the owner key
    let token     = token_from(&headers);
    let owner_key = headers.get("X-Owner-Key").and_then(|v| v.to_str().ok()).unwrap_or("");
    let is_owner  = !owner_key.is_empty() && owner_key == s.owner_key;

    // Members only get the channels they can see; the admin panel (owner key) gets all.
    let mut visible = None;
    if !is_owner {
        let username = db::verify_token(&s.pool, token)
            .await
            .map_err(|_| db_err())?
            .ok_or_else(unauth)?;
        visible = crate::access::visible_for(&s.pool, &username).await;
    }

    let rows = sqlx::query(
        "SELECT id, room_id, name, is_private FROM boards WHERE room_id = ? ORDER BY position, created_at",
    )
    .bind(&room_id)
    .fetch_all(&s.pool)
    .await
    .map_err(|_| db_err())?;

    Ok(Json(
        rows.iter()
            .filter(|r| visible.as_ref().map_or(true, |v| v.boards.contains(&r.get::<String, _>("id"))))
            .map(|r| Board {
                id:      r.get("id"),
                room_id: r.get("room_id"),
                name:    r.get("name"),
                is_private: r.get::<i64, _>("is_private") != 0,
            })
            .collect(),
    ))
}
