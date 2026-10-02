// access.rs — who can see which categories and channels.
//
// A category (room) or a channel (board) is either for everyone, or
// private: then only members with at least one of its chosen roles can see
// it — see its messages, post, find it in search, or join it if it's a
// voice channel. A channel inside a private category needs both: access to
// the category and to the channel. The crowned owner and anyone with the
// Administrator permission see everything. Hidden channels simply don't
// exist as far as the member's app is concerned: they're left out of the
// channel lists, and none of their messages, typing or voice activity is
// sent to that member.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use sqlx::{Row, SqlitePool};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::state::AppState;

type ApiErr = (StatusCode, Json<serde_json::Value>);

/// Adds the per-channel "private" flag and the role list table.
pub async fn init(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    // Older databases' boards have no is_private column yet (fails harmlessly when it exists).
    let _ = sqlx::query("ALTER TABLE boards ADD COLUMN is_private INTEGER NOT NULL DEFAULT 0").execute(pool).await;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS channel_roles (
            target_id TEXT NOT NULL,   -- a room id or a board id
            role_id   TEXT NOT NULL,
            PRIMARY KEY (target_id, role_id)
        )",
    ).execute(pool).await?;
    Ok(())
}

/// What a member can see: None = everything (owner / Administrator).
#[derive(Clone, Default)]
pub struct Visible {
    pub rooms: HashSet<String>,
    pub boards: HashSet<String>,
}

pub async fn visible_for(pool: &SqlitePool, username: &str) -> Option<Visible> {
    if crate::roles::has(pool, username, crate::roles::ADMINISTRATOR).await {
        return None; // (true for the owner too — they have every permission)
    }
    // The member's roles.
    let my_roles: HashSet<String> = sqlx::query(
        "SELECT m.role_id FROM member_roles m JOIN users u ON u.public_key = m.public_key WHERE u.username = ?",
    ).bind(username).fetch_all(pool).await.unwrap_or_default()
        .iter().map(|r| r.get::<String, _>("role_id")).collect();
    // Which roles each private room/board allows.
    let mut allowed: HashMap<String, HashSet<String>> = HashMap::new();
    for r in sqlx::query("SELECT target_id, role_id FROM channel_roles").fetch_all(pool).await.unwrap_or_default() {
        allowed.entry(r.get("target_id")).or_default().insert(r.get("role_id"));
    }
    let may = |id: &str, private: bool| !private || allowed.get(id).map_or(false, |set| !set.is_disjoint(&my_roles));

    let mut v = Visible::default();
    for r in sqlx::query("SELECT id, is_private FROM rooms").fetch_all(pool).await.unwrap_or_default() {
        let id: String = r.get("id");
        if may(&id, r.get::<i64, _>("is_private") != 0) { v.rooms.insert(id); }
    }
    for b in sqlx::query("SELECT id, room_id, is_private FROM boards").fetch_all(pool).await.unwrap_or_default() {
        let id: String = b.get("id");
        let room: String = b.get("room_id");
        if v.rooms.contains(&room) && may(&id, b.get::<i64, _>("is_private") != 0) { v.boards.insert(id); }
    }
    Some(v)
}

pub async fn can_view_board(pool: &SqlitePool, username: &str, board_id: &str) -> bool {
    visible_for(pool, username).await.map_or(true, |v| v.boards.contains(board_id))
}

/// For HTTP handlers: a 404 (not a 403) for channels you can't see, so
/// their existence isn't given away.
pub async fn require_board(pool: &SqlitePool, username: &str, board_id: &str) -> Result<(), ApiErr> {
    if can_view_board(pool, username, board_id).await { Ok(()) } else {
        Err((StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "Channel not found" }))))
    }
}

// ── Admin panel ──────────────────────────────────────────────────────────────

/// GET /api/admin/access — every room's and board's setting.
pub async fn admin_get(headers: HeaderMap, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    crate::admin::check_owner(&headers, &s)?;
    let mut roles: HashMap<String, Vec<String>> = HashMap::new();
    for r in sqlx::query("SELECT target_id, role_id FROM channel_roles").fetch_all(&s.pool).await.unwrap_or_default() {
        roles.entry(r.get("target_id")).or_default().push(r.get("role_id"));
    }
    let entry = |id: String, private: bool, roles: &HashMap<String, Vec<String>>| {
        let ids = roles.get(&id).cloned().unwrap_or_default();
        (id, serde_json::json!({ "private": private, "role_ids": ids }))
    };
    let rooms: serde_json::Map<_, _> = sqlx::query("SELECT id, is_private FROM rooms").fetch_all(&s.pool).await.unwrap_or_default()
        .iter().map(|r| entry(r.get("id"), r.get::<i64, _>("is_private") != 0, &roles)).collect();
    let boards: serde_json::Map<_, _> = sqlx::query("SELECT id, is_private FROM boards").fetch_all(&s.pool).await.unwrap_or_default()
        .iter().map(|r| entry(r.get("id"), r.get::<i64, _>("is_private") != 0, &roles)).collect();
    Ok(Json(serde_json::json!({ "rooms": rooms, "boards": boards })))
}

#[derive(Deserialize)]
pub struct AccessReq {
    pub private: bool,
    #[serde(default)]
    pub role_ids: Vec<String>,
}

async fn set_access(s: &AppState, table: &str, id: &str, b: &AccessReq) -> Result<(), ApiErr> {
    let dberr = || (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "DB error" })));
    let sql = format!("UPDATE {table} SET is_private = ? WHERE id = ?"); // table is one of two fixed names
    let n = sqlx::query(&sql).bind(b.private as i64).bind(id).execute(&s.pool).await.map_err(|_| dberr())?.rows_affected();
    if n == 0 { return Err((StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "Not found" })))); }
    sqlx::query("DELETE FROM channel_roles WHERE target_id = ?").bind(id).execute(&s.pool).await.map_err(|_| dberr())?;
    for rid in &b.role_ids {
        sqlx::query("INSERT OR IGNORE INTO channel_roles (target_id, role_id) VALUES (?, ?)")
            .bind(id).bind(rid).execute(&s.pool).await.map_err(|_| dberr())?;
    }
    // Everyone's app re-reads its channel list (and connections re-work
    // out what they may forward).
    let _ = s.tx.send(serde_json::json!({ "type": "rooms_updated" }).to_string());
    Ok(())
}

/// PUT /api/admin/rooms/:id/access {private, role_ids}
pub async fn admin_set_room(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>, Json(b): Json<AccessReq>) -> Result<Json<serde_json::Value>, ApiErr> {
    crate::admin::check_owner(&headers, &s)?;
    set_access(&s, "rooms", &id, &b).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// PUT /api/admin/boards/:id/access {private, role_ids}
pub async fn admin_set_board(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>, Json(b): Json<AccessReq>) -> Result<Json<serde_json::Value>, ApiErr> {
    crate::admin::check_owner(&headers, &s)?;
    set_access(&s, "boards", &id, &b).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}
