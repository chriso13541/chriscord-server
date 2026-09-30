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
    pub server_key:        String,
    pub server_name:       String,
    pub description:       String,
    pub max_upload_mb:     u64,
    pub owner_username:    Option<String>,
    pub banner_updated_at: i64,
    pub icon_updated_at:   i64,
}

#[derive(Deserialize)]
pub struct UpdateSettingsReq {
    pub server_name:   Option<String>,
    pub server_key:    Option<String>,
    pub description:   Option<String>,
    pub max_upload_mb: Option<u64>,
}

/// Upload size limit bounds for the setting (MB). 0 means unlimited.
pub const MIN_UPLOAD_MB: u64 = 0;
pub const MAX_UPLOAD_MB: u64 = 4096;
pub const DEFAULT_UPLOAD_MB: u64 = 500;
pub const MAX_DESCRIPTION_CHARS: usize = 300;

/// The configured per-file upload limit in MB, as set in the admin panel
/// (0 = unlimited).
pub async fn upload_limit_mb(pool: &sqlx::SqlitePool) -> u64 {
    db::get_config(pool, "max_upload_mb").await.ok().flatten()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_UPLOAD_MB)
        .clamp(MIN_UPLOAD_MB, MAX_UPLOAD_MB)
}

/// The per-file upload limit in bytes — u64::MAX when unlimited, so the
/// size check in files.rs never trips.
pub async fn upload_limit_bytes(pool: &sqlx::SqlitePool) -> u64 {
    match upload_limit_mb(pool).await {
        0 => u64::MAX,
        mb => mb * 1024 * 1024,
    }
}

/// The member shown with the crown, if one has been chosen.
pub async fn owner_username(pool: &sqlx::SqlitePool) -> Option<String> {
    db::get_config(pool, "owner_username").await.ok().flatten().filter(|u| !u.is_empty())
}

pub async fn icon_updated_at(pool: &sqlx::SqlitePool) -> i64 {
    db::get_config(pool, "icon_updated_at").await.ok().flatten()
        .and_then(|v| v.parse().ok()).unwrap_or(0)
}

pub async fn banner_updated_at(pool: &sqlx::SqlitePool) -> i64 {
    db::get_config(pool, "banner_updated_at").await.ok().flatten()
        .and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// Tells every connected client to refetch the server's name/banner/etc.
fn broadcast_server_updated(s: &AppState) {
    let _ = s.tx.send(serde_json::json!({ "type": "server_updated" }).to_string());
}

#[derive(Deserialize)]
pub struct CreateRoomReq {
    pub name:       String,
    pub is_private: Option<bool>,
    pub room_type:  Option<String>, // "text" or "voice"; defaults to "text"
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

/// Every /api/admin route requires the owner key (printed in the server's
/// console at startup). Compared in constant time so response timing can't
/// be used to guess it a character at a time.
fn check_owner(h: &HeaderMap, state: &AppState) -> Result<(), ApiErr> {
    let given = owner_key_from(h).as_bytes();
    let want = state.owner_key.as_bytes();
    let mut diff = (given.len() ^ want.len()) as u8;
    for (i, b) in want.iter().enumerate() {
        diff |= b ^ given.get(i).copied().unwrap_or(0);
    }
    if !want.is_empty() && diff == 0 { Ok(()) } else { Err(unauth()) }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /admin — serves the embedded admin HTML page.
pub async fn admin_ui() -> Html<&'static str> {
    Html(ADMIN_HTML)
}

/// GET /api/admin/info — the server's settings, for the admin panel.
/// Requires the owner key. (It used to require nothing and returned the
/// owner key itself, which let anyone who could reach port 7070 take over
/// the server — the admin page now asks for the key instead.)
pub async fn get_admin_info(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
) -> Result<Json<AdminInfo>, ApiErr> {
    check_owner(&headers, &s)?;
    let server_key = db::get_config(&s.pool, "server_key").await.unwrap_or(None).unwrap_or_default();
    let server_name = db::get_config(&s.pool, "server_name").await.unwrap_or(None)
        .unwrap_or_else(|| "Chriscord Server".to_string());
    let description = db::get_config(&s.pool, "server_description").await.unwrap_or(None).unwrap_or_default();
    Ok(Json(AdminInfo {
        server_key,
        server_name,
        description,
        max_upload_mb: upload_limit_mb(&s.pool).await,
        owner_username: owner_username(&s.pool).await,
        banner_updated_at: banner_updated_at(&s.pool).await,
        icon_updated_at: icon_updated_at(&s.pool).await,
    }))
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
    if let Some(desc) = body.description {
        let desc = desc.trim();
        if desc.chars().count() > MAX_DESCRIPTION_CHARS {
            return Err(bad(&format!("Description is limited to {MAX_DESCRIPTION_CHARS} characters")));
        }
        db::set_config(&s.pool, "server_description", desc).await.map_err(|_| dberr())?;
    }
    if let Some(mb) = body.max_upload_mb {
        if !(MIN_UPLOAD_MB..=MAX_UPLOAD_MB).contains(&mb) {
            return Err(bad(&format!("Upload limit must be between 1 and {MAX_UPLOAD_MB} MB, or 0 for unlimited")));
        }
        db::set_config(&s.pool, "max_upload_mb", &mb.to_string()).await.map_err(|_| dberr())?;
    }
    broadcast_server_updated(&s);

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
    let room_type = match body.room_type.as_deref() {
        None | Some("text")  => "text",
        Some("voice")        => "voice",
        Some(_)              => return Err(bad("room_type must be 'text' or 'voice'")),
    };

    let id         = uuid::Uuid::now_v7().to_string();
    let is_private = body.is_private.unwrap_or(false) as i64;
    let now        = chrono::Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO rooms (id, name, is_private, room_type, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&name)
    .bind(is_private)
    .bind(room_type)
    .bind(&now)
    .execute(&s.pool)
    .await
    .map_err(|_| dberr())?;

    let _ = s.tx.send(serde_json::json!({ "type": "rooms_updated" }).to_string());

    Ok(Json(serde_json::json!({ "id": id, "name": name, "room_type": room_type })))
}

/// GET /api/admin/rooms — list all rooms, authenticated by owner key.
/// Used by the admin UI (which has no session token).
pub async fn list_rooms_admin(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;

    let rows = sqlx::query("SELECT id, name, is_private, room_type FROM rooms ORDER BY created_at ASC")
        .fetch_all(&s.pool)
        .await
        .map_err(|_| dberr())?;

    let rooms: Vec<_> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "id":         r.get::<String, _>("id"),
                "name":       r.get::<String, _>("name"),
                "is_private": r.get::<i64, _>("is_private") != 0,
                "room_type":  r.get::<String, _>("room_type"),
            })
        })
        .collect();

    Ok(Json(serde_json::json!(rooms)))
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

    let _ = s.tx.send(serde_json::json!({ "type": "rooms_updated" }).to_string());

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

    let id  = uuid::Uuid::now_v7().to_string();
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

    let _ = s.tx.send(serde_json::json!({ "type": "rooms_updated" }).to_string());

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

    let _ = s.tx.send(serde_json::json!({ "type": "rooms_updated" }).to_string());

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── Server banner ─────────────────────────────────────────────────────────────

const BANNER_MAX_BYTES: usize = 8 << 20;

fn server_assets_dir() -> std::path::PathBuf {
    let dir = std::path::PathBuf::from("./server_assets");
    let _ = std::fs::create_dir_all(&dir);
    dir
}
fn banner_path() -> std::path::PathBuf { server_assets_dir().join("banner") }

/// POST /api/admin/banner — raw PNG/JPEG bytes as the request body.
pub async fn upload_banner(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    if body.len() > BANNER_MAX_BYTES {
        return Err(bad("Banner must be 8 MB or smaller"));
    }
    if !matches!(crate::pfp::image_mime(&body), Some("image/png") | Some("image/jpeg")) {
        return Err(bad("Banner must be a PNG or JPEG image"));
    }
    tokio::fs::write(banner_path(), &body).await.map_err(|_| bad("Could not save the banner"))?;
    let now = chrono::Utc::now().timestamp_millis();
    db::set_config(&s.pool, "banner_updated_at", &now.to_string()).await.map_err(|_| dberr())?;
    broadcast_server_updated(&s);
    Ok(Json(serde_json::json!({ "ok": true, "banner_updated_at": now })))
}

/// DELETE /api/admin/banner — back to no banner.
pub async fn delete_banner(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    let _ = tokio::fs::remove_file(banner_path()).await;
    db::set_config(&s.pool, "banner_updated_at", "0").await.map_err(|_| dberr())?;
    broadcast_server_updated(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/server/banner — the banner image, for members (session
/// token) and the admin panel (owner key). 404 if there isn't one.
pub async fn serve_banner(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let is_admin = check_owner(&headers, &s).is_ok();
    let token = headers.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !is_admin && db::verify_token(&s.pool, token).await.ok().flatten().is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match tokio::fs::read(banner_path()).await {
        Ok(data) => (
            [(axum::http::header::CONTENT_TYPE, crate::pfp::image_mime(&data).unwrap_or("application/octet-stream"))],
            data,
        ).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

// ── Server icon ───────────────────────────────────────────────────────────────
//
// The picture shown for this server on the client's server list (instead
// of a coloured letter). The admin panel crops it square and scales it to
// 256×256 before uploading, so it's small; it's public (like /api/info),
// because the server list shows it before you've joined.

const ICON_MAX_BYTES: usize = 2 << 20;
fn icon_path() -> std::path::PathBuf { server_assets_dir().join("icon") }

/// POST /api/admin/icon — raw PNG/JPEG bytes as the request body.
pub async fn upload_icon(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    if body.len() > ICON_MAX_BYTES {
        return Err(bad("Server icon must be 2 MB or smaller"));
    }
    if !matches!(crate::pfp::image_mime(&body), Some("image/png") | Some("image/jpeg")) {
        return Err(bad("Server icon must be a PNG or JPEG image"));
    }
    tokio::fs::write(icon_path(), &body).await.map_err(|_| bad("Could not save the icon"))?;
    let now = chrono::Utc::now().timestamp_millis();
    db::set_config(&s.pool, "icon_updated_at", &now.to_string()).await.map_err(|_| dberr())?;
    broadcast_server_updated(&s);
    Ok(Json(serde_json::json!({ "ok": true, "icon_updated_at": now })))
}

/// DELETE /api/admin/icon — back to the coloured letter.
pub async fn delete_icon(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    let _ = tokio::fs::remove_file(icon_path()).await;
    db::set_config(&s.pool, "icon_updated_at", "0").await.map_err(|_| dberr())?;
    broadcast_server_updated(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/server/icon — public. 404 if there isn't one.
pub async fn serve_icon() -> axum::response::Response {
    use axum::response::IntoResponse;
    match tokio::fs::read(icon_path()).await {
        Ok(data) => (
            [(axum::http::header::CONTENT_TYPE, crate::pfp::image_mime(&data).unwrap_or("application/octet-stream"))],
            data,
        ).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

// ── Server theme ──────────────────────────────────────────────────────────────
//
// The same controls as the app's Settings → Appearance, set for the whole
// server: a theme color, an accent color, and an optional background image
// with blur and darken. Members see it while connected unless they've
// ticked "use my own theme" in their app. Colors live in config as JSON;
// the image is a file in server_assets like the banner and icon.

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct ServerTheme {
    /// "#rrggbb", or None for the default gray.
    pub base:   Option<String>,
    pub accent: Option<String>,
    /// 0–100 (%)
    pub blur:   u8,
    /// 0–90 (%)
    pub dim:    u8,
    /// Changes whenever the background image does (0 = none).
    #[serde(default)]
    pub bg_updated_at: i64,
}

impl ServerTheme {
    pub fn is_default(&self) -> bool {
        self.base.is_none() && self.accent.is_none() && self.bg_updated_at == 0
    }
}

fn valid_hex(c: &str) -> bool {
    c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|ch| ch.is_ascii_hexdigit())
}

pub async fn server_theme(pool: &sqlx::SqlitePool) -> ServerTheme {
    db::get_config(pool, "theme").await.ok().flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or(ServerTheme { blur: 30, dim: 40, ..Default::default() })
}

async fn save_theme(pool: &sqlx::SqlitePool, t: &ServerTheme) -> Result<(), ApiErr> {
    let j = serde_json::to_string(t).map_err(|_| dberr())?;
    db::set_config(pool, "theme", &j).await.map_err(|_| dberr())
}

/// GET /api/admin/theme
pub async fn get_theme(headers: HeaderMap, State(s): State<Arc<AppState>>) -> Result<Json<ServerTheme>, ApiErr> {
    check_owner(&headers, &s)?;
    Ok(Json(server_theme(&s.pool).await))
}

#[derive(Deserialize)]
pub struct SetThemeReq {
    pub base:   Option<String>,
    pub accent: Option<String>,
    pub blur:   u8,
    pub dim:    u8,
}

/// POST /api/admin/theme {base, accent, blur, dim} — colors (null = default);
/// the background image is set separately (it's a file upload).
pub async fn set_theme(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    Json(body): Json<SetThemeReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    let norm = |c: Option<String>| -> Result<Option<String>, ApiErr> {
        match c.map(|c| c.trim().to_lowercase()).filter(|c| !c.is_empty()) {
            Some(c) if valid_hex(&c) => Ok(Some(c)),
            Some(_) => Err(bad("Colors must look like #1a2b3c")),
            None => Ok(None),
        }
    };
    let mut t = server_theme(&s.pool).await;
    t.base = norm(body.base)?;
    t.accent = norm(body.accent)?;
    t.blur = body.blur.min(100);
    t.dim = body.dim.min(90);
    save_theme(&s.pool, &t).await?;
    broadcast_server_updated(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}

const THEME_BG_MAX_BYTES: usize = 8 << 20;
fn theme_bg_path() -> std::path::PathBuf { server_assets_dir().join("theme_background") }

/// POST /api/admin/theme/background — raw PNG/JPEG bytes (the admin panel
/// scales it to at most 1920×1080 first).
pub async fn upload_theme_bg(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    if body.len() > THEME_BG_MAX_BYTES {
        return Err(bad("Background must be 8 MB or smaller"));
    }
    if !matches!(crate::pfp::image_mime(&body), Some("image/png") | Some("image/jpeg")) {
        return Err(bad("Background must be a PNG or JPEG image"));
    }
    tokio::fs::write(theme_bg_path(), &body).await.map_err(|_| bad("Could not save the background"))?;
    let mut t = server_theme(&s.pool).await;
    t.bg_updated_at = chrono::Utc::now().timestamp_millis();
    save_theme(&s.pool, &t).await?;
    broadcast_server_updated(&s);
    Ok(Json(serde_json::json!({ "ok": true, "bg_updated_at": t.bg_updated_at })))
}

/// DELETE /api/admin/theme/background
pub async fn delete_theme_bg(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    let _ = tokio::fs::remove_file(theme_bg_path()).await;
    let mut t = server_theme(&s.pool).await;
    t.bg_updated_at = 0;
    save_theme(&s.pool, &t).await?;
    broadcast_server_updated(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/server/theme/background — members (session token) and the
/// admin panel (owner key). 404 if none.
pub async fn serve_theme_bg(headers: HeaderMap, State(s): State<Arc<AppState>>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let is_admin = check_owner(&headers, &s).is_ok();
    let token = headers.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !is_admin && db::verify_token(&s.pool, token).await.ok().flatten().is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match tokio::fs::read(theme_bg_path()).await {
        Ok(data) => (
            [(axum::http::header::CONTENT_TYPE, crate::pfp::image_mime(&data).unwrap_or("application/octet-stream"))],
            data,
        ).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

// ── Members, owner crown, kicks and bans ─────────────────────────────────────

/// GET /api/admin/members — everyone registered on this server.
pub async fn list_members(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    let rows = sqlx::query("SELECT public_key, username, created_at FROM users ORDER BY username COLLATE NOCASE")
        .fetch_all(&s.pool).await.map_err(|_| dberr())?;
    let owner = owner_username(&s.pool).await;
    let (online, presence) = {
        let o = s.online.lock().unwrap();
        let p = s.presence.lock().unwrap();
        (o.keys().cloned().collect::<std::collections::HashSet<_>>(), p.clone())
    };
    let members: Vec<_> = rows.iter().map(|r| {
        let username: String = r.get("username");
        let pk: String = r.get("public_key");
        let status = if online.contains(&username) {
            presence.get(&username).cloned().unwrap_or_else(|| "online".into())
        } else { "offline".into() };
        serde_json::json!({
            "username": username,
            "fingerprint": pk.get(..16).unwrap_or(&pk),
            "joined_at": r.get::<String, _>("created_at"),
            "status": status,
            "is_owner": owner.as_deref() == Some(username.as_str()),
        })
    }).collect();
    Ok(Json(serde_json::json!(members)))
}

#[derive(Deserialize)]
pub struct SetOwnerReq { pub username: Option<String> }

/// POST /api/admin/owner {username} — who gets the crown (null clears it).
pub async fn set_owner(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    Json(body): Json<SetOwnerReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    let name = body.username.map(|u| u.trim().to_string()).filter(|u| !u.is_empty());
    if let Some(u) = &name {
        let exists = sqlx::query("SELECT 1 FROM users WHERE username = ?").bind(u)
            .fetch_optional(&s.pool).await.map_err(|_| dberr())?.is_some();
        if !exists { return Err(bad("No member with that username")); }
    }
    db::set_config(&s.pool, "owner_username", name.as_deref().unwrap_or("")).await.map_err(|_| dberr())?;
    crate::ws::broadcast_users(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct KickReq {
    #[serde(default)]
    pub ban:    bool,
    #[serde(default)]
    pub reason: String,
}

/// POST /api/admin/members/:username/kick {ban, reason}
///
/// Kick: signs them out everywhere (sessions deleted, open connections
/// closed with a "kicked" notice) and removes their membership, so they
/// drop off the member list. Like a Discord kick, they can come back by
/// joining again (with the join key, if one is set).
/// Ban: the same, plus their account key is refused on every future join.
pub async fn kick_member(
    headers: HeaderMap,
    Path(username): Path<String>,
    State(s): State<Arc<AppState>>,
    Json(body): Json<KickReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    let row = sqlx::query("SELECT public_key FROM users WHERE username = ?").bind(&username)
        .fetch_optional(&s.pool).await.map_err(|_| dberr())?
        .ok_or_else(|| bad("No member with that username"))?;
    let public_key: String = row.get("public_key");
    let reason: String = body.reason.trim().chars().take(200).collect();

    if body.ban {
        sqlx::query("INSERT OR REPLACE INTO bans (public_key, username, reason, banned_at) VALUES (?, ?, ?, ?)")
            .bind(&public_key).bind(&username).bind(&reason).bind(chrono::Utc::now().to_rfc3339())
            .execute(&s.pool).await.map_err(|_| dberr())?;
    }
    sqlx::query("DELETE FROM sessions WHERE username = ?").bind(&username).execute(&s.pool).await.map_err(|_| dberr())?;
    sqlx::query("DELETE FROM users WHERE username = ?").bind(&username).execute(&s.pool).await.map_err(|_| dberr())?;
    if owner_username(&s.pool).await.as_deref() == Some(username.as_str()) {
        let _ = db::set_config(&s.pool, "owner_username", "").await;
    }
    // Their open connections see this, pass it to the client, and close.
    crate::ws::send_to_user(&s, &username, serde_json::json!({
        "type": "kicked", "banned": body.ban, "reason": reason,
    }));
    crate::ws::broadcast_users(&s);
    tracing::info!("admin: {} {username}", if body.ban { "banned" } else { "kicked" });
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/admin/bans
pub async fn list_bans(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    let rows = sqlx::query("SELECT public_key, username, reason, banned_at FROM bans ORDER BY banned_at DESC")
        .fetch_all(&s.pool).await.map_err(|_| dberr())?;
    Ok(Json(serde_json::json!(rows.iter().map(|r| {
        let pk: String = r.get("public_key");
        serde_json::json!({
            "public_key": pk, "fingerprint": pk.get(..16).unwrap_or(&pk),
            "username": r.get::<String, _>("username"), "reason": r.get::<String, _>("reason"),
            "banned_at": r.get::<String, _>("banned_at"),
        })
    }).collect::<Vec<_>>())))
}

/// DELETE /api/admin/bans/:public_key — lift a ban (they can join again).
pub async fn unban(
    headers: HeaderMap,
    Path(public_key): Path<String>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    sqlx::query("DELETE FROM bans WHERE public_key = ?").bind(&public_key).execute(&s.pool).await.map_err(|_| dberr())?;
    Ok(Json(serde_json::json!({ "ok": true })))
}
