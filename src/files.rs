use axum::{
    body::Body,
    extract::{Multipart, Path, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use tower_http::services::ServeFile;
use serde::Serialize;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

use crate::{db, state::AppState};

const UPLOAD_DIR:      &str   = "./uploads";
const MAX_FILE_BYTES:  u64    = 500 * 1024 * 1024; // 500 MB — enough for most videos

#[derive(Serialize)]
pub struct UploadResp {
    pub url:      String,
    pub filename: String,
    pub mime:     String,
}

type ApiErr = (StatusCode, Json<serde_json::Value>);
fn err(code: StatusCode, msg: &str) -> ApiErr {
    (code, Json(serde_json::json!({ "error": msg })))
}
fn token_from(h: &HeaderMap) -> &str {
    h.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("")
}

/// POST /api/upload — streams the multipart body straight to disk.
/// Never loads the whole file into memory.
pub async fn upload(
    headers:       HeaderMap,
    State(s):      State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Result<Json<UploadResp>, ApiErr> {
    db::verify_token(&s.pool, token_from(&headers))
        .await.map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "DB error"))?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Unauthorized"))?;

    tokio::fs::create_dir_all(UPLOAD_DIR)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Cannot create upload dir"))?;

    while let Some(field) = multipart.next_field().await
        .map_err(|e| err(StatusCode::BAD_REQUEST, &e.to_string()))?
    {
        let original_name = field.file_name().unwrap_or("file").to_string();
        let mime = mime_guess::from_path(&original_name)
            .first_or_octet_stream()
            .to_string();

        let ext = std::path::Path::new(&original_name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin");
        let stored_name = format!("{}.{}", uuid::Uuid::new_v4(), ext);
        let path        = format!("{}/{}", UPLOAD_DIR, stored_name);

        // Stream field chunks straight to disk
        let mut file = tokio::fs::File::create(&path)
            .await
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Write failed"))?;

        let mut written: u64 = 0;
        let mut stream = field;

        loop {
            match stream.chunk().await {
                Ok(Some(chunk)) => {
                    written += chunk.len() as u64;
                    if written > MAX_FILE_BYTES {
                        // Clean up partial file
                        drop(file);
                        let _ = tokio::fs::remove_file(&path).await;
                        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "File too large (max 500 MB)"));
                    }
                    file.write_all(&chunk)
                        .await
                        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Write failed"))?;
                }
                Ok(None) => break, // field fully received
                Err(e)   => return Err(err(StatusCode::BAD_REQUEST, &e.to_string())),
            }
        }

        file.flush().await
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Flush failed"))?;

        return Ok(Json(UploadResp {
            url:      format!("/api/files/{}", stored_name),
            filename: original_name,
            mime,
        }));
    }

    Err(err(StatusCode::BAD_REQUEST, "No file in request"))
}

#[derive(serde::Deserialize, Default)]
pub struct FileQuery {
    /// `?download=1` asks for a real download (Content-Disposition:
    /// attachment) instead of inline display — what the client's Download
    /// button opens in the user's browser.
    download: Option<String>,
    /// Original filename to save as. Files are stored under a UUID name,
    /// and the original only lives in the message's attachment record, so
    /// the client passes it along.
    name: Option<String>,
}

/// Uploaded files never change once written (every upload gets a fresh
/// UUID name), so they can be cached forever, CDN-style. This is what stops
/// the webview from downloading the same image or video again every time
/// the message list re-renders.
const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";

/// GET /api/files/:filename[?download=1&name=...] — serves a stored file.
///
/// Streams from disk via tower-http's ServeFile rather than reading the
/// whole file into memory first (the old version did `tokio::fs::read`,
/// i.e. up to 500 MB of RAM per request). ServeFile also handles HTTP
/// Range requests, which is what lets inline <video>/<audio> fetch just the
/// metadata and seek, instead of pulling the entire file before playing,
/// plus conditional requests (Last-Modified / If-Modified-Since → 304).
pub async fn serve_file(
    Path(filename): Path<String>,
    Query(q): Query<FileQuery>,
    req: Request,
) -> Response {
    // Strip any path traversal attempts
    let safe = std::path::Path::new(&filename)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if safe.is_empty() || safe.starts_with('.') {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    }
    let path = format!("{}/{}", UPLOAD_DIR, safe);

    let mut res = match ServeFile::new(&path).try_call(req).await {
        Ok(res) => res.map(Body::new),
        Err(e) => {
            tracing::warn!("files: failed to serve {safe}: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !res.status().is_success() && res.status() != StatusCode::NOT_MODIFIED {
        return res; // 404 for a missing file, 416 for a bad range, etc.
    }

    let headers = res.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE_CACHE));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if q.download.as_deref().is_some_and(|d| d == "1" || d == "true") {
        let name = q.name.as_deref().unwrap_or(safe);
        if let Ok(v) = HeaderValue::from_str(&attachment_disposition(name)) {
            headers.insert(header::CONTENT_DISPOSITION, v);
        }
    }
    res
}

/// Builds `attachment; filename="..."; filename*=UTF-8''...` — the plain
/// `filename` is an ASCII-only fallback, and `filename*` (RFC 6266 / 5987)
/// carries the real name with any Unicode intact; every current browser
/// prefers the latter. Anything that could break out of the header or
/// point at a directory is dropped first.
fn attachment_disposition(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '/' | '\\' | '"'))
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.');
    let cleaned = if cleaned.is_empty() { "file" } else { cleaned };

    let ascii: String = cleaned
        .chars()
        .map(|c| if c.is_ascii() { c } else { '_' })
        .collect();
    let mut encoded = String::new();
    for b in cleaned.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}
