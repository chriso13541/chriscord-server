use axum::{extract::State, http::StatusCode, Json};
use ed25519_dalek::{Signature, VerifyingKey, Verifier};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{db, state::AppState, utils};

const CHALLENGE_TTL: Duration = Duration::from_secs(60);

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ServerInfoResp {
    pub name:         String,
    pub requires_key: bool,
}

#[derive(Deserialize)]
pub struct ChallengeReq {
    pub public_key: String, // hex-encoded Ed25519 public key
}

#[derive(Serialize)]
pub struct ChallengeResp {
    pub nonce: String, // hex-encoded 32-byte random nonce
}

#[derive(Deserialize)]
pub struct JoinReq {
    pub username:   String,
    pub server_key: Option<String>,
    pub public_key: String,  // hex-encoded Ed25519 public key
    pub nonce:      String,  // the nonce we issued
    pub signature:  String,  // hex-encoded Ed25519 signature of nonce bytes
}

#[derive(Serialize)]
pub struct JoinResp {
    pub token:       String,
    pub username:    String,
    pub fingerprint: String, // first 16 hex chars of public key — display identity
}

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn err(code: StatusCode, msg: &str) -> ApiErr {
    (code, Json(serde_json::json!({ "error": msg })))
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /api/info — public.
pub async fn server_info(State(s): State<Arc<AppState>>) -> Json<ServerInfoResp> {
    let name = db::get_config(&s.pool, "server_name").await
        .unwrap_or(None).unwrap_or_else(|| "Chriscord Server".to_string());
    let sk = db::get_config(&s.pool, "server_key").await.unwrap_or(None);
    Json(ServerInfoResp {
        name,
        requires_key: sk.map(|k| !k.is_empty()).unwrap_or(false),
    })
}

/// POST /api/challenge — client sends their public key, gets back a nonce to sign.
/// The nonce is stored server-side and expires after 60 seconds.
pub async fn challenge(
    State(s):   State<Arc<AppState>>,
    Json(body): Json<ChallengeReq>,
) -> Result<Json<ChallengeResp>, ApiErr> {
    // Basic public key validation: Ed25519 pubkey = 32 bytes = 64 hex chars
    if body.public_key.len() != 64 {
        return Err(err(StatusCode::BAD_REQUEST, "Invalid public key format (expected 64 hex chars)"));
    }
    hex::decode(&body.public_key)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "Public key must be hex-encoded"))?;

    // Generate a 32-byte random nonce
    let nonce = utils::generate_hex(64); // 32 bytes = 64 hex chars

    // Store nonce → (issued_at, public_key), clean up expired ones
    {
        let mut challenges = s.challenges.lock().unwrap();
        let now = Instant::now();
        challenges.retain(|_, (issued_at, _)| now.duration_since(*issued_at) < CHALLENGE_TTL);
        challenges.insert(nonce.clone(), (now, body.public_key));
    }

    Ok(Json(ChallengeResp { nonce }))
}

/// POST /api/join — verify the signed nonce and issue a session token.
pub async fn join(
    State(s):   State<Arc<AppState>>,
    Json(body): Json<JoinReq>,
) -> Result<Json<JoinResp>, ApiErr> {
    // ── 1. Validate inputs ────────────────────────────────────────────────────
    let username = body.username.trim().to_string();
    if username.is_empty() || username.len() > 32 {
        return Err(err(StatusCode::BAD_REQUEST, "Username must be 1–32 characters"));
    }

    // ── 2. Enforce server join key if configured ──────────────────────────────
    let sk = db::get_config(&s.pool, "server_key").await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "DB error"))?;
    if let Some(key) = sk {
        if !key.is_empty() {
            let provided = body.server_key.as_deref().unwrap_or("");
            if provided != key {
                return Err(err(StatusCode::UNAUTHORIZED, "Invalid server key"));
            }
        }
    }

    // ── 3. Retrieve and consume the challenge nonce ───────────────────────────
    let stored_pubkey = {
        let mut challenges = s.challenges.lock().unwrap();
        let now = Instant::now();
        // Remove and return the stored entry for this nonce
        match challenges.remove(&body.nonce) {
            Some((issued_at, pk)) if now.duration_since(issued_at) < CHALLENGE_TTL => pk,
            Some(_) => return Err(err(StatusCode::UNAUTHORIZED, "Challenge expired")),
            None    => return Err(err(StatusCode::UNAUTHORIZED, "Unknown or already-used challenge")),
        }
    };

    // The public key in the join request must match the one we issued the challenge to
    if stored_pubkey != body.public_key {
        return Err(err(StatusCode::UNAUTHORIZED, "Public key mismatch"));
    }

    // ── 4. Verify Ed25519 signature ───────────────────────────────────────────
    let pubkey_bytes = hex::decode(&body.public_key)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid public key encoding"))?;
    let sig_bytes = hex::decode(&body.signature)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid signature encoding"))?;
    let nonce_bytes = hex::decode(&body.nonce)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid nonce encoding"))?;

    let vk = VerifyingKey::from_bytes(
        pubkey_bytes.as_slice().try_into()
            .map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid public key length"))?,
    ).map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid public key"))?;

    let sig = Signature::from_slice(&sig_bytes)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid signature length"))?;

    vk.verify(&nonce_bytes, &sig)
        .map_err(|_| err(StatusCode::UNAUTHORIZED, "Signature verification failed"))?;

    // ── 5. Register or verify the user record (TOFU) ──────────────────────────
    // TOFU: first time this public key is seen, register it with the claimed username.
    // If the key is already known, verify the username matches.
    // If the username is taken by a different key, reject.
    let existing_user = sqlx::query(
        "SELECT public_key, username FROM users WHERE public_key = ? OR username = ?",
    )
    .bind(&body.public_key)
    .bind(&username)
    .fetch_all(&s.pool)
    .await
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "DB error"))?;

    for row in &existing_user {
        let row_pk:   String = sqlx::Row::get(row, "public_key");
        let row_name: String = sqlx::Row::get(row, "username");

        if row_pk == body.public_key {
            // This key is registered — enforce the stored username
            if row_name != username {
                return Err(err(
                    StatusCode::CONFLICT,
                    &format!("This key is registered as '{}'. Use that username.", row_name),
                ));
            }
        } else {
            // Different key, same username — username is taken
            return Err(err(
                StatusCode::CONFLICT,
                "Username already registered to a different key",
            ));
        }
    }

    // Register on first use
    if existing_user.is_empty() {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO users (public_key, username, created_at) VALUES (?, ?, ?)",
        )
        .bind(&body.public_key)
        .bind(&username)
        .bind(&now)
        .execute(&s.pool)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "DB error"))?;
    }

    // ── 6. Issue session token ────────────────────────────────────────────────
    let token = utils::generate_hex(64);
    let now   = chrono::Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO sessions (token, username, public_key, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&token)
    .bind(&username)
    .bind(&body.public_key)
    .bind(&now)
    .execute(&s.pool)
    .await
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "DB error"))?;

    let fingerprint = body.public_key[..16].to_string();

    Ok(Json(JoinResp { token, username, fingerprint }))
}
