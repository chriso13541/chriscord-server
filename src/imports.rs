// imports.rs — history brought over from another chat service (Discord,
// via chriscord-import), and claiming it.
//
// Every imported message is stored like any other, plus `imported_from`
// ("discord:<their Discord id>") saying whose it originally was. Until a
// member claims that person, their messages show under a placeholder
// name ("imported member"): it isn't an account, nobody can sign in as it,
// and if a real member joins with the same name the placeholder steps
// aside (it gets " (Discord)" added) so the two are never confused.
//
// Claiming: a member clicks an imported name and asks for it ("This is
// me"); the owner approves in the admin panel (or links them directly).
// From then on those messages carry the member's own username, so they
// show with their picture and they can edit or delete them. Unlinking
// puts the placeholder back.
//
// The messages themselves stay end-to-end encrypted: the importer sealed
// each one with who wrote it on Discord inside, and the app checks that
// against imported_from (which the server can only point at another
// imported person, never at a message's real author).
//
// Reactions on Discord came with counts but not names, so they're kept as
// counts here (imported_reactions) and added to the live ones.
//
// Routes:
//   POST /api/admin/import/session    the importer's own member account (owner key)
//   POST /api/admin/import/authors    add/update imported people (owner key)
//   POST /api/admin/import/channel    the channel an exported one goes in, found or made (owner key)
//   POST /api/admin/import/channel/applied  an export of it has been brought in (owner key)
//   POST /api/admin/import/existing   which message ids are already here, with their fingerprints
//   POST /api/admin/import/messages   add (or update) a batch of messages in a channel (owner key)
//
// Importing again later (a newer export) adds what's new and updates what
// changed; the importer decides which, from each message's fingerprint
// (import_digest — a keyed hash the server can't learn anything from) and
// from how recent the last export applied to the channel was.
//   GET  /api/admin/imported          imported people, their claims (owner key)
//   POST /api/admin/imported/:source/:id/link     {username} — give them to a member
//   POST /api/admin/imported/:source/:id/unlink
//   DELETE /api/admin/imported/:source/:id/requests/:public_key — turn down a claim
//   POST /api/claims                  {username} — "this is me" (member)

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use std::sync::Arc;

use crate::{admin::check_owner, db, state::AppState};

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn err(code: StatusCode, msg: &str) -> ApiErr {
    (code, Json(serde_json::json!({ "error": msg })))
}
fn dberr() -> ApiErr { err(StatusCode::INTERNAL_SERVER_ERROR, "DB error") }

/// Who may import: the owner key (Ok(None)), or a member whose roles give
/// them Import Messages (Ok(Some(their name))) — so an importer bot never
/// needs the owner key.
async fn importer(h: &HeaderMap, s: &AppState) -> Result<Option<String>, ApiErr> {
    if check_owner(h, s).is_ok() { return Ok(None); }
    let token = h.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let user = db::verify_token(&s.pool, token).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Importing needs the owner key, or a member with the Import Messages permission"))?;
    crate::roles::require(&s.pool, &user, crate::roles::IMPORT_MESSAGES, "import messages").await?;
    Ok(Some(user))
}
fn bad(msg: &str) -> ApiErr { err(StatusCode::BAD_REQUEST, msg) }

/// Longest username, as auth::join allows.
const MAX_NAME: usize = 32;
/// Most messages in one import request, and the most one may carry.
const MAX_BATCH: usize = 1000;
const MAX_CONTENT: usize = 1 << 20;
/// Largest imported profile picture kept.
const MAX_AVATAR: usize = 8 << 20;

pub async fn init(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS imported_authors (
            source      TEXT NOT NULL,          -- 'discord'
            source_id   TEXT NOT NULL,          -- their id there
            name        TEXT NOT NULL,          -- their name there
            handle      TEXT NOT NULL DEFAULT '',
            colour      TEXT NOT NULL DEFAULT '',
            placeholder TEXT NOT NULL,          -- what their messages show until claimed
            claimed_by  TEXT,                   -- identity key of the member who has them
            created_at  TEXT NOT NULL,
            PRIMARY KEY (source, source_id)
        )",
    ).execute(pool).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS imported_reactions (
            message_id TEXT NOT NULL,
            emoji      TEXT NOT NULL,
            count      INTEGER NOT NULL,
            PRIMARY KEY (message_id, emoji)
        )",
    ).execute(pool).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS claim_requests (
            source       TEXT NOT NULL,
            source_id    TEXT NOT NULL,
            public_key   TEXT NOT NULL,         -- who's asking
            requested_at TEXT NOT NULL,
            PRIMARY KEY (source, source_id, public_key)
        )",
    ).execute(pool).await?;
    // Which channel here each imported one went to, so a later export of
    // it lands in the same place even after a rename — and how recent the
    // last export applied to it was (its newest message, then the file's
    // time), so an older one doesn't undo a newer one's edits.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS imported_channels (
            source      TEXT NOT NULL,
            source_id   TEXT NOT NULL,
            board_id    TEXT NOT NULL,
            as_of_ms    INTEGER NOT NULL DEFAULT 0,
            as_of_mtime INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (source, source_id)
        )",
    ).execute(pool).await?;
    // Imported messages someone deleted here, so importing again doesn't
    // bring them back.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS imported_deleted (
            id         TEXT PRIMARY KEY,
            deleted_at TEXT NOT NULL
        )",
    ).execute(pool).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_imported ON messages (imported_from)")
        .execute(pool).await?;
    Ok(())
}

// ── Names ─────────────────────────────────────────────────────────────────────

fn clean_name(raw: &str) -> String {
    let s: String = raw.chars()
        .filter(|c| !c.is_control() && !matches!(*c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{FEFF}'))
        .collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    s.chars().take(MAX_NAME).collect()
}

/// Whether a name is free: no member and no other imported person (but
/// `except`, given as "source:id") goes by it, ignoring case.
async fn name_free(pool: &SqlitePool, name: &str, except: Option<&str>) -> bool {
    let member = sqlx::query("SELECT 1 FROM users WHERE username = ? COLLATE NOCASE")
        .bind(name).fetch_optional(pool).await.ok().flatten().is_some();
    if member { return false; }
    let rows = sqlx::query("SELECT source, source_id FROM imported_authors WHERE placeholder = ? COLLATE NOCASE")
        .bind(name).fetch_all(pool).await.unwrap_or_default();
    rows.iter().all(|r| Some(format!("{}:{}", r.get::<String, _>("source"), r.get::<String, _>("source_id")).as_str()) == except)
}

/// A free name built from `wanted`: as it is, then with their handle, then
/// numbered.
async fn free_name_like(pool: &SqlitePool, wanted: &str, handle: &str, except: Option<&str>) -> String {
    let base = { let b = clean_name(wanted); if b.is_empty() { "Imported member".to_string() } else { b } };
    let mut tries = vec![base.clone()];
    let handle = clean_name(handle);
    if !handle.is_empty() && !handle.eq_ignore_ascii_case(&base) {
        tries.push(fit(&base, &format!(" ({handle})")));
    }
    tries.push(fit(&base, " (Discord)"));
    for n in 2..1000 {
        tries.push(fit(&base, &format!(" ({n})")));
    }
    for t in tries {
        if name_free(pool, &t, except).await { return t; }
    }
    crate::utils::generate_hex(12)
}

/// base + suffix, shortening base so the whole stays within MAX_NAME.
fn fit(base: &str, suffix: &str) -> String {
    let room = MAX_NAME.saturating_sub(suffix.chars().count());
    format!("{}{suffix}", base.chars().take(room).collect::<String>().trim_end())
}

/// Someone's joining as `username`: an unclaimed imported person who goes
/// by it gets another name (theirs plus " (Discord)"), so the newcomer
/// isn't taken for them — and can't edit their messages. Called by
/// auth::join before registering a new member.
pub async fn make_way_for(s: &AppState, username: &str) {
    let rows: Vec<(String, String, String, String, String)> = sqlx::query(
        "SELECT source, source_id, name, handle, placeholder FROM imported_authors
         WHERE placeholder = ? COLLATE NOCASE AND claimed_by IS NULL")
        .bind(username).fetch_all(&s.pool).await.unwrap_or_default()
        .iter().map(|r| (r.get("source"), r.get("source_id"), r.get("name"), r.get("handle"), r.get("placeholder")))
        .collect();
    for (source, id, name, handle, old) in rows {
        let from = format!("{source}:{id}");
        // The new name must not be the joining name either, whatever happens.
        let mut new = free_name_like(&s.pool, &name, &handle, Some(from.as_str())).await;
        if new.eq_ignore_ascii_case(username) {
            new = fit(&name, " (Discord)");
            if !name_free(&s.pool, &new, Some(from.as_str())).await || new.eq_ignore_ascii_case(username) {
                new = format!("Imported {}", crate::utils::generate_hex(6));
            }
        }
        let _ = sqlx::query("UPDATE imported_authors SET placeholder = ? WHERE source = ? AND source_id = ?")
            .bind(&new).bind(&source).bind(&id).execute(&s.pool).await;
        let _ = sqlx::query("UPDATE messages SET username = ? WHERE imported_from = ? AND username = ?")
            .bind(&new).bind(&from).bind(&old).execute(&s.pool).await;
        tracing::info!("import: {old} (imported) is now shown as {new} — a member joined as {username}");
        history_changed(s, None);
    }
}

/// Who an imported person's messages show as now: the member who claimed
/// them, else their placeholder.
async fn shown_name(pool: &SqlitePool, source: &str, id: &str) -> Option<String> {
    let r = sqlx::query("SELECT a.placeholder AS placeholder, u.username AS member
                         FROM imported_authors a LEFT JOIN users u ON LOWER(u.public_key) = a.claimed_by
                         WHERE a.source = ? AND a.source_id = ?")
        .bind(source).bind(id).fetch_optional(pool).await.ok().flatten()?;
    let member: Option<String> = r.try_get("member").ok().flatten();
    Some(member.unwrap_or_else(|| r.get("placeholder")))
}

fn split_from(from: &str) -> Option<(&str, &str)> {
    let (source, id) = from.split_once(':')?;
    let ok = |x: &str| !x.is_empty() && x.len() <= 64 && x.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    (ok(source) && ok(id)).then_some((source, id))
}

/// Tells the apps that messages changed under them (a claim), so they
/// load the channel again. board_id None: possibly every channel.
fn history_changed(s: &AppState, board_id: Option<&str>) {
    let _ = s.tx.send(serde_json::json!({ "type": "history_changed", "board_id": board_id }).to_string());
}

// ── Pictures and profiles ─────────────────────────────────────────────────────

fn avatar_path(source: &str, id: &str) -> std::path::PathBuf {
    crate::pfp::pfps_dir().join(format!("imported-{source}-{id}.img"))
}

/// The imported person a name stands for, if it's an unclaimed one:
/// (source, id, name there, handle there).
pub async fn placeholder(pool: &SqlitePool, username: &str) -> Option<(String, String, String, String)> {
    let r = sqlx::query("SELECT source, source_id, name, handle FROM imported_authors WHERE placeholder = ? AND claimed_by IS NULL")
        .bind(username).fetch_optional(pool).await.ok().flatten()?;
    Some((r.get("source"), r.get("source_id"), r.get("name"), r.get("handle")))
}

/// An unclaimed imported person's picture, for pfp::serve_pfp.
pub async fn placeholder_avatar(pool: &SqlitePool, username: &str) -> Option<Vec<u8>> {
    let (source, id, _, _) = placeholder(pool, username).await?;
    tokio::fs::read(avatar_path(&source, &id)).await.ok()
}

/// The profile card for an unclaimed imported person, for profile::get_profile.
pub async fn placeholder_profile(pool: &SqlitePool, username: &str, viewer: &str) -> Option<serde_json::Value> {
    let (source, id, name, handle) = placeholder(pool, username).await?;
    let my_key: Option<String> = sqlx::query("SELECT public_key FROM users WHERE username = ?")
        .bind(viewer).fetch_optional(pool).await.ok().flatten().map(|r| r.get::<String, _>("public_key").to_lowercase());
    let requested = match &my_key {
        Some(k) => sqlx::query("SELECT 1 FROM claim_requests WHERE source = ? AND source_id = ? AND public_key = ?")
            .bind(&source).bind(&id).bind(k).fetch_optional(pool).await.ok().flatten().is_some(),
        None => false,
    };
    let first: Option<String> = sqlx::query("SELECT MIN(created_at) AS t FROM messages WHERE imported_from = ?")
        .bind(format!("{source}:{id}")).fetch_one(pool).await.ok().and_then(|r| r.try_get("t").ok().flatten());
    Some(serde_json::json!({
        "username": username, "bio": "", "tint": "", "nickname": "", "global_nickname": "",
        "has_banner": false, "updated_at": 0, "member_since": "",
        "imported": {
            "source": source, "name": name, "handle": handle,
            "first_message": first, "requested": requested,
        },
    }))
}

// ── The importer ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SessionReq {
    public_key: String,
    username:   String,
}

/// POST /api/admin/import/session — the importer's member account: made
/// (quietly — no "joined the server" line) the first time, and a session
/// for it. It's a normal member: it holds the imported channels' keys
/// until it has passed them on.
pub async fn session(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    Json(b): Json<SessionReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let via = importer(&headers, &s).await?;
    // A member with Import Messages (a bot, say) imports as itself, with
    // the session it already has.
    if let Some(user) = via {
        let token = headers.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        return Ok(Json(serde_json::json!({ "token": token, "username": user })));
    }
    let key = b.public_key.trim().to_lowercase();
    if key.len() != 64 || hex::decode(&key).is_err() {
        return Err(bad("Invalid public key"));
    }
    let existing: Option<String> = sqlx::query("SELECT username FROM users WHERE public_key = ?")
        .bind(&key).fetch_optional(&s.pool).await.map_err(|_| dberr())?.map(|r| r.get("username"));
    let username = match existing {
        Some(u) => u,
        None => {
            let wanted = { let w = clean_name(&b.username); if w.is_empty() { "Discord Import".to_string() } else { w } };
            let mut name = wanted.clone();
            let mut n = 2;
            while !name_free(&s.pool, &name, None).await {
                name = fit(&wanted, &format!(" {n}"));
                n += 1;
                if n > 1000 { return Err(bad("No free name for the importer")); }
            }
            sqlx::query("INSERT INTO users (public_key, username, created_at) VALUES (?, ?, ?)")
                .bind(&key).bind(&name).bind(chrono::Utc::now().to_rfc3339())
                .execute(&s.pool).await.map_err(|_| dberr())?;
            tracing::info!("import: made the importer's account {name}");
            crate::ws::broadcast_users(&s);
            name
        }
    };
    let _ = db::set_config(&s.pool, "import_account", &key).await;
    let token = crate::utils::generate_hex(64);
    sqlx::query("INSERT INTO sessions (token, username, public_key, created_at) VALUES (?, ?, ?, ?)")
        .bind(&token).bind(&username).bind(&key).bind(chrono::Utc::now().to_rfc3339())
        .execute(&s.pool).await.map_err(|_| dberr())?;
    Ok(Json(serde_json::json!({ "token": token, "username": username })))
}

#[derive(Deserialize)]
pub struct AuthorIn {
    id:     String,
    #[serde(default)]
    name:   String,
    #[serde(default)]
    handle: String,
    #[serde(default)]
    colour: String,
    /// Their picture, base64.
    #[serde(default)]
    avatar: Option<String>,
}

#[derive(Deserialize)]
pub struct AuthorsReq {
    source:  String,
    authors: Vec<AuthorIn>,
}

/// POST /api/admin/import/authors — adds imported people (or updates their
/// details), and answers with the name each one's messages show under.
pub async fn authors(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    Json(b): Json<AuthorsReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    importer(&headers, &s).await?;
    let mut out = serde_json::Map::new();
    for a in &b.authors {
        let from = format!("{}:{}", b.source, a.id);
        let Some((source, id)) = split_from(&from) else { return Err(bad("Invalid source or id")) };
        let colour = a.colour.trim().to_ascii_lowercase();
        let colour = if colour.len() == 7 && colour.starts_with('#') && colour[1..].chars().all(|c| c.is_ascii_hexdigit()) { colour } else { String::new() };
        let known = sqlx::query("SELECT 1 FROM imported_authors WHERE source = ? AND source_id = ?")
            .bind(source).bind(id).fetch_optional(&s.pool).await.map_err(|_| dberr())?.is_some();
        if known {
            sqlx::query("UPDATE imported_authors SET name = ?, handle = ?, colour = ? WHERE source = ? AND source_id = ?")
                .bind(clean_name(&a.name)).bind(clean_name(&a.handle)).bind(&colour).bind(source).bind(id)
                .execute(&s.pool).await.map_err(|_| dberr())?;
        } else {
            let placeholder = free_name_like(&s.pool, &a.name, &a.handle, None).await;
            sqlx::query("INSERT INTO imported_authors (source, source_id, name, handle, colour, placeholder, created_at)
                         VALUES (?, ?, ?, ?, ?, ?, ?)")
                .bind(source).bind(id).bind(clean_name(&a.name)).bind(clean_name(&a.handle)).bind(&colour)
                .bind(&placeholder).bind(chrono::Utc::now().to_rfc3339())
                .execute(&s.pool).await.map_err(|_| dberr())?;
        }
        if let Some(data) = a.avatar.as_deref().filter(|d| !d.is_empty()) {
            use base64::Engine as _;
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                if bytes.len() <= MAX_AVATAR && crate::pfp::is_profile_image(&bytes) {
                    let _ = tokio::fs::write(avatar_path(source, id), &bytes).await;
                }
            }
        }
        if let Some(name) = shown_name(&s.pool, source, id).await {
            out.insert(id.to_string(), serde_json::Value::String(name));
        }
    }
    Ok(Json(serde_json::json!({ "usernames": out })))
}

#[derive(Deserialize)]
pub struct ChannelReq {
    source:    String,
    source_id: String,
    /// The category and channel names, for one that's new here.
    room:      String,
    name:      String,
    /// Only look: don't make anything.
    #[serde(default)]
    dry_run:   bool,
}

/// POST /api/admin/import/channel — the channel an exported one goes in:
/// wherever it went last time (however it's been renamed or moved since),
/// else a text channel of that name in a category of that name, else a new
/// one. Answers with how recent the last export applied to it was.
pub async fn channel(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    Json(b): Json<ChannelReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    importer(&headers, &s).await?;
    let from = format!("{}:{}", b.source, b.source_id);
    let Some((source, sid)) = split_from(&from) else { return Err(bad("Invalid source or id")) };
    let (source, sid) = (source.to_string(), sid.to_string());

    let known = sqlx::query(
        "SELECT c.board_id AS board_id, c.as_of_ms AS as_of_ms, c.as_of_mtime AS as_of_mtime, b.name AS name, r.name AS room
         FROM imported_channels c JOIN boards b ON b.id = c.board_id JOIN rooms r ON r.id = b.room_id
         WHERE c.source = ? AND c.source_id = ?",
    ).bind(&source).bind(&sid).fetch_optional(&s.pool).await.map_err(|_| dberr())?;
    if let Some(r) = known {
        return Ok(Json(serde_json::json!({
            "board_id": r.get::<String, _>("board_id"), "name": r.get::<String, _>("name"),
            "room": r.get::<String, _>("room"), "created": false,
            "as_of": [r.get::<i64, _>("as_of_ms"), r.get::<i64, _>("as_of_mtime")],
        })));
    }

    let room: String = { let r: String = b.room.trim().chars().take(100).collect(); if r.is_empty() { "Imported".into() } else { r } };
    let name: String = { let n: String = b.name.trim().chars().take(100).collect(); if n.is_empty() { "imported".into() } else { n } };
    let room_id: Option<String> = sqlx::query("SELECT id FROM rooms WHERE room_type = 'text' AND name = ? COLLATE NOCASE ORDER BY position, created_at LIMIT 1")
        .bind(&room).fetch_optional(&s.pool).await.map_err(|_| dberr())?.map(|r| r.get("id"));
    let board_id: Option<String> = match &room_id {
        Some(rid) => sqlx::query("SELECT id FROM boards WHERE room_id = ? AND name = ? COLLATE NOCASE ORDER BY position, created_at LIMIT 1")
            .bind(rid).bind(&name).fetch_optional(&s.pool).await.map_err(|_| dberr())?.map(|r| r.get("id")),
        None => None,
    };
    if b.dry_run {
        return Ok(Json(serde_json::json!({
            "board_id": board_id.clone().unwrap_or_default(), "name": name, "room": room,
            "created": board_id.is_none(), "as_of": null,
        })));
    }
    let now = chrono::Utc::now().to_rfc3339();
    let mut created = false;
    let room_id = match room_id {
        Some(r) => r,
        None => {
            let id = uuid::Uuid::now_v7().to_string();
            sqlx::query("INSERT INTO rooms (id, name, is_private, room_type, created_at, position) VALUES (?, ?, 0, 'text', ?, ?)")
                .bind(&id).bind(&room).bind(&now).bind(crate::layout::next_room_position(&s.pool).await)
                .execute(&s.pool).await.map_err(|_| dberr())?;
            created = true;
            id
        }
    };
    let board_id = match board_id {
        Some(bid) => bid,
        None => {
            let id = uuid::Uuid::now_v7().to_string();
            sqlx::query("INSERT INTO boards (id, room_id, name, created_at, position) VALUES (?, ?, ?, ?, ?)")
                .bind(&id).bind(&room_id).bind(&name).bind(&now).bind(crate::layout::next_board_position(&s.pool, &room_id).await)
                .execute(&s.pool).await.map_err(|_| dberr())?;
            created = true;
            id
        }
    };
    sqlx::query("INSERT OR REPLACE INTO imported_channels (source, source_id, board_id, as_of_ms, as_of_mtime) VALUES (?, ?, ?, 0, 0)")
        .bind(&source).bind(&sid).bind(&board_id).execute(&s.pool).await.map_err(|_| dberr())?;
    if created {
        let _ = s.tx.send(serde_json::json!({ "type": "rooms_updated" }).to_string());
    }
    Ok(Json(serde_json::json!({
        "board_id": board_id, "name": name, "room": room, "created": created, "as_of": [0, 0],
    })))
}

#[derive(Deserialize)]
pub struct AppliedReq {
    source:      String,
    source_id:   String,
    as_of_ms:    i64,
    as_of_mtime: i64,
}

/// POST /api/admin/import/channel/applied — an export of this channel has
/// been brought in; remembered if it's the newest so far.
pub async fn channel_applied(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    Json(b): Json<AppliedReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    importer(&headers, &s).await?;
    sqlx::query("UPDATE imported_channels SET as_of_ms = ?, as_of_mtime = ?
                 WHERE source = ? AND source_id = ? AND (as_of_ms < ? OR (as_of_ms = ? AND as_of_mtime < ?))")
        .bind(b.as_of_ms).bind(b.as_of_mtime).bind(&b.source).bind(&b.source_id)
        .bind(b.as_of_ms).bind(b.as_of_ms).bind(b.as_of_mtime)
        .execute(&s.pool).await.map_err(|_| dberr())?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct ExistingReq {
    ids: Vec<String>,
}

/// POST /api/admin/import/existing — which of these message ids are here
/// already (so a second run skips them).
pub async fn existing(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    Json(b): Json<ExistingReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    importer(&headers, &s).await?;
    if b.ids.len() > 5000 { return Err(bad("Too many ids at once")); }
    let mut found = Vec::new();
    let mut info = serde_json::Map::new();
    for chunk in b.ids.chunks(500) {
        let sql = format!("SELECT id, import_digest FROM messages WHERE id IN ({})", vec!["?"; chunk.len()].join(","));
        let mut q = sqlx::query(&sql);
        for id in chunk { q = q.bind(id); }
        for r in q.fetch_all(&s.pool).await.map_err(|_| dberr())? {
            let id: String = r.get("id");
            let digest: Option<String> = r.try_get("import_digest").ok().flatten();
            info.insert(id.clone(), serde_json::json!({ "digest": digest.unwrap_or_default() }));
            found.push(id);
        }
        // Deleted here since: counts as there (and isn't to be brought back).
        let sql = format!("SELECT id FROM imported_deleted WHERE id IN ({})", vec!["?"; chunk.len()].join(","));
        let mut q = sqlx::query(&sql);
        for id in chunk { q = q.bind(id); }
        for r in q.fetch_all(&s.pool).await.map_err(|_| dberr())? {
            let id: String = r.get("id");
            if info.contains_key(&id) { continue; }
            info.insert(id.clone(), serde_json::json!({ "digest": "", "deleted": true }));
            found.push(id);
        }
    }
    Ok(Json(serde_json::json!({ "existing": found, "info": info })))
}

#[derive(Deserialize)]
pub struct ReactionIn {
    emoji: String,
    count: i64,
}

#[derive(Deserialize)]
pub struct MessageIn {
    id:            String,
    imported_from: String,
    #[serde(default)]
    content:       String,
    #[serde(default)]
    attachments:   Vec<crate::messages::Attachment>,
    #[serde(default)]
    edited:        bool,
    created_at:    String,
    #[serde(default)]
    reply_to:      Option<String>,
    #[serde(default)]
    kind:          Option<String>,
    #[serde(default)]
    pinned:        bool,
    #[serde(default)]
    reactions:     Vec<ReactionIn>,
    /// The importer's fingerprint of it (opaque here).
    #[serde(default)]
    digest:        Option<String>,
    /// For one that's here already, what to replace: "content" (text and
    /// files), "meta" (pin and reaction counts) or "all". None: add it.
    #[serde(default)]
    update:        Option<String>,
}

#[derive(Deserialize)]
pub struct MessagesReq {
    board_id: String,
    messages: Vec<MessageIn>,
}

/// POST /api/admin/import/messages — adds a batch of messages to a
/// channel, as they were: their own ids (UUIDv7 at the original time, so
/// they sort before anything sent here), times, authors, replies, pins and
/// reaction counts. Ids already here are left alone.
pub async fn messages(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    Json(b): Json<MessagesReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    importer(&headers, &s).await?;
    if b.messages.len() > MAX_BATCH { return Err(bad("Too many messages at once")); }
    let board_ok = sqlx::query("SELECT 1 FROM boards WHERE id = ?").bind(&b.board_id)
        .fetch_optional(&s.pool).await.map_err(|_| dberr())?.is_some();
    if !board_ok { return Err(err(StatusCode::NOT_FOUND, "Channel not found")); }

    // Who each imported person shows as, worked out once per batch.
    let mut names: HashMap<String, String> = HashMap::new();
    for m in &b.messages {
        if names.contains_key(&m.imported_from) { continue; }
        let Some((source, id)) = split_from(&m.imported_from) else { return Err(bad("Invalid imported_from")) };
        let Some(name) = shown_name(&s.pool, source, id).await else {
            return Err(bad(&format!("Unknown imported person {}: add them first", m.imported_from)));
        };
        names.insert(m.imported_from.clone(), name);
    }

    let mut tx = s.pool.begin().await.map_err(|_| dberr())?;
    let mut inserted = 0;
    let mut updated = 0;
    // Files that updated messages no longer use: removed once it's saved.
    let mut dropped_files: Vec<String> = Vec::new();
    for m in &b.messages {
        let id = uuid::Uuid::parse_str(&m.id).map_err(|_| bad("Message ids must be UUIDs"))?.to_string();
        let digest: Option<String> = m.digest.as_deref().map(|d| d.chars().take(128).collect());
        if let Some(what) = m.update.as_deref() {
            let (content, meta) = match what {
                "content" => (true, false),
                "meta" => (false, true),
                "all" => (true, true),
                _ => return Err(bad("update must be content, meta or all")),
            };
            // Only an imported message of that same person, in this channel.
            let Some(row) = sqlx::query("SELECT attachments, created_at FROM messages WHERE id = ? AND board_id = ? AND imported_from = ?")
                .bind(&id).bind(&b.board_id).bind(&m.imported_from)
                .fetch_optional(&mut *tx).await.map_err(|_| dberr())? else { continue };
            if content {
                if m.content.len() > MAX_CONTENT { return Err(bad("A message is too long")); }
                let old: Vec<crate::messages::Attachment> = row.try_get::<Option<String>, _>("attachments").ok().flatten()
                    .and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
                for a in &old {
                    if !m.attachments.iter().any(|n| n.url == a.url) { dropped_files.push(a.url.clone()); }
                }
                let reply_to = m.reply_to.as_deref().and_then(|r| uuid::Uuid::parse_str(r).ok()).map(|u| u.to_string());
                sqlx::query("UPDATE messages SET content = ?, attachments = ?, edited = ?, reply_to = ? WHERE id = ?")
                    .bind(&m.content).bind(serde_json::to_string(&m.attachments).unwrap_or_else(|_| "[]".into()))
                    .bind(m.edited as i64).bind(&reply_to).bind(&id)
                    .execute(&mut *tx).await.map_err(|_| dberr())?;
            }
            if meta {
                // Pinned on Discord now: pinned here too (keeping an earlier
                // pin's time); unpinned there: unpinned here.
                let created: String = row.get("created_at");
                if m.pinned {
                    sqlx::query("UPDATE messages SET pinned_at = COALESCE(pinned_at, ?) WHERE id = ?")
                        .bind(&created).bind(&id).execute(&mut *tx).await.map_err(|_| dberr())?;
                } else {
                    sqlx::query("UPDATE messages SET pinned_at = NULL, pinned_by = NULL WHERE id = ?")
                        .bind(&id).execute(&mut *tx).await.map_err(|_| dberr())?;
                }
                sqlx::query("DELETE FROM imported_reactions WHERE message_id = ?")
                    .bind(&id).execute(&mut *tx).await.map_err(|_| dberr())?;
                for re in m.reactions.iter().take(crate::messages::MAX_REACTION_EMOJI) {
                    let emoji = re.emoji.trim();
                    if emoji.is_empty() || emoji.len() > 64 || re.count <= 0 { continue; }
                    sqlx::query("INSERT OR REPLACE INTO imported_reactions (message_id, emoji, count) VALUES (?, ?, ?)")
                        .bind(&id).bind(emoji).bind(re.count.min(1_000_000))
                        .execute(&mut *tx).await.map_err(|_| dberr())?;
                }
            }
            sqlx::query("UPDATE messages SET import_digest = ? WHERE id = ?")
                .bind(&digest).bind(&id).execute(&mut *tx).await.map_err(|_| dberr())?;
            updated += 1;
            continue;
        }
        let created = chrono::DateTime::parse_from_rfc3339(&m.created_at)
            .map_err(|_| bad("created_at must be an RFC 3339 time"))?
            .with_timezone(&chrono::Utc).to_rfc3339();
        let kind = match m.kind.as_deref().unwrap_or("") {
            "" => None,
            "join" => Some("join"),
            _ => return Err(bad("Unknown message kind")),
        };
        if m.content.len() > MAX_CONTENT { return Err(bad("A message is too long")); }
        let reply_to = m.reply_to.as_deref().and_then(|r| uuid::Uuid::parse_str(r).ok()).map(|u| u.to_string());
        let pinned_at = m.pinned.then(|| created.clone());
        let r = sqlx::query(
            "INSERT OR IGNORE INTO messages (id, board_id, username, content, attachments, edited, created_at, reply_to, kind, pinned_at, imported_from, import_digest)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id).bind(&b.board_id).bind(&names[&m.imported_from]).bind(&m.content)
        .bind(serde_json::to_string(&m.attachments).unwrap_or_else(|_| "[]".into()))
        .bind(m.edited as i64).bind(&created).bind(&reply_to).bind(kind).bind(&pinned_at).bind(&m.imported_from).bind(&digest)
        .execute(&mut *tx).await.map_err(|e| { tracing::warn!("import: {e}"); dberr() })?;
        if r.rows_affected() == 0 { continue; } // already imported
        inserted += 1;
        for re in m.reactions.iter().take(crate::messages::MAX_REACTION_EMOJI) {
            let emoji = re.emoji.trim();
            if emoji.is_empty() || emoji.len() > 64 || re.count <= 0 { continue; }
            sqlx::query("INSERT OR REPLACE INTO imported_reactions (message_id, emoji, count) VALUES (?, ?, ?)")
                .bind(&id).bind(emoji).bind(re.count.min(1_000_000))
                .execute(&mut *tx).await.map_err(|_| dberr())?;
        }
    }
    tx.commit().await.map_err(|_| dberr())?;
    // The encrypted files updated messages replaced (nothing else uses them).
    for url in &dropped_files {
        let Some(name) = url.strip_prefix("/api/files/").and_then(crate::files::safe_name) else { continue };
        let still_used = sqlx::query("SELECT 1 FROM messages WHERE attachments LIKE ? LIMIT 1")
            .bind(format!("%\"/api/files/{name}\"%")).fetch_optional(&s.pool).await.ok().flatten().is_some();
        if !still_used {
            let _ = tokio::fs::remove_file(format!("{}/{}", crate::files::UPLOAD_DIR, name)).await;
        }
    }
    if inserted > 0 || updated > 0 {
        tracing::info!("import: {inserted} new and {updated} updated message(s) in board {}", b.board_id);
        history_changed(&s, Some(b.board_id.as_str()));
    }
    Ok(Json(serde_json::json!({ "ok": true, "inserted": inserted, "updated": updated })))
}

/// Imported reaction counts for these messages: id → [(emoji, count)], in
/// the order they were added.
pub async fn imported_reactions(pool: &SqlitePool, ids: &[String]) -> HashMap<String, Vec<(String, i64)>> {
    let mut out: HashMap<String, Vec<(String, i64)>> = HashMap::new();
    if ids.is_empty() { return out; }
    let sql = format!(
        "SELECT message_id, emoji, count FROM imported_reactions WHERE message_id IN ({}) ORDER BY rowid",
        vec!["?"; ids.len()].join(","),
    );
    let mut q = sqlx::query(&sql);
    for id in ids { q = q.bind(id); }
    for r in q.fetch_all(pool).await.unwrap_or_default() {
        out.entry(r.get("message_id")).or_default().push((r.get("emoji"), r.get("count")));
    }
    out
}

// ── Claims ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ClaimReq {
    /// The imported name, as their messages show it.
    username: String,
}

/// POST /api/claims — "this is me": asks the owner to give you an imported
/// person's messages.
pub async fn request_claim(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    Json(b): Json<ClaimReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let token = headers.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let me = db::verify_token(&s.pool, token).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Unauthorized"))?;
    let my_key: String = sqlx::query("SELECT public_key FROM users WHERE username = ?")
        .bind(&me).fetch_optional(&s.pool).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Unauthorized"))?
        .get::<String, _>("public_key").to_lowercase();
    let Some((source, id, name, _)) = placeholder(&s.pool, &b.username).await else {
        return Err(err(StatusCode::NOT_FOUND, "That isn't an unclaimed imported member"));
    };
    sqlx::query("INSERT OR IGNORE INTO claim_requests (source, source_id, public_key, requested_at) VALUES (?, ?, ?, ?)")
        .bind(&source).bind(&id).bind(&my_key).bind(chrono::Utc::now().to_rfc3339())
        .execute(&s.pool).await.map_err(|_| dberr())?;
    tracing::info!("import: {me} asked to claim {name} ({source}:{id})");
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/admin/imported — everyone imported, who has them, and who's asking.
pub async fn admin_list(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    let names: HashMap<String, String> = sqlx::query("SELECT public_key, username FROM users")
        .fetch_all(&s.pool).await.map_err(|_| dberr())?
        .iter().map(|r| (r.get::<String, _>("public_key").to_lowercase(), r.get::<String, _>("username"))).collect();
    let counts: HashMap<String, i64> = sqlx::query("SELECT imported_from, COUNT(*) AS n FROM messages WHERE imported_from IS NOT NULL GROUP BY imported_from")
        .fetch_all(&s.pool).await.map_err(|_| dberr())?
        .iter().map(|r| (r.get::<String, _>("imported_from"), r.get::<i64, _>("n"))).collect();
    let mut requests: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    for r in sqlx::query("SELECT source, source_id, public_key, requested_at FROM claim_requests ORDER BY requested_at")
        .fetch_all(&s.pool).await.map_err(|_| dberr())? {
        let key: String = r.get("public_key");
        let Some(user) = names.get(&key) else { continue }; // they've left
        requests.entry(format!("{}:{}", r.get::<String, _>("source"), r.get::<String, _>("source_id"))).or_default()
            .push(serde_json::json!({
                "username": user, "public_key": key, "fingerprint": key.get(..16).unwrap_or(&key),
                "requested_at": r.get::<String, _>("requested_at"),
            }));
    }
    let rows = sqlx::query("SELECT source, source_id, name, handle, colour, placeholder, claimed_by FROM imported_authors ORDER BY name COLLATE NOCASE")
        .fetch_all(&s.pool).await.map_err(|_| dberr())?;
    let list: Vec<serde_json::Value> = rows.iter().map(|r| {
        let (source, id): (String, String) = (r.get("source"), r.get("source_id"));
        let from = format!("{source}:{id}");
        let claimed_by: Option<String> = r.try_get("claimed_by").ok().flatten();
        serde_json::json!({
            "source": source, "id": id,
            "name": r.get::<String, _>("name"), "handle": r.get::<String, _>("handle"),
            "colour": r.get::<String, _>("colour"), "placeholder": r.get::<String, _>("placeholder"),
            "claimed_by": claimed_by.as_ref().and_then(|k| names.get(&k.to_lowercase())).cloned(),
            "messages": counts.get(&from).copied().unwrap_or(0),
            "has_avatar": avatar_path(&source, &id).exists(),
            "requests": requests.remove(&from).unwrap_or_default(),
        })
    }).collect();
    let import_account = db::get_config(&s.pool, "import_account").await.ok().flatten()
        .and_then(|k| names.get(&k).cloned());
    Ok(Json(serde_json::json!({ "authors": list, "import_account": import_account })))
}

#[derive(Deserialize)]
pub struct LinkReq {
    username: String,
}

/// POST /api/admin/imported/:source/:id/link {username} — gives an imported
/// person's messages to a member (approving their claim, or directly).
pub async fn admin_link(
    headers: HeaderMap,
    Path((source, id)): Path<(String, String)>,
    State(s): State<Arc<AppState>>,
    Json(b): Json<LinkReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    let from = format!("{source}:{id}");
    if split_from(&from).is_none() { return Err(bad("Invalid id")); }
    let key: String = sqlx::query("SELECT public_key FROM users WHERE username = ?")
        .bind(b.username.trim()).fetch_optional(&s.pool).await.map_err(|_| dberr())?
        .ok_or_else(|| bad("No member with that username"))?
        .get::<String, _>("public_key").to_lowercase();
    let username = b.username.trim().to_string();
    let mut tx = s.pool.begin().await.map_err(|_| dberr())?;
    let r = sqlx::query("UPDATE imported_authors SET claimed_by = ? WHERE source = ? AND source_id = ?")
        .bind(&key).bind(&source).bind(&id).execute(&mut *tx).await.map_err(|_| dberr())?;
    if r.rows_affected() == 0 { return Err(err(StatusCode::NOT_FOUND, "No such imported member")); }
    let moved = sqlx::query("UPDATE messages SET username = ? WHERE imported_from = ?")
        .bind(&username).bind(&from).execute(&mut *tx).await.map_err(|_| dberr())?.rows_affected();
    sqlx::query("DELETE FROM claim_requests WHERE source = ? AND source_id = ?")
        .bind(&source).bind(&id).execute(&mut *tx).await.map_err(|_| dberr())?;
    tx.commit().await.map_err(|_| dberr())?;
    tracing::info!("import: {from} linked to {username} ({moved} message(s))");
    history_changed(&s, None);
    Ok(Json(serde_json::json!({ "ok": true, "messages": moved })))
}

/// POST /api/admin/imported/:source/:id/unlink — back to the placeholder.
pub async fn admin_unlink(
    headers: HeaderMap,
    Path((source, id)): Path<(String, String)>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    let from = format!("{source}:{id}");
    if split_from(&from).is_none() { return Err(bad("Invalid id")); }
    let (name, handle, mut placeholder): (String, String, String) =
        sqlx::query("SELECT name, handle, placeholder FROM imported_authors WHERE source = ? AND source_id = ?")
        .bind(&source).bind(&id).fetch_optional(&s.pool).await.map_err(|_| dberr())?
        .map(|r| (r.get("name"), r.get("handle"), r.get("placeholder")))
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "No such imported member"))?;
    // Someone may have joined under the old placeholder since.
    if !name_free(&s.pool, &placeholder, Some(from.as_str())).await {
        placeholder = free_name_like(&s.pool, &name, &handle, Some(from.as_str())).await;
    }
    let mut tx = s.pool.begin().await.map_err(|_| dberr())?;
    sqlx::query("UPDATE imported_authors SET claimed_by = NULL, placeholder = ? WHERE source = ? AND source_id = ?")
        .bind(&placeholder).bind(&source).bind(&id).execute(&mut *tx).await.map_err(|_| dberr())?;
    sqlx::query("UPDATE messages SET username = ? WHERE imported_from = ?")
        .bind(&placeholder).bind(&from).execute(&mut *tx).await.map_err(|_| dberr())?;
    tx.commit().await.map_err(|_| dberr())?;
    tracing::info!("import: {from} unlinked (shown as {placeholder} again)");
    history_changed(&s, None);
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// DELETE /api/admin/imported/:source/:id/requests/:public_key — turns a claim down.
pub async fn admin_deny(
    headers: HeaderMap,
    Path((source, id, public_key)): Path<(String, String, String)>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    sqlx::query("DELETE FROM claim_requests WHERE source = ? AND source_id = ? AND public_key = ?")
        .bind(&source).bind(&id).bind(public_key.to_lowercase()).execute(&s.pool).await.map_err(|_| dberr())?;
    Ok(Json(serde_json::json!({ "ok": true })))
}
