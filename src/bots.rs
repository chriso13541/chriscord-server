// bots.rs — bots: identities that join with a certificate from the person
// who made them.
//
// A bot is an Ed25519 key, like any member's. What makes it a bot is its
// *certificate*: the bot's key, name and description and its maker's key,
// signed by the maker's key (the app makes these in Settings → Bots). A
// bot always joins with its certificate, so the server always knows it's a
// bot, and a key that was added as a bot can never join without one.
//
// A bot gets in only when someone with Manage Bots adds it here (from its
// "Add to server" link). Never with the join key, an invite or the owner
// key. Whoever adds it chooses:
//   - what it may do: never Administrator or Manage Bots, never anything the
//     person adding it can't do themselves, and Import Messages only from
//     the owner;
//   - which channels it sees: none until picked. Its roles, @everyone and
//     View Channels don't apply to it.
// Bots are also held to rate limits a person never notices (below), and
// there's a cap on how many a server has (25 unless the owner changes it).
//
// The maker can revoke a bot by signing a revocation with their key: the
// server drops it and refuses its key from then on.
//
//   GET    /api/bots                  every bot here (any member)
//   POST   /api/bots                  add one {cert, permissions, channels, username?} (Manage Bots)
//   PUT    /api/bots/:public_key      change {permissions?, channels?} (Manage Bots)
//   DELETE /api/bots/:public_key      remove it (Manage Bots)
//   POST   /api/bots/revoke           a maker's signed revocation (no session needed)
//
// What's signed must match botproto.go in the app and the bot SDK.

use axum::{
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::{access::Visible, db, roles, state::AppState};

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn err(code: StatusCode, msg: &str) -> ApiErr { (code, Json(serde_json::json!({ "error": msg }))) }
fn dberr() -> ApiErr { err(StatusCode::INTERNAL_SERVER_ERROR, "DB error") }
fn bad(msg: &str) -> ApiErr { err(StatusCode::BAD_REQUEST, msg) }

/// Never given to a bot, whatever anyone asks for.
pub const BOT_NEVER: u64 = roles::ADMINISTRATOR | roles::MANAGE_BOTS;
/// How many bots a server takes until the owner says otherwise.
pub const DEFAULT_CAP: i64 = 25;
pub const MAX_CAP: i64 = 1000;

/// Rate limits (see the Bot Rules): messages per channel, requests overall,
/// and how soon a bot may connect again.
const MSG_LIMIT: usize = 5;
const MSG_WINDOW: Duration = Duration::from_secs(5);
pub const REQ_PER_SEC: u32 = 50;
/// Past this many in a second on its connection, a bot is disconnected.
pub const REQ_DISCONNECT: u32 = 150;
const RECONNECT_GAP: Duration = Duration::from_secs(1);

// ── What's signed ────────────────────────────────────────────────────────────

const PROTO: &str = "chriscord-bot-v1";

/// "chriscord-bot-v1/<action>", then each field after a zero byte. Fields
/// can't contain a zero byte (checked before signing and verifying), so
/// every message reads only one way.
pub fn sig_message(action: &str, fields: &[&str]) -> Vec<u8> {
    let mut m = Vec::with_capacity(64);
    m.extend_from_slice(PROTO.as_bytes());
    m.push(b'/');
    m.extend_from_slice(action.as_bytes());
    for f in fields {
        m.push(0);
        m.extend_from_slice(f.as_bytes());
    }
    m
}

fn verify(pub_hex: &str, sig_hex: &str, msg: &[u8]) -> bool {
    let (Ok(pk), Ok(sig)) = (hex::decode(pub_hex), hex::decode(sig_hex)) else { return false };
    let Ok(pk): Result<[u8; 32], _> = pk.as_slice().try_into() else { return false };
    let Ok(vk) = VerifyingKey::from_bytes(&pk) else { return false };
    let Ok(sig) = Signature::from_slice(&sig) else { return false };
    vk.verify(msg, &sig).is_ok()
}

fn is_key(k: &str) -> bool {
    k.len() == 64 && k.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A bot's certificate, as the app makes it.
#[derive(Deserialize, Serialize, Clone)]
pub struct BotCert {
    pub v:           u32,
    pub bot_key:     String,
    pub name:        String,
    #[serde(default)]
    pub description: String,
    pub maker_key:   String,
    pub created:     i64,
    pub sig:         String,
}

pub fn valid_name(n: &str) -> bool {
    let count = n.chars().count();
    n.trim() == n && (2..=32).contains(&count) && !n.chars().any(|c| c.is_control())
}

impl BotCert {
    pub(crate) fn message(&self) -> Vec<u8> {
        let created = self.created.to_string();
        sig_message("cert", &[self.bot_key.as_str(), self.name.as_str(), self.description.as_str(), self.maker_key.as_str(), created.as_str()])
    }

    /// Everything a certificate must be. The error says what's wrong.
    pub fn check(&self) -> Result<(), &'static str> {
        if self.v != 1 { return Err("This bot's certificate is from a newer version of chriscord"); }
        if !is_key(&self.bot_key) || !is_key(&self.maker_key) { return Err("The bot's certificate has a malformed key"); }
        if self.bot_key == self.maker_key { return Err("A bot needs its own key, not its maker's"); }
        if !valid_name(&self.name) { return Err("A bot's name is 2–32 characters"); }
        if self.description.chars().count() > 190 || self.description.chars().any(|c| c == '\0') {
            return Err("A bot's description is up to 190 characters");
        }
        if !verify(&self.maker_key, &self.sig, &self.message()) {
            return Err("The bot's certificate isn't signed by its maker");
        }
        Ok(())
    }
}

/// A maker's "this bot is retired".
#[derive(Deserialize)]
pub struct Revocation {
    pub bot_key:   String,
    pub maker_key: String,
    pub at:        i64,
    pub sig:       String,
}

impl Revocation {
    fn valid(&self) -> bool {
        let at = self.at.to_string();
        is_key(&self.bot_key) && is_key(&self.maker_key)
            && verify(&self.maker_key, &self.sig, &sig_message("revoke", &[self.bot_key.as_str(), self.maker_key.as_str(), at.as_str()]))
    }
}

// ── Tables ───────────────────────────────────────────────────────────────────

pub async fn init(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    // The bot's name here is its users row (public_key → username), like anyone's.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS bots (
            public_key  TEXT PRIMARY KEY,
            maker_key   TEXT NOT NULL,
            name        TEXT NOT NULL,         -- the name in its certificate
            description TEXT NOT NULL DEFAULT '',
            cert        TEXT NOT NULL,         -- the certificate it was added with (JSON)
            permissions INTEGER NOT NULL DEFAULT 0,
            added_by    TEXT NOT NULL,
            added_at    TEXT NOT NULL,
            client      TEXT NOT NULL DEFAULT '' -- what it said it was when it last joined
        )",
    ).execute(pool).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS bot_channels (
            public_key TEXT NOT NULL,
            board_id   TEXT NOT NULL,
            PRIMARY KEY (public_key, board_id)
        )",
    ).execute(pool).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS bot_revocations (
            public_key TEXT PRIMARY KEY,
            maker_key  TEXT NOT NULL,
            revoked_at INTEGER NOT NULL,
            sig        TEXT NOT NULL
        )",
    ).execute(pool).await?;
    // Bots' sessions from before a restart keep their rate limits.
    let rows = sqlx::query("SELECT s.token AS token, s.public_key AS public_key FROM sessions s JOIN bots b ON b.public_key = s.public_key")
        .fetch_all(pool).await?;
    let mut l = live();
    for r in rows { l.tokens.insert(r.get("token"), r.get("public_key")); }
    Ok(())
}

// ── Rate limits and sessions (in memory) ─────────────────────────────────────

#[derive(Default)]
struct Live {
    /// session token → bot key, for every bot session
    tokens:  HashMap<String, String>,
    /// bot key → when it last opened a connection (the WebSocket)
    connect: HashMap<String, Instant>,
    /// bot key → when it last signed in (/api/join)
    joined:  HashMap<String, Instant>,
    /// session token → (this second's start, requests in it)
    reqs:    HashMap<String, (Instant, u32)>,
    /// (bot username, channel) → when its recent messages were sent
    msgs:    HashMap<(String, String), VecDeque<Instant>>,
}

fn live() -> std::sync::MutexGuard<'static, Live> {
    static L: OnceLock<Mutex<Live>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(Live::default())).lock().unwrap_or_else(|e| e.into_inner())
}

pub fn is_bot_token(token: &str) -> bool { live().tokens.contains_key(token) }

/// A bot connecting again too soon: how long it has to wait.
pub fn connect_wait(token: &str) -> Option<Duration> {
    let mut l = live();
    let key = l.tokens.get(token)?.clone();
    let now = Instant::now();
    if let Some(last) = l.connect.get(&key) {
        let since = now.duration_since(*last);
        if since < RECONNECT_GAP { return Some(RECONNECT_GAP - since); }
    }
    l.connect.insert(key, now);
    None
}

/// Counts one request for a bot session: false = over the limit.
pub fn allow_request(token: &str) -> bool {
    let mut l = live();
    if !l.tokens.contains_key(token) { return true; }
    let now = Instant::now();
    let e = l.reqs.entry(token.to_string()).or_insert((now, 0));
    if now.duration_since(e.0) >= Duration::from_secs(1) { *e = (now, 0); }
    e.1 += 1;
    e.1 <= REQ_PER_SEC
}

/// Counts one message from a bot in a channel: false = over the limit.
pub fn allow_message(username: &str, board_id: &str) -> bool {
    let mut l = live();
    let now = Instant::now();
    let q = l.msgs.entry((username.to_string(), board_id.to_string())).or_default();
    while q.front().map_or(false, |t| now.duration_since(*t) >= MSG_WINDOW) { q.pop_front(); }
    if q.len() >= MSG_LIMIT { return false; }
    q.push_back(now);
    true
}

/// Middleware for the HTTP API: bots past 50 requests a second get 429.
pub async fn rate_limit(req: Request, next: Next) -> Response {
    let token = req.headers().get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !token.is_empty() && !allow_request(token) {
        let mut r = (StatusCode::TOO_MANY_REQUESTS, Json(serde_json::json!({ "error": "Too many requests — slow down" }))).into_response();
        r.headers_mut().insert("Retry-After", HeaderValue::from_static("1"));
        return r;
    }
    next.run(req).await
}

// ── Lookups used everywhere else ─────────────────────────────────────────────

/// A bot's key, by its name here (None = not a bot).
pub async fn key_of(pool: &SqlitePool, username: &str) -> Option<String> {
    sqlx::query("SELECT b.public_key AS k FROM bots b JOIN users u ON u.public_key = b.public_key WHERE u.username = ?")
        .bind(username).fetch_optional(pool).await.ok().flatten().map(|r| r.get("k"))
}

pub async fn is_bot(pool: &SqlitePool, username: &str) -> bool { key_of(pool, username).await.is_some() }

/// What a bot may do (None = not a bot). Only what it was given: no
/// @everyone, no roles, and never BOT_NEVER.
pub async fn permissions(pool: &SqlitePool, username: &str) -> Option<u64> {
    sqlx::query("SELECT b.permissions AS p FROM bots b JOIN users u ON u.public_key = b.public_key WHERE u.username = ?")
        .bind(username).fetch_optional(pool).await.ok().flatten()
        .map(|r| (r.get::<i64, _>("p") as u64) & roles::ALL & !BOT_NEVER)
}

async fn channels_of(pool: &SqlitePool, key: &str) -> HashSet<String> {
    sqlx::query("SELECT c.board_id AS b FROM bot_channels c JOIN boards ON boards.id = c.board_id WHERE c.public_key = ?")
        .bind(key).fetch_all(pool).await.unwrap_or_default()
        .iter().map(|r| r.get::<String, _>("b")).collect()
}

/// What a bot sees (None = not a bot): the channels it was given, and the
/// categories they're in. Nothing else, whether private or not.
pub async fn visible_for(pool: &SqlitePool, username: &str) -> Option<Visible> {
    let key = key_of(pool, username).await?;
    let boards = channels_of(pool, &key).await;
    let mut rooms = HashSet::new();
    for r in sqlx::query("SELECT id, room_id FROM boards").fetch_all(pool).await.unwrap_or_default() {
        if boards.contains(&r.get::<String, _>("id")) { rooms.insert(r.get::<String, _>("room_id")); }
    }
    Some(Visible { rooms, boards })
}

/// For the member list: bot name → {maker_key, maker (their name here, if
/// they're a member), name (its own), description}.
pub async fn tags(pool: &SqlitePool) -> serde_json::Map<String, serde_json::Value> {
    sqlx::query("SELECT u.username AS username, b.maker_key AS maker_key, b.name AS name, b.description AS description,
                        m.username AS maker, EXISTS(SELECT 1 FROM plugins p WHERE p.bot_key = b.public_key) AS plugin
                 FROM bots b JOIN users u ON u.public_key = b.public_key LEFT JOIN users m ON m.public_key = b.maker_key")
        .fetch_all(pool).await.unwrap_or_default().iter().map(|r| (r.get::<String, _>("username"), serde_json::json!({
            "maker_key": r.get::<String, _>("maker_key"), "name": r.get::<String, _>("name"),
            "description": r.get::<String, _>("description"),
            "maker": r.try_get::<Option<String>, _>("maker").ok().flatten().unwrap_or_default(),
            // A plugin this server runs (plugins.rs): the app says so.
            "plugin": r.try_get::<i64, _>("plugin").unwrap_or(0) != 0,
        }))).collect()
}

/// Forgets a bot that's no longer here (kicked, banned or removed — see
/// admin::kick_user, which deletes its member row).
pub async fn forget(pool: &SqlitePool, key: &str) {
    let _ = sqlx::query("DELETE FROM bots WHERE public_key = ?").bind(key).execute(pool).await;
    let _ = sqlx::query("DELETE FROM bot_channels WHERE public_key = ?").bind(key).execute(pool).await;
    let mut l = live();
    let gone: Vec<String> = l.tokens.iter().filter(|(_, k)| k.as_str() == key).map(|(t, _)| t.clone()).collect();
    for t in gone { l.tokens.remove(&t); l.reqs.remove(&t); }
}

pub async fn cap(pool: &SqlitePool) -> i64 {
    db::get_config(pool, "bot_cap").await.ok().flatten().and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_CAP).clamp(0, MAX_CAP)
}

async fn revoked(pool: &SqlitePool, key: &str) -> bool {
    sqlx::query("SELECT 1 FROM bot_revocations WHERE public_key = ?").bind(key)
        .fetch_optional(pool).await.ok().flatten().is_some()
}

// ── Joining ──────────────────────────────────────────────────────────────────

/// A bot's join (auth.rs, after its challenge signature has been checked):
/// its name here, or why it can't come in.
pub async fn join(s: &AppState, cert: &BotCert, public_key: &str, client: Option<&str>) -> Result<String, ApiErr> {
    cert.check().map_err(|m| err(StatusCode::UNAUTHORIZED, m))?;
    if cert.bot_key != public_key.to_lowercase() {
        return Err(err(StatusCode::UNAUTHORIZED, "That certificate is for a different bot"));
    }
    let client: String = client.unwrap_or("").trim().chars().filter(|c| !c.is_control()).take(64).collect();
    if client.is_empty() {
        return Err(bad("A bot has to say what it is when it joins (its name and version, e.g. \"chriscord-musicbot/0.5.0\")"));
    }
    if revoked(&s.pool, &cert.bot_key).await {
        return Err(err(StatusCode::FORBIDDEN, "This bot was revoked by its maker"));
    }
    let row = sqlx::query("SELECT b.maker_key AS maker_key, u.username AS username FROM bots b JOIN users u ON u.public_key = b.public_key WHERE b.public_key = ?")
        .bind(&cert.bot_key).fetch_optional(&s.pool).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::FORBIDDEN, "This bot hasn't been added to this server. Someone with Manage Bots has to add it first, from its Add to server link."))?;
    if row.get::<String, _>("maker_key") != cert.maker_key {
        return Err(err(StatusCode::FORBIDDEN, "This certificate is from a different maker than the bot that was added here"));
    }
    {
        let mut l = live();
        let now = Instant::now();
        if let Some(last) = l.joined.get(&cert.bot_key) {
            if now.duration_since(*last) < RECONNECT_GAP {
                return Err(err(StatusCode::TOO_MANY_REQUESTS, "Signing in again too quickly — wait a second"));
            }
        }
        l.joined.insert(cert.bot_key.clone(), now);
    }
    let _ = sqlx::query("UPDATE bots SET client = ?, cert = ? WHERE public_key = ?")
        .bind(&client).bind(serde_json::to_string(cert).unwrap_or_default()).bind(&cert.bot_key)
        .execute(&s.pool).await;
    tracing::info!("bots: {} joined ({client})", row.get::<String, _>("username"));
    Ok(row.get("username"))
}

/// A session was made for a bot (auth.rs).
pub fn session_started(token: &str, key: &str) {
    live().tokens.insert(token.to_string(), key.to_lowercase());
}

// ── Handlers ─────────────────────────────────────────────────────────────────

async fn me(h: &HeaderMap, s: &AppState) -> Result<String, ApiErr> {
    let token = h.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    db::verify_token(&s.pool, token).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Unauthorized"))
}

async fn manager(h: &HeaderMap, s: &AppState) -> Result<String, ApiErr> {
    let user = me(h, s).await?;
    if is_bot(&s.pool, &user).await {
        return Err(err(StatusCode::FORBIDDEN, "Bots can't manage bots"));
    }
    roles::require(&s.pool, &user, roles::MANAGE_BOTS, "manage bots").await?;
    Ok(user)
}

/// The permissions `user` may hand to a bot.
async fn grantable(pool: &SqlitePool, user: &str) -> u64 {
    let mut p = roles::permissions_of(pool, user).await & !BOT_NEVER;
    if crate::admin::owner_username(pool).await.as_deref() != Some(user) {
        p &= !roles::IMPORT_MESSAGES;
    }
    p
}

pub(crate) fn changed(s: &Arc<AppState>) {
    crate::ws::broadcast_users(s);
    // Connections work out again what each can see (and the apps share or
    // rotate channel keys to match).
    let _ = s.tx.send(serde_json::json!({ "type": "roles_updated" }).to_string());
    let _ = s.tx.send(serde_json::json!({ "type": "rooms_updated" }).to_string());
    let _ = s.tx.send(serde_json::json!({ "type": "e2ee_changed", "board_id": null }).to_string());
    let _ = s.tx.send(serde_json::json!({ "type": "bots_updated" }).to_string());
}

/// GET /api/bots — every bot here, what it may do and which channels it
/// sees (only the channels you can see too; the rest are counted).
pub async fn list(headers: HeaderMap, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    let user = me(&headers, &s).await?;
    let mine = crate::access::visible_for(&s.pool, &user).await;
    let online: HashSet<String> = s.online.lock().unwrap().keys().cloned().collect();
    let rows = sqlx::query(
        "SELECT b.public_key AS public_key, u.username AS username, b.maker_key AS maker_key, b.name AS name,
                b.description AS description, b.permissions AS permissions, b.added_by AS added_by,
                b.added_at AS added_at, b.client AS client, m.username AS maker,
                EXISTS(SELECT 1 FROM plugins p WHERE p.bot_key = b.public_key) AS plugin
         FROM bots b JOIN users u ON u.public_key = b.public_key LEFT JOIN users m ON m.public_key = b.maker_key
         ORDER BY u.username COLLATE NOCASE",
    ).fetch_all(&s.pool).await.map_err(|_| dberr())?;
    let mut bots = Vec::new();
    for r in rows {
        let key: String = r.get("public_key");
        let username: String = r.get("username");
        let chans = channels_of(&s.pool, &key).await;
        let (shown, hidden): (Vec<String>, Vec<String>) = chans.into_iter()
            .partition(|b| mine.as_ref().map_or(true, |v| v.boards.contains(b)));
        bots.push(serde_json::json!({
            "public_key": key, "username": username, "name": r.get::<String, _>("name"),
            "maker_key": r.get::<String, _>("maker_key"), "description": r.get::<String, _>("description"),
            "maker": r.try_get::<Option<String>, _>("maker").ok().flatten().unwrap_or_default(),
            "permissions": (r.get::<i64, _>("permissions") as u64) & roles::ALL & !BOT_NEVER,
            "channels": shown, "hidden_channels": hidden.len(),
            "added_by": r.get::<String, _>("added_by"), "added_at": r.get::<String, _>("added_at"),
            "client": r.get::<String, _>("client"), "online": online.contains(&username),
            "plugin": r.try_get::<i64, _>("plugin").unwrap_or(0) != 0,
        }));
    }
    let can_manage = !is_bot(&s.pool, &user).await && roles::has(&s.pool, &user, roles::MANAGE_BOTS).await;
    let grant = if can_manage { grantable(&s.pool, &user).await } else { 0 };
    let cap = cap(&s.pool).await;
    Ok(Json(serde_json::json!({
        "bots": bots, "cap": cap, "can_manage": can_manage, "grantable": grant, "never": BOT_NEVER,
    })))
}

#[derive(Deserialize)]
pub struct AddReq {
    pub cert:        BotCert,
    #[serde(default)]
    pub permissions: u64,
    #[serde(default)]
    pub channels:    Vec<String>,
    /// What it's called here, if not its own name (when that's taken).
    #[serde(default)]
    pub username:    Option<String>,
}

/// POST /api/bots — add a bot.
pub async fn add(headers: HeaderMap, State(s): State<Arc<AppState>>, Json(b): Json<AddReq>) -> Result<Json<serde_json::Value>, ApiErr> {
    let user = manager(&headers, &s).await?;
    b.cert.check().map_err(bad)?;
    let key = b.cert.bot_key.clone();
    if revoked(&s.pool, &key).await { return Err(bad("This bot was revoked by its maker")); }
    let banned = sqlx::query("SELECT 1 FROM bans WHERE public_key = ?").bind(&key)
        .fetch_optional(&s.pool).await.map_err(|_| dberr())?.is_some();
    if banned { return Err(bad("This bot is banned from this server. Lift the ban in the admin panel first.")); }
    let existing = sqlx::query("SELECT username FROM users WHERE public_key = ?").bind(&key)
        .fetch_optional(&s.pool).await.map_err(|_| dberr())?;
    if existing.is_some() {
        return Err(if is_bot_key(&s.pool, &key).await {
            err(StatusCode::CONFLICT, "This bot is already here — change it instead")
        } else {
            bad("That key already belongs to a member here, so it can't be added as a bot")
        });
    }
    let count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM bots").fetch_one(&s.pool).await.map_err(|_| dberr())?.get("n");
    let cap = cap(&s.pool).await;
    if count >= cap {
        return Err(bad(&format!("This server has {count} bots, its limit. The owner can change the limit in the admin panel.")));
    }
    let name = b.username.as_deref().map(str::trim).filter(|n| !n.is_empty()).unwrap_or(b.cert.name.as_str()).to_string();
    if !valid_name(&name) { return Err(bad("A bot's name is 2–32 characters")); }
    let taken = sqlx::query("SELECT 1 FROM users WHERE username = ?").bind(&name)
        .fetch_optional(&s.pool).await.map_err(|_| dberr())?.is_some();
    if taken {
        return Err(err(StatusCode::CONFLICT, &format!("There's already a member called {name} — choose another name for the bot here")));
    }
    let perms = b.permissions & grantable(&s.pool, &user).await;
    let mine = crate::access::visible_for(&s.pool, &user).await;
    let mut channels = Vec::new();
    for c in b.channels.iter().take(500) {
        let exists = sqlx::query("SELECT 1 FROM boards WHERE id = ?").bind(c).fetch_optional(&s.pool).await.map_err(|_| dberr())?.is_some();
        if exists && mine.as_ref().map_or(true, |v| v.boards.contains(c)) && !channels.contains(c) { channels.push(c.clone()); }
    }

    // An imported (Discord) member going by this name gets another one, as
    // for anyone joining (auth.rs).
    crate::imports::make_way_for(&s, &name).await;
    let now = chrono::Utc::now().to_rfc3339();
    let mut tx = s.pool.begin().await.map_err(|_| dberr())?;
    sqlx::query("INSERT INTO users (public_key, username, created_at) VALUES (?, ?, ?)")
        .bind(&key).bind(&name).bind(&now).execute(&mut *tx).await.map_err(|_| dberr())?;
    sqlx::query("INSERT INTO bots (public_key, maker_key, name, description, cert, permissions, added_by, added_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(&key).bind(&b.cert.maker_key).bind(&b.cert.name).bind(&b.cert.description)
        .bind(serde_json::to_string(&b.cert).unwrap_or_default()).bind(perms as i64).bind(&user).bind(&now)
        .execute(&mut *tx).await.map_err(|_| dberr())?;
    for c in &channels {
        sqlx::query("INSERT OR IGNORE INTO bot_channels (public_key, board_id) VALUES (?, ?)").bind(&key).bind(c)
            .execute(&mut *tx).await.map_err(|_| dberr())?;
    }
    tx.commit().await.map_err(|_| dberr())?;
    tracing::info!("bots: {user} added {name} ({} channels)", channels.len());
    changed(&s);
    Ok(Json(serde_json::json!({ "ok": true, "username": name, "permissions": perms, "channels": channels })))
}

async fn is_bot_key(pool: &SqlitePool, key: &str) -> bool {
    sqlx::query("SELECT 1 FROM bots WHERE public_key = ?").bind(key).fetch_optional(pool).await.ok().flatten().is_some()
}

#[derive(Deserialize)]
pub struct UpdateReq {
    pub permissions: Option<u64>,
    pub channels:    Option<Vec<String>>,
}

/// PUT /api/bots/:public_key — change what a bot may do and see. Whoever
/// changes it can only add or take away what they have themselves; the
/// rest stays as it was.
pub async fn update(headers: HeaderMap, Path(key): Path<String>, State(s): State<Arc<AppState>>, Json(b): Json<UpdateReq>) -> Result<Json<serde_json::Value>, ApiErr> {
    let user = manager(&headers, &s).await?;
    let key = key.to_lowercase();
    let old: u64 = sqlx::query("SELECT permissions FROM bots WHERE public_key = ?").bind(&key)
        .fetch_optional(&s.pool).await.map_err(|_| dberr())?
        .map(|r| r.get::<i64, _>("permissions") as u64)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "No such bot here"))?;
    if let Some(p) = b.permissions {
        let mine = grantable(&s.pool, &user).await;
        let new = (p & mine) | (old & !mine);
        sqlx::query("UPDATE bots SET permissions = ? WHERE public_key = ?").bind(new as i64).bind(&key)
            .execute(&s.pool).await.map_err(|_| dberr())?;
    }
    if let Some(chans) = b.channels {
        let mine = crate::access::visible_for(&s.pool, &user).await;
        let sees = |c: &str| mine.as_ref().map_or(true, |v| v.boards.contains(c));
        let old_chans = channels_of(&s.pool, &key).await;
        let mut keep: HashSet<String> = old_chans.into_iter().filter(|c| !sees(c)).collect();
        for c in chans.iter().take(500) {
            if sees(c) { keep.insert(c.clone()); }
        }
        let mut tx = s.pool.begin().await.map_err(|_| dberr())?;
        sqlx::query("DELETE FROM bot_channels WHERE public_key = ?").bind(&key).execute(&mut *tx).await.map_err(|_| dberr())?;
        for c in &keep {
            sqlx::query("INSERT INTO bot_channels (public_key, board_id) SELECT ?, id FROM boards WHERE id = ?").bind(&key).bind(c)
                .execute(&mut *tx).await.map_err(|_| dberr())?;
        }
        tx.commit().await.map_err(|_| dberr())?;
    }
    tracing::info!("bots: {user} changed {key}");
    changed(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn username_of_key(pool: &SqlitePool, key: &str) -> Option<String> {
    sqlx::query("SELECT username FROM users WHERE public_key = ?").bind(key)
        .fetch_optional(pool).await.ok().flatten().map(|r| r.get("username"))
}

/// DELETE /api/bots/:public_key — remove a bot: it's disconnected now, and
/// can only come back if someone adds it again.
pub async fn remove(headers: HeaderMap, Path(key): Path<String>, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    let user = manager(&headers, &s).await?;
    let key = key.to_lowercase();
    if !is_bot_key(&s.pool, &key).await { return Err(err(StatusCode::NOT_FOUND, "No such bot here")); }
    if crate::plugins::is_plugin_key(&s.pool, &key).await {
        return Err(bad("This is one of the server's plugins — the owner removes it from the admin panel (Plugins)"));
    }
    let name = username_of_key(&s.pool, &key).await;
    match name {
        Some(n) => crate::admin::kick_user(&s, &n, false, "Removed from the server").await?,
        None => forget(&s.pool, &key).await,
    }
    tracing::info!("bots: {user} removed {key}");
    changed(&s);
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/bots/revoke — a maker retiring a bot. Anyone may deliver it:
/// the maker's signature is what counts. Kept only for bots this server
/// has had, so it can't be filled with other keys.
pub async fn revoke(State(s): State<Arc<AppState>>, Json(r): Json<Revocation>) -> Result<Json<serde_json::Value>, ApiErr> {
    let bot_key = r.bot_key.to_lowercase();
    let maker = r.maker_key.to_lowercase();
    if !r.valid() { return Err(err(StatusCode::UNAUTHORIZED, "That revocation isn't signed by the bot's maker")); }
    let here = sqlx::query("SELECT maker_key FROM bots WHERE public_key = ?").bind(&bot_key)
        .fetch_optional(&s.pool).await.map_err(|_| dberr())?.map(|row| row.get::<String, _>("maker_key"));
    match here {
        None => return Ok(Json(serde_json::json!({ "ok": true, "here": false }))),
        Some(m) if m != maker => return Err(err(StatusCode::FORBIDDEN, "Only the bot's maker can revoke it")),
        Some(_) => {}
    }
    sqlx::query("INSERT OR IGNORE INTO bot_revocations (public_key, maker_key, revoked_at, sig) VALUES (?, ?, ?, ?)")
        .bind(&bot_key).bind(&maker).bind(r.at).bind(&r.sig).execute(&s.pool).await.map_err(|_| dberr())?;
    match username_of_key(&s.pool, &bot_key).await {
        Some(n) => crate::admin::kick_user(&s, &n, false, "Revoked by its maker").await?,
        None => forget(&s.pool, &bot_key).await,
    }
    tracing::info!("bots: {bot_key} revoked by its maker");
    changed(&s);
    Ok(Json(serde_json::json!({ "ok": true, "here": true })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    // The same vector is pinned in botproto_test.go (app and SDK).
    #[test]
    fn message_layout() {
        let m = sig_message("cert", &["a", "Music", "", "b", "1700000000"]);
        assert_eq!(m, b"chriscord-bot-v1/cert\0a\0Music\0\0b\01700000000".to_vec());
    }

    #[test]
    fn cert_checks() {
        let maker = SigningKey::from_bytes(&[7u8; 32]);
        let bot = SigningKey::from_bytes(&[9u8; 32]);
        let mut c = BotCert {
            v: 1, bot_key: hex::encode(bot.verifying_key().to_bytes()), name: "Music".into(),
            description: "Plays music".into(), maker_key: hex::encode(maker.verifying_key().to_bytes()),
            created: 1_700_000_000, sig: String::new(),
        };
        c.sig = hex::encode(maker.sign(&c.message()).to_bytes());
        // Pinned in botproto_test.go too (the app and the SDK).
        assert_eq!(c.bot_key, "fd1724385aa0c75b64fb78cd602fa1d991fdebf76b13c58ed702eac835e9f618");
        assert_eq!(c.maker_key, "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c");
        assert_eq!(c.sig, "7c0888bb7cb5cb6c53f1502e44e2fa811920e49772c864473ca007333662e1323933dfed914ebbee982570096d972c9a37e751ba84346e6664cf6728dc30150e");
        assert!(c.check().is_ok());
        let mut forged = c.clone();
        forged.name = "Musik".into();
        assert!(forged.check().is_err());
        let mut self_signed = c.clone();
        self_signed.maker_key = self_signed.bot_key.clone();
        assert!(self_signed.check().is_err());
        assert!(!valid_name("x") && !valid_name(" padded") && valid_name("DJ"));
    }
}
