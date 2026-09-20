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
            is_admin   INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- Un projet regroupe des tâches et référence un dépôt git.
        CREATE TABLE IF NOT EXISTS projects (
            id         INTEGER PRIMARY KEY,
            user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            name       TEXT NOT NULL,
            git_repo   TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE (user_id, name)
        );

        -- Tâches du tableau kanban : draft → todo → progress → review → deploy → done.
        CREATE TABLE IF NOT EXISTS tasks (
            id          INTEGER PRIMARY KEY,
            user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            project_id  INTEGER REFERENCES projects(id) ON DELETE SET NULL,
            status      TEXT NOT NULL DEFAULT 'draft'
                        CHECK (status IN ('draft','todo','progress','review','deploy','done')),
            title       TEXT NOT NULL,
            description TEXT NOT NULL DEFAULT '',
            created_at  TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_tasks_user_status ON tasks(user_id, status);

        -- Pièces jointes (stockées en BLOB dans SQLite).
        CREATE TABLE IF NOT EXISTS files (
            id         INTEGER PRIMARY KEY,
            task_id    INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            name       TEXT NOT NULL,
            size       INTEGER NOT NULL,
            mime       TEXT NOT NULL DEFAULT '',
            data       BLOB NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_files_task ON files(task_id);
        "#,
    )?;

    // Migration légère : ajouter is_admin aux schémas locaux antérieurs.
    let has_col = conn
        .prepare("PRAGMA table_info(users)")
        .and_then(|mut st| {
            st.query_map([], |r| r.get::<_, String>(1))
                .map(|it| it.flatten().any(|n| n == "is_admin"))
        })
        .unwrap_or(true);
    if !has_col {
        let _ = conn.execute("ALTER TABLE users ADD COLUMN is_admin INTEGER NOT NULL DEFAULT 0", []);
    }

    // Migration : schémas antérieurs ont une CHECK restreinte à
    // draft/todo/progress/done (pas de 'review' ni 'deploy'). Le CREATE TABLE
    // IF NOT EXISTS ne peut pas la mettre à jour : on reconstruit la table
    // (copie des données) si la contrainte est ancienne.
    let tasks_sql = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'tasks'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_default();
    if !tasks_sql.contains("'review'") && !tasks_sql.is_empty() {
        conn.execute_batch(
            r#"
            PRAGMA foreign_keys = OFF;
            BEGIN;
            CREATE TABLE tasks_new (
                id          INTEGER PRIMARY KEY,
                user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                project_id  INTEGER REFERENCES projects(id) ON DELETE SET NULL,
                status      TEXT NOT NULL DEFAULT 'draft'
                            CHECK (status IN ('draft','todo','progress','review','deploy','done')),
                title       TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                created_at  TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
            );
            INSERT INTO tasks_new SELECT * FROM tasks;
            DROP TABLE tasks;
            ALTER TABLE tasks_new RENAME TO tasks;
            CREATE INDEX idx_tasks_user_status ON tasks(user_id, status);
            COMMIT;
            PRAGMA foreign_keys = ON;
            "#,
        )?;
    }
    Ok(())
}
