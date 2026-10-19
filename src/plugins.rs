// plugins.rs — plugins: programs this server runs and looks after.
//
// A plugin is a folder with a plugin.json (its manifest) and the program
// that is the plugin. The owner installs one from the admin panel
// (Plugins), uploading it as a .tar or picking a folder already put in this
// server's plugins/ folder, after seeing what it asks for. Then this server:
//
//   - gives it an identity of its own: a bot key, with a certificate signed
//     by this server's own plugin key (made the first time it's needed). It
//     joins like any bot — a member with a BOT tag, marked as a plugin, that
//     sees only the channels it was given and can do only what it was
//     allowed — but nobody ever handles a token: it's handed to the plugin
//     on its standard input when it starts (never in its environment, where
//     programs it runs would inherit it);
//   - runs it, restarts it when it stops on its own (waiting longer each
//     time it keeps failing), keeps its recent output for the panel, and
//     stops it with the server.
//
// A plugin runs on this machine with this server's access, so installing
// one means trusting whoever wrote it, as with any program. And since it
// runs here, whoever runs this server can see whatever it can: the app says
// so on its tag ("runs on this server").
//
// Each server has its own plugins: they live in its data folder (plugins/
// for their code, plugin-data/ for what they keep), so servers this one
// hosts (hosting.rs) have their own.
//
// The manifest (plugin.json):
//   {
//     "manifest": 1,
//     "id": "music",                       a-z, 0-9 and -, 2–40, its folder name
//     "name": "Music",                     its name here (2–32 characters)
//     "version": "0.1.0",
//     "description": "Plays music in voice channels.",
//     "author": "…", "homepage": "https://…",
//     "run": {                             the program, per platform (OS-arch as
//       "linux-x86_64":  ["bin/music-linux-amd64"],    Rust names them), or "any"
//       "linux-aarch64": ["bin/music-linux-arm64"]
//     },
//     "permissions": ["send_messages", "connect", "speak"],   what it asks for
//     "internet": true,                    whether it needs web sites (else, in
//                                          the sandbox, only this server and DNS)
//     "channels": "Pick the text channels for its commands and the voice
//                  channels it may play in.",   why it wants channels ("" = none)
//     "settings": [ { "key": "prefix", "label": "Command prefix",
//                     "type": "text" | "number" | "bool" | "choice",
//                     "default": "!", "help": "…", "choices": […], "min": 0, "max": 2 } ]
//   }
//
// What it's given when it starts:
//   stdin, one line of JSON: {"chriscord_plugin": 1, "id", "server" (this
//     server's address), "token" (its bot token: ccbot_1_…, for the bot SDK),
//     "settings" {key: value}, "data_dir"} — then stdin closes;
//   environment: CHRISCORD_PLUGIN=1, CHRISCORD_PLUGIN_ID, CHRISCORD_URL,
//     CHRISCORD_PLUGIN_DIR (its code), CHRISCORD_PLUGIN_DATA (where it keeps
//     things; also its working directory). Stopping is SIGTERM, then a kill
//     after 10 s.
//
//   GET    /api/admin/plugins                 installed, found in the folder, and what's needed to install
//   POST   /api/admin/plugins/upload          a plugin as a .tar → staged, for installing
//   POST   /api/admin/plugins/install         {source, permissions, channels, settings}
//   PUT    /api/admin/plugins/:id             {permissions?, channels?, settings?}
//   POST   /api/admin/plugins/:id/start|stop|restart
//   GET    /api/admin/plugins/:id/logs
//   DELETE /api/admin/plugins/:id[?delete_data=1]

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use crate::{admin::check_owner, bots, db, roles, state::AppState};

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn err(code: StatusCode, msg: &str) -> ApiErr { (code, Json(serde_json::json!({ "error": msg }))) }
fn dberr() -> ApiErr { err(StatusCode::INTERNAL_SERVER_ERROR, "DB error") }
fn bad(msg: &str) -> ApiErr { err(StatusCode::BAD_REQUEST, msg) }
fn now() -> String { chrono::Utc::now().to_rfc3339() }

/// Most plugins one server runs, and the biggest upload.
const MAX_PLUGINS: i64 = 50;
pub const MAX_UPLOAD: usize = 512 << 20;
/// Lines of a plugin's output kept for the panel.
const LOG_LINES: usize = 400;
/// An uploaded plugin waits this long to be installed, then it's cleared.
const STAGED_FOR: Duration = Duration::from_secs(30 * 60);

// ── Permissions by name ──────────────────────────────────────────────────────

/// What a manifest may ask for. Never Administrator or Manage Bots (no bot
/// gets those).
const PERMS: &[(&str, u64)] = &[
    ("view_channels", roles::VIEW_CHANNELS),
    ("send_messages", roles::SEND_MESSAGES),
    ("embed_links", roles::EMBED_LINKS),
    ("attach_files", roles::ATTACH_FILES),
    ("add_reactions", roles::ADD_REACTIONS),
    ("read_history", roles::READ_HISTORY),
    ("pin_messages", roles::PIN_MESSAGES),
    ("manage_messages", roles::MANAGE_MESSAGES),
    ("connect", roles::CONNECT),
    ("listen", roles::LISTEN),
    ("speak", roles::SPEAK),
    ("video", roles::VIDEO),
    ("mute_members", roles::MUTE_MEMBERS),
    ("deafen_members", roles::DEAFEN_MEMBERS),
    ("move_members", roles::MOVE_MEMBERS),
    ("manage_channels", roles::MANAGE_CHANNELS),
    ("create_expressions", roles::CREATE_EXPRESSIONS),
    ("manage_expressions", roles::MANAGE_EMOJIS),
    ("create_invite", roles::CREATE_INVITE),
    ("change_nickname", roles::CHANGE_NICKNAME),
    ("manage_nicknames", roles::MANAGE_NICKNAMES),
    ("kick_members", roles::KICK_MEMBERS),
    ("ban_members", roles::BAN_MEMBERS),
    ("import_messages", roles::IMPORT_MESSAGES),
];

fn perm_bit(name: &str) -> Option<u64> {
    PERMS.iter().find(|(n, _)| *n == name).map(|(_, b)| *b)
}

fn perm_bits(names: &[String]) -> u64 {
    names.iter().filter_map(|n| perm_bit(n)).fold(0, |a, b| a | b)
}

fn perm_names(bits: u64) -> Vec<&'static str> {
    PERMS.iter().filter(|(_, b)| bits & b != 0).map(|(n, _)| *n).collect()
}

// ── The manifest ─────────────────────────────────────────────────────────────

fn one() -> u32 { 1 }
fn text_kind() -> String { "text".into() }

#[derive(Deserialize, Serialize, Clone)]
pub struct Setting {
    pub key: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub help: String,
    #[serde(rename = "type", default = "text_kind")]
    pub kind: String,
    #[serde(default)]
    pub default: serde_json::Value,
    #[serde(default)]
    pub choices: Vec<String>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct Manifest {
    #[serde(default = "one")]
    pub manifest: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub homepage: String,
    pub run: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub channels: String,
    #[serde(default)]
    pub settings: Vec<Setting>,
    /// Whether it needs the internet (web sites: TCP ports 80 and 443). In
    /// the sandbox it may otherwise only connect to this server (and DNS).
    #[serde(default)]
    pub internet: bool,
    /// Whether it takes end-to-end encrypted lanes (the app's lane.go): its
    /// commands, clicks, answers and panel then pass through this server
    /// sealed, between each member's app and the plugin.
    #[serde(default)]
    pub lanes: bool,
    /// Commands members type in the message box: "music::play <song>".
    #[serde(default)]
    pub commands: Vec<Command>,
    /// Whether it shows a panel in the app's plugins area, and how.
    #[serde(default)]
    pub panel: Option<PanelInfo>,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct Command {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub args: Vec<Arg>,
}

/// One of a command's arguments, typed after it in order (the last text
/// one takes the rest of the line).
#[derive(Deserialize, Serialize, Clone)]
pub struct Arg {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// "text", "number" or "choice"
    #[serde(rename = "type", default = "text_kind")]
    pub kind: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub choices: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Default)]
pub struct PanelInfo {
    /// Its title in the plugins area (default: the plugin's name).
    #[serde(default)]
    pub title: String,
    /// A Material Symbols icon name, e.g. "music_note".
    #[serde(default)]
    pub icon: String,
}

/// A command's or argument's name: a-z, 0-9, - and _, 1–32, starting with a letter.
fn valid_word(w: &str) -> bool {
    (1..=32).contains(&w.len()) && w.starts_with(|c: char| c.is_ascii_lowercase())
        && w.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn valid_id(id: &str) -> bool {
    (2..=40).contains(&id.len())
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// This server's platform, as manifests name it: "linux-x86_64", "linux-aarch64"…
pub fn platform() -> String { format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH) }

impl Manifest {
    /// Everything a manifest must be. The error says what's wrong.
    pub fn check(&self) -> Result<(), String> {
        if self.manifest != 1 {
            return Err("This plugin is for a newer version of Chriscord — update this server first".into());
        }
        if !valid_id(&self.id) {
            return Err("The plugin's id must be 2–40 lowercase letters, digits and dashes, starting with a letter".into());
        }
        if !bots::valid_name(&self.name) {
            return Err("The plugin's name must be 2–32 characters".into());
        }
        if self.version.trim().is_empty() || self.version.chars().count() > 32 {
            return Err("The plugin needs a version (up to 32 characters)".into());
        }
        if self.description.chars().count() > 500 || self.author.chars().count() > 100 || self.homepage.chars().count() > 300 {
            return Err("The plugin's description, author or homepage is too long".into());
        }
        if !self.homepage.is_empty() && !(self.homepage.starts_with("https://") || self.homepage.starts_with("http://")) {
            return Err("The plugin's homepage must be a web address".into());
        }
        if self.run.is_empty() || self.run.values().any(|c| c.is_empty()) {
            return Err("The plugin doesn't say how to run it (\"run\")".into());
        }
        for p in &self.permissions {
            if perm_bit(p).is_none() {
                return Err(format!("The plugin asks for \"{p}\", which this server doesn't know (it may need a newer server)"));
            }
        }
        let mut keys = HashSet::new();
        for s in &self.settings {
            let ok_key = (1..=40).contains(&s.key.len()) && s.key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            if !ok_key || !keys.insert(s.key.clone()) {
                return Err(format!("The plugin has a setting with a bad or repeated key: \"{}\"", s.key));
            }
            if !matches!(s.kind.as_str(), "text" | "number" | "bool" | "choice") {
                return Err(format!("The plugin's setting \"{}\" has an unknown type", s.key));
            }
        }
        if self.commands.len() > 50 {
            return Err("A plugin can have up to 50 commands".into());
        }
        let mut names = HashSet::new();
        for c in &self.commands {
            if !valid_word(&c.name) || !names.insert(c.name.clone()) {
                return Err(format!("The plugin has a command with a bad or repeated name: \"{}\"", c.name));
            }
            if c.description.chars().count() > 200 || c.args.len() > 8 {
                return Err(format!("The plugin's command \"{}\" has too long a description or too many arguments", c.name));
            }
            let mut args = HashSet::new();
            for a in &c.args {
                if !valid_word(&a.name) || !args.insert(a.name.clone()) || a.description.chars().count() > 200
                    || !matches!(a.kind.as_str(), "text" | "number" | "choice") || a.choices.len() > 25 {
                    return Err(format!("The plugin's command \"{}\" has a bad argument \"{}\"", c.name, a.name));
                }
            }
        }
        if let Some(p) = &self.panel {
            if p.title.chars().count() > 32 || p.icon.len() > 40 || !p.icon.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                return Err("The plugin's panel has too long a title or a bad icon name".into());
            }
        }
        Ok(())
    }

    /// The command for this platform: the program and its arguments.
    fn command(&self, code: &std::path::Path) -> Result<(PathBuf, Vec<String>), String> {
        let plat = platform();
        let cmd = self.run.get(&plat).or_else(|| self.run.get("any"))
            .ok_or_else(|| format!("This plugin has no version for this server's platform ({plat})"))?;
        let prog = &cmd[0];
        // A file of the plugin's own (a path, or a file that's there), or a
        // program found on this machine's PATH ("python3", say).
        let program = if prog.contains('/') || code.join(prog).is_file() {
            let rel = crate::tarfile::safe_path(prog).ok_or_else(|| format!("The plugin's program path isn't allowed: {prog}"))?;
            let p = code.join(rel);
            if !p.is_file() { return Err(format!("The plugin's program isn't in its folder: {prog}")); }
            p
        } else {
            PathBuf::from(prog)
        };
        Ok((program, cmd[1..].to_vec()))
    }

    /// Settings as they'll be given to it: what's set, else the default,
    /// each checked against its type.
    fn settings_with(&self, set: &serde_json::Map<String, serde_json::Value>) -> serde_json::Map<String, serde_json::Value> {
        let mut out = serde_json::Map::new();
        for s in &self.settings {
            let v = set.get(&s.key).cloned().unwrap_or_else(|| s.default.clone());
            let v = match s.kind.as_str() {
                "number" => {
                    let n = v.as_f64().or_else(|| v.as_str().and_then(|t| t.trim().parse().ok()))
                        .or_else(|| s.default.as_f64()).unwrap_or(0.0);
                    let n = s.min.map_or(n, |m| n.max(m));
                    let n = s.max.map_or(n, |m| n.min(m));
                    serde_json::json!(n)
                }
                "bool" => serde_json::json!(v.as_bool().unwrap_or_else(|| s.default.as_bool().unwrap_or(false))),
                "choice" => {
                    let t = v.as_str().unwrap_or("").to_string();
                    if s.choices.contains(&t) { serde_json::json!(t) }
                    else { s.default.clone() }
                }
                _ => {
                    let t: String = match &v {
                        serde_json::Value::String(t) => t.clone(),
                        serde_json::Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    serde_json::json!(t.chars().take(2000).collect::<String>())
                }
            };
            out.insert(s.key.clone(), v);
        }
        out
    }
}

// ── Where things are ─────────────────────────────────────────────────────────

fn root() -> PathBuf { std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")) }
fn plugins_dir() -> PathBuf { root().join("plugins") }
fn code_dir(id: &str) -> PathBuf { plugins_dir().join(id) }
fn data_dir(id: &str) -> PathBuf { root().join("plugin-data").join(id) }
fn staging_dir() -> PathBuf { plugins_dir().join(".staging") }

fn read_manifest(dir: &std::path::Path) -> Result<Manifest, String> {
    let raw = std::fs::read(dir.join("plugin.json")).map_err(|_| "There's no plugin.json in it".to_string())?;
    let m: Manifest = serde_json::from_slice(&raw).map_err(|e| format!("Its plugin.json can't be read: {e}"))?;
    m.check()?;
    Ok(m)
}

// ── Tables ───────────────────────────────────────────────────────────────────

pub async fn init(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS plugins (
            id           TEXT PRIMARY KEY,
            manifest     TEXT NOT NULL,            -- its plugin.json, as installed
            bot_key      TEXT NOT NULL,            -- its identity (a bot: bots.rs)
            seed         TEXT NOT NULL,            -- that identity's private key
            cert         TEXT NOT NULL,            -- its certificate, signed with the plugin key
            settings     TEXT NOT NULL DEFAULT '{}',
            enabled      INTEGER NOT NULL DEFAULT 1,
            installed_at TEXT NOT NULL,
            updated_at   TEXT NOT NULL
        )",
    ).execute(pool).await?;
    // Whether the owner let it use the internet (when it asked to).
    let _ = sqlx::query("ALTER TABLE plugins ADD COLUMN internet INTEGER NOT NULL DEFAULT 0").execute(pool).await;
    // Uploads that were never installed (the server restarted meanwhile).
    let _ = std::fs::remove_dir_all(staging_dir());
    Ok(())
}

/// Whether this key is a plugin's (they're managed here, not as bots).
pub async fn is_plugin_key(pool: &SqlitePool, key: &str) -> bool {
    sqlx::query("SELECT 1 FROM plugins WHERE bot_key = ?").bind(key.to_lowercase())
        .fetch_optional(pool).await.ok().flatten().is_some()
}

pub async fn is_plugin_user(pool: &SqlitePool, username: &str) -> bool {
    sqlx::query("SELECT 1 FROM plugins p JOIN users u ON u.public_key = p.bot_key WHERE u.username = ?")
        .bind(username).fetch_optional(pool).await.ok().flatten().is_some()
}

struct Installed {
    manifest: Manifest,
    bot_key:  String,
    seed:     String,
    cert:     String,
    settings: serde_json::Map<String, serde_json::Value>,
    enabled:  bool,
    /// Allowed the internet (it also has to ask for it).
    internet: bool,
    installed_at: String,
    updated_at:   String,
}

async fn load(pool: &SqlitePool, id: &str) -> Result<Option<Installed>, ApiErr> {
    let Some(r) = sqlx::query("SELECT manifest, bot_key, seed, cert, settings, enabled, internet, installed_at, updated_at FROM plugins WHERE id = ?")
        .bind(id).fetch_optional(pool).await.map_err(|_| dberr())? else { return Ok(None) };
    let manifest: Manifest = serde_json::from_str(&r.get::<String, _>("manifest")).map_err(|_| dberr())?;
    let settings = serde_json::from_str::<serde_json::Value>(&r.get::<String, _>("settings")).ok()
        .and_then(|v| v.as_object().cloned()).unwrap_or_default();
    Ok(Some(Installed {
        manifest, bot_key: r.get("bot_key"), seed: r.get("seed"), cert: r.get("cert"), settings,
        enabled: r.get::<i64, _>("enabled") != 0,
        internet: r.try_get::<i64, _>("internet").unwrap_or(0) != 0,
        installed_at: r.get("installed_at"), updated_at: r.get("updated_at"),
    }))
}

// ── Identities ───────────────────────────────────────────────────────────────

/// This server's plugin key: it signs its plugins' certificates.
async fn maker(pool: &SqlitePool) -> Result<SigningKey, ApiErr> {
    let seed = db::get_or_create_config(pool, "plugin_maker_seed", || crate::utils::generate_hex(64)).await.map_err(|_| dberr())?;
    let bytes: [u8; 32] = hex::decode(seed.trim()).ok().and_then(|v| v.try_into().ok())
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "This server's plugin key is damaged"))?;
    Ok(SigningKey::from_bytes(&bytes))
}

fn new_seed() -> [u8; 32] {
    use rand::RngCore;
    let mut s = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut s);
    s
}

/// A new identity for a plugin: (seed hex, certificate).
fn make_identity(maker: &SigningKey, m: &Manifest) -> Result<(String, bots::BotCert), ApiErr> {
    let seed = new_seed();
    let bot = SigningKey::from_bytes(&seed);
    let mut cert = bots::BotCert {
        v: 1,
        bot_key: hex::encode(bot.verifying_key().to_bytes()),
        name: m.name.clone(),
        description: m.description.chars().filter(|c| *c != '\0').take(190).collect(),
        maker_key: hex::encode(maker.verifying_key().to_bytes()),
        created: chrono::Utc::now().timestamp(),
        sig: String::new(),
    };
    cert.sig = hex::encode(maker.sign(&cert.message()).to_bytes());
    cert.check().map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok((hex::encode(seed), cert))
}

/// Its bot token, as the bot SDK reads it: "ccbot_1_" + base64url(seed ‖ certificate).
fn token(seed_hex: &str, cert_json: &str) -> Option<String> {
    use base64::Engine as _;
    let mut raw = hex::decode(seed_hex).ok()?;
    if raw.len() != 32 { return None; }
    raw.extend_from_slice(cert_json.as_bytes());
    Some(format!("ccbot_1_{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)))
}

// ── Running them ─────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Default)]
pub struct Status {
    /// "starting", "running", "restarting" (it stopped; trying again),
    /// "stopped" or "failed" (couldn't start: see the log).
    state:    String,
    pid:      Option<u32>,
    since:    String,
    restarts: u32,
    /// Whether it's running in the sandbox (sandbox.rs), and if not, why.
    sandboxed: bool,
    sandbox_note: String,
}

struct Running {
    stop:   tokio::sync::watch::Sender<bool>,
    status: Arc<Mutex<Status>>,
    task:   tokio::task::JoinHandle<()>,
}

/// An upload waiting to be installed.
struct Staged {
    dir:      PathBuf, // the folder with its plugin.json
    holder:   PathBuf, // what to delete when it's done with
    manifest: Manifest,
    at:       Instant,
}

/// Everything about running plugins (in AppState).
#[derive(Default)]
pub struct Plugins {
    running: Mutex<HashMap<String, Running>>,
    logs:    Mutex<HashMap<String, Arc<Mutex<VecDeque<String>>>>>,
    staged:  Mutex<HashMap<String, Staged>>,
    /// What each plugin shows in the app: plugin id → (the channel it's
    /// about, "" for everyone) → its panel.
    panels:  Mutex<HashMap<String, HashMap<String, serde_json::Value>>>,
    /// Commands and clicks passed to plugins, so their answers reach the
    /// member who sent them.
    pending: Mutex<HashMap<String, Pending>>,
    /// How much each member has asked of plugins lately.
    asked:   Mutex<HashMap<String, (Instant, u32)>>,
}

struct Pending {
    user:   String,
    plugin: String,
    board:  Option<String>,
    at:     Instant,
}

/// What it takes to start one.
struct Launch {
    id:       String,
    name:     String,
    program:  PathBuf,
    args:     Vec<String>,
    code:     PathBuf,
    data:     PathBuf,
    url:      String,
    hello:    String,
    /// The TCP ports it may connect to in the sandbox.
    tcp:      Vec<u16>,
}

impl Plugins {
    pub fn new() -> Self { Self::default() }

    fn status(&self, id: &str) -> Status {
        self.running.lock().unwrap().get(id).map(|r| r.status.lock().unwrap().clone())
            .unwrap_or_else(|| Status { state: "stopped".into(), ..Default::default() })
    }

    fn log(&self, id: &str) -> Arc<Mutex<VecDeque<String>>> {
        self.logs.lock().unwrap().entry(id.to_string()).or_default().clone()
    }

    fn log_tail(&self, id: &str, n: usize) -> Vec<String> {
        let log = self.log(id);
        let l = log.lock().unwrap();
        let tail: Vec<String> = l.iter().skip(l.len().saturating_sub(n)).cloned().collect();
        tail
    }

    /// Starts one (if it isn't running already) and keeps it going.
    fn start(&self, l: Launch) {
        let mut running = self.running.lock().unwrap();
        if let Some(r) = running.get(&l.id) {
            if !r.task.is_finished() { return; }
        }
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let status = Arc::new(Mutex::new(Status { state: "starting".into(), since: now(), ..Default::default() }));
        let log = self.log(&l.id);
        let id = l.id.clone();
        let task = tokio::spawn(supervise(l, stop_rx, status.clone(), log));
        running.insert(id, Running { stop, status, task });
    }

    /// Stops one and waits (a few seconds at most) for it to exit.
    async fn stop(&self, id: &str) {
        let r = self.running.lock().unwrap().remove(id);
        if let Some(r) = r {
            let _ = r.stop.send(true);
            let _ = tokio::time::timeout(Duration::from_secs(12), r.task).await;
        }
    }

    /// Stops them all (the server is shutting down).
    pub async fn stop_all(&self) {
        let all: Vec<Running> = self.running.lock().unwrap().drain().map(|(_, r)| r).collect();
        for r in &all { let _ = r.stop.send(true); }
        for r in all { let _ = tokio::time::timeout(Duration::from_secs(12), r.task).await; }
    }

    fn clear_stale_staged(&self) {
        let mut st = self.staged.lock().unwrap();
        let old: Vec<String> = st.iter().filter(|(_, x)| x.at.elapsed() > STAGED_FOR).map(|(k, _)| k.clone()).collect();
        for k in old {
            if let Some(x) = st.remove(&k) { let _ = std::fs::remove_dir_all(&x.holder); }
        }
    }
}

fn push_log(log: &Arc<Mutex<VecDeque<String>>>, line: String) {
    let mut l = log.lock().unwrap();
    l.push_back(line);
    while l.len() > LOG_LINES { l.pop_front(); }
}

/// Runs one plugin until told to stop, restarting it when it exits on its
/// own (after 2 s, doubling up to a minute while it keeps failing quickly).
async fn supervise(l: Launch, mut stop: tokio::sync::watch::Receiver<bool>, status: Arc<Mutex<Status>>, log: Arc<Mutex<VecDeque<String>>>) {
    let mut backoff = Duration::from_secs(2);
    let set_failed = |status: &Arc<Mutex<Status>>, log: &Arc<Mutex<VecDeque<String>>>, why: String| {
        push_log(log, format!("[chriscord] {why}"));
        let mut st = status.lock().unwrap();
        st.state = "failed".into();
        st.pid = None;
        st.since = now();
    };
    loop {
        if *stop.borrow() { break; }
        if let Err(e) = std::fs::create_dir_all(&l.data) {
            set_failed(&status, &log, format!("can't make {}: {e}", l.data.display()));
            return;
        }
        let tmp = crate::sandbox::temp_dir(&l.data);
        let _ = std::fs::create_dir_all(&tmp);
        let mut cmd = tokio::process::Command::new(&l.program);
        cmd.args(&l.args)
            .current_dir(&l.data)
            // Only what it needs from the server's environment (nothing the
            // server was given in confidence), and its own folder as home.
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| PASS_ENV.contains(&k.as_str()) || k.starts_with("LC_")))
            .env("HOME", &l.data)
            .env("TMPDIR", &tmp)
            .env("XDG_CACHE_HOME", l.data.join(".cache"))
            .env("XDG_CONFIG_HOME", l.data.join(".config"))
            .env("XDG_DATA_HOME", l.data.join(".local/share"))
            .env("CHRISCORD_PLUGIN", "1")
            .env("CHRISCORD_PLUGIN_ID", &l.id)
            .env("CHRISCORD_URL", &l.url)
            .env("CHRISCORD_PLUGIN_DIR", &l.code)
            .env("CHRISCORD_PLUGIN_DATA", &l.data)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Into the sandbox, where there is one (else it runs as before, and
        // the panel says so).
        let sandbox = if sandbox_off() {
            Err("the sandbox is turned off (CHRISCORD_PLUGIN_SANDBOX=off)".to_string())
        } else {
            crate::sandbox::prepare(&l.code, &l.data, &root(), Some(&l.tcp))
        };
        let sandbox_note = match &sandbox {
            Ok(sb) => {
                #[cfg(unix)]
                {
                    let fd = sb.raw();
                    // SAFETY: enter only makes system calls (no allocation or
                    // locks), as is required between fork and exec.
                    unsafe { cmd.pre_exec(move || crate::sandbox::enter(fd)); }
                }
                #[cfg(not(unix))]
                let _ = sb;
                String::new()
            }
            Err(why) => why.clone(),
        };
        let child = cmd.spawn();
        drop(sandbox); // the child has its own copy of the ruleset
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                set_failed(&status, &log, format!("couldn't start {}: {e}", l.program.display()));
                return;
            }
        };
        let pid = child.id();
        tracing::info!("plugins: started {} (pid {})", l.name, pid.unwrap_or(0));
        push_log(&log, if sandbox_note.is_empty() {
            format!("[chriscord] started in the sandbox (pid {})", pid.unwrap_or(0))
        } else {
            format!("[chriscord] started (pid {}) — NOT sandboxed: {sandbox_note}", pid.unwrap_or(0))
        });
        {
            let mut st = status.lock().unwrap();
            st.state = "running".into();
            st.pid = pid;
            st.since = now();
            st.sandboxed = sandbox_note.is_empty();
            st.sandbox_note = sandbox_note.clone();
        }
        // Its identity and settings, privately, then nothing more.
        if let Some(mut stdin) = child.stdin.take() {
            let hello = l.hello.clone();
            tokio::spawn(async move {
                let _ = stdin.write_all(hello.as_bytes()).await;
                let _ = stdin.write_all(b"\n").await;
                let _ = stdin.shutdown().await;
            });
        }
        // Its output: into the panel's log, and ours, marked with its name.
        for out in [child.stdout.take().map(|o| Box::new(o) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
                    child.stderr.take().map(|o| Box::new(o) as Box<dyn tokio::io::AsyncRead + Unpin + Send>)].into_iter().flatten() {
            let (name, log) = (l.name.clone(), log.clone());
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(out).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line: String = line.chars().take(2000).collect();
                    eprintln!("[plugin {name}] {line}");
                    push_log(&log, line);
                }
            });
        }
        let started = Instant::now();
        tokio::select! {
            r = child.wait() => {
                let why = match r { Ok(s) => s.to_string(), Err(e) => e.to_string() };
                tracing::warn!("plugins: {} stopped by itself ({why})", l.name);
                push_log(&log, format!("[chriscord] stopped by itself ({why}); starting it again"));
                {
                    let mut st = status.lock().unwrap();
                    st.state = "restarting".into();
                    st.pid = None;
                    st.restarts += 1;
                }
                if started.elapsed() > Duration::from_secs(120) { backoff = Duration::from_secs(2); }
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = stop.changed() => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
            _ = stop.changed() => {
                crate::hosting::terminate(&mut child).await;
                break;
            }
        }
    }
    push_log(&log, "[chriscord] stopped".into());
    let mut st = status.lock().unwrap();
    st.state = "stopped".into();
    st.pid = None;
    st.since = now();
    tracing::info!("plugins: stopped {}", l.name);
}

/// What a plugin is passed from the server's environment.
const PASS_ENV: &[&str] = &["PATH", "LANG", "LANGUAGE", "TZ", "SSL_CERT_FILE", "SSL_CERT_DIR", "http_proxy", "https_proxy",
    "HTTP_PROXY", "HTTPS_PROXY", "no_proxy", "NO_PROXY"];

/// CHRISCORD_PLUGIN_SANDBOX=off runs plugins without the sandbox (for a
/// plugin that needs more than it allows; the panel shows it).
fn sandbox_off() -> bool {
    std::env::var("CHRISCORD_PLUGIN_SANDBOX").map_or(false, |v| v.eq_ignore_ascii_case("off") || v == "0")
}

/// What's needed to start an installed plugin.
async fn launch(s: &AppState, id: &str) -> Result<Launch, String> {
    let row = load(&s.pool, id).await.map_err(|_| "database error".to_string())?
        .ok_or_else(|| "not installed".to_string())?;
    let code = code_dir(id);
    let (program, args) = row.manifest.command(&code)?;
    let token = token(&row.seed, &row.cert).ok_or_else(|| "its identity is damaged — reinstall it".to_string())?;
    let data = data_dir(id);
    let url = format!("http://127.0.0.1:{}", crate::utils::port());
    let hello = serde_json::json!({
        "chriscord_plugin": 1, "id": id, "server": url, "token": token,
        "settings": row.manifest.settings_with(&row.settings),
        "data_dir": data.display().to_string(),
    }).to_string();
    // This server, DNS, and the web if it may.
    let mut tcp = vec![crate::utils::port(), 53];
    if row.manifest.internet && row.internet {
        tcp.extend([80, 443]);
        // A web proxy the server's told to use (passed on in PASS_ENV).
        for k in ["https_proxy", "HTTPS_PROXY", "http_proxy", "HTTP_PROXY"] {
            if let Some(p) = std::env::var(k).ok().and_then(|v| v.trim_end_matches('/').rsplit(':').next().and_then(|p| p.parse::<u16>().ok())) {
                tcp.push(p);
            }
        }
    }
    Ok(Launch { id: id.to_string(), name: row.manifest.name.clone(), program, args, code, data, url, hello, tcp })
}

/// Starts it, or notes in its log why it can't.
async fn start_one(s: &AppState, id: &str) {
    match launch(s, id).await {
        Ok(l) => s.plugins.start(l),
        Err(e) => {
            tracing::warn!("plugins: can't start {id}: {e}");
            push_log(&s.plugins.log(id), format!("[chriscord] can't start: {e}"));
        }
    }
}

/// Starts every enabled plugin (when the server starts, once it's listening).
pub fn start_all(s: Arc<AppState>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let rows = sqlx::query("SELECT id FROM plugins WHERE enabled = 1").fetch_all(&s.pool).await.unwrap_or_default();
        for r in rows {
            start_one(&s, &r.get::<String, _>("id")).await;
        }
    });
}

// ── Using plugins from the app ───────────────────────────────────────────────
//
// Members use a plugin two ways: a command typed in the message box
// ("music::play never gonna") and clicks in its panel (the app draws the
// panel the plugin describes; plugins never run code in the app).
//
//   app → server: {"type": "plugin_command", "plugin", "command", "data": {arg: value}, "board_id"}
//                 {"type": "plugin_action",  "plugin", "action", "value"?, "board_id"?}
//   server → plugin (to its bot connection), with an "id" to answer:
//                 {"type": "plugin_command", "id", "user", "board_id", "voice_board", "command", "args"}
//                 {"type": "plugin_action",  "id", "user", "board_id", "voice_board", "action", "value"}
//   plugin → server: {"type": "plugin_reply", "to": id, "content": text}   (only that member sees it)
//                    {"type": "plugin_panel", "board_id"?, "data": panel or null}
//   server → app: plugin_reply, plugin_panel, plugins_updated
//
// These pass through the server in the clear, which costs nothing here:
// the plugin runs on this server, so whoever runs it sees what the plugin
// sees anyway. A panel about one channel (a call, say) only goes to members
// who can see that channel.

/// The biggest panel a plugin may show (its JSON).
const MAX_PANEL: usize = 64 << 10;
/// Commands and clicks a member may send plugins: this many per 5 seconds.
const ASK_PER_5S: u32 = 10;
/// How long a plugin may answer a command.
const ANSWER_FOR: Duration = Duration::from_secs(15 * 60);

/// The plugin this member is, if it's one (for its connection, in ws.rs).
pub async fn id_of_user(pool: &SqlitePool, username: &str) -> Option<String> {
    sqlx::query("SELECT p.id AS id FROM plugins p JOIN users u ON u.public_key = p.bot_key WHERE u.username = ?")
        .bind(username).fetch_optional(pool).await.ok().flatten().map(|r| r.get("id"))
}

/// Tells every app to load the plugins again (installed, changed, started…).
fn announce(s: &AppState) {
    let _ = s.tx.send(serde_json::json!({ "type": "plugins_updated" }).to_string());
}

impl Plugins {
    /// Whether this member may ask plugins for something right now.
    fn may_ask(&self, user: &str) -> bool {
        let mut a = self.asked.lock().unwrap();
        if a.len() > 5000 { a.retain(|_, (t, _)| t.elapsed() < Duration::from_secs(5)); }
        let e = a.entry(user.to_string()).or_insert((Instant::now(), 0));
        if e.0.elapsed() >= Duration::from_secs(5) { *e = (Instant::now(), 0); }
        e.1 += 1;
        e.1 <= ASK_PER_5S
    }

    fn remember(&self, id: &str, p: Pending) {
        let mut m = self.pending.lock().unwrap();
        if m.len() > 2000 { m.retain(|_, x| x.at.elapsed() < ANSWER_FOR); }
        m.insert(id.to_string(), p);
    }

    fn has_panel(&self, plugin: &str, board: &str) -> bool {
        self.panels.lock().unwrap().get(plugin).map_or(false, |m| m.contains_key(board))
    }

    /// A plugin's panels a member can see: [{board_id, panel}].
    fn panels_for(&self, plugin: &str, sees: impl Fn(&str) -> bool) -> Vec<serde_json::Value> {
        let all = self.panels.lock().unwrap();
        let Some(m) = all.get(plugin) else { return Vec::new() };
        m.iter().filter(|(b, _)| b.is_empty() || sees(b)).map(|(b, p)| serde_json::json!({
            "board_id": if b.is_empty() { serde_json::Value::Null } else { serde_json::json!(b) }, "panel": p,
        })).collect()
    }
}

/// The plugin's connection ended (it stopped or restarted): its panels go.
pub fn gone(s: &AppState, plugin: &str) {
    let boards: Vec<String> = s.plugins.panels.lock().unwrap().remove(plugin).map(|m| m.into_keys().collect()).unwrap_or_default();
    for b in boards {
        let board = if b.is_empty() { serde_json::Value::Null } else { serde_json::json!(b) };
        let _ = s.tx.send(serde_json::json!({ "type": "plugin_panel", "plugin": plugin, "board_id": board, "panel": null }).to_string());
    }
}

async fn plugin_sees(pool: &SqlitePool, plugin_user: &str, board: &str) -> bool {
    sqlx::query("SELECT 1 FROM bot_channels c JOIN users u ON u.public_key = c.public_key WHERE u.username = ? AND c.board_id = ?")
        .bind(plugin_user).bind(board).fetch_optional(pool).await.ok().flatten().is_some()
}

/// One value a member gave: text or a number, as text, not too long.
fn short_text(v: &serde_json::Value) -> Option<String> {
    let t = match v {
        serde_json::Value::String(t) => t.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => return None,
    };
    Some(t.chars().filter(|c| !c.is_control() || *c == '\n').take(2000).collect())
}

/// What a member sent to use a plugin (from ws.rs).
pub struct Use {
    pub kind:   String, // "plugin_command" or "plugin_action"
    pub plugin: String,
    pub name:   String, // the command, or the action
    pub args:   Option<serde_json::Value>,
    pub value:  Option<String>,
    pub board:  Option<String>,
    /// Whether the member can see board (true without one).
    pub board_visible: bool,
}

/// A member used a plugin: passed on to it, or why not (shown to them).
pub async fn from_member(s: &Arc<AppState>, user: &str, u: Use) -> Result<(), String> {
    if !s.plugins.may_ask(user) {
        return Err("Slow down — that's a lot of plugin commands at once".into());
    }
    let row = sqlx::query("SELECT p.manifest AS manifest, p.enabled AS enabled, u.username AS username
                           FROM plugins p LEFT JOIN users u ON u.public_key = p.bot_key WHERE p.id = ?")
        .bind(&u.plugin).fetch_optional(&s.pool).await.map_err(|_| "Database error".to_string())?
        .ok_or_else(|| format!("There's no plugin called {} on this server", u.plugin))?;
    let m: Manifest = serde_json::from_str(&row.get::<String, _>("manifest")).map_err(|_| "Database error".to_string())?;
    let Some(bot) = row.try_get::<Option<String>, _>("username").ok().flatten() else {
        return Err(format!("{} isn't set up right — the server's owner can reinstall it", m.name));
    };
    let online = s.online.lock().unwrap().contains_key(&bot);
    if row.get::<i64, _>("enabled") == 0 {
        return Err(format!("{} is turned off on this server", m.name));
    }
    if !online {
        return Err(format!("{} isn't running right now — try again in a moment", m.name));
    }
    if !u.board_visible {
        return Err("You can't see that channel".into());
    }

    let id = crate::utils::generate_hex(16);
    let voice_board = s.voice.lock().unwrap().get(user).cloned();
    let payload = if u.kind == "plugin_command" {
        let cmd = m.commands.iter().find(|c| c.name == u.name)
            .ok_or_else(|| format!("{} has no command called {}", m.name, u.name))?;
        let board = u.board.clone().ok_or_else(|| "Commands are sent from a channel".to_string())?;
        if !plugin_sees(&s.pool, &bot, &board).await {
            return Err(format!("{} can't see this channel — the server's owner can give it access in the admin panel (Plugins)", m.name));
        }
        // Only its own arguments, as text; the required ones there.
        let given = u.args.as_ref().and_then(|a| a.as_object().cloned()).unwrap_or_default();
        let mut args = serde_json::Map::new();
        for a in &cmd.args {
            match given.get(&a.name).and_then(short_text).filter(|t| !t.trim().is_empty()) {
                Some(t) => {
                    if a.kind == "choice" && !a.choices.is_empty() && !a.choices.contains(&t) {
                        return Err(format!("{} must be one of: {}", a.name, a.choices.join(", ")));
                    }
                    if a.kind == "number" && t.trim().parse::<f64>().is_err() {
                        return Err(format!("{} must be a number", a.name));
                    }
                    args.insert(a.name.clone(), serde_json::json!(t));
                }
                None if a.required => return Err(format!("{}::{} needs {}", m.id, cmd.name, a.name)),
                None => {}
            }
        }
        serde_json::json!({ "type": "plugin_command", "id": id, "user": user, "board_id": board,
            "voice_board": voice_board, "command": cmd.name, "args": args })
    } else {
        // A click in its panel: the panel has to be one this member can see.
        if u.name.is_empty() || u.name.len() > 100 {
            return Err("That button doesn't do anything".into());
        }
        let key = u.board.clone().unwrap_or_default();
        if !s.plugins.has_panel(&u.plugin, &key) {
            return Err(format!("{} has changed since — try again", m.name));
        }
        serde_json::json!({ "type": "plugin_action", "id": id, "user": user, "board_id": u.board,
            "voice_board": voice_board, "action": u.name,
            "value": u.value.as_ref().map(|v| v.chars().filter(|c| !c.is_control()).take(2000).collect::<String>()) })
    };
    s.plugins.remember(&id, Pending { user: user.to_string(), plugin: u.plugin.clone(), board: u.board.clone(), at: Instant::now() });
    crate::ws::send_to_user(s, &bot, payload);
    Ok(())
}

/// What a plugin sent about its interface (from ws.rs): an answer to a
/// member, or its panel. Err: why it was refused (told to the plugin).
pub async fn from_plugin(s: &Arc<AppState>, plugin: &str, bot: &str, kind: &str, board: Option<String>,
                         data: Option<serde_json::Value>, to: Option<String>, content: Option<String>) -> Result<(), String> {
    match kind {
        "plugin_reply" => {
            let to = to.unwrap_or_default();
            let (user, board) = {
                let p = s.plugins.pending.lock().unwrap();
                let x = p.get(&to).filter(|x| x.plugin == plugin && x.at.elapsed() < ANSWER_FOR)
                    .ok_or_else(|| "plugin_reply: no such command (or it's too old to answer)".to_string())?;
                (x.user.clone(), x.board.clone())
            };
            let text: String = content.unwrap_or_default().chars().take(2000).collect();
            if text.trim().is_empty() { return Ok(()); }
            crate::ws::send_to_user(s, &user, serde_json::json!({
                "type": "plugin_reply", "plugin": plugin, "id": to, "board_id": board, "content": text,
            }));
            Ok(())
        }
        "plugin_panel" => {
            let panel = data.unwrap_or(serde_json::Value::Null);
            if !(panel.is_null() || panel.is_object()) {
                return Err("plugin_panel: data must be an object (or null to remove it)".into());
            }
            if panel.to_string().len() > MAX_PANEL {
                return Err("plugin_panel: the panel is too big (64 KB at most)".into());
            }
            let key = board.clone().unwrap_or_default();
            if !key.is_empty() && !plugin_sees(&s.pool, bot, &key).await {
                return Err("plugin_panel: that's not one of this plugin's channels".into());
            }
            {
                let mut all = s.plugins.panels.lock().unwrap();
                let m = all.entry(plugin.to_string()).or_default();
                if panel.is_null() { m.remove(&key); } else { m.insert(key, panel.clone()); }
            }
            let _ = s.tx.send(serde_json::json!({
                "type": "plugin_panel", "plugin": plugin, "board_id": board, "panel": panel,
            }).to_string());
            Ok(())
        }
        _ => Ok(()),
    }
}

// ── Lanes ────────────────────────────────────────────────────────────────────
//
// End-to-end encrypted lines between a member's app and a plugin (the app's
// and the SDK's lane.go). This server only passes them along: it can't read
// them, and doesn't need to.
//
//   app → server: {"type": "plugin_lane", "plugin", "sid", "n", "eph"?, "data"}
//   server → plugin: the same, with "from": the member
//   plugin → server: {"type": "plugin_lane" | "plugin_lane_gone", "to", "sid", "n", "data"}
//   server → app: {"type": "plugin_lane" | "plugin_lane_gone", "plugin", "sid", "n", "data"}

/// Biggest sealed lane message (base64).
const MAX_LANE: usize = 96 << 10;

fn lane_ok(sid: &str, data: &str) -> bool {
    sid.len() == 32 && sid.chars().all(|c| c.is_ascii_hexdigit()) && data.len() <= MAX_LANE
}

/// A member's lane message for a plugin.
pub async fn lane_from_member(s: &Arc<AppState>, user: &str, plugin: &str, sid: &str, n: u64, eph: Option<String>, data: &str) -> Result<(), String> {
    if !s.plugins.may_ask(user) {
        return Err("Slow down — that's a lot of plugin commands at once".into());
    }
    if !lane_ok(sid, data) || eph.as_ref().map_or(false, |e| e.len() > 64) {
        return Err("That message to the plugin was malformed".into());
    }
    let row = sqlx::query("SELECT p.manifest AS manifest, u.username AS username FROM plugins p
                           LEFT JOIN users u ON u.public_key = p.bot_key WHERE p.id = ? AND p.enabled = 1")
        .bind(plugin).fetch_optional(&s.pool).await.map_err(|_| "Database error".to_string())?
        .ok_or_else(|| format!("There's no plugin called {plugin} on this server"))?;
    let name = serde_json::from_str::<Manifest>(&row.get::<String, _>("manifest")).map(|m| m.name).unwrap_or_else(|_| plugin.to_string());
    let Some(bot) = row.try_get::<Option<String>, _>("username").ok().flatten() else {
        return Err(format!("{name} isn't set up right — the server's owner can reinstall it"));
    };
    if !s.online.lock().unwrap().contains_key(&bot) {
        return Err(format!("{name} isn't running right now — try again in a moment"));
    }
    crate::ws::send_to_user(s, &bot, serde_json::json!({
        "type": "plugin_lane", "from": user, "sid": sid, "n": n, "eph": eph, "data": data,
    }));
    Ok(())
}

/// A plugin's lane message (or "I don't know that lane") for a member.
pub fn lane_from_plugin(s: &Arc<AppState>, plugin: &str, kind: &str, to: &str, sid: &str, n: u64, data: Option<String>) -> Result<(), String> {
    let data = data.unwrap_or_default();
    if to.is_empty() || !lane_ok(sid, &data) {
        return Err(format!("{kind}: malformed"));
    }
    let mut msg = serde_json::json!({ "type": kind, "plugin": plugin, "sid": sid });
    if kind == "plugin_lane" {
        msg["n"] = serde_json::json!(n);
        msg["data"] = serde_json::json!(data);
    }
    crate::ws::send_to_user(s, to, msg);
    Ok(())
}

/// GET /api/plugins — for members: the plugins here, their commands, and
/// the panels this member can see.
pub async fn member_list(headers: HeaderMap, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    let user = bots::me(&headers, &s).await?;
    let vis = crate::access::visible_for(&s.pool, &user).await;
    let sees = |b: &str| vis.as_ref().map_or(true, |v| v.boards.contains(b));
    let rows = sqlx::query("SELECT p.id AS id, p.manifest AS manifest, p.bot_key AS bot_key, u.username AS username,
                                   k.enc_key AS enc_key, k.sig AS enc_sig
                            FROM plugins p LEFT JOIN users u ON u.public_key = p.bot_key
                                           LEFT JOIN user_enc_keys k ON k.public_key = p.bot_key
                            WHERE p.enabled = 1 ORDER BY p.installed_at")
        .fetch_all(&s.pool).await.map_err(|_| dberr())?;
    let online: HashSet<String> = s.online.lock().unwrap().keys().cloned().collect();
    let mut out = Vec::new();
    for r in rows {
        let id: String = r.get("id");
        let Ok(m) = serde_json::from_str::<Manifest>(&r.get::<String, _>("manifest")) else { continue };
        let username: Option<String> = r.try_get("username").ok().flatten();
        let enc_key: Option<String> = r.try_get("enc_key").ok().flatten();
        let enc_sig: Option<String> = r.try_get("enc_sig").ok().flatten();
        let panel = m.panel.clone();
        out.push(serde_json::json!({
            "id": id, "name": m.name, "version": m.version, "description": m.description,
            "username": username, "connected": username.as_ref().map_or(false, |u| online.contains(u)),
            // Its keys, for lanes: the app checks the encryption key's
            // signature, and (with a signed member list) that the identity
            // key is a plugin on it.
            "lanes": m.lanes, "public_key": r.get::<String, _>("bot_key"), "enc_key": enc_key, "enc_sig": enc_sig,
            "commands": m.commands,
            "panel": panel.map(|p| serde_json::json!({ "title": if p.title.is_empty() { m.name.clone() } else { p.title }, "icon": p.icon })),
            "panels": s.plugins.panels_for(&id, sees),
        }));
    }
    Ok(Json(serde_json::json!({ "plugins": out })))
}

// ── Admin routes ─────────────────────────────────────────────────────────────

async fn channels_of(pool: &SqlitePool, key: &str) -> Vec<String> {
    sqlx::query("SELECT board_id FROM bot_channels WHERE public_key = ?").bind(key)
        .fetch_all(pool).await.unwrap_or_default().iter().map(|r| r.get::<String, _>("board_id")).collect()
}

/// GET /api/admin/plugins
pub async fn list(headers: HeaderMap, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    s.plugins.clear_stale_staged();
    let rows = sqlx::query("SELECT p.id AS id, u.username AS username, b.permissions AS permissions
                            FROM plugins p LEFT JOIN users u ON u.public_key = p.bot_key LEFT JOIN bots b ON b.public_key = p.bot_key
                            ORDER BY p.installed_at")
        .fetch_all(&s.pool).await.map_err(|_| dberr())?;
    let online: HashSet<String> = s.online.lock().unwrap().keys().cloned().collect();
    let mut installed = Vec::new();
    let mut ids = HashSet::new();
    for r in rows {
        let id: String = r.get("id");
        let Some(p) = load(&s.pool, &id).await? else { continue };
        let username: Option<String> = r.try_get("username").ok().flatten();
        let perms = r.try_get::<Option<i64>, _>("permissions").ok().flatten().unwrap_or(0) as u64;
        ids.insert(id.clone());
        installed.push(serde_json::json!({
            "id": id, "manifest": p.manifest, "enabled": p.enabled,
            "status": s.plugins.status(&id), "log": s.plugins.log_tail(&id, 12),
            "username": username.clone(), "connected": username.map_or(false, |u| online.contains(&u)),
            "permissions": perm_names(perms), "channels": channels_of(&s.pool, &p.bot_key).await,
            "settings": p.manifest.settings_with(&p.settings),
            "internet": p.manifest.internet && p.internet,
            "installed_at": p.installed_at, "updated_at": p.updated_at,
            "code_dir": code_dir(&id).display().to_string(), "data_dir": data_dir(&id).display().to_string(),
        }));
    }
    // Folders put in plugins/ by hand that aren't installed yet.
    let mut found = Vec::new();
    if let Ok(rd) = std::fs::read_dir(plugins_dir()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || !e.path().is_dir() || ids.contains(&name) { continue; }
            match read_manifest(&e.path()) {
                Ok(m) if m.id == name => found.push(serde_json::json!({ "folder": name, "manifest": m })),
                Ok(m) => found.push(serde_json::json!({ "folder": name, "error": format!("Its id is \"{}\": rename the folder to that", m.id) })),
                Err(why) => found.push(serde_json::json!({ "folder": name, "error": why })),
            }
        }
    }
    let channels: Vec<serde_json::Value> = sqlx::query(
        "SELECT b.id AS id, b.name AS name, r.name AS room, r.room_type AS room_type
         FROM boards b JOIN rooms r ON r.id = b.room_id ORDER BY r.position, r.created_at, b.position, b.created_at")
        .fetch_all(&s.pool).await.map_err(|_| dberr())?.iter().map(|r| serde_json::json!({
            "id": r.get::<String, _>("id"), "name": r.get::<String, _>("name"),
            "room": r.get::<String, _>("room"), "room_type": r.get::<String, _>("room_type"),
        })).collect();
    let perms: Vec<serde_json::Value> = PERMS.iter().map(|(n, b)| serde_json::json!({ "name": n, "bit": b })).collect();
    let sp = crate::sandbox::support();
    let off = sandbox_off();
    let sandbox_info = serde_json::json!({
        "available": sp.sandboxed && !off,
        "net": sp.net && !off,
        "abi": sp.abi, // Landlock's version here (0: none)
        "note": if off { "Turned off with CHRISCORD_PLUGIN_SANDBOX=off".to_string() } else { sp.note },
    });
    Ok(Json(serde_json::json!({
        "sandbox": sandbox_info,
        "platform": platform(), "plugins": installed, "found": found, "channels": channels,
        "permissions": perms, "folder": plugins_dir().display().to_string(),
    })))
}

/// POST /api/admin/plugins/upload — a plugin as a .tar (plugin.json at the
/// top, or in its one folder). It's unpacked and checked, and waits to be
/// installed (what it asks for is shown first).
pub async fn upload(headers: HeaderMap, State(s): State<Arc<AppState>>, body: axum::body::Bytes) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    s.plugins.clear_stale_staged();
    if body.is_empty() { return Err(bad("That file is empty")); }
    if body.len() > MAX_UPLOAD { return Err(bad("A plugin can be up to 512 MB")); }
    let sid = crate::utils::generate_hex(16);
    let holder = staging_dir().join(&sid);
    let unpack_into = holder.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<PathBuf, String> {
        std::fs::create_dir_all(&unpack_into).map_err(|e| format!("can't make a folder for it: {e}"))?;
        crate::tarfile::unpack(&mut &body[..], &unpack_into).map_err(|e| e.to_string())?;
        if unpack_into.join("plugin.json").is_file() { return Ok(unpack_into); }
        // One folder at the top with the plugin in it.
        let dirs: Vec<PathBuf> = std::fs::read_dir(&unpack_into).map_err(|e| e.to_string())?
            .flatten().map(|e| e.path()).collect();
        match dirs.as_slice() {
            [d] if d.join("plugin.json").is_file() => Ok(d.clone()),
            _ => Err("There's no plugin.json in it".into()),
        }
    }).await.map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Unpacking it failed"))?;
    let dir = match res {
        Ok(d) => d,
        Err(why) => {
            let _ = std::fs::remove_dir_all(&holder);
            return Err(bad(&format!("That isn't a plugin this server can install: {why}")));
        }
    };
    let manifest = match read_manifest(&dir) {
        Ok(m) => m,
        Err(why) => { let _ = std::fs::remove_dir_all(&holder); return Err(bad(&why)); }
    };
    let runs_here = manifest.command(&dir).err();
    let current = load(&s.pool, &manifest.id).await?.map(|r| r.manifest);
    s.plugins.staged.lock().unwrap().insert(sid.clone(), Staged { dir, holder, manifest: manifest.clone(), at: Instant::now() });
    Ok(Json(serde_json::json!({
        "source": format!("upload:{sid}"), "manifest": manifest, "installed": current,
        "problem": runs_here, "platform": platform(),
    })))
}

#[derive(Deserialize)]
pub struct InstallReq {
    /// "upload:<id from upload>" or "folder:<folder in plugins/>".
    pub source: String,
    /// What it may do: of what it asks for, the ones the owner allowed.
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default)]
    pub settings: serde_json::Map<String, serde_json::Value>,
    /// Whether it may use the internet, if it asked (default: yes).
    #[serde(default)]
    pub internet: Option<bool>,
}

/// A free name for it here: its own, else with " (plugin)" or a number.
async fn free_name(pool: &SqlitePool, want: &str) -> Result<String, ApiErr> {
    let mut tries: Vec<String> = vec![want.to_string()];
    let base: String = want.chars().take(23).collect();
    tries.push(format!("{} (plugin)", base.trim_end()));
    for n in 2..50 { tries.push(format!("{} {n}", want.chars().take(29).collect::<String>().trim_end())); }
    for t in tries {
        if !bots::valid_name(&t) { continue; }
        let taken = sqlx::query("SELECT 1 FROM users WHERE username = ? COLLATE NOCASE").bind(&t)
            .fetch_optional(pool).await.map_err(|_| dberr())?.is_some();
        if !taken { return Ok(t); }
    }
    Err(bad("Couldn't find a free name for it here"))
}

async fn existing_boards(pool: &SqlitePool, wanted: &[String]) -> Result<Vec<String>, ApiErr> {
    let mut out = Vec::new();
    for c in wanted.iter().take(500) {
        let ok = sqlx::query("SELECT 1 FROM boards WHERE id = ?").bind(c).fetch_optional(pool).await.map_err(|_| dberr())?.is_some();
        if ok && !out.contains(c) { out.push(c.clone()); }
    }
    Ok(out)
}

/// POST /api/admin/plugins/install — installs (or updates) a plugin with what
/// the owner allowed, and starts it.
pub async fn install(headers: HeaderMap, State(s): State<Arc<AppState>>, Json(b): Json<InstallReq>) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    // Where it comes from.
    let (src_dir, holder, manifest) = if let Some(sid) = b.source.strip_prefix("upload:") {
        let st = s.plugins.staged.lock().unwrap().remove(sid)
            .ok_or_else(|| bad("That upload has expired — upload the plugin again"))?;
        (st.dir, Some(st.holder), st.manifest)
    } else if let Some(folder) = b.source.strip_prefix("folder:") {
        if !valid_id(folder) { return Err(bad("No such folder")); }
        let dir = code_dir(folder);
        let m = read_manifest(&dir).map_err(|e| bad(&e))?;
        if m.id != folder { return Err(bad("The folder's name has to be the plugin's id")); }
        (dir, None, m)
    } else {
        return Err(bad("Unknown source"));
    };
    let cleanup = |h: &Option<PathBuf>| if let Some(h) = h { let _ = std::fs::remove_dir_all(h); };
    let id = manifest.id.clone();
    if let Err(why) = manifest.command(&src_dir) { cleanup(&holder); return Err(bad(&why)); }
    let existing = match load(&s.pool, &id).await { Ok(e) => e, Err(e) => { cleanup(&holder); return Err(e); } };
    if existing.is_none() {
        let count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM plugins").fetch_one(&s.pool).await.map_err(|_| dberr())?.get("n");
        if count >= MAX_PLUGINS { cleanup(&holder); return Err(bad("That's as many plugins as one server runs")); }
    }

    // Only what it asks for, and only what the owner ticked.
    let asked = perm_bits(&manifest.permissions);
    let perms = perm_bits(&b.permissions) & asked & roles::ALL & !bots::BOT_NEVER;
    let channels = existing_boards(&s.pool, &b.channels).await?;
    let settings = manifest.settings_with(&b.settings);
    let internet = manifest.internet && b.internet.unwrap_or(true);

    // Its code into place (an update replaces the old code; what it keeps
    // in plugin-data/ stays).
    s.plugins.stop(&id).await;
    gone(&s, &id);
    let dest = code_dir(&id);
    if src_dir != dest {
        let _ = std::fs::create_dir_all(plugins_dir());
        let old = plugins_dir().join(format!(".old-{id}-{}", crate::utils::generate_hex(6)));
        if dest.exists() {
            if let Err(e) = std::fs::rename(&dest, &old) { cleanup(&holder); return Err(bad(&format!("Couldn't replace its old version: {e}"))); }
        }
        if let Err(e) = std::fs::rename(&src_dir, &dest) {
            let _ = std::fs::rename(&old, &dest); // put the old one back
            cleanup(&holder);
            return Err(bad(&format!("Couldn't put it in place: {e}")));
        }
        let _ = std::fs::remove_dir_all(&old);
        cleanup(&holder);
    }
    let _ = std::fs::create_dir_all(data_dir(&id));

    let manifest_json = serde_json::to_string(&manifest).unwrap_or_default();
    let settings_json = serde_json::Value::Object(settings).to_string();
    let at = now();
    let username;
    match existing {
        Some(old) => {
            // An update: same identity; what it may do and see as now chosen.
            sqlx::query("UPDATE plugins SET manifest = ?, settings = ?, enabled = 1, internet = ?, updated_at = ? WHERE id = ?")
                .bind(&manifest_json).bind(&settings_json).bind(internet as i64).bind(&at).bind(&id).execute(&s.pool).await.map_err(|_| dberr())?;
            ensure_member(&s, &id, &old.bot_key, &old.cert, &manifest).await?;
            set_access(&s.pool, &old.bot_key, perms, &channels).await?;
            username = username_of(&s.pool, &old.bot_key).await;
            tracing::info!("plugins: updated {id} to {}", manifest.version);
        }
        None => {
            let maker = maker(&s.pool).await?;
            let (seed, cert) = make_identity(&maker, &manifest)?;
            let cert_json = serde_json::to_string(&cert).unwrap_or_default();
            sqlx::query("INSERT INTO plugins (id, manifest, bot_key, seed, cert, settings, enabled, internet, installed_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?, ?)")
                .bind(&id).bind(&manifest_json).bind(&cert.bot_key).bind(&seed).bind(&cert_json).bind(&settings_json).bind(internet as i64).bind(&at).bind(&at)
                .execute(&s.pool).await.map_err(|_| dberr())?;
            ensure_member(&s, &id, &cert.bot_key, &cert_json, &manifest).await?;
            set_access(&s.pool, &cert.bot_key, perms, &channels).await?;
            username = username_of(&s.pool, &cert.bot_key).await;
            tracing::info!("plugins: installed {id} {}", manifest.version);
        }
    }
    bots::changed(&s);
    start_one(&s, &id).await;
    announce(&s);
    Ok(Json(serde_json::json!({ "ok": true, "id": id, "username": username })))
}

async fn username_of(pool: &SqlitePool, key: &str) -> Option<String> {
    sqlx::query("SELECT username FROM users WHERE public_key = ?").bind(key)
        .fetch_optional(pool).await.ok().flatten().map(|r| r.get("username"))
}

/// Makes sure its identity is a member here, as a bot (put back if it was
/// removed some other way).
async fn ensure_member(s: &Arc<AppState>, id: &str, key: &str, cert_json: &str, m: &Manifest) -> Result<(), ApiErr> {
    let cert: bots::BotCert = serde_json::from_str(cert_json).map_err(|_| dberr())?;
    let _ = sqlx::query("DELETE FROM bans WHERE public_key = ?").bind(key).execute(&s.pool).await;
    if username_of(&s.pool, key).await.is_none() {
        let name = free_name(&s.pool, &m.name).await?;
        crate::imports::make_way_for(s, &name).await;
        sqlx::query("INSERT INTO users (public_key, username, created_at) VALUES (?, ?, ?)")
            .bind(key).bind(&name).bind(now()).execute(&s.pool).await.map_err(|_| dberr())?;
    }
    let added_by = crate::admin::owner_username(&s.pool).await.unwrap_or_else(|| "the server owner".into());
    sqlx::query("INSERT INTO bots (public_key, maker_key, name, description, cert, permissions, added_by, added_at, client)
                 VALUES (?, ?, ?, ?, ?, 0, ?, ?, ?)
                 ON CONFLICT(public_key) DO UPDATE SET name = excluded.name, description = excluded.description")
        .bind(key).bind(&cert.maker_key).bind(&cert.name).bind(m.description.chars().take(190).collect::<String>())
        .bind(cert_json).bind(&added_by).bind(now()).bind(format!("plugin:{id}"))
        .execute(&s.pool).await.map_err(|_| dberr())?;
    Ok(())
}

async fn set_access(pool: &SqlitePool, key: &str, perms: u64, channels: &[String]) -> Result<(), ApiErr> {
    let mut tx = pool.begin().await.map_err(|_| dberr())?;
    sqlx::query("UPDATE bots SET permissions = ? WHERE public_key = ?").bind(perms as i64).bind(key)
        .execute(&mut *tx).await.map_err(|_| dberr())?;
    sqlx::query("DELETE FROM bot_channels WHERE public_key = ?").bind(key).execute(&mut *tx).await.map_err(|_| dberr())?;
    for c in channels {
        sqlx::query("INSERT OR IGNORE INTO bot_channels (public_key, board_id) VALUES (?, ?)").bind(key).bind(c)
            .execute(&mut *tx).await.map_err(|_| dberr())?;
    }
    tx.commit().await.map_err(|_| dberr())?;
    Ok(())
}

#[derive(Deserialize)]
pub struct UpdateReq {
    pub permissions: Option<Vec<String>>,
    pub channels:    Option<Vec<String>>,
    pub settings:    Option<serde_json::Map<String, serde_json::Value>>,
    pub internet:    Option<bool>,
}

/// PUT /api/admin/plugins/:id — change what it may do and see, and its
/// settings (it restarts to pick those up).
pub async fn update(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>, Json(b): Json<UpdateReq>) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    let p = load(&s.pool, &id).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "No such plugin"))?;
    let key = p.bot_key.clone();
    if b.permissions.is_some() || b.channels.is_some() {
        let cur_perms: u64 = sqlx::query("SELECT permissions FROM bots WHERE public_key = ?").bind(&key)
            .fetch_optional(&s.pool).await.map_err(|_| dberr())?.map(|r| r.get::<i64, _>("permissions") as u64).unwrap_or(0);
        let perms = match &b.permissions {
            Some(names) => perm_bits(names) & perm_bits(&p.manifest.permissions) & roles::ALL & !bots::BOT_NEVER,
            None => cur_perms,
        };
        let channels = match &b.channels {
            Some(c) => existing_boards(&s.pool, c).await?,
            None => channels_of(&s.pool, &key).await,
        };
        ensure_member(&s, &id, &key, &p.cert, &p.manifest).await?;
        set_access(&s.pool, &key, perms, &channels).await?;
        bots::changed(&s);
    }
    if b.settings.is_some() || b.internet.is_some() {
        let settings = p.manifest.settings_with(b.settings.as_ref().unwrap_or(&p.settings));
        let internet = p.manifest.internet && b.internet.unwrap_or(p.internet);
        sqlx::query("UPDATE plugins SET settings = ?, internet = ?, updated_at = ? WHERE id = ?")
            .bind(serde_json::Value::Object(settings).to_string()).bind(internet as i64).bind(now()).bind(&id)
            .execute(&s.pool).await.map_err(|_| dberr())?;
        if p.enabled {
            s.plugins.stop(&id).await;
            gone(&s, &id);
            start_one(&s, &id).await;
        }
    }
    announce(&s);
    Ok(Json(serde_json::json!({ "ok": true, "status": s.plugins.status(&id) })))
}

/// POST /api/admin/plugins/:id/start
pub async fn start(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    action(headers, id, "start", s).await
}
/// POST /api/admin/plugins/:id/stop — and it stays stopped (also across
/// server restarts) until started again.
pub async fn stop(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    action(headers, id, "stop", s).await
}
/// POST /api/admin/plugins/:id/restart
pub async fn restart(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    action(headers, id, "restart", s).await
}

async fn action(headers: HeaderMap, id: String, action: &str, s: Arc<AppState>) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    let p = load(&s.pool, &id).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "No such plugin"))?;
    let enable = action != "stop";
    sqlx::query("UPDATE plugins SET enabled = ? WHERE id = ?").bind(enable as i64).bind(&id)
        .execute(&s.pool).await.map_err(|_| dberr())?;
    if action != "start" {
        s.plugins.stop(&id).await;
        gone(&s, &id);
    }
    if enable {
        ensure_member(&s, &id, &p.bot_key, &p.cert, &p.manifest).await?;
        start_one(&s, &id).await;
    }
    announce(&s);
    Ok(Json(serde_json::json!({ "ok": true, "status": s.plugins.status(&id) })))
}

/// GET /api/admin/plugins/:id/logs — its recent output.
pub async fn logs(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    Ok(Json(serde_json::json!({ "lines": s.plugins.log_tail(&id, LOG_LINES), "status": s.plugins.status(&id) })))
}

#[derive(Deserialize)]
pub struct RemoveQuery {
    /// "1": delete what it keeps (plugin-data/<id>) too.
    #[serde(default)]
    delete_data: Option<String>,
}

/// DELETE /api/admin/plugins/:id — stops it, takes its member off the
/// server, and deletes its code (and what it keeps, if asked).
pub async fn remove(headers: HeaderMap, Path(id): Path<String>, Query(q): Query<RemoveQuery>, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    let p = load(&s.pool, &id).await?.ok_or_else(|| err(StatusCode::NOT_FOUND, "No such plugin"))?;
    s.plugins.stop(&id).await;
    sqlx::query("DELETE FROM plugins WHERE id = ?").bind(&id).execute(&s.pool).await.map_err(|_| dberr())?;
    match username_of(&s.pool, &p.bot_key).await {
        Some(n) => crate::admin::kick_user(&s, &n, false, "The plugin was removed").await?,
        None => bots::forget(&s.pool, &p.bot_key).await,
    }
    let _ = std::fs::remove_dir_all(code_dir(&id));
    let deleted = q.delete_data.as_deref() == Some("1") && std::fs::remove_dir_all(data_dir(&id)).is_ok();
    s.plugins.logs.lock().unwrap().remove(&id);
    gone(&s, &id);
    bots::changed(&s);
    announce(&s);
    tracing::info!("plugins: removed {id}{}", if deleted { " and its data" } else { "" });
    Ok(Json(serde_json::json!({ "ok": true, "data_deleted": deleted, "data_dir": data_dir(&id).display().to_string() })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        serde_json::from_str(r#"{
            "id": "music", "name": "Music", "version": "0.1.0",
            "run": {"any": ["bin/music"]},
            "permissions": ["send_messages", "connect", "speak"],
            "settings": [
                {"key": "prefix", "type": "text", "default": "!"},
                {"key": "volume", "type": "number", "default": 0.5, "min": 0, "max": 2},
                {"key": "mono", "type": "bool", "default": true},
                {"key": "format", "type": "choice", "choices": ["best", "mp3"], "default": "best"}
            ]
        }"#).unwrap()
    }

    #[test]
    fn manifests() {
        let m = manifest();
        assert!(m.check().is_ok());
        let mut bad = m.clone(); bad.id = "Music!".into(); assert!(bad.check().is_err());
        let mut bad = m.clone(); bad.permissions.push("administrator".into()); assert!(bad.check().is_err());
        let mut bad = m.clone(); bad.run.clear(); assert!(bad.check().is_err());
        assert_eq!(perm_bits(&m.permissions), roles::SEND_MESSAGES | roles::CONNECT | roles::SPEAK);
    }

    #[test]
    fn settings_are_checked() {
        let m = manifest();
        let mut set = serde_json::Map::new();
        set.insert("volume".into(), serde_json::json!("7"));
        set.insert("format".into(), serde_json::json!("flac"));
        set.insert("other".into(), serde_json::json!(1));
        let got = m.settings_with(&set);
        assert_eq!(got["volume"], serde_json::json!(2.0));
        assert_eq!(got["format"], serde_json::json!("best"));
        assert_eq!(got["prefix"], serde_json::json!("!"));
        assert_eq!(got["mono"], serde_json::json!(true));
        assert!(got.get("other").is_none());
    }

    #[test]
    fn identity_and_token() {
        let maker = SigningKey::from_bytes(&[7u8; 32]);
        let (seed, cert) = make_identity(&maker, &manifest()).unwrap();
        assert!(cert.check().is_ok());
        let t = token(&seed, &serde_json::to_string(&cert).unwrap()).unwrap();
        assert!(t.starts_with("ccbot_1_"));
    }
}
