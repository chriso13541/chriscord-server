use sqlx::{sqlite::{SqliteConnectOptions, SqlitePoolOptions}, Row, SqlitePool};
use std::str::FromStr;

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
        "CREATE TABLE IF NOT EXISTS sessions (
            token      TEXT PRIMARY KEY,
            username   TEXT NOT NULL,
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

    // ── Migrations ────────────────────────────────────────────────────────────
    // Try to add new columns to existing tables. SQLite returns an error if the
    // column already exists — we silently ignore those so this is always safe to
    // run against both fresh and older databases.
    let migrations: &[&str] = &[
        "ALTER TABLE messages ADD COLUMN attachment_url  TEXT",
        "ALTER TABLE messages ADD COLUMN attachment_name TEXT",
        "ALTER TABLE messages ADD COLUMN attachment_mime TEXT",
        "ALTER TABLE messages ADD COLUMN edited      INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE messages ADD COLUMN attachments TEXT",  // JSON array of {url,name,mime}
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
