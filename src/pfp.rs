// pfp.rs — server-side profile picture caching.
//
// Each user's uploaded picture is cached on disk as pfps/<key>.png,
// alongside a small sidecar pfps/<key>.ts holding the client-reported
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
/// Files here are named by the account's public key (hex), not its
/// username: a username can be freed up (a kick removes the account) and
/// then taken by someone else, who must not inherit the old picture,
/// banner and bio. The key belongs to exactly one account, forever.
/// Resolves the key for a username via the users table.
pub async fn storage_key_for(pool: &sqlx::SqlitePool, username: &str) -> Option<String> {
    use sqlx::Row;
    let row = sqlx::query("SELECT public_key FROM users WHERE username = ?")
        .bind(username).fetch_optional(pool).await.ok().flatten()?;
    let key: String = row.get("public_key");
    // Only ever hex — but never let anything else near a file name.
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_hexdigit()) { return None; }
    Some(key.to_lowercase())
}

/// One-time move of files saved under the old username-based names to the
/// key-based ones, for every account this server knows. Safe to run on
/// every start: it only renames when the old file exists and the new one
/// doesn't.
pub async fn migrate_to_key_names(pool: &sqlx::SqlitePool) {
    use sqlx::Row;
    let Ok(rows) = sqlx::query("SELECT public_key, username FROM users").fetch_all(pool).await else { return };
    let dir = pfps_dir();
    let mut moved = 0;
    for r in rows {
        let key: String = r.get::<String, _>("public_key").to_lowercase();
        let user: String = r.get("username");
        if !key.chars().all(|c| c.is_ascii_hexdigit()) { continue; }
        let Some(user) = sanitize_username(&user) else { continue };
        for ext in ["png", "ts", "profile.json", "banner"] {
            let (old, new) = (dir.join(format!("{user}.{ext}")), dir.join(format!("{key}.{ext}")));
            if old.exists() && !new.exists() && std::fs::rename(&old, &new).is_ok() { moved += 1; }
        }
    }
    if moved > 0 { tracing::info!("pfp: moved {moved} profile file(s) to account-key names"); }
}

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
pub fn sanitize_username(username: &str) -> Option<String> {
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

/// Largest pfp the server will cache. The client caps its own at the same
/// value; the cropper's real output (1024px max) is far smaller.
pub const MAX_PFP_BYTES: usize = 8 << 20;

/// Sniffs the image type from its magic bytes. The cache file keeps its
/// historical .png name, but can hold a JPEG (and later GIF/WebP for
/// animated pfps), so the served Content-Type has to come from the bytes.
pub fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"BM") && bytes.len() > 26 {
        Some("image/bmp")
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && (&bytes[8..12] == b"avif" || &bytes[8..12] == b"avis") {
        Some("image/avif")
    } else {
        None
    }
}

/// Image types accepted for profile pictures, profile/server banners and
/// the server icon: the common static formats plus the animated ones
/// (GIF, animated WebP, APNG — which is a PNG). Animated files are stored
/// and served byte-for-byte, so they keep moving. Server theme backgrounds
/// stay static and have their own check.
pub fn is_profile_image(bytes: &[u8]) -> bool {
    matches!(image_mime(bytes),
        Some("image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp" | "image/avif"))
}

/// Writes a new cached picture and its timestamp, overwriting whatever
/// was cached for this user before.
pub fn save_cached(username: &str, png_bytes: &[u8], updated_at: i64) -> std::io::Result<()> {
    let Some(username) = sanitize_username(username) else {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid username"));
    };
    // Static PNG/JPEG only for now — GIF/WebP get sniffed above so serving
    // them later is ready, but accepting them waits on animated-pfp work.
    if !is_profile_image(png_bytes) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "not a PNG or JPEG image"));
    }
    if png_bytes.len() > MAX_PFP_BYTES {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "image too large"));
    }
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
    let Some(key) = storage_key_for(&s.pool, &username).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = pfps_dir().join(format!("{key}.png"));
    match std::fs::read(&path) {
        Ok(data) => {
            tracing::info!("voice: serving {} bytes for {username}'s pfp", data.len());
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, image_mime(&data).unwrap_or("application/octet-stream"))
                .body(Body::from(data))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
