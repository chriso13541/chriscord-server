//! "Download all": one .zip of every file attached to a message.
//!
//!   GET /api/messages/:id/zip/link   (session token) builds the zip if
//!       needed and returns a short-lived link to it, like
//!       /api/files/:filename/link does for a single file
//!   GET /api/zips/:file?t=TOKEN      serves it (what the browser downloads)
//!
//! The zip is a temporary file in ./zips and is deleted once the last link
//! handed out for it expires (LINK_TTL, 5 minutes). Asking again for the
//! same message while its zip still exists reuses it and pushes the
//! deletion back, so two people grabbing the same files don't make two
//! zips. A download that's still running when the zip is deleted finishes
//! normally: on Linux an open file stays readable until it's closed.
//!
//! Zips are built one at a time (the files are copied, not compressed; see
//! zipfile.rs), so a burst of requests can't fill the disk or tie up the
//! server. Anything left in ./zips from before a restart is deleted at
//! startup: its links only ever lived in memory.

use axum::{
    body::Body,
    extract::{Path, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tower_http::services::ServeFile;

use crate::{db, files, state::AppState, zipfile};

const ZIP_DIR: &str = "./zips";

type ApiErr = (StatusCode, Json<serde_json::Value>);
fn err(code: StatusCode, msg: &str) -> ApiErr {
    (code, Json(serde_json::json!({ "error": msg })))
}
fn token_from(h: &HeaderMap) -> &str {
    h.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("")
}

#[derive(Clone)]
struct BuiltZip {
    /// Its name in ZIP_DIR (a UUID, never shown to anyone).
    file:      String,
    /// What the browser saves it as.
    save_as:   String,
    size:      u64,
    /// Deleted at this point: the expiry of the last link handed out.
    delete_at: Instant,
}

pub struct Zips {
    /// message id → its zip, while it exists
    by_message: Mutex<HashMap<String, BuiltZip>>,
    /// link token → (zip file, save-as name, expiry)
    links: Mutex<HashMap<String, (String, String, Instant)>>,
    /// Held while building, so only one zip is built at a time.
    building: tokio::sync::Mutex<()>,
}

impl Zips {
    pub fn new() -> Self {
        Self {
            by_message: Mutex::new(HashMap::new()),
            links:      Mutex::new(HashMap::new()),
            building:   tokio::sync::Mutex::new(()),
        }
    }
}

/// Startup: deletes zips left over from before a restart.
pub fn clear_leftovers() {
    let _ = std::fs::remove_dir_all(ZIP_DIR);
}

fn zip_path(file: &str) -> PathBuf {
    PathBuf::from(ZIP_DIR).join(file)
}

/// GET /api/messages/:id/zip/link — a link to a zip of every file attached
/// to the message, for anyone who can see the channel it's in.
pub async fn zip_link(
    headers:  HeaderMap,
    Path(id): Path<String>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let username = db::verify_token(&s.pool, token_from(&headers))
        .await.map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "DB error"))?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Unauthorized"))?;

    let row = sqlx::query("SELECT * FROM messages WHERE id = ?")
        .bind(&id).fetch_optional(&s.pool).await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "DB error"))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Message not found"))?;
    let msg = crate::messages::row_to_msg(&row);
    // A message in a channel you can't see is "not found", same as the
    // channel itself.
    crate::access::require_board(&s.pool, &username, &msg.board_id).await
        .map_err(|_| err(StatusCode::NOT_FOUND, "Message not found"))?;

    // The files this server stores (not linked videos from elsewhere) that
    // are still on disk.
    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    for a in &msg.attachments {
        let Some(stored) = a.url.strip_prefix("/api/files/") else { continue };
        if files::safe_name(stored) != Some(stored) { continue; }
        let path = PathBuf::from(files::UPLOAD_DIR).join(stored);
        if tokio::fs::metadata(&path).await.is_ok() {
            let name = if a.name.trim().is_empty() { stored.to_string() } else { a.name.clone() };
            entries.push((name, path));
        }
    }
    if entries.len() < 2 {
        return Err(err(StatusCode::BAD_REQUEST, "This message doesn't have several files to zip"));
    }

    // One build at a time; whoever waits here may find the zip already made.
    let _building = s.zips.building.lock().await;
    let now = Instant::now();
    let link_expires = now + files::LINK_TTL;

    let reused = {
        let mut map = s.zips.by_message.lock().unwrap();
        match map.get_mut(&id) {
            Some(z) if zip_path(&z.file).exists() => {
                if z.delete_at < link_expires { z.delete_at = link_expires; }
                Some(z.clone())
            }
            _ => None,
        }
    };
    let zip = match reused {
        Some(z) => z,
        None => {
            let file = format!("{}.zip", uuid::Uuid::new_v4());
            let (time, date, day) = zip_timestamp(&msg.created_at);
            let path = zip_path(&file);
            let build_path = path.clone();
            let built = tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
                std::fs::create_dir_all(ZIP_DIR)?;
                let mut z = zipfile::ZipWriter::create(&build_path)?;
                let mut taken = HashSet::new();
                for (name, src) in &entries {
                    z.add_file(&zipfile::unique_entry_name(name, &mut taken), src, time, date)?;
                }
                z.finish()?;
                Ok(std::fs::metadata(&build_path)?.len())
            }).await;
            let size = match built {
                Ok(Ok(size)) => size,
                Ok(Err(e)) => {
                    tracing::warn!("zips: building a zip for message {id} failed: {e}");
                    let _ = tokio::fs::remove_file(&path).await;
                    return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "Couldn't make the zip on the server"));
                }
                Err(e) => {
                    tracing::warn!("zips: zip task for message {id} failed: {e}");
                    let _ = tokio::fs::remove_file(&path).await;
                    return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "Couldn't make the zip on the server"));
                }
            };
            let z = BuiltZip {
                file,
                save_as: format!("{}-files-{}.zip", msg.username, day),
                size,
                delete_at: link_expires,
            };
            s.zips.by_message.lock().unwrap().insert(id.clone(), z.clone());
            schedule_delete(s.clone(), id.clone(), z.file.clone());
            z
        }
    };
    drop(_building);

    let token = uuid::Uuid::new_v4().to_string();
    {
        let mut links = s.zips.links.lock().unwrap();
        links.retain(|_, (_, _, expires)| *expires > now); // prune as we go
        links.insert(token.clone(), (zip.file.clone(), zip.save_as.clone(), link_expires));
    }
    Ok(Json(serde_json::json!({
        "url":        format!("/api/zips/{}?t={token}", zip.file),
        "name":       zip.save_as,
        "size":       zip.size,
        "expires_in": files::LINK_TTL.as_secs(),
    })))
}

/// Deletes a zip once its last link has expired. Each new link for the
/// same zip pushes delete_at back, so this re-checks after every sleep.
fn schedule_delete(s: Arc<AppState>, message_id: String, file: String) {
    tokio::spawn(async move {
        loop {
            let at = {
                let mut map = s.zips.by_message.lock().unwrap();
                let delete_at = map.get(&message_id).filter(|z| z.file == file).map(|z| z.delete_at);
                match delete_at {
                    Some(at) if at > Instant::now() => Some(at),
                    Some(_) => { map.remove(&message_id); None }
                    None => None,
                }
            };
            match at {
                Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
                None => break,
            }
        }
        if let Err(e) = tokio::fs::remove_file(zip_path(&file)).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("zips: couldn't delete {file}: {e}");
            }
        }
        s.zips.links.lock().unwrap().retain(|_, (f, _, _)| *f != file);
    });
}

/// The zip entries' modified time (the message's, in the server's local
/// time, which is how zip tools show it) and the date for the file name.
fn zip_timestamp(created_at: &str) -> (u16, u16, String) {
    use chrono::{Datelike, Timelike};
    let t = chrono::DateTime::parse_from_rfc3339(created_at)
        .map(|t| t.with_timezone(&chrono::Local))
        .unwrap_or_else(|_| chrono::Local::now());
    let (time, date) = zipfile::dos_datetime(t.year(), t.month(), t.day(), t.hour(), t.minute(), t.second());
    (time, date, t.format("%Y-%m-%d").to_string())
}

#[derive(serde::Deserialize, Default)]
pub struct ZipQuery {
    t: Option<String>,
}

/// GET /api/zips/:file?t=TOKEN — the zip itself, always as a download.
/// Like /api/files, the link is checked when the request starts, and
/// ranges work, so a browser can resume an interrupted download while the
/// zip still exists.
pub async fn serve_zip(
    Path(file): Path<String>,
    Query(q):   Query<ZipQuery>,
    State(s):   State<Arc<AppState>>,
    req:        Request,
) -> Response {
    let save_as = {
        let links = s.zips.links.lock().unwrap();
        match q.t.as_deref().and_then(|t| links.get(t)) {
            Some((for_file, save_as, expires)) if *for_file == file && *expires > Instant::now() => save_as.clone(),
            _ => return (StatusCode::FORBIDDEN, "Link expired or invalid").into_response(),
        }
    };
    // `file` matched a name this server made (a UUID), so it's safe as a path.
    let mut res = match ServeFile::new(zip_path(&file)).try_call(req).await {
        Ok(res) => res.map(Body::new),
        Err(e) => {
            tracing::warn!("zips: failed to serve {file}: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !res.status().is_success() {
        return res;
    }
    let headers = res.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if let Ok(v) = HeaderValue::from_str(&files::attachment_disposition(&save_as)) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    res
}
