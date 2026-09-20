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
    /// username → the voice board_id they're currently connected to. A user
    /// can be in at most one voice channel at a time — joining a new one
    /// simply overwrites their existing entry, which is what makes "moving"
    /// between voice channels work without any separate leave step.
    pub voice: Mutex<HashMap<String, String>>,
    /// The actual WebRTC SFU state — PeerConnections and forwarded audio
    /// sources per participant. Deliberately a separate field from `voice`
    /// above (which is pure presence tracking) to avoid conflating "who's
    /// in the channel" with "what's their live media connection state".
    pub voice_runtime: crate::voice::VoiceRuntime,
}
