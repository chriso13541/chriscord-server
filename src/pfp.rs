// pfp.rs — server-side profile picture caching.
//
// Each user's uploaded picture is cached on disk as pfps/<username>.png,
// alongside a small sidecar pfps/<username>.ts holding the client-reported
// Unix timestamp of when that picture was last changed. That sidecar is
// what lets the server tell a connecting client "I already have your
// current picture, no need to re-upload" without a database migration or
// any in-memory record that wouldn't survive a restart — the timestamp
// lives right next to the file it describes, so a fresh server process
// reads exactly the same answer an already-running one would have given.
//
// The actual upload/request handshake lives in ws.rs, alongside every
// other signaling message this project already has: a client reports its
// own timestamp once on connecting (or right after changing its picture),
// the server compares that against what it has cached and asks for the
// picture only if it's missing or the timestamp differs, and once a
// picture is (re)cached the server broadcasts that to everyone so anyone
// currently displaying that user's avatar knows to refetch it. This file
// only owns the on-disk cache itself and the one HTTP route other clients
// actually fetch pictures through — GET /api/pfp/:username, gated behind
// the same session-token header every other route here uses.
//
// This is a new, self-contained module rather than an extension of
// files.rs (which already serves uploaded attachments at /api/files) —
// that file isn't in reach to safely extend here, and the two caches are
// unrelated anyway: attachments are user-supplied content tied to a
// message, this is a small, frequently-fetched, per-user cache with its
// own freshness-check protocol.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use std::path::PathBuf;
use std::sync::Arc;

use crate::state::AppState;

fn token_from(h: &HeaderMap) -> &str {
    h.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("")
}

/// The on-disk cache directory, created on first use if it doesn't exist
/// yet — same relative-to-cwd convention as chriscord.db itself.
pub fn pfps_dir() -> PathBuf {
    let dir = PathBuf::from("./pfps");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Usernames are already constrained at account-creation time elsewhere in
/// this project, but this guards against ever writing outside pfps/ from
/// a malformed value reaching this file directly — no path separators, no
/// leading dot (which could otherwise reach a dotfile or, with "..",
/// escape the directory entirely).
fn sanitize_username(username: &str) -> Option<String> {
    if username.is_empty()
        || username.contains('/')
        || username.contains('\\')
        || username.starts_with('.')
    {
        return None;
    }
    Some(username.to_string())
}

/// The Unix timestamp the server currently has cached for this user's
/// picture, if any — None if there's no cached picture at all yet, which
/// is the normal state for a user who's never uploaded one.
pub fn cached_timestamp(username: &str) -> Option<i64> {
    let username = sanitize_username(username)?;
    let ts_path = pfps_dir().join(format!("{username}.ts"));
    std::fs::read_to_string(ts_path).ok()?.trim().parse().ok()
}

/// Writes a new cached picture and its timestamp, overwriting whatever
/// was cached for this user before.
pub fn save_cached(username: &str, png_bytes: &[u8], updated_at: i64) -> std::io::Result<()> {
    let Some(username) = sanitize_username(username) else {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid username"));
    };
    let dir = pfps_dir();
    std::fs::write(dir.join(format!("{username}.png")), png_bytes)?;
    std::fs::write(dir.join(format!("{username}.ts")), updated_at.to_string())?;
    Ok(())
}

/// GET /api/pfp/:username — serves the cached picture's raw bytes, or 404
/// if this user has none cached. Requires a valid session token like every
/// other route here, even though a picture itself isn't sensitive data —
/// kept consistent with how the rest of this server gates its endpoints
/// rather than carving out a quieter exception for this one.
pub async fn serve_pfp(
    headers: HeaderMap,
    Path(username): Path<String>,
    State(s): State<Arc<AppState>>,
) -> Response {
    if crate::db::verify_token(&s.pool, token_from(&headers)).await.ok().flatten().is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(username) = sanitize_username(&username) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let path = pfps_dir().join(format!("{username}.png"));
    match std::fs::read(&path) {
        Ok(data) => {
            tracing::info!("voice: serving {} bytes for {username}'s pfp", data.len());
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(data))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
