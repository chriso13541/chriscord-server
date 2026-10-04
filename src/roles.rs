// roles.rs — server roles and permissions, in the spirit of Discord's.
//
// Every member has the default role, "@everyone" (it can't be deleted or
// renamed, only given permissions), plus any roles the owner gives them in
// the admin panel. A member's permissions are everything their roles allow,
// together. Roles are ordered by `position` (higher = more senior); that
// order decides which role colours someone's name (their highest role that
// has a colour), which group they're listed under in the online list
// (their highest "shown separately" role), and who can kick whom (only
// people with a lower highest role). The crowned owner has every
// permission, and Administrator grants every permission too.
//
// Roles are tied to accounts by public key, so they follow the account
// rather than the username.

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

// ── Permissions ──────────────────────────────────────────────────────────────
pub const SEND_MESSAGES: u64 = 1 << 0;
pub const ATTACH_FILES: u64 = 1 << 1;
pub const ADD_REACTIONS: u64 = 1 << 2;
pub const MANAGE_MESSAGES: u64 = 1 << 3;
pub const CONNECT: u64 = 1 << 4;
pub const KICK_MEMBERS: u64 = 1 << 5;
pub const BAN_MEMBERS: u64 = 1 << 6;
pub const PIN_MESSAGES: u64 = 1 << 7;
pub const ADMINISTRATOR: u64 = 1 << 8;
pub const MANAGE_EMOJIS: u64 = 1 << 9;
pub const ALL: u64 = (1 << 10) - 1;

/// What @everyone can do on a new server — what everyone could do before
/// roles existed.
pub const DEFAULT_PERMISSIONS: u64 = SEND_MESSAGES | ATTACH_FILES | ADD_REACTIONS | CONNECT | PIN_MESSAGES;

/// The list the admin panel draws its checkboxes from.
fn catalog() -> serde_json::Value {
    serde_json::json!([
        { "bit": SEND_MESSAGES, "name": "Send Messages", "about": "Post messages in text channels." },
        { "bit": ATTACH_FILES, "name": "Attach Files", "about": "Upload pictures, videos and other files." },
        { "bit": ADD_REACTIONS, "name": "Add Reactions", "about": "React to messages with emoji." },
        { "bit": PIN_MESSAGES, "name": "Pin Messages", "about": "Pin and unpin messages in a channel." },
        { "bit": MANAGE_MESSAGES, "name": "Manage Messages", "about": "Delete other people's messages (and pin/unpin)." },
        { "bit": CONNECT, "name": "Connect to Voice", "about": "Join voice channels." },
        { "bit": MANAGE_EMOJIS, "name": "Manage Emoji", "about": "Upload, rename, hide and delete the server's custom emoji." },
        { "bit": KICK_MEMBERS, "name": "Kick Members", "about": "Remove members with a lower role from the server. They can rejoin." },
        { "bit": BAN_MEMBERS, "name": "Ban Members", "about": "Remove members with a lower role and stop them rejoining." },
        { "bit": ADMINISTRATOR, "name": "Administrator", "about": "Every permission above. Give this out carefully." },
    ])
}

pub const DEFAULT_ROLE_ID: &str = "everyone";

/// Creates the tables and the @everyone role. Safe on every start.
pub async fn init(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS roles (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            color       TEXT NOT NULL DEFAULT '',
            position    INTEGER NOT NULL DEFAULT 0,
            hoist       INTEGER NOT NULL DEFAULT 0,
            permissions INTEGER NOT NULL DEFAULT 0,
            is_default  INTEGER NOT NULL DEFAULT 0,
            created_at  TEXT NOT NULL
        )",
    ).execute(pool).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS member_roles (
            public_key TEXT NOT NULL,
            role_id    TEXT NOT NULL,
            PRIMARY KEY (public_key, role_id)
        )",
    ).execute(pool).await?;
    sqlx::query(
        "INSERT OR IGNORE INTO roles (id, name, color, position, hoist, permissions, is_default, created_at)
         VALUES (?, '@everyone', '', 0, 0, ?, 1, ?)",
    ).bind(DEFAULT_ROLE_ID).bind(DEFAULT_PERMISSIONS as i64).bind(chrono::Utc::now().to_rfc3339())
     .execute(pool).await?;
    Ok(())
}

// ── Working out what someone may do ──────────────────────────────────────────

async fn public_key_of(pool: &SqlitePool, username: &str) -> Option<String> {
    sqlx::query("SELECT public_key FROM users WHERE username = ?").bind(username)
        .fetch_optional(pool).await.ok().flatten().map(|r| r.get("public_key"))
}

/// Everything this member is allowed to do.
pub async fn permissions_of(pool: &SqlitePool, username: &str) -> u64 {
    if crate::admin::owner_username(pool).await.as_deref() == Some(username) {
        return ALL;
    }
    let key = public_key_of(pool, username).await.unwrap_or_default();
    let rows = sqlx::query(
        "SELECT permissions FROM roles WHERE is_default = 1
            OR id IN (SELECT role_id FROM member_roles WHERE public_key = ?)",
    ).bind(&key).fetch_all(pool).await.unwrap_or_default();
    let perms = rows.iter().fold(0u64, |acc, r| acc | r.get::<i64, _>("permissions") as u64);
    if perms & ADMINISTRATOR != 0 { ALL } else { perms }
}

pub async fn has(pool: &SqlitePool, username: &str, perm: u64) -> bool {
    permissions_of(pool, username).await & perm == perm
}

/// An error for an HTTP handler when the permission is missing.
pub async fn require(pool: &SqlitePool, username: &str, perm: u64, what: &str) -> Result<(), ApiErr> {
    if has(pool, username, perm).await { Ok(()) } else {
        Err((StatusCode::FORBIDDEN, Json(serde_json::json!({ "error": format!("You don't have permission to {what} on this server") }))))
    }
}

/// The position of someone's most senior role (@everyone is 0; the owner
/// outranks everyone).
async fn top_position(pool: &SqlitePool, username: &str) -> i64 {
    if crate::admin::owner_username(pool).await.as_deref() == Some(username) {
        return i64::MAX;
    }
    let key = public_key_of(pool, username).await.unwrap_or_default();
    sqlx::query("SELECT MAX(r.position) AS p FROM member_roles m JOIN roles r ON r.id = m.role_id WHERE m.public_key = ?")
        .bind(&key).fetch_one(pool).await.ok()
        .and_then(|r| r.try_get::<Option<i64>, _>("p").ok().flatten()).unwrap_or(0)
}

fn broadcast_roles(s: &AppState) {
    let _ = s.tx.send(serde_json::json!({ "type": "roles_updated" }).to_string());
}

async fn roles_json(pool: &SqlitePool) -> Result<(Vec<serde_json::Value>, serde_json::Map<String, serde_json::Value>), sqlx::Error> {
    let roles = sqlx::query("SELECT id, name, color, position, hoist, permissions, is_default FROM roles ORDER BY position DESC, created_at")
        .fetch_all(pool).await?.iter().map(|r| serde_json::json!({
            "id": r.get::<String, _>("id"), "name": r.get::<String, _>("name"), "color": r.get::<String, _>("color"),
            "position": r.get::<i64, _>("position"), "hoist": r.get::<i64, _>("hoist") != 0,
            "permissions": r.get::<i64, _>("permissions"), "is_default": r.get::<i64, _>("is_default") != 0,
        })).collect();
    let mut members = serde_json::Map::new();
    for r in sqlx::query("SELECT u.username AS username, m.role_id AS role_id FROM member_roles m JOIN users u ON u.public_key = m.public_key")
        .fetch_all(pool).await? {
        let user: String = r.get("username");
        let entry = members.entry(user).or_insert_with(|| serde_json::json!([]));
        entry.as_array_mut().unwrap().push(serde_json::Value::String(r.get("role_id")));
    }
    Ok((roles, members))
}

fn dberr() -> ApiErr { (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "DB error" }))) }
fn bad(msg: &str) -> ApiErr { (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": msg }))) }
fn session_user(h: &HeaderMap) -> &str { h.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("") }

// ── For members (the app) ────────────────────────────────────────────────────

/// GET /api/roles — every role, who has which, and what *you* can do.
pub async fn get_roles(headers: HeaderMap, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    let username = db::verify_token(&s.pool, session_user(&headers)).await.map_err(|_| dberr())?
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "Unauthorized" }))))?;
    let (roles, members) = roles_json(&s.pool).await.map_err(|_| dberr())?;
    Ok(Json(serde_json::json!({
        "roles": roles, "members": members, "my_permissions": permissions_of(&s.pool, &username).await,
    })))
}

#[derive(Deserialize)]
pub struct KickReq {
    #[serde(default)] pub ban: bool,
    #[serde(default)] pub reason: String,
}

/// POST /api/members/:username/kick {ban, reason} — for members whose roles
/// allow it, and only on members whose highest role is lower than theirs.
pub async fn kick_member(
    headers: HeaderMap,
    Path(target): Path<String>,
    State(s): State<Arc<AppState>>,
    Json(body): Json<KickReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let username = db::verify_token(&s.pool, session_user(&headers)).await.map_err(|_| dberr())?
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "Unauthorized" }))))?;
    if body.ban { require(&s.pool, &username, BAN_MEMBERS, "ban members").await?; }
    else { require(&s.pool, &username, KICK_MEMBERS, "kick members").await?; }
    if target == username { return Err(bad("You can't remove yourself")); }
    if top_position(&s.pool, &target).await >= top_position(&s.pool, &username).await {
        return Err((StatusCode::FORBIDDEN, Json(serde_json::json!({
            "error": format!("{target}'s highest role is the same as or above yours") }))));
    }
    crate::admin::kick_user(&s, &target, body.ban, &body.reason).await?;
    tracing::info!("{username} {} {target}", if body.ban { "banned" } else { "kicked" });
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── Admin panel ──────────────────────────────────────────────────────────────

/// GET /api/admin/roles
pub async fn admin_list(headers: HeaderMap, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    crate::admin::check_owner(&headers, &s)?;
    let (roles, members) = roles_json(&s.pool).await.map_err(|_| dberr())?;
    Ok(Json(serde_json::json!({ "roles": roles, "members": members, "permissions": catalog() })))
}

#[derive(Deserialize)]
pub struct RoleReq {
    pub name: Option<String>,
    pub color: Option<String>,
    pub hoist: Option<bool>,
    pub permissions: Option<u64>,
}

fn clean_name(n: &str) -> Result<String, ApiErr> {
    let n = n.trim();
    if n.is_empty() || n.chars().count() > 32 { return Err(bad("Role names are 1–32 characters")); }
    if n.eq_ignore_ascii_case("@everyone") { return Err(bad("That name is reserved")); }
    Ok(n.to_string())
}
fn clean_color(c: &str) -> Result<String, ApiErr> {
    let c = c.trim().to_lowercase();
    if c.is_empty() || (c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|x| x.is_ascii_hexdigit())) { Ok(c) }
    else { Err(bad("Colours look like #1a2b3c")) }
}

/// POST /api/admin/roles — a new role, placed above all the others.
pub async fn admin_create(headers: HeaderMap, State(s): State<Arc<AppState>>, Json(b): Json<RoleReq>) -> Result<Json<serde_json::Value>, ApiErr> {
    crate::admin::check_owner(&headers, &s)?;
    let name = clean_name(b.name.as_deref().unwrap_or("new role"))?;
    let color = clean_color(b.color.as_deref().unwrap_or(""))?;
    let top: i64 = sqlx::query("SELECT COALESCE(MAX(position), 0) AS p FROM roles").fetch_one(&s.pool).await.map_err(|_| dberr())?.get("p");
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO roles (id, name, color, position, hoist, permissions, is_default, created_at) VALUES (?, ?, ?, ?, ?, ?, 0, ?)")
        .bind(&id).bind(&name).bind(&color).bind(top + 1).bind(b.hoist.unwrap_or(false) as i64)
        .bind((b.permissions.unwrap_or(0) & ALL) as i64).bind(chrono::Utc::now().to_rfc3339())
        .execute(&s.pool).await.map_err(|_| dberr())?;
    broadcast_roles(&s);
    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

/// PUT /api/admin/roles/:id — change any of name, colour, "show separately"
/// and permissions. @everyone keeps its name and isn't shown separately.
pub async fn admin_update(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>, Json(b): Json<RoleReq>) -> Result<Json<serde_json::Value>, ApiErr> {
    crate::admin::check_owner(&headers, &s)?;
    let is_default = id == DEFAULT_ROLE_ID;
    if let Some(n) = b.name.as_deref() {
        if !is_default { sqlx::query("UPDATE roles SET name = ? WHERE id = ?").bind(clean_name(n)?).bind(&id).execute(&s.pool).await.map_err(|_| dberr())?; }
    }
    if let Some(c) = b.color.as_deref() {
        if !is_default { sqlx::query("UPDATE roles SET color = ? WHERE id = ?").bind(clean_color(c)?).bind(&id).execute(&s.pool).await.map_err(|_| dberr())?; }
    }
    if let Some(h) = b.hoist {
        if !is_default { sqlx::query("UPDATE roles SET hoist = ? WHERE id = ?").bind(h as i64).bind(&id).execute(&s.pool).await.map_err(|_| dberr())?; }
    }
    if let Some(p) = b.permissions {
        sqlx::query("UPDATE roles SET permissions = ? WHERE id = ?").bind((p & ALL) as i64).bind(&id).execute(&s.pool).await.map_err(|_| dberr())?;
    }
    broadcast_roles(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// DELETE /api/admin/roles/:id — removes it from everyone too.
pub async fn admin_delete(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    crate::admin::check_owner(&headers, &s)?;
    if id == DEFAULT_ROLE_ID { return Err(bad("@everyone can't be deleted")); }
    sqlx::query("DELETE FROM member_roles WHERE role_id = ?").bind(&id).execute(&s.pool).await.map_err(|_| dberr())?;
    sqlx::query("DELETE FROM roles WHERE id = ?").bind(&id).execute(&s.pool).await.map_err(|_| dberr())?;
    broadcast_roles(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct OrderReq { pub ids: Vec<String> }

/// POST /api/admin/roles/order {ids} — most senior first. @everyone always
/// stays at the bottom.
pub async fn admin_order(headers: HeaderMap, State(s): State<Arc<AppState>>, Json(b): Json<OrderReq>) -> Result<Json<serde_json::Value>, ApiErr> {
    crate::admin::check_owner(&headers, &s)?;
    let ids: Vec<&String> = b.ids.iter().filter(|i| i.as_str() != DEFAULT_ROLE_ID).collect();
    let n = ids.len() as i64;
    for (i, id) in ids.iter().enumerate() {
        sqlx::query("UPDATE roles SET position = ? WHERE id = ?").bind(n - i as i64).bind(id.as_str()).execute(&s.pool).await.map_err(|_| dberr())?;
    }
    broadcast_roles(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct MemberRolesReq { pub role_ids: Vec<String> }

/// PUT /api/admin/members/:username/roles {role_ids} — replaces their roles.
pub async fn admin_set_member_roles(headers: HeaderMap, Path(username): Path<String>, State(s): State<Arc<AppState>>, Json(b): Json<MemberRolesReq>) -> Result<Json<serde_json::Value>, ApiErr> {
    crate::admin::check_owner(&headers, &s)?;
    let key = public_key_of(&s.pool, &username).await.ok_or_else(|| bad("No member with that username"))?;
    sqlx::query("DELETE FROM member_roles WHERE public_key = ?").bind(&key).execute(&s.pool).await.map_err(|_| dberr())?;
    for rid in b.role_ids.iter().filter(|r| r.as_str() != DEFAULT_ROLE_ID) {
        let exists = sqlx::query("SELECT 1 FROM roles WHERE id = ?").bind(rid).fetch_optional(&s.pool).await.map_err(|_| dberr())?.is_some();
        if exists {
            sqlx::query("INSERT OR IGNORE INTO member_roles (public_key, role_id) VALUES (?, ?)").bind(&key).bind(rid).execute(&s.pool).await.map_err(|_| dberr())?;
        }
    }
    broadcast_roles(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}
