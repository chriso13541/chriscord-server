mod admin;
mod auth;
mod db;
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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let pool = db::init().await.expect("Failed to initialize database");
    pfp::migrate_to_key_names(&pool).await; // pfps/<username>.* → pfps/<public key>.*
    roles::init(&pool).await.expect("Failed to set up roles");

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
    });

    println!();
    println!("  ╔══════════════════════════════════════════╗");
    println!("  ║         C H R I S C O R D                ║");
    println!("  ╠══════════════════════════════════════════╣");
    println!("  ║  Owner key : {}  ║", &owner_key);
    println!("  ║  Admin UI  : http://0.0.0.0:7070/admin   ║");
    println!("  ║  Port      : 7070 (TCP + UDP)             ║");
    println!("  ╚══════════════════════════════════════════╝");
    println!();

    let app = Router::new()
        .route("/admin",                get(admin::admin_ui))
        .route("/api/admin/info",       get(admin::get_admin_info))
        .route("/api/admin/settings",   post(admin::update_settings))
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
        .route("/api/admin/roles/order", post(roles::admin_order))
        .route("/api/admin/roles/:id",  put(roles::admin_update).delete(roles::admin_delete))
        .route("/api/admin/members/:username/roles", put(roles::admin_set_member_roles))
        .route("/api/roles",            get(roles::get_roles))
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
    axum::serve(listener, app).await.unwrap();
}
