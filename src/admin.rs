use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Html,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::sync::Arc;

use crate::{db, state::AppState};

// Embed the admin HTML at compile time — no extra files needed at runtime.
const ADMIN_HTML: &str = include_str!("admin.html");

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct AdminInfo {
    pub owner_key:   String,
    pub server_key:  String,
    pub server_name: String,
}

#[derive(Deserialize)]
pub struct UpdateSettingsReq {
    pub server_name: Option<String>,
    pub server_key:  Option<String>,
}

#[derive(Deserialize)]
pub struct CreateRoomReq {
    pub name:       String,
    pub is_private: Option<bool>,
}

#[derive(Deserialize)]
pub struct CreateBoardReq {
    pub room_id: String,
    pub name:    String,
}

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn unauth() -> ApiErr {
    (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "Invalid owner key" })))
}

fn bad(msg: &str) -> ApiErr {
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": msg })))
}

fn dberr() -> ApiErr {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "DB error" })))
}

fn owner_key_from(h: &HeaderMap) -> &str {
    h.get("X-Owner-Key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

fn check_owner(h: &HeaderMap, state: &AppState) -> Result<(), ApiErr> {
    if owner_key_from(h) == state.owner_key {
        Ok(())
    } else {
        Err(unauth())
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /admin — serves the embedded admin HTML page.
pub async fn admin_ui() -> Html<&'static str> {
    Html(ADMIN_HTML)
}

/// GET /api/admin/info — returns owner key, join key, and server name.
/// This endpoint has no auth — the admin page is assumed to be LAN-only.
/// If you expose the server publicly, put this behind a reverse proxy with IP restriction.
pub async fn get_admin_info(State(s): State<Arc<AppState>>) -> Json<AdminInfo> {
    let server_key = db::get_config(&s.pool, "server_key")
        .await
        .unwrap_or(None)
        .unwrap_or_default();
    let server_name = db::get_config(&s.pool, "server_name")
        .await
        .unwrap_or(None)
        .unwrap_or_else(|| "Chriscord Server".to_string());

    Json(AdminInfo {
        owner_key: s.owner_key.clone(),
        server_key,
        server_name,
    })
}

/// POST /api/admin/settings — update server name and/or join key.
pub async fn update_settings(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    Json(body): Json<UpdateSettingsReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;

    if let Some(name) = body.server_name {
        let name = name.trim().to_string();
        if !name.is_empty() {
            db::set_config(&s.pool, "server_name", &name).await.map_err(|_| dberr())?;
        }
    }
    if let Some(key) = body.server_key {
        db::set_config(&s.pool, "server_key", &key.trim().to_string())
            .await
            .map_err(|_| dberr())?;
    }

    Ok(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/admin/rooms — create a new room.
pub async fn create_room(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    Json(body): Json<CreateRoomReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;

    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(bad("Room name is required"));
    }

    let id         = uuid::Uuid::new_v4().to_string();
    let is_private = body.is_private.unwrap_or(false) as i64;
    let now        = chrono::Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO rooms (id, name, is_private, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&name)
    .bind(is_private)
    .bind(&now)
    .execute(&s.pool)
    .await
    .map_err(|_| dberr())?;

    Ok(Json(serde_json::json!({ "id": id, "name": name })))
}

/// DELETE /api/admin/rooms/:id — delete a room and all its boards/messages.
pub async fn delete_room(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;

    // Get all board IDs for this room so we can delete their messages
    let board_ids: Vec<String> = sqlx::query("SELECT id FROM boards WHERE room_id = ?")
        .bind(&id)
        .fetch_all(&s.pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.get("id"))
        .collect();

    for bid in &board_ids {
        sqlx::query("DELETE FROM messages WHERE board_id = ?").bind(bid).execute(&s.pool).await.ok();
    }
    sqlx::query("DELETE FROM boards WHERE room_id = ?").bind(&id).execute(&s.pool).await.ok();
    sqlx::query("DELETE FROM rooms WHERE id = ?").bind(&id).execute(&s.pool).await.ok();

    Ok(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/admin/boards — create a board inside a room.
pub async fn create_board(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    Json(body): Json<CreateBoardReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;

    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(bad("Board name is required"));
    }

    // Verify the room exists
    let exists = sqlx::query("SELECT 1 FROM rooms WHERE id = ?")
        .bind(&body.room_id)
        .fetch_optional(&s.pool)
        .await
        .map_err(|_| dberr())?
        .is_some();

    if !exists {
        return Err(bad("Room not found"));
    }

    let id  = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO boards (id, room_id, name, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&body.room_id)
    .bind(&name)
    .bind(&now)
    .execute(&s.pool)
    .await
    .map_err(|_| dberr())?;

    Ok(Json(serde_json::json!({ "id": id, "name": name })))
}

/// DELETE /api/admin/boards/:id — delete a board and its messages.
pub async fn delete_board(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;

    sqlx::query("DELETE FROM messages WHERE board_id = ?").bind(&id).execute(&s.pool).await.ok();
    sqlx::query("DELETE FROM boards WHERE id = ?").bind(&id).execute(&s.pool).await.ok();

    Ok(Json(serde_json::json!({ "ok": true })))
}
