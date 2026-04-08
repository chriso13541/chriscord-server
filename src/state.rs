use sqlx::SqlitePool;
use tokio::sync::broadcast;

pub struct AppState {
    pub pool:      SqlitePool,
    pub owner_key: String,
    /// Global broadcast channel — all WS connections subscribe to this.
    /// Payload is a JSON string; clients filter by board_id.
    pub tx: broadcast::Sender<String>,
}
