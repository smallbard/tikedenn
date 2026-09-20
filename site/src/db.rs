use rusqlite::Connection;

pub fn init_app_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS users (
            id         INTEGER PRIMARY KEY,
            username   TEXT NOT NULL UNIQUE COLLATE NOCASE,
            email      TEXT NOT NULL UNIQUE COLLATE NOCASE,
            pass_hash  TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS notes (
            id         INTEGER PRIMARY KEY,
            user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            title      TEXT NOT NULL,
            body       TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        "#,
    )?;
    Ok(())
}
