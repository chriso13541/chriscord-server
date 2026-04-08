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
}

#[derive(Serialize)]
pub struct Board {
    pub id:      String,
    pub room_id: String,
    pub name:    String,
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
    if db::verify_token(&s.pool, token_from(&headers))
        .await
        .map_err(|_| db_err())?
        .is_none()
    {
        return Err(unauth());
    }

    let rows = sqlx::query(
        "SELECT id, name, is_private FROM rooms ORDER BY created_at ASC",
    )
    .fetch_all(&s.pool)
    .await
    .map_err(|_| db_err())?;

    Ok(Json(
        rows.iter()
            .map(|r| Room {
                id:         r.get("id"),
                name:       r.get("name"),
                is_private: r.get::<i64, _>("is_private") != 0,
            })
            .collect(),
    ))
}

/// GET /api/rooms/:id/boards — boards are readable by any authenticated user.
pub async fn list_boards(
    headers: HeaderMap,
    Path(room_id): Path<String>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<Vec<Board>>, ApiErr> {
    if db::verify_token(&s.pool, token_from(&headers))
        .await
        .map_err(|_| db_err())?
        .is_none()
    {
        return Err(unauth());
    }

    let rows = sqlx::query(
        "SELECT id, room_id, name FROM boards WHERE room_id = ? ORDER BY created_at ASC",
    )
    .bind(&room_id)
    .fetch_all(&s.pool)
    .await
    .map_err(|_| db_err())?;

    Ok(Json(
        rows.iter()
            .map(|r| Board {
                id:      r.get("id"),
                room_id: r.get("room_id"),
                name:    r.get("name"),
            })
            .collect(),
    ))
}
