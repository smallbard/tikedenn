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
        -- Partagé entre TOUS les utilisateurs (pas de user_id) ; le nom est
        -- unique globalement. Le token git (dépôt privé en https, pour le
        -- clone lors du diff branche) est stocké dans `project_tokens`,
        -- lié au couple (projet, utilisateur) : chacun son propre token.
        CREATE TABLE IF NOT EXISTS projects (
            id         INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            git_repo   TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE (name)
        );

        -- Token git par couple (projet, utilisateur) — voir commentaire
        -- de la table `projects`.
        CREATE TABLE IF NOT EXISTS project_tokens (
            project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            token      TEXT NOT NULL,
            PRIMARY KEY (project_id, user_id)
        );

        -- Tâches du tableau kanban : draft → todo → progress → review → deploy → done.
        -- `branch` : branche git optionnelle (différentielle vs main, page /tasks/{id}/diff).
        CREATE TABLE IF NOT EXISTS tasks (
            id          INTEGER PRIMARY KEY,
            user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            project_id  INTEGER REFERENCES projects(id) ON DELETE SET NULL,
            status      TEXT NOT NULL DEFAULT 'draft'
                        CHECK (status IN ('draft','todo','progress','review','deploy','done')),
            branch      TEXT NOT NULL DEFAULT '',
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
                branch      TEXT NOT NULL DEFAULT '',
                title       TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                created_at  TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
            );
            -- L'ancienne table peut ne pas avoir `branch` : liste explicite.
            INSERT INTO tasks_new (id, user_id, project_id, status, title, description, created_at, updated_at)
            SELECT id, user_id, project_id, status, title, description, created_at, updated_at FROM tasks;
            DROP TABLE tasks;
            ALTER TABLE tasks_new RENAME TO tasks;
            CREATE INDEX idx_tasks_user_status ON tasks(user_id, status);
            COMMIT;
            PRAGMA foreign_keys = ON;
            "#,
        )?;
    }

    // Migration : schémas antérieurs à la colonne `branch` (branche git
    // optionnelle liée à la tâche).
    let has_branch = conn
        .prepare("PRAGMA table_info(tasks)")
        .and_then(|mut st| {
            st.query_map([], |r| r.get::<_, String>(1))
                .map(|it| it.flatten().any(|n| n == "branch"))
        })
        .unwrap_or(true);
    if !has_branch {
        let _ = conn.execute("ALTER TABLE tasks ADD COLUMN branch TEXT NOT NULL DEFAULT ''", []);
    }

    // Migration : schémas antérieurs à la colonne `projects.git_token`
    // (token des dépôts privés en https).
    let has_token = conn
        .prepare("PRAGMA table_info(projects)")
        .and_then(|mut st| {
            st.query_map([], |r| r.get::<_, String>(1))
                .map(|it| it.flatten().any(|n| n == "git_token"))
        })
        .unwrap_or(true);
    if !has_token {
        let _ = conn.execute(
            "ALTER TABLE projects ADD COLUMN git_token TEXT NOT NULL DEFAULT ''",
            [],
        );
    }

    // Migration : les schémas antérieurs donnaient aux projets un `user_id`
    // (un projet par utilisateur). Les projets sont maintenant partagés par
    // tous : on reconstruit la table sans `user_id`. En cas de doublons de
    // nom entre utilisateurs, on garde le projet de plus petit id et on
    // redirige vers lui les tâches qui référençaient le doublon éliminé.
    let projects_sql = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'projects'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_default();
    if projects_sql.contains("user_id") {
        conn.execute_batch(
            r#"
            PRAGMA foreign_keys = OFF;
            BEGIN;
            CREATE TABLE projects_new (
                id         INTEGER PRIMARY KEY,
                name       TEXT NOT NULL,
                git_repo   TEXT NOT NULL DEFAULT '',
                git_token  TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE (name)
            );
            -- Tâches qui référençaient un doublon : redirection vers le survivant.
            UPDATE tasks SET project_id = (
                SELECT MIN(p.id) FROM projects p WHERE p.name = (
                    SELECT p2.name FROM projects p2 WHERE p2.id = tasks.project_id
                )
            );
            INSERT INTO projects_new (id, name, git_repo, git_token, created_at)
            SELECT p.id, p.name, p.git_repo, p.git_token, p.created_at
            FROM projects p
            WHERE p.id = (SELECT MIN(p2.id) FROM projects p2 WHERE p2.name = p.name);
            DROP TABLE projects;
            ALTER TABLE projects_new RENAME TO projects;
            COMMIT;
            PRAGMA foreign_keys = ON;
            "#,
        )?;
    }

    // Migration : les schémas antérieurs à `project_tokens` stockaient un
    // `git_token` dans la table projects (un token par projet). Les tokens
    // sont maintenant liés au couple (projet, utilisateur) : les tokens
    // existants non vides sont associés à l'unique (plus ancien) utilisateur
    // de la BDD, puis la colonne est retirée de `projects`.
    let projects_sql2 = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'projects'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_default();
    if projects_sql2.contains("git_token") {
        let users_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .unwrap_or(0);
        let move_tokens = if users_count > 0 {
            "INSERT OR IGNORE INTO project_tokens (project_id, user_id, token)
             SELECT p.id, (SELECT u.id FROM users u ORDER BY u.id LIMIT 1), p.git_token
             FROM projects p WHERE p.git_token != '';"
        } else {
            ""
        };
        conn.execute_batch(&format!(
            r#"
            PRAGMA foreign_keys = OFF;
            BEGIN;
            CREATE TABLE IF NOT EXISTS project_tokens (
                project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                token      TEXT NOT NULL,
                PRIMARY KEY (project_id, user_id)
            );
            {move_tokens}
            CREATE TABLE projects_new (
                id         INTEGER PRIMARY KEY,
                name       TEXT NOT NULL,
                git_repo   TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE (name)
            );
            INSERT INTO projects_new (id, name, git_repo, created_at)
            SELECT id, name, git_repo, created_at FROM projects;
            DROP TABLE projects;
            ALTER TABLE projects_new RENAME TO projects;
            COMMIT;
            PRAGMA foreign_keys = ON;
            "#,
        ))?;
    }
    Ok(())
}
