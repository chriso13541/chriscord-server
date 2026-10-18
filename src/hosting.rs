// hosting.rs — more than one Chriscord server on one machine.
//
// The server you run (the "main" one) can start and look after others,
// from its admin panel (Hosted servers). Each is its own chriscord-server
// process — this same program — with its own data folder, servers/<id>/
// inside this one's, and its own port, used for both chat (TCP) and voice
// (UDP), exactly like the main server's 7070. They share nothing: own
// database, uploads, owner key and admin panel (at its own port). People
// add one to their app as host:port (or a domain your reverse proxy
// points at that port); forward the port as TCP + UDP like 7070.
//
// The main server starts them when it starts, restarts one that stops on
// its own (waiting longer each time it keeps failing), and they stop with
// it — under systemd they're in the same service, so `systemctl stop`
// stops them all.
//
// A hosted server can't host servers itself (CHRISCORD_HOSTED is set for
// it); its panel says to use the main one.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;

use crate::{admin::check_owner, state::AppState};

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn err(code: StatusCode, msg: &str) -> ApiErr {
    (code, Json(serde_json::json!({ "error": msg })))
}
fn dberr() -> ApiErr { err(StatusCode::INTERNAL_SERVER_ERROR, "DB error") }
fn bad(msg: &str) -> ApiErr { err(StatusCode::BAD_REQUEST, msg) }

/// Set for a server started by another one.
const HOSTED_ENV: &str = "CHRISCORD_HOSTED";
/// Most hosted servers one machine runs from here.
const MAX_SERVERS: i64 = 50;

pub fn is_hosted() -> bool {
    std::env::var(HOSTED_ENV).map_or(false, |v| !v.is_empty())
}

pub async fn init(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS hosted_servers (
            id         TEXT PRIMARY KEY,   -- also its folder name under servers/
            name       TEXT NOT NULL,      -- what the panel calls it
            port       INTEGER NOT NULL UNIQUE,
            enabled    INTEGER NOT NULL DEFAULT 1,  -- started with the main server
            created_at TEXT NOT NULL
        )",
    ).execute(pool).await?;
    Ok(())
}

fn data_dir(id: &str) -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join("servers").join(id)
}

#[derive(Clone, Serialize, Default)]
pub struct Status {
    /// "starting", "running", "restarting" (it stopped; trying again),
    /// "stopped" or "failed" (couldn't start: see last_log).
    state:    String,
    pid:      Option<u32>,
    since:    String,
    restarts: u32,
    /// The last few lines it logged — why it failed, usually.
    last_log: Vec<String>,
}

struct Running {
    stop:   tokio::sync::watch::Sender<bool>,
    status: Arc<Mutex<Status>>,
    task:   tokio::task::JoinHandle<()>,
}

/// The hosted servers this one is running.
#[derive(Default)]
pub struct Hosting {
    running: Mutex<HashMap<String, Running>>,
}

impl Hosting {
    pub fn new() -> Self { Self::default() }

    fn status(&self, id: &str) -> Status {
        self.running.lock().unwrap().get(id).map(|r| r.status.lock().unwrap().clone())
            .unwrap_or_else(|| Status { state: "stopped".into(), ..Default::default() })
    }

    /// Starts one (if it isn't running already) and keeps it going.
    fn start(&self, id: &str, name: &str, port: u16) {
        let mut running = self.running.lock().unwrap();
        if let Some(r) = running.get(id) {
            if !r.task.is_finished() { return; }
        }
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let status = Arc::new(Mutex::new(Status { state: "starting".into(), since: now(), ..Default::default() }));
        let task = tokio::spawn(supervise(id.to_string(), name.to_string(), port, stop_rx, status.clone()));
        running.insert(id.to_string(), Running { stop, status, task });
    }

    /// Stops one and waits (a few seconds at most) for it to exit.
    async fn stop(&self, id: &str) {
        let r = self.running.lock().unwrap().remove(id);
        if let Some(r) = r {
            let _ = r.stop.send(true);
            let _ = tokio::time::timeout(Duration::from_secs(12), r.task).await;
        }
    }

    /// Stops them all (the main server is shutting down).
    pub async fn stop_all(&self) {
        let all: Vec<Running> = self.running.lock().unwrap().drain().map(|(_, r)| r).collect();
        for r in &all { let _ = r.stop.send(true); }
        for r in all { let _ = tokio::time::timeout(Duration::from_secs(12), r.task).await; }
    }
}

fn now() -> String { chrono::Utc::now().to_rfc3339() }

/// Runs one hosted server until told to stop, restarting it when it exits
/// on its own (after 2 s, doubling up to a minute while it keeps failing
/// quickly).
async fn supervise(id: String, name: String, port: u16, mut stop: tokio::sync::watch::Receiver<bool>, status: Arc<Mutex<Status>>) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            let mut st = status.lock().unwrap();
            st.state = "failed".into();
            st.last_log = vec![format!("can't find this program to start it: {e}")];
            return;
        }
    };
    let dir = data_dir(&id);
    let mut backoff = Duration::from_secs(2);
    loop {
        if *stop.borrow() { break; }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            let mut st = status.lock().unwrap();
            st.state = "failed".into();
            st.last_log = vec![format!("can't make {}: {e}", dir.display())];
            return;
        }
        let child = tokio::process::Command::new(&exe)
            .arg("--data-dir").arg(&dir)
            .arg("--port").arg(port.to_string())
            .env(HOSTED_ENV, &id)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let mut st = status.lock().unwrap();
                st.state = "failed".into();
                st.last_log = vec![format!("couldn't start: {e}")];
                return;
            }
        };
        let pid = child.id();
        tracing::info!("hosting: started \"{name}\" on port {port} (pid {})", pid.unwrap_or(0));
        {
            let mut st = status.lock().unwrap();
            st.state = "running".into();
            st.pid = pid;
            st.since = now();
            st.last_log.clear();
        }
        // Its log goes into ours, marked with its name; the last lines are
        // kept for the panel.
        if let Some(err) = child.stderr.take() {
            let (name, status) = (name.clone(), status.clone());
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(err).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("[{name}] {line}");
                    let mut st = status.lock().unwrap();
                    st.last_log.push(line);
                    let n = st.last_log.len();
                    if n > 8 { st.last_log.drain(..n - 8); }
                }
            });
        }
        let started = std::time::Instant::now();
        tokio::select! {
            r = child.wait() => {
                let why = match r { Ok(s) => s.to_string(), Err(e) => e.to_string() };
                tracing::warn!("hosting: \"{name}\" stopped by itself ({why})");
                {
                    let mut st = status.lock().unwrap();
                    st.state = "restarting".into();
                    st.pid = None;
                    st.restarts += 1;
                }
                // Ran a good while: it was a one-off, so try again soon.
                if started.elapsed() > Duration::from_secs(120) { backoff = Duration::from_secs(2); }
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = stop.changed() => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
            _ = stop.changed() => {
                // Ask it to stop cleanly (it saves and tells its apps it's
                // restarting), then make sure.
                terminate(&mut child).await;
                break;
            }
        }
    }
    let mut st = status.lock().unwrap();
    st.state = "stopped".into();
    st.pid = None;
    st.since = now();
    tracing::info!("hosting: stopped \"{name}\"");
}

pub(crate) async fn terminate(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            let _ = std::process::Command::new("kill").arg("-TERM").arg(pid.to_string()).status();
            if tokio::time::timeout(Duration::from_secs(10), child.wait()).await.is_ok() { return; }
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Starts every enabled hosted server (when the main one starts).
pub async fn start_all(s: &AppState) {
    if is_hosted() { return; }
    let rows = sqlx::query("SELECT id, name, port FROM hosted_servers WHERE enabled = 1")
        .fetch_all(&s.pool).await.unwrap_or_default();
    for r in rows {
        let port: i64 = r.get("port");
        s.hosting.start(&r.get::<String, _>("id"), &r.get::<String, _>("name"), port as u16);
    }
}

// ── Admin routes ──────────────────────────────────────────────────────────────

fn require_main() -> Result<(), ApiErr> {
    if is_hosted() {
        Err(err(StatusCode::FORBIDDEN, "This server is hosted by another Chriscord server — manage servers from that one's admin panel"))
    } else { Ok(()) }
}

async fn row(pool: &SqlitePool, id: &str) -> Result<(String, u16, bool), ApiErr> {
    let r = sqlx::query("SELECT name, port, enabled FROM hosted_servers WHERE id = ?")
        .bind(id).fetch_optional(pool).await.map_err(|_| dberr())?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "No such server"))?;
    Ok((r.get("name"), r.get::<i64, _>("port") as u16, r.get::<i64, _>("enabled") != 0))
}

/// GET /api/admin/hosting — the hosted servers and how they're doing.
pub async fn list(headers: HeaderMap, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    if is_hosted() {
        return Ok(Json(serde_json::json!({ "available": false, "port": crate::utils::port(), "servers": [] })));
    }
    let rows = sqlx::query("SELECT id, name, port, enabled, created_at FROM hosted_servers ORDER BY created_at")
        .fetch_all(&s.pool).await.map_err(|_| dberr())?;
    let servers: Vec<serde_json::Value> = rows.iter().map(|r| {
        let id: String = r.get("id");
        serde_json::json!({
            "id": id, "name": r.get::<String, _>("name"), "port": r.get::<i64, _>("port"),
            "enabled": r.get::<i64, _>("enabled") != 0, "created_at": r.get::<String, _>("created_at"),
            "data_dir": data_dir(&id).display().to_string(),
            "status": s.hosting.status(&id),
        })
    }).collect();
    Ok(Json(serde_json::json!({ "available": true, "port": crate::utils::port(), "servers": servers })))
}

#[derive(Deserialize)]
pub struct CreateReq {
    name: String,
    port: u16,
}

/// POST /api/admin/hosting {name, port} — makes a new server and starts it.
pub async fn create(headers: HeaderMap, State(s): State<Arc<AppState>>, Json(b): Json<CreateReq>) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    require_main()?;
    let name: String = b.name.trim().chars().take(64).collect();
    if name.is_empty() { return Err(bad("Give it a name")); }
    if b.port < 1024 { return Err(bad("Use a port from 1024 to 65535")); }
    if b.port == crate::utils::port() { return Err(bad("That's this server's own port")); }
    let taken = sqlx::query("SELECT 1 FROM hosted_servers WHERE port = ?").bind(b.port as i64)
        .fetch_optional(&s.pool).await.map_err(|_| dberr())?.is_some();
    if taken { return Err(bad("Another hosted server already uses that port")); }
    let count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM hosted_servers").fetch_one(&s.pool).await
        .map_err(|_| dberr())?.get("n");
    if count >= MAX_SERVERS { return Err(bad("That's as many servers as this one hosts")); }
    // Free right now? (Something else on the machine may be using it.)
    if std::net::TcpListener::bind(("0.0.0.0", b.port)).is_err() || std::net::UdpSocket::bind(("0.0.0.0", b.port)).is_err() {
        return Err(bad("Something on this machine is already using that port"));
    }
    let id = format!("srv-{}", crate::utils::generate_hex(10));
    sqlx::query("INSERT INTO hosted_servers (id, name, port, enabled, created_at) VALUES (?, ?, ?, 1, ?)")
        .bind(&id).bind(&name).bind(b.port as i64).bind(now())
        .execute(&s.pool).await.map_err(|_| dberr())?;
    // Its name in its own settings too, so the app shows it.
    if let Ok(pool) = child_pool(&id).await {
        let _ = crate::db::set_config(&pool, "server_name", &name).await;
        pool.close().await;
    }
    s.hosting.start(&id, &name, b.port);
    tracing::info!("hosting: made \"{name}\" on port {}", b.port);
    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

/// The hosted server's own database (made if it isn't there yet), for
/// reading its owner key and setting its name before it first starts.
async fn child_pool(id: &str) -> Result<SqlitePool, sqlx::Error> {
    let dir = data_dir(id);
    let _ = std::fs::create_dir_all(&dir);
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.join("chriscord.db"))
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
    let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect_with(opts).await?;
    sqlx::query("CREATE TABLE IF NOT EXISTS config (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
        .execute(&pool).await?;
    Ok(pool)
}

/// POST /api/admin/hosting/:id/start
pub async fn start(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    action(headers, id, "start", s).await
}
/// POST /api/admin/hosting/:id/stop — keeps it stopped (also across
/// restarts of the main server) until it's started again.
pub async fn stop(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    action(headers, id, "stop", s).await
}
/// POST /api/admin/hosting/:id/restart
pub async fn restart(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    action(headers, id, "restart", s).await
}

async fn action(headers: HeaderMap, id: String, action: &str, s: Arc<AppState>) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    require_main()?;
    let (name, port, _) = row(&s.pool, &id).await?;
    match action {
        "start" => {
            sqlx::query("UPDATE hosted_servers SET enabled = 1 WHERE id = ?").bind(&id).execute(&s.pool).await.map_err(|_| dberr())?;
            s.hosting.start(&id, &name, port);
        }
        "stop" => {
            sqlx::query("UPDATE hosted_servers SET enabled = 0 WHERE id = ?").bind(&id).execute(&s.pool).await.map_err(|_| dberr())?;
            s.hosting.stop(&id).await;
        }
        "restart" => {
            sqlx::query("UPDATE hosted_servers SET enabled = 1 WHERE id = ?").bind(&id).execute(&s.pool).await.map_err(|_| dberr())?;
            s.hosting.stop(&id).await;
            s.hosting.start(&id, &name, port);
        }
        _ => return Err(err(StatusCode::NOT_FOUND, "Unknown action")),
    }
    Ok(Json(serde_json::json!({ "ok": true, "status": s.hosting.status(&id) })))
}

#[derive(Deserialize)]
pub struct RenameReq {
    name: String,
}

/// PATCH /api/admin/hosting/:id {name} — renames it in this list (its own
/// server name is set in its own panel).
pub async fn rename(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>, Json(b): Json<RenameReq>) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    require_main()?;
    let name: String = b.name.trim().chars().take(64).collect();
    if name.is_empty() { return Err(bad("Give it a name")); }
    row(&s.pool, &id).await?;
    sqlx::query("UPDATE hosted_servers SET name = ? WHERE id = ?").bind(&name).bind(&id)
        .execute(&s.pool).await.map_err(|_| dberr())?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/admin/hosting/:id/owner-key — its owner key, for its admin panel.
pub async fn owner_key(headers: HeaderMap, Path(id): Path<String>, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    require_main()?;
    row(&s.pool, &id).await?;
    let pool = child_pool(&id).await.map_err(|_| dberr())?;
    let key = crate::db::get_or_create_config(&pool, "owner_key", || crate::utils::generate_hex(32)).await.map_err(|_| dberr())?;
    pool.close().await;
    Ok(Json(serde_json::json!({ "owner_key": key })))
}

#[derive(Deserialize)]
pub struct RemoveQuery {
    /// "1": delete its data folder too.
    #[serde(default)]
    delete_data: Option<String>,
}

/// DELETE /api/admin/hosting/:id[?delete_data=1] — stops it and takes it
/// off the list. Its data folder stays unless delete_data is set.
pub async fn remove(headers: HeaderMap, Path(id): Path<String>, Query(q): Query<RemoveQuery>, State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    check_owner(&headers, &s)?;
    require_main()?;
    let (name, _, _) = row(&s.pool, &id).await?;
    s.hosting.stop(&id).await;
    sqlx::query("DELETE FROM hosted_servers WHERE id = ?").bind(&id).execute(&s.pool).await.map_err(|_| dberr())?;
    let dir = data_dir(&id);
    let deleted = q.delete_data.as_deref() == Some("1")
        && id.starts_with("srv-") && !id.contains('/') && !id.contains('\\') && !id.contains("..")
        && std::fs::remove_dir_all(&dir).is_ok();
    tracing::info!("hosting: removed \"{name}\"{}", if deleted { " and its data" } else { "" });
    Ok(Json(serde_json::json!({ "ok": true, "data_deleted": deleted, "data_dir": dir.display().to_string() })))
}
