// profile.rs — per-user profile: bio + optional banner image.
//
// Same model as pfp.rs, and cached alongside it in ./pfps/: the profile is
// owned by the user's own client (it travels with their account, like the
// pfp does, so it's the same on every server they join), and each server
// just caches the latest copy it was sent. The handshake mirrors the pfp
// one in ws.rs — the client reports a profile_updated_at on connect (and
// after every change), the server asks for an upload only if its cached
// copy is missing or older, and a fresh upload is broadcast as
// profile_updated so anyone showing that user's card knows to refetch.
//
// On disk, per user:
//   pfps/<username>.profile.json  — { bio, updated_at, has_banner }
//   pfps/<username>.banner        — the banner image, if they set one
//
// The default banner (a colour picked from the user's pfp) is computed by
// the viewing client, not stored here — there's nothing to cache for it.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::sync::Arc;

use crate::pfp::{image_mime, pfps_dir, sanitize_username, MAX_PFP_BYTES};
use crate::state::AppState;

/// Longest bio accepted, in characters (not bytes) — matches the client's
/// counter.
pub const MAX_BIO_CHARS: usize = 500;

#[derive(Serialize, Deserialize, Default)]
struct StoredProfile {
    bio:        String,
    updated_at: i64,
    has_banner: bool,
}

fn token_from(h: &HeaderMap) -> &str {
    h.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("")
}

fn read_stored(username: &str) -> Option<StoredProfile> {
    let username = sanitize_username(username)?;
    let raw = std::fs::read_to_string(pfps_dir().join(format!("{username}.profile.json"))).ok()?;
    serde_json::from_str(&raw).ok()
}

/// The profile timestamp this server has cached for a user, if any.
pub fn cached_timestamp(username: &str) -> Option<i64> {
    read_stored(username).map(|p| p.updated_at)
}

/// Replaces a user's cached profile. `banner` None means "no banner" —
/// any previously cached one is removed.
pub fn save_cached(username: &str, bio: &str, banner: Option<&[u8]>, updated_at: i64) -> std::io::Result<()> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
    let Some(username) = sanitize_username(username) else { return Err(bad("invalid username")) };
    let bio = bio.trim();
    if bio.chars().count() > MAX_BIO_CHARS {
        return Err(bad("bio too long"));
    }
    if let Some(bytes) = banner {
        if !matches!(image_mime(bytes), Some("image/png") | Some("image/jpeg")) {
            return Err(bad("banner is not a PNG or JPEG image"));
        }
        if bytes.len() > MAX_PFP_BYTES {
            return Err(bad("banner too large"));
        }
    }
    let dir = pfps_dir();
    let banner_path = dir.join(format!("{username}.banner"));
    match banner {
        Some(bytes) => std::fs::write(&banner_path, bytes)?,
        None => { let _ = std::fs::remove_file(&banner_path); }
    }
    let stored = StoredProfile { bio: bio.to_string(), updated_at, has_banner: banner.is_some() };
    std::fs::write(dir.join(format!("{username}.profile.json")), serde_json::to_string(&stored)?)?;
    Ok(())
}

/// GET /api/profile/:username — bio, whether there's a banner, and when
/// they joined this server. Users who've never uploaded a profile get an
/// empty one rather than a 404; only unknown usernames 404.
pub async fn get_profile(
    headers: HeaderMap,
    Path(username): Path<String>,
    State(s): State<Arc<AppState>>,
) -> Response {
    if crate::db::verify_token(&s.pool, token_from(&headers)).await.ok().flatten().is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let row = sqlx::query("SELECT created_at FROM users WHERE username = ?")
        .bind(&username).fetch_optional(&s.pool).await;
    let member_since: String = match row {
        Ok(Some(r)) => r.get("created_at"),
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let p = read_stored(&username).unwrap_or_default();
    Json(serde_json::json!({
        "username": username,
        "bio": p.bio,
        "has_banner": p.has_banner,
        "updated_at": p.updated_at,
        "member_since": member_since,
    })).into_response()
}

/// GET /api/banner/:username — the cached banner image, or 404.
pub async fn serve_banner(
    headers: HeaderMap,
    Path(username): Path<String>,
    State(s): State<Arc<AppState>>,
) -> Response {
    if crate::db::verify_token(&s.pool, token_from(&headers)).await.ok().flatten().is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(username) = sanitize_username(&username) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(pfps_dir().join(format!("{username}.banner"))).await {
        Ok(data) => Response::builder()
            .header(header::CONTENT_TYPE, image_mime(&data).unwrap_or("application/octet-stream"))
            .body(Body::from(data))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
