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

    // Migrations versionnées (PRAGMA user_version) : chaque étape ne
    // s'exécute qu'une fois. Ajouter les suivantes en `if version < N`.
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        migrate_legacy(conn)?;
        conn.execute_batch("PRAGMA user_version = 1;")?;
    }
    if version < 2 {
        // Les cascades n'étaient pas appliquées (foreign_keys n'était actif
        // que sur une connexion du pool) : purge des lignes orphelines.
        conn.execute_batch(
            r#"
            BEGIN;
            DELETE FROM tasks WHERE user_id NOT IN (SELECT id FROM users);
            UPDATE tasks SET project_id = NULL
                WHERE project_id IS NOT NULL AND project_id NOT IN (SELECT id FROM projects);
            DELETE FROM files WHERE task_id NOT IN (SELECT id FROM tasks);
            DELETE FROM project_tokens
                WHERE project_id NOT IN (SELECT id FROM projects)
                   OR user_id NOT IN (SELECT id FROM users);
            PRAGMA user_version = 2;
            COMMIT;
            "#,
        )?;
    }
    if version < 3 {
        migrate_v3(conn)?;
    }
    if version < 4 {
        // Agent du worker par projet ('' = défaut, claude, pi, none = humains seulement).
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE projects ADD COLUMN agent TEXT NOT NULL DEFAULT '';
             PRAGMA user_version = 4;
             COMMIT;",
        )?;
    }
    Ok(())
}

/// v3 — tableau partagé et travail des agents :
/// - `tasks.user_id` devient le créateur (nullable, SET NULL : supprimer un
///   compte ne supprime plus les tâches du tableau commun) ; nouvelles
///   colonnes critères d'acceptation, priorité et prise en charge (bail) ;
/// - `projects` : branche de base et nom de publication (`publish <nom>`) ;
/// - dépendances entre tâches, journal des tâches, tokens d'API.
/// Le schéma de base (CREATE TABLE IF NOT EXISTS plus haut) reste celui de la
/// v2 : les évolutions ultérieures passent toutes par les migrations.
fn migrate_v3(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA foreign_keys = OFF;
        BEGIN;
        CREATE TABLE tasks_new (
            id            INTEGER PRIMARY KEY,
            user_id       INTEGER REFERENCES users(id) ON DELETE SET NULL,
            project_id    INTEGER REFERENCES projects(id) ON DELETE SET NULL,
            status        TEXT NOT NULL DEFAULT 'draft'
                          CHECK (status IN ('draft','todo','progress','review','deploy','done')),
            priority      INTEGER NOT NULL DEFAULT 1 CHECK (priority BETWEEN 0 AND 3),
            branch        TEXT NOT NULL DEFAULT '',
            title         TEXT NOT NULL,
            description   TEXT NOT NULL DEFAULT '',
            acceptance    TEXT NOT NULL DEFAULT '',
            claimed_by    TEXT NOT NULL DEFAULT '',
            claimed_until TEXT,
            created_at    TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
        );
        INSERT INTO tasks_new (id, user_id, project_id, status, branch, title, description, created_at, updated_at)
            SELECT id, user_id, project_id, status, branch, title, description, created_at, updated_at FROM tasks;
        DROP TABLE tasks;
        ALTER TABLE tasks_new RENAME TO tasks;
        CREATE INDEX idx_tasks_status ON tasks(status, priority);

        ALTER TABLE projects ADD COLUMN default_branch TEXT NOT NULL DEFAULT '';
        ALTER TABLE projects ADD COLUMN publish_name TEXT NOT NULL DEFAULT '';

        -- task_id est bloquée tant que depends_on n'est pas terminée.
        CREATE TABLE task_deps (
            task_id    INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            depends_on INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            PRIMARY KEY (task_id, depends_on),
            CHECK (task_id != depends_on)
        );

        -- Journal d'une tâche : commentaires, rapports d'agents, revues,
        -- changements d'état, prises en charge, logs de déploiement.
        -- `agent` : nom du token si l'entrée vient de l'API ('' = humain via l'UI).
        CREATE TABLE task_log (
            id         INTEGER PRIMARY KEY,
            task_id    INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            user_id    INTEGER REFERENCES users(id) ON DELETE SET NULL,
            author     TEXT NOT NULL,
            agent      TEXT NOT NULL DEFAULT '',
            kind       TEXT NOT NULL
                       CHECK (kind IN ('comment','report','review','move','claim','deploy')),
            body       TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX idx_task_log_task ON task_log(task_id, id);

        -- Tokens d'API des agents : seul le SHA-256 du token est stocké.
        CREATE TABLE api_tokens (
            id           INTEGER PRIMARY KEY,
            user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            name         TEXT NOT NULL,
            token_hash   TEXT NOT NULL UNIQUE,
            scope        TEXT NOT NULL DEFAULT 'write' CHECK (scope IN ('read','write')),
            created_at   TEXT NOT NULL DEFAULT (datetime('now')),
            last_used_at TEXT,
            UNIQUE (user_id, name)
        );
        PRAGMA user_version = 3;
        COMMIT;
        PRAGMA foreign_keys = ON;
        "#,
    )?;
    Ok(())
}

/// Migrations historiques (schémas antérieurs à `user_version`), détectées
/// d'après le schéma existant. Sans effet sur une base neuve ou à jour.
fn migrate_legacy(conn: &Connection) -> anyhow::Result<()> {
    // Migration légère : ajouter is_admin aux schémas locaux antérieurs.
    if !has_column(conn, "users", "is_admin")? {
        conn.execute("ALTER TABLE users ADD COLUMN is_admin INTEGER NOT NULL DEFAULT 0", [])?;
    }

    // Migration : schémas antérieurs ont une CHECK restreinte à
    // draft/todo/progress/done (pas de 'review' ni 'deploy'). Le CREATE TABLE
    // IF NOT EXISTS ne peut pas la mettre à jour : on reconstruit la table
    // (copie des données) si la contrainte est ancienne.
    if !table_sql(conn, "tasks")?.contains("'review'") {
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
    if !has_column(conn, "tasks", "branch")? {
        conn.execute("ALTER TABLE tasks ADD COLUMN branch TEXT NOT NULL DEFAULT ''", [])?;
    }

    // Migration : schémas (par utilisateur, avec `user_id`) antérieurs à la
    // colonne `projects.git_token`, que la migration suivante recopie.
    // Uniquement pour ces schémas : le schéma actuel n'a plus `git_token`.
    let old_projects = has_column(conn, "projects", "user_id")?;
    if old_projects && !has_column(conn, "projects", "git_token")? {
        conn.execute(
            "ALTER TABLE projects ADD COLUMN git_token TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }

    // Migration : les schémas antérieurs donnaient aux projets un `user_id`
    // (un projet par utilisateur). Les projets sont maintenant partagés par
    // tous : on reconstruit la table sans `user_id`. En cas de doublons de
    // nom entre utilisateurs, on garde le projet de plus petit id et on
    // redirige vers lui les tâches qui référençaient le doublon éliminé.
    if old_projects {
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
    if has_column(conn, "projects", "git_token")? {
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

/// La table `table` a-t-elle la colonne `col` ?
fn has_column(conn: &Connection, table: &str, col: &str) -> rusqlite::Result<bool> {
    let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = st.query_map([], |r| r.get::<_, String>(1))?;
    for n in names {
        if n? == col {
            return Ok(true);
        }
    }
    Ok(false)
}

/// DDL (`CREATE TABLE …`) de la table `table`, vide si elle n'existe pas.
fn table_sql(conn: &Connection, table: &str) -> rusqlite::Result<String> {
    match conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get(0),
    ) {
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(String::new()),
        r => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn fresh_schema_is_stable_across_restarts() {
        let conn = Connection::open_in_memory().unwrap();
        init_app_schema(&conn).unwrap();
        let ddl = table_sql(&conn, "projects").unwrap();
        init_app_schema(&conn).unwrap();
        assert_eq!(version(&conn), 4);
        assert_eq!(table_sql(&conn, "projects").unwrap(), ddl);
        assert!(!has_column(&conn, "projects", "git_token").unwrap());
    }

    #[test]
    fn legacy_per_user_projects_are_merged() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, username TEXT NOT NULL UNIQUE,
                                 email TEXT NOT NULL UNIQUE, pass_hash TEXT NOT NULL,
                                 created_at TEXT NOT NULL DEFAULT (datetime('now')));
             CREATE TABLE projects (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL,
                                    name TEXT NOT NULL, git_repo TEXT NOT NULL DEFAULT '',
                                    created_at TEXT NOT NULL DEFAULT (datetime('now')));
             CREATE TABLE tasks (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL,
                                 project_id INTEGER, status TEXT NOT NULL DEFAULT 'draft'
                                     CHECK (status IN ('draft','todo','progress','done')),
                                 title TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
                                 created_at TEXT NOT NULL DEFAULT (datetime('now')),
                                 updated_at TEXT NOT NULL DEFAULT (datetime('now')));
             INSERT INTO users (username, email, pass_hash) VALUES ('a', 'a@x', 'h'), ('b', 'b@x', 'h');
             INSERT INTO projects (user_id, name) VALUES (1, 'P'), (2, 'P'), (2, 'Q');
             INSERT INTO tasks (user_id, project_id, title) VALUES (1, 1, 't1'), (2, 2, 't2'), (2, 3, 't3'), (9, 1, 'orphan');",
        )
        .unwrap();
        init_app_schema(&conn).unwrap();
        assert_eq!(version(&conn), 4);
        assert!(has_column(&conn, "users", "is_admin").unwrap());
        assert!(has_column(&conn, "tasks", "branch").unwrap());
        assert!(table_sql(&conn, "tasks").unwrap().contains("'review'"));
        let projects: Vec<(i64, String)> = conn
            .prepare("SELECT id, name FROM projects ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(projects, vec![(1, "P".into()), (3, "Q".into())]);
        let tasks: Vec<(i64, Option<i64>)> = conn
            .prepare("SELECT id, project_id FROM tasks ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // Doublon « P » redirigé vers le projet 1 ; tâche orpheline purgée.
        assert_eq!(tasks, vec![(1, Some(1)), (2, Some(1)), (3, Some(3))]);
    }

    #[test]
    fn v3_shared_board_keeps_tasks_of_deleted_users() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        init_app_schema(&conn).unwrap();
        assert!(has_column(&conn, "tasks", "claimed_by").unwrap());
        assert!(has_column(&conn, "projects", "publish_name").unwrap());
        conn.execute_batch(
            "INSERT INTO users (username, email, pass_hash) VALUES ('a', 'a@x', 'h');
             INSERT INTO tasks (user_id, title) VALUES (1, 't1'), (1, 't2');
             INSERT INTO task_deps (task_id, depends_on) VALUES (2, 1);
             INSERT INTO task_log (task_id, user_id, author, kind) VALUES (1, 1, 'a', 'comment');
             DELETE FROM users;",
        )
        .unwrap();
        let (tasks, orphans): (i64, i64) = conn
            .query_row("SELECT COUNT(*), COUNT(*) FILTER (WHERE user_id IS NULL) FROM tasks", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((tasks, orphans), (2, 2));
        conn.execute("DELETE FROM tasks WHERE id = 1", []).unwrap();
        let deps: i64 = conn.query_row("SELECT COUNT(*) FROM task_deps", [], |r| r.get(0)).unwrap();
        assert_eq!(deps, 0);
    }
}
