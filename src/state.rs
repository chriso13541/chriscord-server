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
    /// username → chosen presence while connected: "online", "idle" (away,
    /// manual or automatic) or "invisible" (connected but shown offline to
    /// everyone else). Absent = "online". Cleared when their last
    /// connection closes.
    pub presence: Mutex<HashMap<String, String>>,
    /// nonce_hex → (issued_at, public_key_hex)
    /// Challenges expire after 60 seconds.
    pub challenges: Mutex<HashMap<String, (Instant, String)>>,
    /// username → the voice board_id they're currently connected to. A user
    /// can be in at most one voice channel at a time — joining a new one
    /// simply overwrites their existing entry, which is what makes "moving"
    /// between voice channels work without any separate leave step.
    pub voice: Mutex<HashMap<String, String>>,
    /// username → (muted, deafened) for whoever is currently in a voice
    /// channel. Keyed by username alone, matching `voice` above, since a
    /// user can only be in one voice channel at a time. Missing entry
    /// means not muted/deafened — used so a client joining a voice
    /// channel already in progress can be told everyone's current status
    /// immediately, rather than only learning it the next time someone
    /// happens to toggle.
    pub voice_status: Mutex<HashMap<String, (bool, bool)>>,
    /// People whose connection dropped while they were in a call:
    /// username → (voice board, drop id). They stay listed in that channel
    /// as "reconnecting" (faded, red name) for VOICE_REJOIN_GRACE, so
    /// everyone can tell a dropped connection from someone leaving. Cleared
    /// when they rejoin, leave on purpose, or the grace period runs out.
    pub voice_reconnecting: Mutex<HashMap<String, (String, u64)>>,
    /// The actual WebRTC SFU state — PeerConnections and forwarded audio
    /// sources per participant. Deliberately a separate field from `voice`
    /// above (which is pure presence tracking) to avoid conflating "who's
    /// in the channel" with "what's their live media connection state".
    pub voice_runtime: crate::voice::VoiceRuntime,
    /// link token (UUID v4) → (stored filename it unlocks, expiry). Issued
    /// by GET /api/files/:filename/link to logged-in users; /api/files only
    /// serves a file to a valid, unexpired token for that exact file (or to
    /// a request carrying a session token). In memory on purpose: a server
    /// restart just means clients ask for fresh links.
    pub file_links: Mutex<HashMap<String, (String, Instant)>>,
    /// "Download all" zips and their links (see zips.rs).
    pub zips: crate::zips::Zips,
}
