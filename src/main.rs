mod access;
mod admin;
mod invites;
mod layout;
mod auth;
mod db;
mod emojis;
mod stickers;
mod files;
mod messages;
mod pfp;
mod profile;
mod preview;
mod rooms;
mod state;
mod utils;
mod voice;
mod roles;
mod ws;
mod zipfile;
mod zips;

use axum::{
    extract::DefaultBodyLimit,
    routing::{delete, get, patch, post, put},
    Router,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;

use state::AppState;

const USAGE: &str = "\
chriscord-server — self-hosted chat server

USAGE:
    chriscord-server [--data-dir DIR] [COMMAND]

COMMANDS:
    (none)             Run the server (port 7070, TCP + UDP)
    owner-key          Print the owner key (for the admin panel) and exit
    reset-owner-key    Make a new owner key, print it, and exit — the old one
                       stops working once the server restarts
    help               Show this

DATA DIRECTORY:
    Where the database, uploads, profile pictures and server images live.
    --data-dir DIR, or the CHRISCORD_DATA_DIR environment variable, or else
    the current directory. The systemd unit uses /var/lib/chriscord.

    sudo -u chriscord chriscord-server --data-dir /var/lib/chriscord owner-key
";

/// Moves into the data directory (everything the server stores uses paths
/// relative to it) and returns the command, if any.
fn parse_args() -> Option<String> {
    let mut args = std::env::args().skip(1);
    let mut data_dir = std::env::var("CHRISCORD_DATA_DIR").ok().filter(|d| !d.is_empty());
    let mut command = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--data-dir" => data_dir = Some(args.next().unwrap_or_else(|| { eprintln!("--data-dir needs a directory"); std::process::exit(2) })),
            "-h" | "--help" | "help" => { print!("{USAGE}"); std::process::exit(0) }
            c if command.is_none() && !c.starts_with('-') => command = Some(c.to_string()),
            other => { eprintln!("Unknown argument: {other}\n\n{USAGE}"); std::process::exit(2) }
        }
    }
    if let Some(dir) = data_dir {
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::env::set_current_dir(&dir)) {
            eprintln!("Can't use data directory {dir}: {e}");
            std::process::exit(1);
        }
    }
    command
}

#[tokio::main]
async fn main() {
    let command = parse_args();
    // Colours only in a terminal — not in the systemd journal.
    use std::io::IsTerminal;
    let interactive = std::io::stdout().is_terminal();
    // Logs go to stderr, so a command like `owner-key` prints only the key
    // on stdout (handy for scripts); systemd's journal collects both.
    tracing_subscriber::fmt().with_ansi(std::io::stderr().is_terminal()).with_writer(std::io::stderr).init();

    if command.is_some() { refuse_root_in_service_dir(); }
    let pool = db::init().await.expect("Failed to initialize database");

    // One-off commands: answer and exit without starting the server.
    match command.as_deref() {
        None => {}
        Some("owner-key") => {
            let key = db::get_or_create_config(&pool, "owner_key", || utils::generate_hex(32))
                .await.expect("Failed to load owner key");
            println!("{key}");
            return;
        }
        Some("reset-owner-key") => {
            let key = utils::generate_hex(32);
            sqlx::query("INSERT INTO config (key, value) VALUES ('owner_key', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
                .bind(&key).execute(&pool).await.expect("Failed to save the new owner key");
            println!("{key}");
            eprintln!("New owner key saved. Restart the server for it to take effect (e.g. sudo systemctl restart chriscord).");
            return;
        }
        Some(other) => { eprintln!("Unknown command: {other}\n\n{USAGE}"); std::process::exit(2) }
    }

    pfp::migrate_to_key_names(&pool).await; // pfps/<username>.* → pfps/<public key>.*
    roles::init(&pool).await.expect("Failed to set up roles");
    access::init(&pool).await.expect("Failed to set up channel access");
    invites::init(&pool).await.expect("Failed to set up invites");
    layout::init(&pool).await.expect("Failed to set up channel ordering");
    emojis::init(&pool).await.expect("Failed to set up custom emoji");
    stickers::init(&pool).await.expect("Failed to set up stickers");
    zips::clear_leftovers(); // "Download all" zips from before a restart

    let owner_key =
        db::get_or_create_config(&pool, "owner_key", || utils::generate_hex(32))
            .await.expect("Failed to load owner key");

    db::get_or_create_config(&pool, "server_key", || utils::generate_hex(8))
        .await.expect("Failed to load server key");

    let (tx, _) = broadcast::channel::<String>(1024);

    let state = Arc::new(AppState {
        pool,
        owner_key: owner_key.clone(),
        tx,
        online:     Mutex::new(HashMap::new()),
        presence: Mutex::new(HashMap::new()),
        challenges: Mutex::new(HashMap::new()),
        voice:      Mutex::new(HashMap::new()),
        voice_status: Mutex::new(HashMap::new()),
        voice_reconnecting: Mutex::new(HashMap::new()),
        voice_runtime: voice::VoiceRuntime::new().await,
        file_links: Mutex::new(HashMap::new()),
        zips: zips::Zips::new(),
    });
    let state_for_shutdown = state.clone();

    if interactive {
        println!();
        println!("  ╔══════════════════════════════════════════╗");
        println!("  ║         C H R I S C O R D                ║");
        println!("  ╠══════════════════════════════════════════╣");
        println!("  ║  Owner key : {}  ║", &owner_key);
        println!("  ║  Admin UI  : http://0.0.0.0:7070/admin   ║");
        println!("  ║  Port      : 7070 (TCP + UDP)             ║");
        println!("  ╚══════════════════════════════════════════╝");
        println!();
    } else {
        // Running as a service: the key stays out of the logs. Ask for it
        // with the owner-key command instead.
        let dir = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
        tracing::info!("data directory: {dir}");
        tracing::info!("owner key: run `chriscord-server --data-dir {dir} owner-key` to see it");
    }

    let app = Router::new()
        .route("/admin",                get(admin::admin_ui))
        .route("/api/admin/info",       get(admin::get_admin_info))
        .route("/api/admin/settings",   post(admin::update_settings))
        .route("/api/admin/storage",    get(admin::storage))
        .route("/api/admin/rooms",      get(admin::list_rooms_admin).post(admin::create_room))
        .route("/api/admin/rooms/:id",  delete(admin::delete_room))
        .route("/api/admin/boards",     post(admin::create_board))
        .route("/api/admin/boards/:id", delete(admin::delete_board))
        .route("/api/admin/banner",     post(admin::upload_banner).delete(admin::delete_banner)
            .layer(DefaultBodyLimit::max(9 << 20))) // banner uploads are up to 8 MB
        .route("/api/admin/members",    get(admin::list_members))
        .route("/api/admin/members/:username/kick", post(admin::kick_member))
        .route("/api/admin/owner",      post(admin::set_owner))
        .route("/api/admin/roles",      get(roles::admin_list).post(roles::admin_create))
        .route("/api/admin/access",     get(access::admin_get))
        .route("/api/admin/rooms/:id/access",  put(access::admin_set_room))
        .route("/api/admin/boards/:id/access", put(access::admin_set_board))
        .route("/api/admin/roles/order", post(roles::admin_order))
        .route("/api/admin/roles/:id",  put(roles::admin_update).delete(roles::admin_delete))
        .route("/api/admin/members/:username/roles", put(roles::admin_set_member_roles))
        .route("/api/roles",            get(roles::get_roles))
        .route("/api/layout/rooms",     put(layout::order_rooms))
        .route("/api/emojis",           get(emojis::list).post(emojis::upload)
            .layer(DefaultBodyLimit::disable())) // emoji images have no size limit
        .route("/api/emojis/:id",       get(emojis::image).patch(emojis::update).delete(emojis::remove))
        .route("/api/stickers",         get(stickers::list).post(stickers::upload)
            .layer(DefaultBodyLimit::disable())) // no size limit, like emoji
        .route("/api/stickers/:id",     get(stickers::image).patch(stickers::update).delete(stickers::remove))
        .route("/api/layout/boards/:id", put(layout::move_board))
        .route("/api/invites",          post(invites::create))
        .route("/invite/:code",         get(invites::landing))
        .route("/api/members/:username/kick", post(roles::kick_member))
        .route("/api/admin/bans",       get(admin::list_bans))
        .route("/api/admin/bans/:public_key", delete(admin::unban))
        .route("/api/server/banner",    get(admin::serve_banner))
        .route("/api/admin/icon",       post(admin::upload_icon).delete(admin::delete_icon)
            .layer(DefaultBodyLimit::max(9 << 20)))
        .route("/api/server/icon",      get(admin::serve_icon))
        .route("/api/admin/theme",      get(admin::get_theme).post(admin::set_theme))
        .route("/api/admin/theme/background", post(admin::upload_theme_bg).delete(admin::delete_theme_bg)
            .layer(DefaultBodyLimit::max(9 << 20)))
        .route("/api/server/theme/background", get(admin::serve_theme_bg))
        .route("/api/info",             get(auth::server_info))
        .route("/api/challenge",        post(auth::challenge))
        .route("/api/join",             post(auth::join))
        .route("/api/rooms",            get(rooms::list_rooms))
        .route("/api/rooms/:id/boards", get(rooms::list_boards))
        .route("/api/boards/:id/messages",
            get(messages::get_messages).post(messages::post_message))
        .route("/api/boards/:id/messages/around/:message_id", get(messages::get_messages_around))
        .route("/api/search", get(messages::search_messages))
        .route("/api/messages/:id",
            patch(messages::edit_message).delete(messages::delete_message))
        .route("/api/boards/:id/pins",   get(messages::get_pins))
        .route("/api/messages/:id/reactions", post(messages::react))
        .route("/api/messages/:id/pin",  put(messages::pin_message).delete(messages::unpin_message))
        .route("/api/upload", post(files::upload)
            .layer(DefaultBodyLimit::disable()))
        .route("/api/files/:filename",  get(files::serve_file))
        .route("/api/files/:filename/link", get(files::file_link))
        .route("/api/messages/:id/zip/link", get(zips::zip_link))
        .route("/api/zips/:file",       get(zips::serve_zip))
        .route("/api/pfp/:username",    get(pfp::serve_pfp))
        .route("/api/profile/:username", get(profile::get_profile))
        .route("/api/banner/:username",  get(profile::serve_banner))
        .route("/api/preview",          get(preview::get_preview))
        .route("/ws",                   get(ws::ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:7070")
        .await.expect("Failed to bind port 7070");

    tracing::info!("chriscord-server listening on 0.0.0.0:7070");

    // Stop cleanly on Ctrl+C or SIGTERM (`systemctl stop/restart`): tell the
    // apps it's a restart (they reconnect on their own, and rejoin calls),
    // flush the database, and exit — without waiting on open connections,
    // which would otherwise hold up a restart.
    let shutdown_state = state_for_shutdown;
    tokio::select! {
        r = axum::serve(listener, app) => { r.unwrap(); }
        _ = shutdown_signal() => {
            tracing::info!("shutting down");
            let _ = shutdown_state.tx.send(serde_json::json!({ "type": "server_restarting" }).to_string());
            tokio::time::sleep(std::time::Duration::from_millis(300)).await; // let that notice go out
            shutdown_state.pool.close().await;
        }
    }
}

/// Running `owner-key` as root inside the service's data directory would
/// leave root-owned database files behind that the service then can't
/// write to. Point people at the right way instead.
fn refuse_root_in_service_dir() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let am_root = std::fs::read_to_string("/proc/self/status").ok()
            .and_then(|st| st.lines().find(|l| l.starts_with("Uid:")).map(|l| l.split_whitespace().nth(1) == Some("0")))
            .unwrap_or(false);
        let dir_owner = std::fs::metadata(".").map(|m| m.uid()).unwrap_or(0);
        if am_root && dir_owner != 0 {
            let dir = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
            eprintln!("This data directory belongs to the service's user — run it as that user so file ownership stays right:");
            eprintln!("    sudo -u chriscord chriscord-server --data-dir {dir} {}", std::env::args().last().unwrap_or_default());
            std::process::exit(1);
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async { let _ = tokio::signal::ctrl_c().await; };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => { s.recv().await; }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}
