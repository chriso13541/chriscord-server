use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;
use tokio::sync::broadcast;

pub struct AppState {
    pub pool:      SqlitePool,
    pub owner_key: String,
    pub tx:        broadcast::Sender<String>,
    /// username → connection count for presence tracking
    pub online: Mutex<HashMap<String, usize>>,
    /// nonce_hex → (issued_at, public_key_hex)
    /// Challenges expire after 60 seconds.
    pub challenges: Mutex<HashMap<String, (Instant, String)>>,
}
