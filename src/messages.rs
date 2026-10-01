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
    /// Words to find (may be empty when only filters are used). "quoted
    /// phrases" are matched as a phrase.
    #[serde(default)]
    pub q: String,
    /// from: — exact username (case-insensitive)
    pub from: Option<String>,
    /// has: — comma-separated: link, image, video, audio, file
    pub has: Option<String>,
    /// in: — a board (channel) id
    #[serde(rename = "in")]
    pub in_board: Option<String>,
    /// date range, as RFC 3339 instants: after is inclusive, before exclusive
    pub after: Option<String>,
    pub before: Option<String>,
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
    /// Whether the message is pinned in its board. `default` so any JSON
    /// produced before this field existed still deserializes.
    #[serde(default)]
    pub pinned:      bool,
    /// Emoji reactions, in the order each emoji was first added.
    #[serde(default)]
    pub reactions:   Vec<Reaction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reaction {
    pub emoji: String,
    pub count: usize,
    /// Who reacted, in order — lets clients show "you reacted" and a
    /// hover list of names without another request.
    pub users: Vec<String>,
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
        pinned:      r.try_get::<Option<String>, _>("pinned_at").ok().flatten().is_some(),
        reactions:   Vec::new(), // filled in by attach_reactions
    }
}

const SELECT: &str =
    "SELECT id, board_id, username, content,
            attachment_url, attachment_name, attachment_mime,
            attachments, edited, created_at, pinned_at
     FROM messages";

// Same columns as SELECT above, plus the joined board's name and room —
// used only by search_messages, which spans every board.
const SEARCH_SELECT: &str =
    "SELECT m.id, m.board_id, m.username, m.content,
            m.attachment_url, m.attachment_name, m.attachment_mime,
            m.attachments, m.edited, m.created_at, m.pinned_at,
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

    let mut msgs: Vec<ChatMessage> = rows.iter().map(row_to_msg).collect();
    attach_reactions(&s.pool, &mut msgs).await;
    Ok(Json(msgs))
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
    attach_reactions(&s.pool, &mut result).await;

    Ok(Json(result))
}

/// Searches every board on this host (or one, with in:), combining the
/// text query with Discord-style filters: from:, has:, in: and a date range.
/// Text matching uses the FTS5 full-text index (whole words, ignoring case
/// and accents); filters are plain SQL conditions on the same query. At
/// least one of text or a filter is needed. Newest SEARCH_LIMIT matches,
/// returned in chronological order like get_messages.
pub async fn search_messages(
    headers:  HeaderMap,
    Query(q): Query<SearchQuery>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<Vec<SearchResult>>, ApiErr> {
    db::verify_token(&s.pool, token_from(&headers)).await
        .map_err(|_| db_err())?.ok_or_else(unauth)?;

    let term = q.q.trim().to_string();
    let mut wheres: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    let use_fts = db::FTS_AVAILABLE.load(std::sync::atomic::Ordering::Relaxed);

    if !term.is_empty() {
        if use_fts {
            match fts_query(&term) {
                Some(m) => {
                    wheres.push("m.rowid IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?)".into());
                    binds.push(m);
                }
                None => return Ok(Json(vec![])), // nothing searchable in it (only punctuation)
            }
        } else {
            let escaped = term.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            wheres.push("m.content LIKE ? ESCAPE '\\'".into());
            binds.push(format!("%{escaped}%"));
        }
    }
    if let Some(from) = q.from.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        wheres.push("m.username = ? COLLATE NOCASE".into());
        binds.push(from.to_string());
    }
    if let Some(board) = q.in_board.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
        wheres.push("m.board_id = ?".into());
        binds.push(board.to_string());
    }
    for kind in q.has.as_deref().unwrap_or("").split(',').map(|k| k.trim().to_lowercase()).filter(|k| !k.is_empty()) {
        let mime = |prefix: &str| format!(
            "(m.attachments LIKE '%\"mime\":\"{prefix}/%' OR m.attachment_mime LIKE '{prefix}/%')"
        );
        let cond = match kind.as_str() {
            "link" => "(m.content LIKE '%http://%' OR m.content LIKE '%https://%')".to_string(),
            "image" => mime("image"),
            "video" => mime("video"),
            "audio" | "sound" => mime("audio"),
            "file" | "attachment" =>
                "((m.attachments IS NOT NULL AND m.attachments NOT IN ('', '[]')) OR m.attachment_url IS NOT NULL)".to_string(),
            other => return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "error": format!("Unknown has: filter \"{other}\" — use link, image, video, audio or file")
            })))),
        };
        wheres.push(cond);
    }
    if let Some(after) = q.after.as_deref().filter(|d| !d.is_empty()) {
        wheres.push("julianday(m.created_at) >= julianday(?)".into());
        binds.push(after.to_string());
    }
    if let Some(before) = q.before.as_deref().filter(|d| !d.is_empty()) {
        wheres.push("julianday(m.created_at) < julianday(?)".into());
        binds.push(before.to_string());
    }
    if wheres.is_empty() {
        return Ok(Json(vec![]));
    }

    // The LIKE fallback can't tell words from substrings, so it fetches
    // everything and filters in Rust (the old behaviour); FTS needs neither.
    let limit = if use_fts || term.is_empty() { format!(" LIMIT {SEARCH_LIMIT}") } else { String::new() };
    let sql = format!("{} WHERE {} ORDER BY m.id DESC{limit}", SEARCH_SELECT, wheres.join(" AND "));
    let mut query = sqlx::query(&sql);
    for b in &binds {
        query = query.bind(b);
    }
    let rows = query.fetch_all(&s.pool).await.map_err(|e| {
        tracing::warn!("search failed: {e}");
        db_err()
    })?;

    let mut matches: Vec<SearchResult> = rows.iter()
        .map(row_to_search_result)
        .filter(|r| use_fts || term.is_empty() || contains_whole_word(&r.message.content, &term))
        .take(SEARCH_LIMIT)
        .collect();
    matches.reverse(); // newest-first -> chronological, matching get_messages
    let mut msgs: Vec<ChatMessage> = matches.iter().map(|m| m.message.clone()).collect();
    attach_reactions(&s.pool, &mut msgs).await;
    for (m, with) in matches.iter_mut().zip(msgs) { m.message.reactions = with.reactions; }

    Ok(Json(matches))
}

/// Turns typed text into an FTS5 query: every word must appear (in any
/// order), "quoted text" must appear as a phrase. Each piece is quoted so
/// FTS5 operators/punctuation in what people type can't break the query.
/// None if nothing searchable is left.
fn fts_query(text: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() { break; }
        let (piece, next) = if let Some(after_quote) = rest.strip_prefix('"') {
            match after_quote.find('"') {
                Some(end) => (&after_quote[..end], &after_quote[end + 1..]),
                None => (after_quote, ""),
            }
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            (&rest[..end], &rest[end..])
        };
        rest = next;
        // Keep letters, digits and inner spaces (for phrases); everything
        // else just separates words, as the tokenizer would.
        let cleaned: String = piece.chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect();
        let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
        if !cleaned.is_empty() {
            parts.push(format!("\"{cleaned}\""));
        }
    }
    if parts.is_empty() { None } else { Some(parts.join(" ")) }
}

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

    let msg = ChatMessage { id, board_id, username, content, attachments, edited: false, created_at: now, pinned: false, reactions: Vec::new() };
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

    let row = sqlx::query("SELECT username, board_id, attachments FROM messages WHERE id = ?")
        .bind(&id).fetch_optional(&s.pool).await.map_err(|_| db_err())?
        .ok_or_else(not_found)?;

    if row.get::<String, _>("username") != username { return Err(forbidden()); }
    // Empty text is fine on a message that still has attachments (an
    // image-only message, like when it was first sent); a message with
    // neither would be blank, so that's still refused.
    if content.is_empty() {
        let attachments: Option<String> = row.try_get("attachments").ok().flatten();
        let has_attachments = attachments
            .and_then(|a| serde_json::from_str::<Vec<serde_json::Value>>(&a).ok())
            .is_some_and(|v| !v.is_empty());
        if !has_attachments {
            return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "Content cannot be empty" }))));
        }
    }
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
    let _ = sqlx::query("DELETE FROM reactions WHERE message_id = ?").bind(&id).execute(&s.pool).await;

    let _ = s.tx.send(serde_json::json!({
        "type": "message_delete", "id": id, "board_id": board_id,
    }).to_string());
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── Pins ──────────────────────────────────────────────────────────────────────
//
// Any member can pin or unpin any message: chriscord has no per-member
// permissions yet (the admin panel is the only elevated role), and on a
// small friends' server that matches how pins get used. If roles are added
// later, this is the one check to tighten.

/// GET /api/boards/:id/pins — the board's pinned messages, most recently
/// pinned first (Discord's order).
pub async fn get_pins(
    headers:        HeaderMap,
    Path(board_id): Path<String>,
    State(s):       State<Arc<AppState>>,
) -> Result<Json<Vec<ChatMessage>>, ApiErr> {
    db::verify_token(&s.pool, token_from(&headers)).await
        .map_err(|_| db_err())?.ok_or_else(unauth)?;
    let rows = sqlx::query(&format!(
        "{} WHERE board_id = ? AND pinned_at IS NOT NULL ORDER BY pinned_at DESC", SELECT
    )).bind(&board_id).fetch_all(&s.pool).await.map_err(|_| db_err())?;
    let mut msgs: Vec<ChatMessage> = rows.iter().map(row_to_msg).collect();
    attach_reactions(&s.pool, &mut msgs).await;
    Ok(Json(msgs))
}

/// PUT /api/messages/:id/pin — pins it (idempotent: re-pinning keeps the
/// original pin time rather than bumping it to the top).
pub async fn pin_message(
    headers:  HeaderMap,
    Path(id): Path<String>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    set_pinned(&s, &headers, &id, true).await
}

/// DELETE /api/messages/:id/pin — unpins it.
pub async fn unpin_message(
    headers:  HeaderMap,
    Path(id): Path<String>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    set_pinned(&s, &headers, &id, false).await
}

async fn set_pinned(s: &AppState, headers: &HeaderMap, id: &str, pinned: bool) -> Result<Json<serde_json::Value>, ApiErr> {
    let username = db::verify_token(&s.pool, token_from(headers)).await
        .map_err(|_| db_err())?.ok_or_else(unauth)?;
    let row = sqlx::query("SELECT board_id FROM messages WHERE id = ?")
        .bind(id).fetch_optional(&s.pool).await.map_err(|_| db_err())?
        .ok_or_else(not_found)?;
    let board_id: String = row.get("board_id");

    if pinned {
        sqlx::query("UPDATE messages SET pinned_at = ?, pinned_by = ? WHERE id = ? AND pinned_at IS NULL")
            .bind(chrono::Utc::now().to_rfc3339()).bind(&username).bind(id)
            .execute(&s.pool).await.map_err(|_| db_err())?;
    } else {
        sqlx::query("UPDATE messages SET pinned_at = NULL, pinned_by = NULL WHERE id = ?")
            .bind(id).execute(&s.pool).await.map_err(|_| db_err())?;
    }

    let _ = s.tx.send(serde_json::json!({
        "type": "message_pin", "id": id, "board_id": board_id, "pinned": pinned, "by": username,
    }).to_string());
    Ok(Json(serde_json::json!({ "ok": true, "pinned": pinned })))
}

// ── Reactions ─────────────────────────────────────────────────────────────────

/// Most distinct emoji one message can collect — well past what any real
/// conversation uses, but bounded so a script can't grow one row forever.
pub const MAX_REACTION_EMOJI: usize = 50;

/// Fills in `reactions` for a batch of messages with one query.
pub async fn attach_reactions(pool: &sqlx::SqlitePool, msgs: &mut [ChatMessage]) {
    if msgs.is_empty() {
        return;
    }
    let placeholders = vec!["?"; msgs.len()].join(",");
    let sql = format!(
        "SELECT message_id, emoji, username FROM reactions WHERE message_id IN ({placeholders})
         ORDER BY message_id, created_at, rowid"
    );
    let mut q = sqlx::query(&sql);
    for m in msgs.iter() {
        q = q.bind(&m.id);
    }
    let rows = match q.fetch_all(pool).await {
        Ok(r) => r,
        Err(e) => { tracing::warn!("reactions: load failed: {e}"); return; }
    };
    let mut by_msg: std::collections::HashMap<String, Vec<Reaction>> = std::collections::HashMap::new();
    for r in &rows {
        let list = by_msg.entry(r.get("message_id")).or_default();
        let emoji: String = r.get("emoji");
        let user: String = r.get("username");
        match list.iter_mut().find(|x| x.emoji == emoji) {
            Some(x) => { x.count += 1; x.users.push(user); }
            None => list.push(Reaction { emoji, count: 1, users: vec![user] }),
        }
    }
    for m in msgs.iter_mut() {
        if let Some(list) = by_msg.remove(&m.id) {
            m.reactions = list;
        }
    }
}

/// A reaction must look like a single emoji: short, no whitespace or
/// control characters. The client only ever sends picker emoji; this just
/// keeps arbitrary text out of the table.
fn valid_reaction_emoji(e: &str) -> bool {
    !e.is_empty() && e.len() <= 64 && e.chars().count() <= 16
        && !e.chars().any(|c| c.is_whitespace() || c.is_control() || c.is_ascii_alphanumeric())
}

#[derive(Deserialize)]
pub struct ReactBody {
    pub emoji: String,
    /// true = add your reaction, false = remove it.
    pub on:    bool,
}

/// POST /api/messages/:id/reactions {emoji, on} — adds or removes your
/// reaction, then broadcasts the change to everyone viewing that channel.
pub async fn react(
    headers:  HeaderMap,
    Path(id): Path<String>,
    State(s): State<Arc<AppState>>,
    Json(body): Json<ReactBody>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let username = db::verify_token(&s.pool, token_from(&headers)).await
        .map_err(|_| db_err())?.ok_or_else(unauth)?;
    let emoji = body.emoji.trim().to_string();
    if !valid_reaction_emoji(&emoji) {
        return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "Not a valid reaction" }))));
    }
    let row = sqlx::query("SELECT board_id FROM messages WHERE id = ?")
        .bind(&id).fetch_optional(&s.pool).await.map_err(|_| db_err())?
        .ok_or_else(not_found)?;
    let board_id: String = row.get("board_id");

    let changed = if body.on {
        // A brand-new emoji on this message counts against the cap; adding
        // yourself to an existing one never does.
        let exists = sqlx::query("SELECT 1 FROM reactions WHERE message_id = ? AND emoji = ? LIMIT 1")
            .bind(&id).bind(&emoji).fetch_optional(&s.pool).await.map_err(|_| db_err())?.is_some();
        if !exists {
            let distinct: i64 = sqlx::query("SELECT COUNT(DISTINCT emoji) AS n FROM reactions WHERE message_id = ?")
                .bind(&id).fetch_one(&s.pool).await.map_err(|_| db_err())?.get("n");
            if distinct as usize >= MAX_REACTION_EMOJI {
                return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({
                    "error": format!("This message already has {MAX_REACTION_EMOJI} different reactions")
                }))));
            }
        }
        sqlx::query("INSERT OR IGNORE INTO reactions (message_id, emoji, username, created_at) VALUES (?, ?, ?, ?)")
            .bind(&id).bind(&emoji).bind(&username).bind(chrono::Utc::now().to_rfc3339())
            .execute(&s.pool).await.map_err(|_| db_err())?.rows_affected() > 0
    } else {
        sqlx::query("DELETE FROM reactions WHERE message_id = ? AND emoji = ? AND username = ?")
            .bind(&id).bind(&emoji).bind(&username)
            .execute(&s.pool).await.map_err(|_| db_err())?.rows_affected() > 0
    };

    if changed {
        let _ = s.tx.send(serde_json::json!({
            "type": "message_reaction", "id": id, "board_id": board_id,
            "emoji": emoji, "username": username, "on": body.on,
        }).to_string());
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}
