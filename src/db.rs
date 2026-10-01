use sqlx::{sqlite::{SqliteConnectOptions, SqlitePoolOptions}, Row, SqlitePool};
use std::str::FromStr;

/// Whether the messages_fts full-text index is available (see init).
pub static FTS_AVAILABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub async fn init() -> Result<SqlitePool, sqlx::Error> {
    let opts = SqliteConnectOptions::from_str("sqlite:./chriscord.db")?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal); // WAL mode for concurrent reads

    let pool = SqlitePoolOptions::new()
        .max_connections(20)
        .connect_with(opts)
        .await?;

    // Create all tables on first run
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS config (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS users (
            public_key TEXT PRIMARY KEY,
            username   TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS sessions (
            token      TEXT PRIMARY KEY,
            username   TEXT NOT NULL,
            public_key TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS rooms (
            id         TEXT PRIMARY KEY,
            name       TEXT NOT NULL,
            is_private INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS boards (
            id         TEXT PRIMARY KEY,
            room_id    TEXT NOT NULL,
            name       TEXT NOT NULL,
            created_at TEXT NOT NULL,
            FOREIGN KEY (room_id) REFERENCES rooms(id)
        )",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS messages (
            id              TEXT PRIMARY KEY,
            board_id        TEXT NOT NULL,
            username        TEXT NOT NULL,
            content         TEXT NOT NULL,
            attachment_url  TEXT,
            attachment_name TEXT,
            attachment_mime TEXT,
            created_at      TEXT NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    // Index for fast message lookups per board
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_messages_board ON messages (board_id, created_at)",
    )
    .execute(&pool)
    .await?;

    // Supports the cursor-based "load older messages" pagination in
    // messages::get_messages / ws::load_history, which orders and filters by
    // (board_id, id) rather than created_at.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_messages_board_id ON messages (board_id, id)",
    )
    .execute(&pool)
    .await?;

    // Full-text search index over message text (SQLite FTS5), kept in step
    // with the messages table by triggers. Word-based, case- and accent-
    // insensitive, and indexed — so search stays fast however many messages
    // there are. Built from existing messages the first time it's created.
    // If this SQLite lacks FTS5, search falls back to the old LIKE scan.
    let fts_existed = sqlx::query("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'messages_fts'")
        .fetch_optional(&pool).await?.is_some();
    let fts_ok = sqlx::query(
        "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            content, content='messages', content_rowid='rowid',
            tokenize='unicode61 remove_diacritics 2'
        )",
    ).execute(&pool).await.is_ok();
    if fts_ok {
        for trigger in [
            "CREATE TRIGGER IF NOT EXISTS messages_fts_ai AFTER INSERT ON messages BEGIN
                INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content); END",
            "CREATE TRIGGER IF NOT EXISTS messages_fts_ad AFTER DELETE ON messages BEGIN
                INSERT INTO messages_fts(messages_fts, rowid, content) VALUES ('delete', old.rowid, old.content); END",
            "CREATE TRIGGER IF NOT EXISTS messages_fts_au AFTER UPDATE OF content ON messages BEGIN
                INSERT INTO messages_fts(messages_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
                INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content); END",
        ] {
            sqlx::query(trigger).execute(&pool).await?;
        }
        if !fts_existed {
            sqlx::query("INSERT INTO messages_fts(messages_fts) VALUES ('rebuild')").execute(&pool).await?;
            tracing::info!("search: built the full-text index from existing messages");
        }
    } else {
        tracing::warn!("search: FTS5 isn't available in this SQLite build — using slower substring search");
    }
    FTS_AVAILABLE.store(fts_ok, std::sync::atomic::Ordering::Relaxed);

    // Emoji reactions: one row per (message, emoji, person). The primary key
    // makes reacting idempotent; created_at orders a message's reactions by
    // when each emoji was first used on it.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS reactions (
            message_id TEXT NOT NULL,
            emoji      TEXT NOT NULL,
            username   TEXT NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY (message_id, emoji, username)
        )",
    )
    .execute(&pool)
    .await?;

    // Banned account keys — refused at /api/join.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS bans (
            public_key TEXT PRIMARY KEY,
            username   TEXT NOT NULL,
            reason     TEXT NOT NULL DEFAULT '',
            banned_at  TEXT NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    // ── Migrations ────────────────────────────────────────────────────────────
    // Try to add new columns to existing tables. SQLite returns an error if the
    // column already exists — we silently ignore those so this is always safe to
    // run against both fresh and older databases.
    let migrations: &[&str] = &[
        "ALTER TABLE messages ADD COLUMN attachment_url  TEXT",
        "ALTER TABLE messages ADD COLUMN attachment_name TEXT",
        "ALTER TABLE messages ADD COLUMN attachment_mime TEXT",
        "ALTER TABLE messages ADD COLUMN edited      INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE messages ADD COLUMN attachments TEXT",
        "ALTER TABLE sessions ADD COLUMN public_key  TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE rooms    ADD COLUMN room_type   TEXT NOT NULL DEFAULT 'text'",
        // Pinned messages: NULL = not pinned. pinned_at orders the pins list
        // (most recently pinned first); pinned_by is who pinned it.
        "ALTER TABLE messages ADD COLUMN pinned_at   TEXT",
        "ALTER TABLE messages ADD COLUMN pinned_by   TEXT",
    ];
    for sql in migrations {
        let _ = sqlx::query(sql).execute(&pool).await;
    }

    Ok(pool)
}

// ── Config helpers ────────────────────────────────────────────────────────────

pub async fn get_or_create_config(
    pool: &SqlitePool,
    key: &str,
    gen: impl Fn() -> String,
) -> Result<String, sqlx::Error> {
    if let Some(row) = sqlx::query("SELECT value FROM config WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?
    {
        return Ok(row.get("value"));
    }
    let value = gen();
    sqlx::query("INSERT INTO config (key, value) VALUES (?, ?)")
        .bind(key)
        .bind(&value)
        .execute(pool)
        .await?;
    Ok(value)
}

pub async fn get_config(
    pool: &SqlitePool,
    key: &str,
) -> Result<Option<String>, sqlx::Error> {
    Ok(
        sqlx::query("SELECT value FROM config WHERE key = ?")
            .bind(key)
            .fetch_optional(pool)
            .await?
            .map(|r| r.get("value")),
    )
}

pub async fn set_config(
    pool: &SqlitePool,
    key: &str,
    value: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT OR REPLACE INTO config (key, value) VALUES (?, ?)")
        .bind(key)
        .bind(value)
        .execute(pool)
        .await?;
    Ok(())
}

// ── Session helpers ───────────────────────────────────────────────────────────

/// Returns the username associated with the token, or None if invalid.
pub async fn verify_token(
    pool: &SqlitePool,
    token: &str,
) -> Result<Option<String>, sqlx::Error> {
    Ok(
        sqlx::query("SELECT username FROM sessions WHERE token = ?")
            .bind(token)
            .fetch_optional(pool)
            .await?
            .map(|r| r.get("username")),
    )
}

/// Returns all distinct usernames that have ever joined this server.
pub async fn all_known_users(pool: &SqlitePool) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT DISTINCT username FROM sessions ORDER BY username ASC"
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(|r| r.get("username")).collect())
}
