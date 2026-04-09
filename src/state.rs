use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::broadcast;

pub struct AppState {
    pub pool:      SqlitePool,
    pub owner_key: String,
    /// Global broadcast channel — all WS connections subscribe to this.
    pub tx: broadcast::Sender<String>,
    /// Tracks currently connected usernames → connection count.
    /// A user connecting from two tabs counts as one "online" user.
    pub online: Mutex<HashMap<String, usize>>,
}
