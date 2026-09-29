//! Cycle de vie des tâches, commun à l'UI (humains) et à l'API (agents) :
//! changements d'état (avec les restrictions des agents), prise en charge
//! d'une tâche par un agent (bail renouvelable), journal de la tâche.

use crate::auth::CurrentUser;
use crate::notify;
use axum::http::StatusCode;
use rusqlite::{Connection, OptionalExtension};
use tikedenn_common::{agent_may_move, api::LogEntry, is_valid_status};

/// Durée par défaut d'une prise en charge (renouvelée par le worker).
pub const DEFAULT_CLAIM_MINUTES: i64 = 60;
const MAX_CLAIM_MINUTES: i64 = 24 * 60;

/// Auteur d'une action : un humain (UI, session) ou un agent (API, token).
#[derive(Clone)]
pub enum Actor {
    Human(CurrentUser),
    Agent {
        user: CurrentUser,
        /// Nom du token (identité de l'agent).
        token: String,
    },
}

impl Actor {
    pub fn user(&self) -> &CurrentUser {
        match self {
            Actor::Human(u) | Actor::Agent { user: u, .. } => u,
        }
    }

    /// Nom de l'agent ('' pour un humain).
    pub fn agent_name(&self) -> &str {
        match self {
            Actor::Human(_) => "",
            Actor::Agent { token, .. } => token,
        }
    }

    pub fn is_agent(&self) -> bool {
        matches!(self, Actor::Agent { .. })
    }

    /// Identité de prise en charge : « utilisateur » ou « utilisateur/agent ».
    pub fn claimer(&self) -> String {
        match self {
            Actor::Human(u) => u.username.clone(),
            Actor::Agent { user, token } => format!("{}/{token}", user.username),
        }
    }
}

/// Refus d'une action du workflow (message destiné à l'utilisateur / l'agent).
#[derive(Debug)]
pub enum WfError {
    NotFound,
    Forbidden(String),
    Conflict(String),
    Invalid(String),
    Db(rusqlite::Error),
}

impl WfError {
    pub fn status(&self) -> StatusCode {
        match self {
            WfError::NotFound => StatusCode::NOT_FOUND,
            WfError::Forbidden(_) => StatusCode::FORBIDDEN,
            WfError::Conflict(_) => StatusCode::CONFLICT,
            WfError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            WfError::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn message(&self) -> String {
        match self {
            WfError::NotFound => "Task not found.".into(),
            WfError::Forbidden(m) | WfError::Conflict(m) | WfError::Invalid(m) => m.clone(),
            WfError::Db(_) => "Internal error.".into(),
        }
    }
}

impl From<rusqlite::Error> for WfError {
    fn from(e: rusqlite::Error) -> Self {
        WfError::Db(e)
    }
}

pub type WfResult<T = ()> = Result<T, WfError>;

/// État courant d'une tâche, pour décider d'une transition.
struct TaskState {
    status: String,
    title: String,
    claimed_by: String,
    claim_active: bool,
    blocked_by: Vec<i64>,
}

/// Condition SQL « prise en charge active » (bail non expiré) sur l'alias `t`.
pub const CLAIM_ACTIVE_SQL: &str =
    "(t.claimed_by != '' AND t.claimed_until IS NOT NULL AND t.claimed_until >= datetime('now'))";

/// Dépendances non terminées d'une tâche.
pub fn blocked_by(conn: &Connection, task_id: i64) -> rusqlite::Result<Vec<i64>> {
    conn.prepare(
        "SELECT d.depends_on FROM task_deps d JOIN tasks o ON o.id = d.depends_on
         WHERE d.task_id = ?1 AND o.status != 'done' ORDER BY d.depends_on",
    )?
    .query_map([task_id], |r| r.get(0))?
    .collect()
}

fn task_state(conn: &Connection, id: i64) -> WfResult<TaskState> {
    let sql = format!("SELECT t.status, t.title, t.claimed_by, {CLAIM_ACTIVE_SQL} FROM tasks t WHERE t.id = ?1");
    let (status, title, claimed_by, claim_active) = conn
        .query_row(&sql, [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .optional()?
        .ok_or(WfError::NotFound)?;
    Ok(TaskState {
        status,
        title,
        claimed_by,
        claim_active,
        blocked_by: blocked_by(conn, id)?,
    })
}

fn blocked_message(ids: &[i64]) -> String {
    let list: Vec<String> = ids.iter().map(|i| format!("#{i}")).collect();
    format!("The task is blocked by {} (not done yet).", list.join(", "))
}

/// Ajoute une entrée au journal de la tâche.
pub fn log(conn: &Connection, task_id: i64, actor: &Actor, kind: &str, body: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO task_log (task_id, user_id, author, agent, kind, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![task_id, actor.user().id, actor.user().username, actor.agent_name(), kind, body],
    )?;
    Ok(())
}

/// Journal d'une tâche, du plus ancien au plus récent.
pub fn fetch_log(conn: &Connection, task_id: i64) -> rusqlite::Result<Vec<LogEntry>> {
    conn.prepare(
        "SELECT id, author, agent, kind, body, created_at FROM task_log WHERE task_id = ?1 ORDER BY id",
    )?
    .query_map([task_id], |r| {
        let agent: String = r.get(2)?;
        Ok(LogEntry {
            id: r.get(0)?,
            author: r.get(1)?,
            agent: (!agent.is_empty()).then_some(agent),
            kind: r.get(3)?,
            body: r.get(4)?,
            created_at: r.get(5)?,
        })
    })?
    .collect()
}

/// Change l'état d'une tâche. Un agent n'a droit qu'aux transitions de
/// `agent_may_move` et ne peut pas toucher une tâche prise en charge par un
/// autre ; un humain peut tout faire. Tout changement d'état libère la prise
/// en charge, sauf quand un agent passe lui-même la tâche en cours. `note`
/// (rapport, erreur…) est ajoutée au journal.
pub fn move_task(conn: &Connection, actor: &Actor, id: i64, to: &str, note: Option<&str>) -> WfResult {
    if !is_valid_status(to) {
        return Err(WfError::Invalid(format!("Unknown task state '{to}'.")));
    }
    let st = task_state(conn, id)?;
    let me = actor.claimer();
    if actor.is_agent() {
        if st.status != to && !agent_may_move(&st.status, to) {
            return Err(WfError::Forbidden(format!(
                "An agent cannot move a task from {} to {to}; this step needs a human.",
                st.status
            )));
        }
        if st.claim_active && st.claimed_by != me {
            return Err(WfError::Conflict(format!("The task is claimed by {}.", st.claimed_by)));
        }
        if to == "progress" && !st.blocked_by.is_empty() {
            return Err(WfError::Conflict(blocked_message(&st.blocked_by)));
        }
    }
    if st.status != to {
        let (claimed_by, ttl) = if actor.is_agent() && to == "progress" {
            (me.as_str(), Some(DEFAULT_CLAIM_MINUTES))
        } else {
            ("", None)
        };
        let changed = conn.execute(
            "UPDATE tasks SET status = ?1, claimed_by = ?2,
                    claimed_until = CASE WHEN ?3 IS NULL THEN NULL
                                         ELSE datetime('now', '+' || ?3 || ' minutes') END,
                    updated_at = datetime('now')
             WHERE id = ?4 AND status = ?5",
            rusqlite::params![to, claimed_by, ttl, id, st.status],
        )?;
        if changed == 0 {
            return Err(WfError::Conflict("The task changed meanwhile; retry.".into()));
        }
        log(conn, id, actor, "move", &format!("{} → {to}", st.status))?;
    }
    if let Some(note) = note.map(str::trim).filter(|n| !n.is_empty()) {
        log(conn, id, actor, if actor.is_agent() { "report" } else { "comment" }, note)?;
    }
    if st.status != to {
        notify_move(id, &st.title, &st.status, to, actor, note);
    }
    Ok(())
}

/// Notifie l'humain des étapes qui l'attendent : revue, échec de déploiement.
fn notify_move(id: i64, title: &str, from: &str, to: &str, actor: &Actor, note: Option<&str>) {
    let path = format!("/tasks/{id}/edit");
    if to == "review" && from == "deploy" {
        notify::send(&format!("Deploy failed: #{id} {title}"), note.unwrap_or("See the task log."), &path);
    } else if to == "review" {
        let who = actor.claimer();
        notify::send(
            &format!("To review: #{id} {title}"),
            &format!("{who}: {}", note.unwrap_or("ready for review")),
            &format!("/tasks/{id}/diff"),
        );
    } else if to == "done" && actor.is_agent() {
        notify::send(&format!("Deployed: #{id} {title}"), "The task is done.", &path);
    }
}

fn clamp_ttl(ttl: Option<i64>) -> i64 {
    ttl.unwrap_or(DEFAULT_CLAIM_MINUTES).clamp(1, MAX_CLAIM_MINUTES)
}

/// Prend en charge une tâche (ou renouvelle le bail si on la détient déjà).
/// Une tâche `todo` passe en `progress` ; `progress` (bail expiré d'un agent
/// disparu) et `deploy` restent dans leur état. Renvoie vrai si la tâche
/// vient d'être prise (faux pour un simple renouvellement).
pub fn claim(conn: &Connection, actor: &Actor, id: i64, ttl: Option<i64>) -> WfResult<bool> {
    let st = task_state(conn, id)?;
    if !matches!(st.status.as_str(), "todo" | "progress" | "deploy") {
        return Err(WfError::Conflict(format!(
            "Only tasks in todo, progress or deploy can be claimed (this one is in {}).",
            st.status
        )));
    }
    let me = actor.claimer();
    if st.claim_active && st.claimed_by != me {
        return Err(WfError::Conflict(format!("The task is claimed by {}.", st.claimed_by)));
    }
    if st.status == "todo" && !st.blocked_by.is_empty() {
        return Err(WfError::Conflict(blocked_message(&st.blocked_by)));
    }
    let ttl = clamp_ttl(ttl);
    let to = if st.status == "todo" { "progress" } else { st.status.as_str() };
    // Condition répétée dans l'UPDATE : deux agents concurrents ne peuvent
    // pas prendre la même tâche.
    let changed = conn.execute(
        "UPDATE tasks SET status = ?1, claimed_by = ?2,
                claimed_until = datetime('now', '+' || ?3 || ' minutes'),
                updated_at = CASE WHEN status = ?1 THEN updated_at ELSE datetime('now') END
         WHERE id = ?4 AND status = ?5
           AND (claimed_by = '' OR claimed_by = ?2 OR claimed_until IS NULL
                OR claimed_until < datetime('now'))",
        rusqlite::params![to, me, ttl, id, st.status],
    )?;
    if changed == 0 {
        return Err(WfError::Conflict("The task was claimed meanwhile.".into()));
    }
    let newly = !(st.claim_active && st.claimed_by == me);
    if newly {
        log(conn, id, actor, "claim", &format!("Claimed by {me} ({ttl} min lease)"))?;
    }
    if to != st.status {
        log(conn, id, actor, "move", &format!("{} → {to}", st.status))?;
    }
    Ok(newly)
}

/// Libère une tâche : par son détenteur, ou par un humain. Avec `to`, la
/// tâche change aussi d'état (ex. rendue en `todo`).
pub fn release(conn: &Connection, actor: &Actor, id: i64, to: Option<&str>, note: Option<&str>) -> WfResult {
    let st = task_state(conn, id)?;
    if actor.is_agent() && st.claim_active && st.claimed_by != actor.claimer() {
        return Err(WfError::Conflict(format!("The task is claimed by {}.", st.claimed_by)));
    }
    if let Some(to) = to.filter(|t| *t != st.status) {
        return move_task(conn, actor, id, to, note);
    }
    conn.execute(
        "UPDATE tasks SET claimed_by = '', claimed_until = NULL WHERE id = ?1",
        [id],
    )?;
    if !st.claimed_by.is_empty() {
        log(conn, id, actor, "claim", &format!("Released (was claimed by {})", st.claimed_by))?;
    }
    if let Some(note) = note.map(str::trim).filter(|n| !n.is_empty()) {
        log(conn, id, actor, if actor.is_agent() { "report" } else { "comment" }, note)?;
    }
    Ok(())
}

/// Prend en charge la prochaine tâche disponible : la plus prioritaire puis
/// la plus ancienne, non bloquée, sans prise en charge active. `status` :
/// « todo » (y compris les tâches en cours abandonnées par un agent dont le
/// bail a expiré) ou « deploy ». `projects` vide = tous les projets.
pub fn claim_next(
    conn: &Connection,
    actor: &Actor,
    projects: &[i64],
    status: &str,
    ttl: Option<i64>,
) -> WfResult<Option<i64>> {
    let status_cond = match status {
        "todo" => "(t.status = 'todo' OR (t.status = 'progress' AND t.claimed_by != ''))",
        "deploy" => "t.status = 'deploy'",
        _ => return Err(WfError::Invalid("Only todo or deploy tasks can be claimed.".into())),
    };
    let project_cond = if projects.is_empty() {
        String::new()
    } else {
        let ids: Vec<String> = projects.iter().map(i64::to_string).collect();
        format!("AND t.project_id IN ({})", ids.join(","))
    };
    let sql = format!(
        "SELECT t.id FROM tasks t
         WHERE {status_cond} {project_cond} AND NOT {CLAIM_ACTIVE_SQL}
           AND NOT EXISTS (SELECT 1 FROM task_deps d JOIN tasks o ON o.id = d.depends_on
                           WHERE d.task_id = t.id AND o.status != 'done')
         ORDER BY t.priority DESC, t.id ASC LIMIT 1"
    );
    // Quelques essais : un autre agent peut prendre le candidat entre-temps.
    for _ in 0..5 {
        let Some(id) = conn.query_row(&sql, [], |r| r.get::<_, i64>(0)).optional()? else {
            return Ok(None);
        };
        match claim(conn, actor, id, ttl) {
            Ok(_) => return Ok(Some(id)),
            Err(WfError::Conflict(_)) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_app_schema;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        init_app_schema(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO users (username, email, pass_hash) VALUES ('olivier', 'o@x', 'h');
             INSERT INTO tasks (user_id, title, status, priority) VALUES
                (1, 'low', 'todo', 0), (1, 'high', 'todo', 2), (1, 'draft', 'draft', 3);",
        )
        .unwrap();
        conn
    }

    fn user() -> CurrentUser {
        CurrentUser { id: 1, username: "olivier".into(), is_admin: false, to_review: 0 }
    }

    fn agent(name: &str) -> Actor {
        Actor::Agent { user: user(), token: name.into() }
    }

    fn status(conn: &Connection, id: i64) -> String {
        conn.query_row("SELECT status FROM tasks WHERE id = ?1", [id], |r| r.get(0)).unwrap()
    }

    #[test]
    fn claim_next_takes_priority_order_and_is_exclusive() {
        let conn = setup();
        let a = agent("claude");
        let b = agent("pi");
        assert_eq!(claim_next(&conn, &a, &[], "todo", None).unwrap(), Some(2));
        assert_eq!(status(&conn, 2), "progress");
        assert_eq!(claim_next(&conn, &b, &[], "todo", None).unwrap(), Some(1));
        assert_eq!(claim_next(&conn, &b, &[], "todo", None).unwrap(), None);
        // Le bail d'un autre agent est respecté, le sien se renouvelle.
        assert!(matches!(claim(&conn, &b, 2, None), Err(WfError::Conflict(_))));
        assert!(!claim(&conn, &a, 2, Some(30)).unwrap());
        // Bail expiré : la tâche abandonnée redevient disponible.
        conn.execute("UPDATE tasks SET claimed_until = datetime('now', '-1 minute') WHERE id = 2", [])
            .unwrap();
        assert_eq!(claim_next(&conn, &b, &[], "todo", None).unwrap(), Some(2));
    }

    #[test]
    fn agents_cannot_take_human_decisions() {
        let conn = setup();
        let a = agent("claude");
        assert!(matches!(move_task(&conn, &a, 3, "todo", None), Err(WfError::Forbidden(_))));
        claim(&conn, &a, 2, None).unwrap();
        move_task(&conn, &a, 2, "review", Some("Done, tests pass.")).unwrap();
        assert_eq!(status(&conn, 2), "review");
        assert!(matches!(move_task(&conn, &a, 2, "deploy", None), Err(WfError::Forbidden(_))));
        // L'humain approuve ; la prise en charge a été libérée au passage en revue.
        move_task(&conn, &Actor::Human(user()), 2, "deploy", None).unwrap();
        let claimed: String = conn.query_row("SELECT claimed_by FROM tasks WHERE id = 2", [], |r| r.get(0)).unwrap();
        assert_eq!(claimed, "");
        let kinds: Vec<String> = fetch_log(&conn, 2).unwrap().into_iter().map(|l| l.kind).collect();
        assert_eq!(kinds, ["claim", "move", "move", "report", "move"]);
    }

    #[test]
    fn blocked_tasks_are_skipped() {
        let conn = setup();
        conn.execute("INSERT INTO task_deps (task_id, depends_on) VALUES (2, 1)", []).unwrap();
        let a = agent("claude");
        assert!(matches!(claim(&conn, &a, 2, None), Err(WfError::Conflict(_))));
        assert_eq!(claim_next(&conn, &a, &[], "todo", None).unwrap(), Some(1));
        conn.execute("UPDATE tasks SET status = 'done' WHERE id = 1", []).unwrap();
        assert_eq!(claim_next(&conn, &a, &[], "todo", None).unwrap(), Some(2));
    }
}
