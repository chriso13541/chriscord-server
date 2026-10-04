// stickers.rs — the server's custom stickers.
//
// Built the same way as custom emoji (see emojis.rs): an image under
// ./stickers/<id> plus a row here. A sticker is sent as a message whose
// content carries `<s:name:id>` — by id, so renames never break it, and
// with no change to how messages are stored (the Discord importer writes
// the same token for a message's stickers). Clients show the token as a
// big image; a deleted sticker shows as its name.
//
// Names are freer than emoji names (Discord allows spaces and most
// punctuation): 2–30 characters, anything except < > : and control
// characters, which would break the token. Names needn't be unique — a
// sticker is picked from a grid, never typed. Hidden ones still render
// in old messages but aren't offered in the sticker menu.
//
// Same permission as emoji: Manage Emoji & Stickers (or the owner key).
// No size limit. PNG, APNG, GIF, WebP and JPEG; Discord's Lottie (JSON)
// stickers aren't drawn yet — the importer will need to convert those.

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use sqlx::{Row, SqlitePool};
use std::sync::Arc;

use crate::{db, emojis, roles, state::AppState};

type ApiErr = (StatusCode, Json<serde_json::Value>);

const STICKER_DIR: &str = "./stickers";
pub const MAX_STICKER_COUNT: i64 = 1000;

fn err(code: StatusCode, msg: &str) -> ApiErr {
    (code, Json(serde_json::json!({ "error": msg })))
}
fn dberr() -> ApiErr { err(StatusCode::INTERNAL_SERVER_ERROR, "DB error") }
fn bad(msg: &str) -> ApiErr { err(StatusCode::BAD_REQUEST, msg) }

pub async fn init(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS stickers (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            description TEXT NOT NULL DEFAULT '',
            animated    INTEGER NOT NULL DEFAULT 0,
            hidden      INTEGER NOT NULL DEFAULT 0,
            mime        TEXT NOT NULL,
            created_by  TEXT NOT NULL DEFAULT '',
            discord_id  TEXT,              -- set by the Discord importer
            created_at  TEXT NOT NULL
        )",
    ).execute(pool).await?;
    sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS idx_stickers_discord ON stickers (discord_id) WHERE discord_id IS NOT NULL")
        .execute(pool).await?;
    let _ = tokio::fs::create_dir_all(STICKER_DIR).await;
    Ok(())
}

pub fn valid_name(name: &str) -> bool {
    let n = name.chars().count();
    (2..=30).contains(&n) && !name.chars().any(|c| c == '<' || c == '>' || c == ':' || c.is_control())
        && name.trim() == name
}

fn file_path(id: &str) -> String { format!("{STICKER_DIR}/{id}") }

fn broadcast(s: &AppState) {
    let _ = s.tx.send(serde_json::json!({ "type": "stickers_updated" }).to_string());
}

async fn member(h: &HeaderMap, s: &AppState) -> Result<String, ApiErr> {
    let token = h.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    db::verify_token(&s.pool, token).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Unauthorized"))
}

async fn manager(h: &HeaderMap, s: &AppState) -> Result<String, ApiErr> {
    if crate::admin::check_owner(h, s).is_ok() {
        return Ok(String::new());
    }
    let user = member(h, s).await?;
    roles::require(&s.pool, &user, roles::MANAGE_EMOJIS, "manage stickers").await?;
    Ok(user)
}

fn row_json(r: &sqlx::sqlite::SqliteRow) -> serde_json::Value {
    serde_json::json!({
        "id": r.get::<String, _>("id"),
        "name": r.get::<String, _>("name"),
        "description": r.get::<String, _>("description"),
        "animated": r.get::<i64, _>("animated") != 0,
        "hidden": r.get::<i64, _>("hidden") != 0,
        "created_by": r.get::<String, _>("created_by"),
    })
}

const COLS: &str = "id, name, description, animated, hidden, created_by";

/// GET /api/stickers — every sticker, hidden ones flagged. Oldest first.
pub async fn list(headers: HeaderMap, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    if crate::admin::check_owner(&headers, &s).is_err() { member(&headers, &s).await?; }
    let rows = sqlx::query(&format!("SELECT {COLS} FROM stickers ORDER BY created_at, id"))
        .fetch_all(&s.pool).await.map_err(|_| dberr())?;
    Ok(Json(serde_json::Value::Array(rows.iter().map(row_json).collect())))
}

/// GET /api/stickers/:id — the image.
pub async fn image(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>) -> Response {
    if crate::admin::check_owner(&headers, &s).is_err() && member(&headers, &s).await.is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !emojis::valid_id(&id) { return StatusCode::NOT_FOUND.into_response(); }
    let mime: Option<String> = sqlx::query("SELECT mime FROM stickers WHERE id = ?").bind(&id)
        .fetch_optional(&s.pool).await.ok().flatten().map(|r| r.get("mime"));
    let Some(mime) = mime else { return StatusCode::NOT_FOUND.into_response(); };
    match tokio::fs::read(file_path(&id)).await {
        Ok(data) => (
            [
                (header::CONTENT_TYPE, mime),
                (header::CACHE_CONTROL, "private, max-age=31536000, immutable".to_string()),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            ],
            data,
        ).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
pub struct UploadQuery {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hidden: bool,
    pub discord_id: Option<String>,
}

/// POST /api/stickers?name=…[&description=…][&hidden=true][&discord_id=…]
/// — body: the image.
pub async fn upload(
    headers: HeaderMap,
    Query(q): Query<UploadQuery>,
    State(s): State<Arc<AppState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let by = manager(&headers, &s).await?;
    let name = q.name.trim();
    if !valid_name(name) {
        return Err(bad("Sticker names are 2–30 characters, without < > or :"));
    }
    let description: String = q.description.trim().chars().take(100).collect();
    let discord_id = q.discord_id.filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()));
    if let Some(did) = &discord_id {
        if let Some(r) = sqlx::query(&format!("SELECT {COLS} FROM stickers WHERE discord_id = ?"))
            .bind(did).fetch_optional(&s.pool).await.map_err(|_| dberr())? {
            return Ok(Json(row_json(&r)));
        }
    }
    let mime = match crate::pfp::image_mime(&body) {
        Some(m @ ("image/png" | "image/jpeg" | "image/gif" | "image/webp")) => m,
        _ => return Err(bad("Stickers must be a PNG, JPEG, GIF or WebP image")),
    };
    let count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM stickers").fetch_one(&s.pool).await
        .map_err(|_| dberr())?.get("n");
    if count >= MAX_STICKER_COUNT {
        return Err(bad(&format!("This server already has {MAX_STICKER_COUNT} stickers")));
    }

    let id = uuid::Uuid::now_v7().to_string();
    let animated = emojis::is_animated(mime, &body);
    tokio::fs::write(file_path(&id), &body).await.map_err(|_| bad("Could not save the sticker"))?;
    let res = sqlx::query(
        "INSERT INTO stickers (id, name, description, animated, hidden, mime, created_by, discord_id, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id).bind(name).bind(&description).bind(animated as i64).bind(q.hidden as i64).bind(mime)
    .bind(&by).bind(&discord_id).bind(chrono::Utc::now().to_rfc3339())
    .execute(&s.pool).await;
    if res.is_err() {
        let _ = tokio::fs::remove_file(file_path(&id)).await;
        return Err(dberr());
    }
    tracing::info!("sticker \"{name}\" added{}", if by.is_empty() { String::new() } else { format!(" by {by}") });
    broadcast(&s);
    Ok(Json(serde_json::json!({
        "id": id, "name": name, "description": description, "animated": animated,
        "hidden": q.hidden, "created_by": by,
    })))
}

#[derive(Deserialize)]
pub struct UpdateReq {
    pub name: Option<String>,
    pub description: Option<String>,
    pub hidden: Option<bool>,
}

/// PATCH /api/stickers/:id {name?, description?, hidden?}
pub async fn update(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(s): State<Arc<AppState>>,
    Json(b): Json<UpdateReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    manager(&headers, &s).await?;
    let row = sqlx::query("SELECT name, description, hidden FROM stickers WHERE id = ?").bind(&id)
        .fetch_optional(&s.pool).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Sticker not found"))?;
    let name = b.name.map(|n| n.trim().to_string()).unwrap_or_else(|| row.get("name"));
    if !valid_name(&name) {
        return Err(bad("Sticker names are 2–30 characters, without < > or :"));
    }
    let description: String = b.description.map(|d| d.trim().chars().take(100).collect())
        .unwrap_or_else(|| row.get("description"));
    let hidden = b.hidden.unwrap_or(row.get::<i64, _>("hidden") != 0);
    sqlx::query("UPDATE stickers SET name = ?, description = ?, hidden = ? WHERE id = ?")
        .bind(&name).bind(&description).bind(hidden as i64).bind(&id)
        .execute(&s.pool).await.map_err(|_| dberr())?;
    broadcast(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// DELETE /api/stickers/:id — messages with it show its name instead.
pub async fn remove(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    manager(&headers, &s).await?;
    if !emojis::valid_id(&id) { return Err(err(StatusCode::NOT_FOUND, "Sticker not found")); }
    let gone = sqlx::query("DELETE FROM stickers WHERE id = ?").bind(&id)
        .execute(&s.pool).await.map_err(|_| dberr())?.rows_affected() > 0;
    if !gone { return Err(err(StatusCode::NOT_FOUND, "Sticker not found")); }
    let _ = tokio::fs::remove_file(file_path(&id)).await;
    broadcast(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}
