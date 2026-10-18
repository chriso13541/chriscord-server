//! search_sync.rs — what the app needs to search a server whose messages it
//! can't have the server read.
//!
//! With end-to-end encryption the app does the matching, and it can keep a
//! copy of the history it has decrypted (encrypted on its own disk) so old
//! messages are found without downloading them all again. For that copy to
//! stay right it has to hear about what changed in the history it already
//! has, which the live socket only tells it about while it's connected:
//!
//! - `message_changes` notes each edit, delete, imported batch and removed
//!   channel, numbered in order. `GET /api/search/changes?after=N` hands the
//!   app what happened since the last one it saw (edits with the message as
//!   it is now, still encrypted). Notes older than [`KEEP_DAYS`] are
//!   dropped; an app further behind than that is told to start over.
//! - `GET /api/search/authors` lists everyone who has written in the
//!   channels you can see, ever (members, people whose history was
//!   imported, members since gone), for `from:` — not just whoever's
//!   messages the app happens to have loaded.

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use sqlx::{Row, SqlitePool};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use crate::{db, state::AppState};

/// How long a change is remembered for apps that were away.
const KEEP_DAYS: i64 = 90;
/// Most changes handed over at once (the app asks again for the rest).
const CHANGES_MAX: i64 = 2000;
/// The config key holding the last change number dropped by pruning.
const PRUNED_KEY: &str = "message_changes_pruned_through";

pub async fn init(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS message_changes (
            seq           INTEGER PRIMARY KEY AUTOINCREMENT,
            kind          TEXT NOT NULL,   -- edit | delete | board | board_gone | author
            message_id    TEXT,
            board_id      TEXT,
            imported_from TEXT,            -- author: whose imported messages
            username      TEXT,            -- author: who they show as now
            at            TEXT NOT NULL
        )",
    ).execute(pool).await?;
    // from: and the list of everyone who wrote, without reading every message.
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_author ON messages (username, board_id, created_at)")
        .execute(pool).await?;
    prune(pool).await;
    Ok(())
}

/// Something changed in messages people may already have a copy of.
pub enum Change<'a> {
    /// A message's text (or files) changed.
    Edit { id: &'a str, board: &'a str },
    Delete { id: &'a str, board: &'a str },
    /// Many messages in this channel changed or appeared in its past (an
    /// import): copies of it should be fetched again.
    Board { board: &'a str },
    /// The channel and its messages are gone.
    BoardGone { board: &'a str },
    /// An imported person's messages now show as `username`.
    Author { imported_from: &'a str, username: &'a str },
}

pub async fn note(pool: &SqlitePool, c: Change<'_>) {
    let (kind, id, board, from, user) = match c {
        Change::Edit { id, board } => ("edit", Some(id), Some(board), None, None),
        Change::Delete { id, board } => ("delete", Some(id), Some(board), None, None),
        Change::Board { board } => ("board", None, Some(board), None, None),
        Change::BoardGone { board } => ("board_gone", None, Some(board), None, None),
        Change::Author { imported_from, username } => ("author", None, None, Some(imported_from), Some(username)),
    };
    let r = sqlx::query(
        "INSERT INTO message_changes (kind, message_id, board_id, imported_from, username, at) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(kind).bind(id).bind(board).bind(from).bind(user)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(pool).await;
    if let Err(e) = r {
        tracing::warn!("couldn't note a {kind} for search copies: {e}");
    }
}

/// Drops changes older than KEEP_DAYS (at most hourly), remembering how far
/// that went so an app behind it knows to start over.
async fn prune(pool: &SqlitePool) {
    static LAST: AtomicI64 = AtomicI64::new(0);
    let now = chrono::Utc::now().timestamp();
    if now - LAST.load(Ordering::Relaxed) < 3600 {
        return;
    }
    LAST.store(now, Ordering::Relaxed);
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(KEEP_DAYS)).to_rfc3339();
    let through: Option<i64> = sqlx::query("SELECT MAX(seq) AS s FROM message_changes WHERE at < ?")
        .bind(&cutoff).fetch_one(pool).await.ok().and_then(|r| r.try_get::<Option<i64>, _>("s").ok()).flatten();
    let Some(through) = through else { return };
    if sqlx::query("DELETE FROM message_changes WHERE seq <= ?").bind(through).execute(pool).await.is_ok() {
        let _ = db::set_config(pool, PRUNED_KEY, &through.to_string()).await;
    }
}

type ApiErr = (StatusCode, Json<serde_json::Value>);
fn db_err() -> ApiErr { (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "DB error" }))) }
fn unauth() -> ApiErr { (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "Unauthorized" }))) }

async fn signed_in(h: &HeaderMap, s: &AppState) -> Result<String, ApiErr> {
    let token = h.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    db::verify_token(&s.pool, token).await.map_err(|_| db_err())?.ok_or_else(unauth)
}

/// The channels whose history you may read: none without Read Message
/// History (as for /api/search/scan).
async fn readable(pool: &SqlitePool, username: &str) -> HashSet<String> {
    if !crate::roles::has(pool, username, crate::roles::READ_HISTORY).await {
        return HashSet::new();
    }
    match crate::access::visible_for(pool, username).await {
        Some(v) => v.boards,
        None => sqlx::query("SELECT id FROM boards").fetch_all(pool).await.unwrap_or_default()
            .iter().map(|r| r.get::<String, _>("id")).collect(),
    }
}

#[derive(Deserialize)]
pub struct ChangesQuery {
    /// The last change number the app has seen; absent to just learn the
    /// current one (before copying history for the first time).
    pub after: Option<i64>,
}

/// GET /api/search/changes?after=N — what changed in the history since
/// change N: `{changes: [...], last, more, reset, boards}`.
///
/// - `changes`, oldest first: `{seq, kind, id?, board_id?, imported_from?,
///   username?, message?}`; an edit carries the message as it is now (as
///   /api/search/scan has it), or none if it's gone again since.
/// - `last`: the newest change number covered; ask again from there.
/// - `more`: there are more after `last` already.
/// - `reset`: changes from that far back are no longer kept — whatever copy
///   the app has can't be brought up to date, so it should start over.
/// - `boards`: the channels whose history you may read now. A copy should
///   drop (or at least not show) any others.
pub async fn changes(
    headers: HeaderMap,
    Query(q): Query<ChangesQuery>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let username = signed_in(&headers, &s).await?;
    prune(&s.pool).await;
    let boards = readable(&s.pool, &username).await;
    let newest: i64 = sqlx::query("SELECT COALESCE(MAX(seq), 0) AS s FROM message_changes")
        .fetch_one(&s.pool).await.map_err(|_| db_err())?.get("s");
    let mut board_list: Vec<&String> = boards.iter().collect();
    board_list.sort();

    let Some(after) = q.after else {
        return Ok(Json(serde_json::json!({
            "changes": [], "last": newest, "more": false, "reset": false, "boards": board_list,
        })));
    };
    let pruned: i64 = db::get_config(&s.pool, PRUNED_KEY).await.ok().flatten()
        .and_then(|v| v.parse().ok()).unwrap_or(0);
    // Behind what's kept, or ahead of what there is (a server that was
    // reset): either way that copy is of something else now.
    if after < pruned || after > newest {
        return Ok(Json(serde_json::json!({
            "changes": [], "last": newest, "more": false, "reset": true, "boards": board_list,
        })));
    }

    let rows = sqlx::query(
        "SELECT seq, kind, message_id, board_id, imported_from, username FROM message_changes
         WHERE seq > ? ORDER BY seq ASC LIMIT ?",
    ).bind(after).bind(CHANGES_MAX + 1).fetch_all(&s.pool).await.map_err(|_| db_err())?;
    let more = rows.len() as i64 > CHANGES_MAX;
    let rows = &rows[..rows.len().min(CHANGES_MAX as usize)];
    let last = rows.last().map(|r| r.get::<i64, _>("seq")).unwrap_or(after);

    // The edited messages as they are now, in one go.
    let edited: Vec<String> = rows.iter()
        .filter(|r| r.get::<String, _>("kind") == "edit")
        .filter_map(|r| r.get::<Option<String>, _>("message_id"))
        .collect::<HashSet<_>>().into_iter().collect();
    let mut now: HashMap<String, serde_json::Value> = HashMap::new();
    for chunk in edited.chunks(500) {
        let sql = format!("{} WHERE m.id IN ({})", crate::messages::SEARCH_SELECT, vec!["?"; chunk.len()].join(","));
        let mut query = sqlx::query(&sql);
        for id in chunk { query = query.bind(id); }
        for r in query.fetch_all(&s.pool).await.map_err(|_| db_err())? {
            let m = crate::messages::row_to_search_result(&r);
            now.insert(m.message.id.clone(), serde_json::to_value(&m).unwrap_or_default());
        }
    }

    let mut out: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
    for r in rows {
        let kind: String = r.get("kind");
        let board: Option<String> = r.get("board_id");
        // Nothing about channels you can't read (an author's new name is
        // fine: it's who they show as everywhere).
        if let Some(b) = &board {
            if !boards.contains(b) && kind != "board_gone" { continue; }
        }
        let id: Option<String> = r.get("message_id");
        let mut c = serde_json::json!({ "seq": r.get::<i64, _>("seq"), "kind": kind });
        if let Some(id) = &id { c["id"] = id.clone().into(); }
        if let Some(b) = &board { c["board_id"] = b.clone().into(); }
        if let Some(f) = r.get::<Option<String>, _>("imported_from") { c["imported_from"] = f.into(); }
        if let Some(u) = r.get::<Option<String>, _>("username") { c["username"] = u.into(); }
        if kind == "edit" {
            if let Some(m) = id.as_ref().and_then(|id| now.get(id)) { c["message"] = m.clone(); }
        }
        out.push(c);
    }
    Ok(Json(serde_json::json!({
        "changes": out, "last": last, "more": more, "reset": false, "boards": board_list,
    })))
}

/// GET /api/search/authors — everyone who has written in a channel you can
/// read, plus every member: `{authors: [{username, messages, last_at,
/// imported, member}]}`, those who wrote most recently first.
/// `imported`: someone whose history was brought over from elsewhere and
/// who hasn't claimed it (so not a member here).
pub async fn authors(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let username = signed_in(&headers, &s).await?;
    let boards: Vec<String> = readable(&s.pool, &username).await.into_iter().collect();

    struct A { name: String, n: i64, last: String, imported: bool, member: bool }
    let mut by_key: HashMap<String, A> = HashMap::new();
    if !boards.is_empty() {
        let sql = format!(
            "SELECT username, COUNT(*) AS n, MAX(created_at) AS last_at, MAX(imported_from IS NOT NULL) AS imp
             FROM messages WHERE board_id IN ({}) AND (kind IS NULL OR kind != 'join')
             GROUP BY username COLLATE NOCASE",
            vec!["?"; boards.len()].join(","),
        );
        let mut query = sqlx::query(&sql);
        for b in &boards { query = query.bind(b); }
        for r in query.fetch_all(&s.pool).await.map_err(|e| { tracing::warn!("search authors: {e}"); db_err() })? {
            let name: String = r.get("username");
            by_key.insert(name.to_lowercase(), A {
                n: r.get("n"),
                last: r.try_get::<Option<String>, _>("last_at").ok().flatten().unwrap_or_default(),
                imported: r.try_get::<i64, _>("imp").unwrap_or(0) != 0,
                member: false,
                name,
            });
        }
    }
    for r in sqlx::query("SELECT username FROM users").fetch_all(&s.pool).await.unwrap_or_default() {
        let name: String = r.get("username");
        by_key.entry(name.to_lowercase())
            .and_modify(|a| a.member = true)
            .or_insert(A { name, n: 0, last: String::new(), imported: false, member: true });
    }
    let mut list: Vec<A> = by_key.into_values().collect();
    list.sort_by(|a, b| b.last.cmp(&a.last).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    let out: Vec<serde_json::Value> = list.into_iter().map(|a| serde_json::json!({
        "username": a.name,
        "messages": a.n,
        "last_at": if a.last.is_empty() { serde_json::Value::Null } else { a.last.into() },
        "imported": a.imported && !a.member,
        "member": a.member,
    })).collect();
    Ok(Json(serde_json::json!({ "authors": out })))
}
