use axum::{
    extract::{Multipart, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
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

/// GET /api/files/:filename — serve a stored file with correct content-type.
pub async fn serve_file(Path(filename): Path<String>) -> impl IntoResponse {
    // Strip any path traversal attempts
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
                [(header::CONTENT_TYPE, mime)],
                bytes,
            ).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "Not found").into_response(),
    }
}
