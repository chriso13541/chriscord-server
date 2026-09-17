use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::sync::Arc;

use crate::{db, state::AppState};

// Page size for message history, both the initial page (ws.rs's load_history)
// and each "load older" page below. Kept as one constant so both delivery
// paths always agree; the client mirrors this number (HISTORY_PAGE in
// index.html) to know whether a page it received was the last one — if you
// change this, update that too.
pub const HISTORY_PAGE: i64 = 50;
// Cap on search results per query — a plain LIKE scan with no way to rank
// relevance, so this just bounds worst-case response size, not "top N".
const SEARCH_LIMIT: usize = 50;

#[derive(Deserialize)]
pub struct MessagesQuery {
    /// Message id (UUIDv7) to page backward from. Omitted = most recent page.
    pub before: Option<String>,
}

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Attachment {
    pub url:  String,
    pub name: String,
    pub mime: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    pub id:          String,
    pub board_id:    String,
    pub username:    String,
    pub content:     String,
    /// Multi-attachment support. Replaces the old single attachment_url/name/mime columns.
    /// Old messages with the single columns are migrated on read via row_to_msg.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    pub edited:      bool,
    pub created_at:  String,
}

/// A ChatMessage plus which board (and its room) it came from — only
/// meaningful once search spans every channel instead of being scoped to
/// whichever one you're currently viewing. #[serde(flatten)] keeps the JSON
/// shape identical to ChatMessage with two extra fields, rather than nesting.
#[derive(Serialize)]
pub struct SearchResult {
    #[serde(flatten)]
    pub message:    ChatMessage,
    pub board_name: String,
    pub room_id:    String,
}

#[derive(Deserialize)]
pub struct SendMsgReq {
    pub content:     Option<String>,
    pub attachments: Option<Vec<Attachment>>,
}

#[derive(Deserialize)]
pub struct EditMsgReq {
    pub content: String,
}

type ApiErr = (StatusCode, Json<serde_json::Value>);
fn db_err()   -> ApiErr { (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "DB error" }))) }
fn unauth()   -> ApiErr { (StatusCode::UNAUTHORIZED,          Json(serde_json::json!({ "error": "Unauthorized" }))) }
fn not_found()-> ApiErr { (StatusCode::NOT_FOUND,             Json(serde_json::json!({ "error": "Message not found" }))) }
fn forbidden()-> ApiErr { (StatusCode::FORBIDDEN,             Json(serde_json::json!({ "error": "You can only edit/delete your own messages" }))) }

fn token_from(h: &HeaderMap) -> &str {
    h.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("")
}

/// Convert a DB row to a ChatMessage, handling both old single-attachment and
/// new multi-attachment (JSON) rows transparently.
pub fn row_to_msg(r: &sqlx::sqlite::SqliteRow) -> ChatMessage {
    // Try new `attachments` JSON column first
    let attachments: Vec<Attachment> = r
        .try_get::<Option<String>, _>("attachments")
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_else(|| {
            // Fall back to old single-attachment columns for backward compat
            let url:  Option<String> = r.try_get("attachment_url").ok().flatten();
            let name: Option<String> = r.try_get("attachment_name").ok().flatten();
            let mime: Option<String> = r.try_get("attachment_mime").ok().flatten();
            match url {
                Some(u) => vec![Attachment {
                    url:  u,
                    name: name.unwrap_or_default(),
                    mime: mime.unwrap_or_default(),
                }],
                None => vec![],
            }
        });

    ChatMessage {
        id:          r.get("id"),
        board_id:    r.get("board_id"),
        username:    r.get("username"),
        content:     r.get("content"),
        attachments,
        edited:      r.try_get::<i64, _>("edited").unwrap_or(0) != 0,
        created_at:  r.get("created_at"),
    }
}

const SELECT: &str =
    "SELECT id, board_id, username, content,
            attachment_url, attachment_name, attachment_mime,
            attachments, edited, created_at
     FROM messages";

// Same columns as SELECT above, plus the joined board's name and room —
// used only by search_messages, which spans every board.
const SEARCH_SELECT: &str =
    "SELECT m.id, m.board_id, m.username, m.content,
            m.attachment_url, m.attachment_name, m.attachment_mime,
            m.attachments, m.edited, m.created_at,
            b.name AS board_name, b.room_id AS room_id
     FROM messages m JOIN boards b ON m.board_id = b.id";

fn row_to_search_result(r: &sqlx::sqlite::SqliteRow) -> SearchResult {
    SearchResult {
        message:    row_to_msg(r),
        board_name: r.get("board_name"),
        room_id:    r.get("room_id"),
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn get_messages(
    headers:        HeaderMap,
    Path(board_id): Path<String>,
    Query(q):       Query<MessagesQuery>,
    State(s):       State<Arc<AppState>>,
) -> Result<Json<Vec<ChatMessage>>, ApiErr> {
    db::verify_token(&s.pool, token_from(&headers)).await
        .map_err(|_| db_err())?.ok_or_else(unauth)?;

    // Ordering by id (UUIDv7) rather than created_at: UUIDv7 embeds its
    // timestamp as the leading bytes specifically so lexicographic string
    // order matches chronological order, but unlike created_at it's also
    // guaranteed unique — safe to use as a paging cursor with no risk of two
    // messages landing on the same instant and one getting skipped or
    // duplicated across pages.
    let mut rows = match &q.before {
        Some(cursor) => sqlx::query(
            &format!("{} WHERE board_id = ? AND id < ? ORDER BY id DESC LIMIT ?", SELECT)
        ).bind(&board_id).bind(cursor).bind(HISTORY_PAGE)
         .fetch_all(&s.pool).await.map_err(|_| db_err())?,
        None => sqlx::query(
            &format!("{} WHERE board_id = ? ORDER BY id DESC LIMIT ?", SELECT)
        ).bind(&board_id).bind(HISTORY_PAGE)
         .fetch_all(&s.pool).await.map_err(|_| db_err())?,
    };
    // Rows come back newest-first (for an efficient indexed LIMIT); flip
    // back to chronological order before handing them to the client.
    rows.reverse();

    Ok(Json(rows.iter().map(row_to_msg).collect()))
}

/// Fetches a window of messages centered on a specific one — half the page
/// size on either side. This is what makes "jump to message" from search
/// practical: without it, reaching a message from months ago would mean
/// paging backward through history one HISTORY_PAGE chunk at a time until
/// stumbling onto it, which could be dozens of round trips. The target
/// message's own id doesn't need to still exist as a row — the id < / <=
/// comparisons work on the value regardless, so this degrades gracefully
/// if the message was deleted after being found by search: the surrounding
/// conversation still loads correctly, just with nothing to highlight.
pub async fn get_messages_around(
    headers:  HeaderMap,
    Path((board_id, message_id)): Path<(String, String)>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<Vec<ChatMessage>>, ApiErr> {
    db::verify_token(&s.pool, token_from(&headers)).await
        .map_err(|_| db_err())?.ok_or_else(unauth)?;

    const AROUND_HALF: i64 = HISTORY_PAGE / 2;

    // Up to and including the target, newest-first for an efficient LIMIT,
    // then flipped back to chronological order.
    let mut before_and_target = sqlx::query(
        &format!("{} WHERE board_id = ? AND id <= ? ORDER BY id DESC LIMIT ?", SELECT)
    ).bind(&board_id).bind(&message_id).bind(AROUND_HALF + 1)
     .fetch_all(&s.pool).await.map_err(|_| db_err())?;
    before_and_target.reverse();

    // Strictly after the target, already in chronological order.
    let after = sqlx::query(
        &format!("{} WHERE board_id = ? AND id > ? ORDER BY id ASC LIMIT ?", SELECT)
    ).bind(&board_id).bind(&message_id).bind(AROUND_HALF)
     .fetch_all(&s.pool).await.map_err(|_| db_err())?;

    let mut result: Vec<ChatMessage> = before_and_target.iter().map(row_to_msg).collect();
    result.extend(after.iter().map(row_to_msg));

    Ok(Json(result))
}

/// Searches every board on this host, not just one channel — "rooms" here
/// are Discord-style categories inside a single server, not separate
/// joinable spaces, so a server-wide search is the natural default. Per-
/// channel scoping comes back later as the in: filter, applied on top of
/// this same query rather than as a separate endpoint.
pub async fn search_messages(
    headers:  HeaderMap,
    Query(q): Query<SearchQuery>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<Vec<SearchResult>>, ApiErr> {
    db::verify_token(&s.pool, token_from(&headers)).await
        .map_err(|_| db_err())?.ok_or_else(unauth)?;

    let term = q.q.trim();
    if term.is_empty() {
        return Ok(Json(vec![]));
    }
    // Escape SQL LIKE wildcards in the user's query so someone searching for
    // a literal "%" or "_" gets literal matches, not wildcard behaviour.
    let escaped = term.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    let pattern = format!("%{}%", escaped);

    // Stage 1: a cheap SQL substring pre-filter across every board. No
    // index can serve a leading-wildcard LIKE, so this is a full scan of
    // the host's messages every search — fine at thousands of messages;
    // SQLite's FTS5 extension is the documented next step if that changes.
    // No LIMIT here: stage 2 below can shrink the candidate set
    // unpredictably (a substring hit isn't necessarily a whole-word hit),
    // so truncating has to happen after filtering, not before.
    let rows = sqlx::query(
        &format!("{} WHERE m.content LIKE ? ESCAPE '\\' ORDER BY m.id DESC", SEARCH_SELECT)
    ).bind(&pattern)
     .fetch_all(&s.pool).await.map_err(|_| db_err())?;

    // Stage 2: keep only messages where the term appears as a standalone
    // word/phrase — bounded by non-alphanumeric characters or the string's
    // edges — not merely as a substring. This is the whole reason searching
    // "hi" shouldn't return a message that only says "this".
    let mut matches: Vec<SearchResult> = rows.iter()
        .map(row_to_search_result)
        .filter(|r| contains_whole_word(&r.message.content, term))
        .take(SEARCH_LIMIT)
        .collect();
    matches.reverse(); // newest-first -> chronological, matching get_messages

    Ok(Json(matches))
}

/// True if `term` appears in `content` as a standalone word or phrase,
/// rather than merely as a substring — "hi" matches a message that says
/// "hi" but not one that only says "this". A match counts if the character
/// immediately before and after it (if any) is not alphanumeric, so this
/// also works for a multi-word term as an implicit phrase match (the words
/// must be adjacent, in order) without needing separate quote syntax.
/// ASCII-only case folding, matching the LIKE pre-filter's own case
/// behaviour above (SQLite's default LIKE only case-folds ASCII).
fn contains_whole_word(content: &str, term: &str) -> bool {
    let content = content.to_ascii_lowercase();
    let term = term.to_ascii_lowercase();
    if term.is_empty() {
        return false;
    }
    let mut start = 0usize;
    while let Some(rel) = content[start..].find(term.as_str()) {
        let pos = start + rel;
        let before_ok = content[..pos].chars().next_back().map_or(true, |c| !c.is_alphanumeric());
        let after_ok = content[pos + term.len()..].chars().next().map_or(true, |c| !c.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        // Advance past exactly one character (byte-length-safe for UTF-8) to
        // look for the next occurrence rather than stopping at the first.
        let advance = content[pos..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        start = pos + advance;
    }
    false
}

pub async fn post_message(
    headers:        HeaderMap,
    Path(board_id): Path<String>,
    State(s):       State<Arc<AppState>>,
    Json(body):     Json<SendMsgReq>,
) -> Result<Json<ChatMessage>, ApiErr> {
    let username = db::verify_token(&s.pool, token_from(&headers)).await
        .map_err(|_| db_err())?.ok_or_else(unauth)?;

    let content     = body.content.as_deref().unwrap_or("").trim().to_string();
    let attachments = body.attachments.unwrap_or_default();

    if content.is_empty() && attachments.is_empty() {
        return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "Must have content or attachment" }))));
    }

    let id              = uuid::Uuid::now_v7().to_string();
    let now             = chrono::Utc::now().to_rfc3339();
    let attachments_json = serde_json::to_string(&attachments).unwrap_or_default();

    sqlx::query(
        "INSERT INTO messages (id, board_id, username, content, attachments, edited, created_at)
         VALUES (?, ?, ?, ?, ?, 0, ?)",
    )
    .bind(&id).bind(&board_id).bind(&username).bind(&content)
    .bind(&attachments_json).bind(&now)
    .execute(&s.pool).await.map_err(|_| db_err())?;

    let msg = ChatMessage { id, board_id, username, content, attachments, edited: false, created_at: now };
    let _ = s.tx.send(serde_json::json!({ "type": "message", "data": msg }).to_string());
    Ok(Json(msg))
}

pub async fn edit_message(
    headers:    HeaderMap,
    Path(id):   Path<String>,
    State(s):   State<Arc<AppState>>,
    Json(body): Json<EditMsgReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let username = db::verify_token(&s.pool, token_from(&headers)).await
        .map_err(|_| db_err())?.ok_or_else(unauth)?;

    let content = body.content.trim().to_string();
    if content.is_empty() {
        return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "Content cannot be empty" }))));
    }

    let row = sqlx::query("SELECT username, board_id FROM messages WHERE id = ?")
        .bind(&id).fetch_optional(&s.pool).await.map_err(|_| db_err())?
        .ok_or_else(not_found)?;

    if row.get::<String, _>("username") != username { return Err(forbidden()); }
    let board_id: String = row.get("board_id");

    sqlx::query("UPDATE messages SET content = ?, edited = 1 WHERE id = ?")
        .bind(&content).bind(&id).execute(&s.pool).await.map_err(|_| db_err())?;

    let _ = s.tx.send(serde_json::json!({
        "type": "message_edit", "id": id, "board_id": board_id, "content": content,
    }).to_string());
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn delete_message(
    headers:  HeaderMap,
    Path(id): Path<String>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let username = db::verify_token(&s.pool, token_from(&headers)).await
        .map_err(|_| db_err())?.ok_or_else(unauth)?;

    let row = sqlx::query("SELECT username, board_id FROM messages WHERE id = ?")
        .bind(&id).fetch_optional(&s.pool).await.map_err(|_| db_err())?
        .ok_or_else(not_found)?;

    if row.get::<String, _>("username") != username { return Err(forbidden()); }
    let board_id: String = row.get("board_id");

    sqlx::query("DELETE FROM messages WHERE id = ?").bind(&id).execute(&s.pool).await.map_err(|_| db_err())?;

    let _ = s.tx.send(serde_json::json!({
        "type": "message_delete", "id": id, "board_id": board_id,
    }).to_string());
    Ok(Json(serde_json::json!({ "ok": true })))
}
