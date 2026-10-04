// layout.rs — the order of categories (rooms) and channels (boards), and
// moving channels between categories.
//
// Each room and board has a `position`; lists are ordered by
// (position, created_at), so databases from before positions existed
// (everything at 0) keep their old creation order until someone reorders.
// New rooms and boards are created at the end (see next_room_position /
// next_board_position).
//
// Only the crowned owner can rearrange things for now — from the app, with
// their session — or the admin panel with the owner key.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use sqlx::{Row, SqlitePool};
use std::sync::Arc;

use crate::{db, state::AppState};

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn err(code: StatusCode, msg: &str) -> ApiErr {
    (code, Json(serde_json::json!({ "error": msg })))
}
fn dberr() -> ApiErr { err(StatusCode::INTERNAL_SERVER_ERROR, "DB error") }
fn bad(msg: &str) -> ApiErr { err(StatusCode::BAD_REQUEST, msg) }

/// Adds the position columns (fails harmlessly when they already exist).
pub async fn init(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let _ = sqlx::query("ALTER TABLE rooms  ADD COLUMN position INTEGER NOT NULL DEFAULT 0").execute(pool).await;
    let _ = sqlx::query("ALTER TABLE boards ADD COLUMN position INTEGER NOT NULL DEFAULT 0").execute(pool).await;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_boards_room_pos ON boards (room_id, position)")
        .execute(pool).await?;
    Ok(())
}

/// Position for a new room: after every existing one.
pub async fn next_room_position(pool: &SqlitePool) -> i64 {
    sqlx::query("SELECT COALESCE(MAX(position), -1) + 1 AS p FROM rooms")
        .fetch_one(pool).await.map(|r| r.get::<i64, _>("p")).unwrap_or(0)
}

/// Position for a new board in `room_id`: after every existing one there.
pub async fn next_board_position(pool: &SqlitePool, room_id: &str) -> i64 {
    sqlx::query("SELECT COALESCE(MAX(position), -1) + 1 AS p FROM boards WHERE room_id = ?")
        .bind(room_id)
        .fetch_one(pool).await.map(|r| r.get::<i64, _>("p")).unwrap_or(0)
}

/// The crowned owner (by session) or the admin panel (by owner key).
async fn require_owner(h: &HeaderMap, s: &AppState) -> Result<(), ApiErr> {
    if crate::admin::check_owner(h, s).is_ok() {
        return Ok(());
    }
    let token = h.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let user = db::verify_token(&s.pool, token).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Unauthorized"))?;
    match crate::admin::owner_username(&s.pool).await {
        Some(owner) if owner == user => Ok(()),
        _ => Err(err(StatusCode::FORBIDDEN, "Only the server owner can rearrange channels")),
    }
}

fn broadcast(s: &AppState) {
    let _ = s.tx.send(serde_json::json!({ "type": "rooms_updated" }).to_string());
}

#[derive(Deserialize)]
pub struct RoomOrderReq {
    /// Room ids, top first. Rooms left out keep their current order, after
    /// the listed ones, so a client that's missing one can't lose it.
    pub ids: Vec<String>,
}

/// PUT /api/layout/rooms {ids}
pub async fn order_rooms(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    Json(b): Json<RoomOrderReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    require_owner(&headers, &s).await?;

    let mut tx = s.pool.begin().await.map_err(|_| dberr())?;
    let current: Vec<String> = sqlx::query("SELECT id FROM rooms ORDER BY position, created_at")
        .fetch_all(&mut *tx).await.map_err(|_| dberr())?
        .iter().map(|r| r.get("id")).collect();

    let mut order: Vec<&String> = Vec::with_capacity(current.len());
    for id in &b.ids {
        if current.contains(id) && !order.contains(&id) { order.push(id); }
    }
    for id in &current {
        if !order.contains(&id) { order.push(id); }
    }
    for (i, id) in order.iter().enumerate() {
        sqlx::query("UPDATE rooms SET position = ? WHERE id = ?")
            .bind(i as i64).bind(id.as_str())
            .execute(&mut *tx).await.map_err(|_| dberr())?;
    }
    tx.commit().await.map_err(|_| dberr())?;

    broadcast(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct MoveBoardReq {
    /// The category to put the channel in (its current one to just reorder).
    pub room_id: String,
    /// Put it just above this channel; None/missing = at the bottom.
    pub before: Option<String>,
}

/// PUT /api/layout/boards/:id {room_id, before}
///
/// A channel's text/voice type comes from its category, so a channel can
/// only move between categories of the same type.
pub async fn move_board(
    headers: HeaderMap,
    Path(board_id): Path<String>,
    State(s): State<Arc<AppState>>,
    Json(b): Json<MoveBoardReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    require_owner(&headers, &s).await?;
    if b.before.as_deref() == Some(board_id.as_str()) {
        return Ok(Json(serde_json::json!({ "ok": true })));
    }

    let mut tx = s.pool.begin().await.map_err(|_| dberr())?;

    let from_type: String = sqlx::query(
        "SELECT r.room_type AS t FROM boards b JOIN rooms r ON r.id = b.room_id WHERE b.id = ?",
    )
    .bind(&board_id).fetch_optional(&mut *tx).await.map_err(|_| dberr())?
    .map(|r| r.get::<String, _>("t"))
    .ok_or_else(|| err(StatusCode::NOT_FOUND, "Channel not found"))?;

    let to_type: String = sqlx::query("SELECT room_type AS t FROM rooms WHERE id = ?")
        .bind(&b.room_id).fetch_optional(&mut *tx).await.map_err(|_| dberr())?
        .map(|r| r.get::<String, _>("t"))
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Category not found"))?;

    if from_type != to_type {
        return Err(bad(if from_type == "voice" {
            "Voice channels can only go in voice categories"
        } else {
            "Text channels can only go in text categories"
        }));
    }

    let mut ids: Vec<String> = sqlx::query(
        "SELECT id FROM boards WHERE room_id = ? AND id != ? ORDER BY position, created_at",
    )
    .bind(&b.room_id).bind(&board_id)
    .fetch_all(&mut *tx).await.map_err(|_| dberr())?
    .iter().map(|r| r.get("id")).collect();

    let at = b.before.as_ref()
        .and_then(|before| ids.iter().position(|id| id == before))
        .unwrap_or(ids.len());
    ids.insert(at, board_id.clone());

    sqlx::query("UPDATE boards SET room_id = ? WHERE id = ?")
        .bind(&b.room_id).bind(&board_id)
        .execute(&mut *tx).await.map_err(|_| dberr())?;
    for (i, id) in ids.iter().enumerate() {
        sqlx::query("UPDATE boards SET position = ? WHERE id = ?")
            .bind(i as i64).bind(id)
            .execute(&mut *tx).await.map_err(|_| dberr())?;
    }
    tx.commit().await.map_err(|_| dberr())?;

    broadcast(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}
