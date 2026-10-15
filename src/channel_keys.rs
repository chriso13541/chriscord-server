// channel_keys.rs — keys for end-to-end encrypted messages (the app's
// textcrypt.go). The server only stores and hands out sealed copies: it
// can't open any of them, and messages reach it already encrypted.
//
// Each channel's key comes in numbered generations. For each generation the
// server keeps one sealed copy ("box") per member, made by a member who had
// the key, for the recipient's published encryption key. People are
// identified by their identity key (users.public_key), not their name.
//
//   PUT  /api/e2ee/enc-key        publish your encryption key (signed by your identity key)
//   GET  /api/boards/:id/keys     your boxes for a channel, who can see it, and whether
//                                 it needs a new generation (someone lost access)
//   POST /api/boards/:id/keys     start the next generation, with a box for each member
//   GET  /api/e2ee/pending        members missing generations you hold (newcomers)
//   POST /api/e2ee/boxes          boxes you've made for them
//
// Every change is announced as {"type":"e2ee_changed"} so online apps share
// keys with whoever needs them.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;
use sqlx::{Row, SqlitePool};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::{access::Visible, db, state::AppState};

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn err(code: StatusCode, msg: &str) -> ApiErr {
    (code, Json(serde_json::json!({ "error": msg })))
}
fn dberr() -> ApiErr { err(StatusCode::INTERNAL_SERVER_ERROR, "DB error") }
fn bad(msg: &str) -> ApiErr { err(StatusCode::BAD_REQUEST, msg) }

/// What an identity key signs to vouch for an encryption key (must match
/// encKeyMessage in the app).
const ENC_KEY_CONTEXT: &[u8] = b"chriscord-e2ee-v1/enc-key\x00";
/// Most boxes in one request, and the most one box may be (base64): a
/// sealed 32-byte key is about 100 characters.
const MAX_BOXES: usize = 5000;
const MAX_BOX_LEN: usize = 512;
/// Most entries one /api/e2ee/pending answer lists (the app asks again).
const MAX_PENDING: usize = 1000;

pub async fn init(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS user_enc_keys (
            public_key TEXT PRIMARY KEY,   -- identity key (hex, lowercase)
            enc_key    TEXT NOT NULL,      -- X25519 encryption key (hex)
            sig        TEXT NOT NULL       -- identity key's signature over it (hex)
        )",
    ).execute(pool).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS channel_keys (
            board_id   TEXT NOT NULL,
            generation INTEGER NOT NULL,
            created_by TEXT NOT NULL,      -- identity key of whoever made it
            created_at TEXT NOT NULL,
            PRIMARY KEY (board_id, generation)
        )",
    ).execute(pool).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS channel_key_boxes (
            board_id   TEXT NOT NULL,
            generation INTEGER NOT NULL,
            recipient  TEXT NOT NULL,      -- identity key it's sealed for
            sender     TEXT NOT NULL,      -- identity key of whoever sealed it
            box        TEXT NOT NULL,      -- base64
            PRIMARY KEY (board_id, generation, recipient)
        )",
    ).execute(pool).await?;
    Ok(())
}

/// The signed-in member: (username, identity key).
async fn me(h: &HeaderMap, s: &AppState) -> Result<(String, String), ApiErr> {
    let token = h.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let user = db::verify_token(&s.pool, token).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Unauthorized"))?;
    let row = sqlx::query("SELECT public_key FROM users WHERE username = ?")
        .bind(&user).fetch_optional(&s.pool).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Unauthorized"))?;
    let key: String = row.get("public_key");
    Ok((user, key.to_lowercase()))
}

#[derive(Clone)]
struct Member {
    username:   String,
    public_key: String,
    enc_key:    Option<String>,
    sig:        Option<String>,
}

impl Member {
    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "username": self.username, "public_key": self.public_key,
            "enc_key": self.enc_key, "sig": self.sig,
        })
    }
}

/// Everyone on the server, with their encryption keys where published.
async fn all_members(pool: &SqlitePool) -> Vec<Member> {
    let enc = enc_keys(pool).await;
    sqlx::query("SELECT username, public_key FROM users").fetch_all(pool).await.unwrap_or_default()
        .iter().map(|r| {
            let public_key = r.get::<String, _>("public_key").to_lowercase();
            let (enc_key, sig) = match enc.get(&public_key) {
                Some((k, s)) => (Some(k.clone()), Some(s.clone())),
                None => (None, None),
            };
            Member { username: r.get("username"), public_key, enc_key, sig }
        }).collect()
}

/// identity key → (encryption key, signature), including people who've
/// since left (their old boxes still name them as the sender).
async fn enc_keys(pool: &SqlitePool) -> HashMap<String, (String, String)> {
    sqlx::query("SELECT public_key, enc_key, sig FROM user_enc_keys").fetch_all(pool).await.unwrap_or_default()
        .iter().map(|r| (r.get::<String, _>("public_key"), (r.get::<String, _>("enc_key"), r.get::<String, _>("sig"))))
        .collect()
}

/// What each member can see (None = everything), worked out once per request.
async fn visibility(pool: &SqlitePool, members: &[Member]) -> HashMap<String, Option<Visible>> {
    let mut out = HashMap::new();
    for m in members {
        out.insert(m.username.clone(), crate::access::visible_for(pool, &m.username).await);
    }
    out
}

/// The key for status blobs (online/away/invisible and what game — see
/// ws.rs "presence"): one for the whole server, so every member counts as
/// seeing it, and it's replaced when someone leaves, like a channel's.
pub const PRESENCE_KEY_ID: &str = "@presence";

fn sees(vis: &HashMap<String, Option<Visible>>, username: &str, board_id: &str) -> bool {
    if board_id == PRESENCE_KEY_ID {
        return vis.contains_key(username);
    }
    match vis.get(username) {
        Some(Some(v)) => v.boards.contains(board_id),
        Some(None) => true,
        None => false,
    }
}

async fn board_exists(pool: &SqlitePool, board_id: &str) -> bool {
    if board_id == PRESENCE_KEY_ID { return true; }
    sqlx::query("SELECT 1 FROM boards WHERE id = ?").bind(board_id)
        .fetch_optional(pool).await.ok().flatten().is_some()
}

async fn latest_generation(pool: &SqlitePool, board_id: &str) -> i64 {
    sqlx::query("SELECT COALESCE(MAX(generation), 0) AS g FROM channel_keys WHERE board_id = ?")
        .bind(board_id).fetch_one(pool).await.map(|r| r.get::<i64, _>("g")).unwrap_or(0)
}

fn changed(s: &AppState, board_id: Option<&str>) {
    let _ = s.tx.send(serde_json::json!({ "type": "e2ee_changed", "board_id": board_id }).to_string());
}

// ── PUT /api/e2ee/enc-key ─────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct EncKeyReq {
    enc_key: String,
    sig:     String,
}

pub async fn put_enc_key(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    Json(b): Json<EncKeyReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let (_user, my_key) = me(&headers, &s).await?;
    let enc = hex::decode(&b.enc_key).map_err(|_| bad("Invalid encryption key"))?;
    let sig = hex::decode(&b.sig).map_err(|_| bad("Invalid signature"))?;
    if enc.len() != 32 { return Err(bad("Invalid encryption key")); }
    let id = hex::decode(&my_key).map_err(|_| bad("Invalid identity key"))?;
    let vk = VerifyingKey::from_bytes(id.as_slice().try_into().map_err(|_| bad("Invalid identity key"))?)
        .map_err(|_| bad("Invalid identity key"))?;
    let sig = Signature::from_slice(&sig).map_err(|_| bad("Invalid signature"))?;
    let mut msg = ENC_KEY_CONTEXT.to_vec();
    msg.extend_from_slice(&enc);
    vk.verify(&msg, &sig).map_err(|_| bad("The signature doesn't match your identity key"))?;

    let enc_hex = hex::encode(&enc);
    let existing: Option<String> = sqlx::query("SELECT enc_key FROM user_enc_keys WHERE public_key = ?")
        .bind(&my_key).fetch_optional(&s.pool).await.map_err(|_| dberr())?
        .map(|r| r.get("enc_key"));
    if existing.as_deref() == Some(enc_hex.as_str()) {
        return Ok(Json(serde_json::json!({ "ok": true }))); // already have it
    }
    sqlx::query("INSERT OR REPLACE INTO user_enc_keys (public_key, enc_key, sig) VALUES (?, ?, ?)")
        .bind(&my_key).bind(&enc_hex).bind(b.sig.to_lowercase())
        .execute(&s.pool).await.map_err(|_| dberr())?;
    changed(&s, None); // others can now share channel keys with them
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── GET /api/boards/:id/keys ──────────────────────────────────────────────────

pub async fn get_keys(
    headers: HeaderMap,
    Path(board_id): Path<String>,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let (user, my_key) = me(&headers, &s).await?;
    if board_id != PRESENCE_KEY_ID { crate::access::require_board(&s.pool, &user, &board_id).await?; }
    if !board_exists(&s.pool, &board_id).await { return Err(err(StatusCode::NOT_FOUND, "Channel not found")); }

    let all = all_members(&s.pool).await;
    let vis = visibility(&s.pool, &all).await;
    let members: Vec<&Member> = all.iter().filter(|m| sees(&vis, &m.username, &board_id)).collect();
    let member_keys: HashSet<&str> = members.iter().map(|m| m.public_key.as_str()).collect();

    let latest = latest_generation(&s.pool, &board_id).await;
    // Someone who has the newest generation can't see the channel any more
    // (kicked, banned, a role or channel change): it needs a new one.
    let holders = sqlx::query("SELECT recipient FROM channel_key_boxes WHERE board_id = ? AND generation = ?")
        .bind(&board_id).bind(latest).fetch_all(&s.pool).await.map_err(|_| dberr())?;
    let rotate = holders.iter().any(|r| !member_keys.contains(r.get::<String, _>("recipient").as_str()));

    let enc = enc_keys(&s.pool).await;
    let names: HashMap<&str, &str> = all.iter().map(|m| (m.public_key.as_str(), m.username.as_str())).collect();
    let boxes: Vec<serde_json::Value> = sqlx::query(
        "SELECT generation, sender, box FROM channel_key_boxes WHERE board_id = ? AND recipient = ? ORDER BY generation",
    ).bind(&board_id).bind(&my_key).fetch_all(&s.pool).await.map_err(|_| dberr())?
        .iter().map(|r| {
            let sender: String = r.get("sender");
            let (enc_key, sig) = match enc.get(&sender) {
                Some((k, s)) => (Some(k.clone()), Some(s.clone())),
                None => (None, None),
            };
            serde_json::json!({
                "generation": r.get::<i64, _>("generation"),
                "box": r.get::<String, _>("box"),
                "sender": {
                    "username": names.get(sender.as_str()).copied().unwrap_or(""),
                    "public_key": sender, "enc_key": enc_key, "sig": sig,
                },
            })
        }).collect();

    Ok(Json(serde_json::json!({
        "latest": latest,
        "rotate": rotate,
        "boxes": boxes,
        "members": members.iter().map(|m| m.json()).collect::<Vec<_>>(),
    })))
}

// ── POST /api/boards/:id/keys ─────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct BoxIn {
    recipient: String,
    #[serde(rename = "box")]
    sealed: String,
}

#[derive(Deserialize)]
pub struct NewGenerationReq {
    generation: i64,
    boxes: Vec<BoxIn>,
}

pub async fn new_generation(
    headers: HeaderMap,
    Path(board_id): Path<String>,
    State(s): State<Arc<AppState>>,
    Json(b): Json<NewGenerationReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let (user, my_key) = me(&headers, &s).await?;
    if board_id != PRESENCE_KEY_ID { crate::access::require_board(&s.pool, &user, &board_id).await?; }
    if !board_exists(&s.pool, &board_id).await { return Err(err(StatusCode::NOT_FOUND, "Channel not found")); }
    if b.boxes.len() > MAX_BOXES || b.boxes.iter().any(|x| x.sealed.len() > MAX_BOX_LEN) {
        return Err(bad("Too many or too large keys"));
    }
    if !b.boxes.iter().any(|x| x.recipient.to_lowercase() == my_key) {
        return Err(bad("A new key has to include a copy for you"));
    }
    let latest = latest_generation(&s.pool, &board_id).await;
    if b.generation != latest + 1 {
        return Err(err(StatusCode::CONFLICT, "This channel's key just changed — try again"));
    }

    let all = all_members(&s.pool).await;
    let vis = visibility(&s.pool, &all).await;
    // Only members who can see the channel and have published a key.
    let allowed: HashSet<&str> = all.iter()
        .filter(|m| m.enc_key.is_some() && sees(&vis, &m.username, &board_id))
        .map(|m| m.public_key.as_str()).collect();

    let mut tx = s.pool.begin().await.map_err(|_| dberr())?;
    // The primary key makes this fail if someone else made this generation first.
    let made = sqlx::query("INSERT INTO channel_keys (board_id, generation, created_by, created_at) VALUES (?, ?, ?, ?)")
        .bind(&board_id).bind(b.generation).bind(&my_key).bind(chrono::Utc::now().to_rfc3339())
        .execute(&mut *tx).await;
    if made.is_err() {
        return Err(err(StatusCode::CONFLICT, "This channel's key just changed — try again"));
    }
    let mut count = 0;
    for x in &b.boxes {
        let recipient = x.recipient.to_lowercase();
        if !allowed.contains(recipient.as_str()) { continue; }
        sqlx::query("INSERT OR IGNORE INTO channel_key_boxes (board_id, generation, recipient, sender, box) VALUES (?, ?, ?, ?, ?)")
            .bind(&board_id).bind(b.generation).bind(&recipient).bind(&my_key).bind(&x.sealed)
            .execute(&mut *tx).await.map_err(|_| dberr())?;
        count += 1;
    }
    tx.commit().await.map_err(|_| dberr())?;
    tracing::info!("e2ee: {user} started key generation {} for board {board_id} ({count} member(s))", b.generation);
    changed(&s, Some(board_id.as_str()));
    Ok(Json(serde_json::json!({ "ok": true, "generation": b.generation })))
}

// ── GET /api/e2ee/pending ─────────────────────────────────────────────────────

pub async fn pending(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let (user, my_key) = me(&headers, &s).await?;
    // The generations I hold, per board.
    let mut mine: HashMap<String, Vec<i64>> = HashMap::new();
    for r in sqlx::query("SELECT board_id, generation FROM channel_key_boxes WHERE recipient = ?")
        .bind(&my_key).fetch_all(&s.pool).await.map_err(|_| dberr())? {
        mine.entry(r.get("board_id")).or_default().push(r.get("generation"));
    }
    if mine.is_empty() {
        return Ok(Json(serde_json::json!({ "pending": [] })));
    }
    let all = all_members(&s.pool).await;
    let vis = visibility(&s.pool, &all).await;

    let mut out: Vec<serde_json::Value> = Vec::new();
    'boards: for (board_id, gens) in &mine {
        if !sees(&vis, &user, board_id) { continue; }
        let have: HashSet<(i64, String)> = sqlx::query("SELECT generation, recipient FROM channel_key_boxes WHERE board_id = ?")
            .bind(board_id).fetch_all(&s.pool).await.map_err(|_| dberr())?
            .iter().map(|r| (r.get::<i64, _>("generation"), r.get::<String, _>("recipient"))).collect();
        for m in all.iter().filter(|m| m.enc_key.is_some() && sees(&vis, &m.username, board_id)) {
            for g in gens {
                if !have.contains(&(*g, m.public_key.clone())) {
                    out.push(serde_json::json!({ "board_id": board_id, "generation": g, "recipient": m.json() }));
                    if out.len() >= MAX_PENDING { break 'boards; }
                }
            }
        }
    }
    Ok(Json(serde_json::json!({ "pending": out })))
}

// ── POST /api/e2ee/boxes ──────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PendingBox {
    board_id:   String,
    generation: i64,
    recipient:  String,
    #[serde(rename = "box")]
    sealed: String,
}

#[derive(Deserialize)]
pub struct BoxesReq {
    boxes: Vec<PendingBox>,
}

pub async fn add_boxes(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    Json(b): Json<BoxesReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let (user, my_key) = me(&headers, &s).await?;
    if b.boxes.len() > MAX_BOXES { return Err(bad("Too many keys at once")); }
    let all = all_members(&s.pool).await;
    let vis = visibility(&s.pool, &all).await;
    let by_key: HashMap<&str, &Member> = all.iter().map(|m| (m.public_key.as_str(), m)).collect();

    let mut added = 0;
    let mut boards: HashSet<String> = HashSet::new();
    for x in &b.boxes {
        if x.sealed.len() > MAX_BOX_LEN || !sees(&vis, &user, &x.board_id) { continue; }
        // Only someone who holds that generation can pass it on.
        let holds = sqlx::query("SELECT 1 FROM channel_key_boxes WHERE board_id = ? AND generation = ? AND recipient = ?")
            .bind(&x.board_id).bind(x.generation).bind(&my_key)
            .fetch_optional(&s.pool).await.map_err(|_| dberr())?.is_some();
        if !holds { continue; }
        // And only to someone who can see the channel and has published a key.
        let recipient = x.recipient.to_lowercase();
        let Some(m) = by_key.get(recipient.as_str()) else { continue };
        if m.enc_key.is_none() || !sees(&vis, &m.username, &x.board_id) { continue; }
        let r = sqlx::query("INSERT OR IGNORE INTO channel_key_boxes (board_id, generation, recipient, sender, box) VALUES (?, ?, ?, ?, ?)")
            .bind(&x.board_id).bind(x.generation).bind(&recipient).bind(&my_key).bind(&x.sealed)
            .execute(&s.pool).await.map_err(|_| dberr())?;
        if r.rows_affected() > 0 {
            added += 1;
            boards.insert(x.board_id.clone());
        }
    }
    if added > 0 {
        tracing::info!("e2ee: {user} shared {added} channel key(s)");
        for board_id in &boards { changed(&s, Some(board_id.as_str())); }
    }
    Ok(Json(serde_json::json!({ "ok": true, "added": added })))
}
