mod admin;
mod auth;
mod db;
mod messages;
mod rooms;
mod state;
mod utils;
mod ws;

use axum::{
    routing::{delete, get, post},
    Router,
};
use std::sync::Arc;
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;

use state::AppState;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // ── Database ──────────────────────────────────────────────────────────────
    let pool = db::init().await.expect("Failed to initialize database");

    // ── Owner key — generated once, stored in DB forever ─────────────────────
    let owner_key =
        db::get_or_create_config(&pool, "owner_key", || utils::generate_hex(32))
            .await
            .expect("Failed to load owner key");

    // ── Default join key (8-char hex) if not set yet ──────────────────────────
    db::get_or_create_config(&pool, "server_key", || utils::generate_hex(8))
        .await
        .expect("Failed to load server key");

    // ── Broadcast channel for real-time WS messages ───────────────────────────
    // Capacity 1024 — enough headroom for brief spikes; lagged receivers are
    // handled gracefully in ws.rs.
    let (tx, _) = broadcast::channel::<String>(1024);

    let state = Arc::new(AppState {
        pool,
        owner_key: owner_key.clone(),
        tx,
    });

    // ── Print startup banner ──────────────────────────────────────────────────
    println!();
    println!("  ╔══════════════════════════════════════════╗");
    println!("  ║         C H R I S C O R D                ║");
    println!("  ╠══════════════════════════════════════════╣");
    println!("  ║  Owner key : {}  ║", &owner_key);
    println!("  ║  Admin UI  : http://0.0.0.0:7070/admin   ║");
    println!("  ║  Port      : 7070                         ║");
    println!("  ╚══════════════════════════════════════════╝");
    println!();
    println!("  ⚠  Keep your owner key private.");
    println!("  ⚠  Give friends the JOIN key (shown in admin UI), not the owner key.");
    println!();

    // ── Router ────────────────────────────────────────────────────────────────
    let app = Router::new()
        // Admin
        .route("/admin",                  get(admin::admin_ui))
        .route("/api/admin/info",         get(admin::get_admin_info))
        .route("/api/admin/settings",     post(admin::update_settings))
        .route("/api/admin/rooms",        post(admin::create_room))
        .route("/api/admin/rooms/:id",    delete(admin::delete_room))
        .route("/api/admin/boards",       post(admin::create_board))
        .route("/api/admin/boards/:id",   delete(admin::delete_board))
        // Public
        .route("/api/info",               get(auth::server_info))
        .route("/api/join",               post(auth::join))
        // Authenticated
        .route("/api/rooms",              get(rooms::list_rooms))
        .route("/api/rooms/:id/boards",   get(rooms::list_boards))
        .route(
            "/api/boards/:id/messages",
            get(messages::get_messages).post(messages::post_message),
        )
        // WebSocket
        .route("/ws",                     get(ws::ws_handler))
        // Allow all origins — fine for LAN homelab; add restriction in prod
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:7070")
        .await
        .expect("Failed to bind port 7070");

    tracing::info!("chriscord-server listening on 0.0.0.0:7070");
    axum::serve(listener, app).await.unwrap();
}
