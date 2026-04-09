use axum::{
    extract::{Multipart, Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Serialize;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

use crate::{db, state::AppState};

const UPLOAD_DIR: &str = "./uploads";
const MAX_FILE_BYTES: usize = 50 * 1024 * 1024; // 50 MB

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
    h.get("X-Session-Token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// POST /api/upload — upload a file, get back a URL.
/// The client attaches the URL to a subsequent chat message.
pub async fn upload(
    headers:  HeaderMap,
    State(s): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Result<Json<UploadResp>, ApiErr> {
    // Auth
    db::verify_token(&s.pool, token_from(&headers))
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "DB error"))?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Unauthorized"))?;

    // Ensure upload directory exists
    tokio::fs::create_dir_all(UPLOAD_DIR)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Could not create upload dir"))?;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        err(StatusCode::BAD_REQUEST, &format!("Multipart error: {}", e))
    })? {
        let original_name = field
            .file_name()
            .unwrap_or("file")
            .to_string();

        // Detect MIME from extension
        let mime = mime_guess::from_path(&original_name)
            .first_or_octet_stream()
            .to_string();

        // Generate a unique filename to avoid collisions
        let ext = std::path::Path::new(&original_name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin");
        let stored_name = format!("{}.{}", uuid::Uuid::new_v4(), ext);
        let path = format!("{}/{}", UPLOAD_DIR, stored_name);

        // Read with size cap
        let data = field.bytes().await.map_err(|e| {
            err(StatusCode::BAD_REQUEST, &format!("Read error: {}", e))
        })?;
        if data.len() > MAX_FILE_BYTES {
            return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "File too large (max 50 MB)"));
        }

        // Write to disk
        let mut f = tokio::fs::File::create(&path)
            .await
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Write failed"))?;
        f.write_all(&data)
            .await
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Write failed"))?;

        return Ok(Json(UploadResp {
            url:      format!("/api/files/{}", stored_name),
            filename: original_name,
            mime,
        }));
    }

    Err(err(StatusCode::BAD_REQUEST, "No file in request"))
}

/// GET /api/files/:filename — serve a stored file.
pub async fn serve_file(
    Path(filename): Path<String>,
) -> impl axum::response::IntoResponse {
    // Sanitise: strip any path traversal
    let safe = std::path::Path::new(&filename)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    let path = format!("{}/{}", UPLOAD_DIR, safe);

    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mime = mime_guess::from_path(&path)
                .first_or_octet_stream()
                .to_string();
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, mime)],
                bytes,
            )
                .into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "Not found").into_response(),
    }
}

// Bring IntoResponse into scope for the tuple impl
use axum::response::IntoResponse;
