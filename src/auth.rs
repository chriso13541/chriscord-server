use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{db, state::AppState, utils};

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ServerInfoResp {
    pub name:         String,
    pub requires_key: bool,
}

#[derive(Deserialize)]
pub struct JoinReq {
    pub username:   String,
    pub server_key: Option<String>,
}

#[derive(Serialize)]
pub struct JoinResp {
    pub token:    String,
    pub username: String,
}

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn err(code: StatusCode, msg: &str) -> ApiErr {
    (code, Json(serde_json::json!({ "error": msg })))
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /api/info — public, no auth required.
/// Client calls this first to display the server name and know if a key is required.
pub async fn server_info(State(s): State<Arc<AppState>>) -> Json<ServerInfoResp> {
    let name = db::get_config(&s.pool, "server_name")
        .await
        .unwrap_or(None)
        .unwrap_or_else(|| "Chriscord Server".to_string());

    let sk = db::get_config(&s.pool, "server_key")
        .await
        .unwrap_or(None);

    Json(ServerInfoResp {
        name,
        requires_key: sk.map(|k| !k.is_empty()).unwrap_or(false),
    })
}

/// POST /api/join — exchange (username + optional server_key) for a session token.
pub async fn join(
    State(s): State<Arc<AppState>>,
    Json(body): Json<JoinReq>,
) -> Result<Json<JoinResp>, ApiErr> {
    let username = body.username.trim().to_string();

    if username.is_empty() || username.len() > 32 {
        return Err(err(StatusCode::BAD_REQUEST, "Username must be 1–32 characters"));
    }

    // Enforce server key if one is configured
    let sk = db::get_config(&s.pool, "server_key")
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "DB error"))?;

    if let Some(key) = sk {
        if !key.is_empty() {
            let provided = body.server_key.as_deref().unwrap_or("");
            if provided != key {
                return Err(err(StatusCode::UNAUTHORIZED, "Invalid server key"));
            }
        }
    }

    let token = utils::generate_hex(64);
    let now   = chrono::Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO sessions (token, username, created_at) VALUES (?, ?, ?)",
    )
    .bind(&token)
    .bind(&username)
    .bind(&now)
    .execute(&s.pool)
    .await
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "DB error"))?;

    Ok(Json(JoinResp { token, username }))
}
