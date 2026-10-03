// invites.rs — short-lived invite links.
//
// A member makes a link in the app (server info → Invite people). It looks
// like https://<server>/invite/<code>; whoever has it can join the server
// without the join key — the key itself is never part of the link — until
// it expires INVITE_TTL after it was made. Opened in a browser, the link
// shows a small page saying how to use it; pasted into the app's Add
// Server box, or clicked in a chat, it joins.
//
// Making invites will get its own role permission later; for now any
// member can (see create below).

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    Json,
};
use sqlx::{Row, SqlitePool};
use std::sync::Arc;

use crate::{db, state::AppState};

type ApiErr = (StatusCode, Json<serde_json::Value>);

/// How long an invite link works.
pub const INVITE_TTL_MINUTES: i64 = 10;

pub async fn init(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS invites (
            code       TEXT PRIMARY KEY,
            created_by TEXT NOT NULL,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            uses       INTEGER NOT NULL DEFAULT 0
        )",
    ).execute(pool).await?;
    Ok(())
}

/// A short code that's easy to send: 10 letters/digits (~59 bits) — plenty
/// for something that only works for a few minutes.
fn new_code() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    (0..10).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect()
}

/// Whether an invite code is real and hasn't expired.
pub async fn is_valid(pool: &SqlitePool, code: &str) -> bool {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query("SELECT 1 FROM invites WHERE code = ? AND expires_at > ?")
        .bind(code).bind(&now).fetch_optional(pool).await.ok().flatten().is_some()
}

/// Counts a successful join through an invite.
pub async fn record_use(pool: &SqlitePool, code: &str) {
    let _ = sqlx::query("UPDATE invites SET uses = uses + 1 WHERE code = ?").bind(code).execute(pool).await;
}

/// POST /api/invites — make an invite. Returns {code, expires_at, minutes};
/// the app builds the full link from the server address it's connected to.
pub async fn create(headers: HeaderMap, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    let token = headers.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let username = db::verify_token(&s.pool, token).await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "DB error" }))))?
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "Unauthorized" }))))?;
    // (Later: crate::roles::require(&s.pool, &username, crate::roles::CREATE_INVITES, "create invite links").await?;)

    let now = chrono::Utc::now();
    let expires = now + chrono::Duration::minutes(INVITE_TTL_MINUTES);
    // Tidy up expired ones while we're here.
    let _ = sqlx::query("DELETE FROM invites WHERE expires_at <= ?").bind(now.to_rfc3339()).execute(&s.pool).await;
    let code = new_code();
    sqlx::query("INSERT INTO invites (code, created_by, created_at, expires_at) VALUES (?, ?, ?, ?)")
        .bind(&code).bind(&username).bind(now.to_rfc3339()).bind(expires.to_rfc3339())
        .execute(&s.pool).await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "DB error" }))))?;
    tracing::info!("invites: {username} made an invite (expires {})", expires.to_rfc3339());
    Ok(Json(serde_json::json!({ "code": code, "expires_at": expires.to_rfc3339(), "minutes": INVITE_TTL_MINUTES })))
}

/// GET /invite/:code — what someone sees opening the link in a browser.
pub async fn landing(Path(code): Path<String>, State(s): State<Arc<AppState>>) -> impl IntoResponse {
    let name = db::get_config(&s.pool, "server_name").await.ok().flatten().unwrap_or_else(|| "a chriscord server".into());
    let row = sqlx::query("SELECT expires_at FROM invites WHERE code = ?").bind(&code).fetch_optional(&s.pool).await.ok().flatten();
    let valid = row.as_ref().map_or(false, |r| r.get::<String, _>("expires_at") > chrono::Utc::now().to_rfc3339());
    let esc = |t: &str| t.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let body = if valid {
        format!("<h1>You're invited to <b>{}</b></h1>
<p>Open <b>chriscord</b>, choose <b>Add a Server</b>, and paste this page's link into the address box — or click the link in a chriscord chat.</p>
<p class=\"small\">This invite works for a few minutes after it was made, so use it soon.</p>", esc(&name))
    } else {
        "<h1>This invite has expired</h1><p>Invite links only work for a few minutes. Ask whoever sent it for a new one.</p>".to_string()
    };
    Html(format!("<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">
<title>chriscord invite</title><style>
body{{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;background:#0e1117;color:#dcddde;font:16px/1.5 'Segoe UI',system-ui,sans-serif}}
main{{max-width:460px;margin:24px;padding:28px 32px;background:#1e2128;border:1px solid #2a2e37;border-radius:12px}}
h1{{font-size:1.3rem;margin:0 0 12px;color:#fff}} b{{color:#fff}} .small{{color:#8e9297;font-size:.85rem}}
</style></head><body><main>{body}</main></body></html>"))
}
