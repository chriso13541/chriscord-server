// emojis.rs — the server's custom emoji.
//
// Each emoji is an image under ./emojis/<id> plus a row here. Messages
// carry them as tokens, `<:name:id>` (or `<a:name:id>` when animated), and
// reactions as `c:<id>` — always by id, so renaming an emoji never breaks
// an old message, and an emoji is never confused with another of the same
// name. An emoji's image never changes after upload (a new image is a new
// emoji), so clients can cache images by id forever.
//
// Hidden emoji still render wherever they're used but don't show in the
// picker or autocomplete. That's for imported "legacy" emoji: ones people
// used from other Discord servers, kept so old messages still look right.
// Visible emoji names are unique (ignoring case), so `:name:` always means
// one emoji; hidden ones can share a name.
//
// Reading the list and images needs a session. Uploading, renaming,
// hiding and deleting need the Manage Emoji permission (or the owner key,
// for the admin panel and tools like the Discord importer).

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

use crate::{db, roles, state::AppState};

type ApiErr = (StatusCode, Json<serde_json::Value>);

const EMOJI_DIR: &str = "./emojis";
/// Discord's limit is 256 KB; a little headroom for imports and APNGs.
pub const MAX_EMOJI_BYTES: usize = 512 << 10;
pub const MAX_EMOJI_COUNT: i64 = 1000;

fn err(code: StatusCode, msg: &str) -> ApiErr {
    (code, Json(serde_json::json!({ "error": msg })))
}
fn dberr() -> ApiErr { err(StatusCode::INTERNAL_SERVER_ERROR, "DB error") }
fn bad(msg: &str) -> ApiErr { err(StatusCode::BAD_REQUEST, msg) }

pub async fn init(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS emojis (
            id         TEXT PRIMARY KEY,
            name       TEXT NOT NULL,
            animated   INTEGER NOT NULL DEFAULT 0,
            hidden     INTEGER NOT NULL DEFAULT 0,
            mime       TEXT NOT NULL,
            created_by TEXT NOT NULL DEFAULT '',
            discord_id TEXT,              -- set by the Discord importer
            created_at TEXT NOT NULL
        )",
    ).execute(pool).await?;
    sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS idx_emojis_discord ON emojis (discord_id) WHERE discord_id IS NOT NULL")
        .execute(pool).await?;
    let _ = tokio::fs::create_dir_all(EMOJI_DIR).await;
    Ok(())
}

/// 2–32 letters, digits or underscores — the same rule as Discord, so
/// imported names always fit.
pub fn valid_name(name: &str) -> bool {
    (2..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Ids are UUIDs: never let anything else near a file name.
fn valid_id(id: &str) -> bool {
    id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

fn file_path(id: &str) -> String { format!("{EMOJI_DIR}/{id}") }

/// Whether an image moves: any GIF, a WebP with an ANIM chunk, or an APNG
/// (a PNG with an acTL chunk before its image data).
pub fn is_animated(mime: &str, bytes: &[u8]) -> bool {
    let has = |needle: &[u8], within: &[u8]| within.windows(needle.len()).any(|w| w == needle);
    match mime {
        "image/gif" => true,
        "image/webp" => has(b"ANIM", &bytes[..bytes.len().min(64)]),
        "image/png" => {
            let head = &bytes[..bytes.len().min(4096)];
            match head.windows(4).position(|w| w == b"IDAT") {
                Some(idat) => has(b"acTL", &head[..idat]),
                None => has(b"acTL", head),
            }
        }
        _ => false,
    }
}

/// Whether `c:<id>` names an emoji this server has (hidden ones included —
/// they're only left out of the picker).
pub async fn exists(pool: &SqlitePool, id: &str) -> bool {
    valid_id(id) && sqlx::query("SELECT 1 FROM emojis WHERE id = ?").bind(id)
        .fetch_optional(pool).await.ok().flatten().is_some()
}

fn broadcast(s: &AppState) {
    let _ = s.tx.send(serde_json::json!({ "type": "emojis_updated" }).to_string());
}

fn session_token(h: &HeaderMap) -> &str {
    h.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("")
}

async fn member(h: &HeaderMap, s: &AppState) -> Result<String, ApiErr> {
    db::verify_token(&s.pool, session_token(h)).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Unauthorized"))
}

/// Who's managing: the owner key (admin panel / importer) or a member with
/// Manage Emoji. Returns a name to record as the uploader.
async fn manager(h: &HeaderMap, s: &AppState) -> Result<String, ApiErr> {
    if crate::admin::check_owner(h, s).is_ok() {
        return Ok(String::new());
    }
    let user = member(h, s).await?;
    roles::require(&s.pool, &user, roles::MANAGE_EMOJIS, "manage emoji").await?;
    Ok(user)
}

/// Fails if a visible emoji other than `except` already has this name.
async fn name_free(pool: &SqlitePool, name: &str, except: &str) -> Result<(), ApiErr> {
    let taken = sqlx::query("SELECT 1 FROM emojis WHERE hidden = 0 AND name = ? COLLATE NOCASE AND id != ?")
        .bind(name).bind(except).fetch_optional(pool).await.map_err(|_| dberr())?.is_some();
    if taken { Err(bad(&format!("There's already an emoji called :{name}:"))) } else { Ok(()) }
}

fn row_json(r: &sqlx::sqlite::SqliteRow) -> serde_json::Value {
    serde_json::json!({
        "id": r.get::<String, _>("id"),
        "name": r.get::<String, _>("name"),
        "animated": r.get::<i64, _>("animated") != 0,
        "hidden": r.get::<i64, _>("hidden") != 0,
        "created_by": r.get::<String, _>("created_by"),
    })
}

/// GET /api/emojis — every emoji, hidden ones flagged. Oldest first.
pub async fn list(headers: HeaderMap, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    if crate::admin::check_owner(&headers, &s).is_err() { member(&headers, &s).await?; }
    let rows = sqlx::query("SELECT id, name, animated, hidden, created_by FROM emojis ORDER BY created_at, id")
        .fetch_all(&s.pool).await.map_err(|_| dberr())?;
    Ok(Json(serde_json::Value::Array(rows.iter().map(row_json).collect())))
}

/// GET /api/emojis/:id — the image.
pub async fn image(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>) -> Response {
    if crate::admin::check_owner(&headers, &s).is_err() && member(&headers, &s).await.is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !valid_id(&id) { return StatusCode::NOT_FOUND.into_response(); }
    let mime: Option<String> = sqlx::query("SELECT mime FROM emojis WHERE id = ?").bind(&id)
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
    pub hidden: bool,
    /// For the Discord importer: re-importing the same emoji is a no-op
    /// that returns the existing one.
    pub discord_id: Option<String>,
}

/// POST /api/emojis?name=…[&hidden=true][&discord_id=…] — body: the image.
pub async fn upload(
    headers: HeaderMap,
    Query(q): Query<UploadQuery>,
    State(s): State<Arc<AppState>>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let by = manager(&headers, &s).await?;
    let name = q.name.trim();
    if !valid_name(name) {
        return Err(bad("Emoji names are 2–32 letters, numbers or underscores"));
    }
    let discord_id = q.discord_id.filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()));
    if let Some(did) = &discord_id {
        if let Some(r) = sqlx::query("SELECT id, name, animated, hidden, created_by FROM emojis WHERE discord_id = ?")
            .bind(did).fetch_optional(&s.pool).await.map_err(|_| dberr())? {
            return Ok(Json(row_json(&r)));
        }
    }
    if body.len() > MAX_EMOJI_BYTES {
        return Err(bad("Emoji images must be 512 KB or smaller"));
    }
    let mime = match crate::pfp::image_mime(&body) {
        Some(m @ ("image/png" | "image/jpeg" | "image/gif" | "image/webp")) => m,
        _ => return Err(bad("Emoji must be a PNG, JPEG, GIF or WebP image")),
    };
    let count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM emojis").fetch_one(&s.pool).await
        .map_err(|_| dberr())?.get("n");
    if count >= MAX_EMOJI_COUNT {
        return Err(bad(&format!("This server already has {MAX_EMOJI_COUNT} emoji")));
    }
    if !q.hidden { name_free(&s.pool, name, "").await?; }

    let id = uuid::Uuid::now_v7().to_string();
    let animated = is_animated(mime, &body);
    tokio::fs::write(file_path(&id), &body).await.map_err(|_| bad("Could not save the emoji"))?;
    let res = sqlx::query(
        "INSERT INTO emojis (id, name, animated, hidden, mime, created_by, discord_id, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id).bind(name).bind(animated as i64).bind(q.hidden as i64).bind(mime)
    .bind(&by).bind(&discord_id).bind(chrono::Utc::now().to_rfc3339())
    .execute(&s.pool).await;
    if res.is_err() {
        let _ = tokio::fs::remove_file(file_path(&id)).await;
        return Err(dberr());
    }
    tracing::info!("emoji :{name}: added{}", if by.is_empty() { String::new() } else { format!(" by {by}") });
    broadcast(&s);
    Ok(Json(serde_json::json!({
        "id": id, "name": name, "animated": animated, "hidden": q.hidden, "created_by": by,
    })))
}

#[derive(Deserialize)]
pub struct UpdateReq {
    pub name: Option<String>,
    pub hidden: Option<bool>,
}

/// PATCH /api/emojis/:id {name?, hidden?}
pub async fn update(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(s): State<Arc<AppState>>,
    Json(b): Json<UpdateReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    manager(&headers, &s).await?;
    let row = sqlx::query("SELECT name, hidden FROM emojis WHERE id = ?").bind(&id)
        .fetch_optional(&s.pool).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Emoji not found"))?;
    let name = match &b.name {
        Some(n) => n.trim().to_string(),
        None => row.get::<String, _>("name"),
    };
    if !valid_name(&name) {
        return Err(bad("Emoji names are 2–32 letters, numbers or underscores"));
    }
    let hidden = b.hidden.unwrap_or(row.get::<i64, _>("hidden") != 0);
    if !hidden { name_free(&s.pool, &name, &id).await?; }
    sqlx::query("UPDATE emojis SET name = ?, hidden = ? WHERE id = ?")
        .bind(&name).bind(hidden as i64).bind(&id)
        .execute(&s.pool).await.map_err(|_| dberr())?;
    broadcast(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// DELETE /api/emojis/:id — messages using it fall back to showing
/// `:name:` as text; reactions with it are removed.
pub async fn remove(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    manager(&headers, &s).await?;
    if !valid_id(&id) { return Err(err(StatusCode::NOT_FOUND, "Emoji not found")); }
    let gone = sqlx::query("DELETE FROM emojis WHERE id = ?").bind(&id)
        .execute(&s.pool).await.map_err(|_| dberr())?.rows_affected() > 0;
    if !gone { return Err(err(StatusCode::NOT_FOUND, "Emoji not found")); }
    sqlx::query("DELETE FROM reactions WHERE emoji = ?").bind(format!("c:{id}"))
        .execute(&s.pool).await.map_err(|_| dberr())?;
    let _ = tokio::fs::remove_file(file_path(&id)).await;
    broadcast(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}
