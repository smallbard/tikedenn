//! Tableau kanban, partagé par tous les utilisateurs : tâches dans les états
//! draft / todo / progress / review / deploy / done, filtrables par projet,
//! avec description longue, critères d'acceptation, priorité, dépendances,
//! branche git optionnelle, fichiers joints (voir `files`) et journal (voir
//! `workflow`). Les changements d'état passent par `workflow::move_task`.

use crate::auth::CurrentUser;
use crate::projects::{fetch_projects, ProjectRow};
use crate::web::{redirect_with_error, render, AppError, AppResult};
use crate::workflow::{self, Actor, WfError, CLAIM_ACTIVE_SQL};
use crate::AppState;
use askama::Template;
use axum::{
    extract::{Extension, Path, Query, State},
    response::{IntoResponse, Redirect, Response},
    Form,
};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use tikedenn_common::api::{LogEntry, TaskDetail, TaskSummary};
use tikedenn_common::{is_valid_status, priority_label, DEFAULT_PRIORITY, PRIORITIES};

/// Un état du tableau (valeur en BDD + libellé affiché).
pub struct Status {
    pub value: &'static str,
    pub label: &'static str,
}

/// États du tableau, dans l'ordre du gauche vers la droite.
pub const STATUSES: [Status; 6] = [
    Status { value: "draft", label: "Draft" },
    Status { value: "todo", label: "To do" },
    Status { value: "progress", label: "In progress" },
    Status { value: "review", label: "To review" },
    Status { value: "deploy", label: "To deploy" },
    Status { value: "done", label: "Done" },
];

pub struct FileRow {
    pub id: i64,
    pub name: String,
    pub size: i64,
    pub mime: String,
}

impl FileRow {
    pub fn size_kb(&self) -> i64 {
        self.size / 1024
    }
}

pub struct TaskRow {
    pub id: i64,
    pub project: Option<ProjectRow>,
    pub status: String,
    /// 0 = basse … 3 = urgente.
    pub priority: i64,
    /// Branche git optionnelle associée à la tâche (diff vs la branche de base).
    pub branch: String,
    pub title: String,
    pub description: String,
    /// Critères d'acceptation (ce que la revue vérifiera).
    pub acceptance: String,
    /// Première ligne de la description, pour l'affichage compact de la carte.
    pub description_preview: String,
    /// Détenteur de la prise en charge (« utilisateur/agent »), même expirée.
    pub claimed_by: String,
    pub claimed_until: Option<String>,
    pub claim_active: bool,
    /// Toutes les dépendances, et celles qui ne sont pas encore terminées.
    pub depends_on: Vec<i64>,
    pub blocked_by: Vec<i64>,
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Fichiers joints et journal (chargés seulement pour une tâche seule).
    pub files: Vec<FileRow>,
    pub log: Vec<LogEntry>,
}

impl TaskRow {
    /// Id du projet associé (0 si aucun) — pour la comparaison dans les templates.
    pub fn project_id_or_zero(&self) -> i64 {
        self.project.as_ref().map_or(0, |p| p.id)
    }

    /// La tâche a-t-elle la priorité d'index `i` (liste déroulante) ?
    pub fn is_priority(&self, i: &usize) -> bool {
        *i as i64 == self.priority
    }

    pub fn priority_label(&self) -> &'static str {
        priority_label(self.priority)
    }

    /// Dépendances au format du champ d'édition (« 12, 15 »).
    pub fn depends_on_text(&self) -> String {
        join_ids(&self.depends_on, ", ")
    }

    pub fn blocked_text(&self) -> String {
        join_ids(&self.blocked_by, " #")
    }

    /// Description et critères d'acceptation rendus en Markdown (HTML nettoyé).
    pub fn description_html(&self) -> String {
        crate::markdown::render(&self.description)
    }

    pub fn acceptance_html(&self) -> String {
        crate::markdown::render(&self.acceptance)
    }

    /// Dernier rapport d'agent (résumé soumis à la revue).
    pub fn last_report(&self) -> Option<&LogEntry> {
        self.log.iter().rev().find(|l| l.kind == "report")
    }

    pub fn to_summary(&self) -> TaskSummary {
        TaskSummary {
            id: self.id,
            title: self.title.clone(),
            status: self.status.clone(),
            priority: self.priority,
            project_id: self.project.as_ref().map(|p| p.id),
            project: self.project.as_ref().map(|p| p.name.clone()),
            branch: self.branch.clone(),
            claimed_by: (!self.claimed_by.is_empty()).then(|| self.claimed_by.clone()),
            claimed_until: self.claimed_until.clone(),
            claim_active: self.claim_active,
            blocked_by: self.blocked_by.clone(),
            updated_at: self.updated_at.clone(),
        }
    }

    pub fn to_detail(&self) -> TaskDetail {
        TaskDetail {
            summary: self.to_summary(),
            description: self.description.clone(),
            acceptance: self.acceptance.clone(),
            created_by: self.created_by.clone(),
            created_at: self.created_at.clone(),
            depends_on: self.depends_on.clone(),
            repo: self.project.as_ref().map(ProjectRow::to_api),
            files: self
                .files
                .iter()
                .map(|f| tikedenn_common::api::FileInfo {
                    id: f.id,
                    name: f.name.clone(),
                    size: f.size,
                    mime: f.mime.clone(),
                })
                .collect(),
            log: self.log.clone(),
        }
    }
}

fn join_ids(ids: &[i64], sep: &str) -> String {
    ids.iter().map(i64::to_string).collect::<Vec<_>>().join(sep)
}

/// Une colonne du tableau et ses tâches.
pub struct Column {
    pub status: &'static Status,
    pub tasks: Vec<TaskRow>,
}

/// Colonnes communes à `fetch_tasks` et `fetch_one_task` (lues par `task_from_row`).
/// ?1 = user_id : le token (col 6) vient de `project_tokens`, c'est le token
/// de l'utilisateur courant pour le projet de la tâche.
fn task_select() -> String {
    format!(
        "SELECT t.id, p.id, p.name, p.git_repo, p.default_branch, p.publish_name, COALESCE(pt.token, ''),
                t.status, t.priority, t.branch, t.title, t.description, t.acceptance,
                t.claimed_by, t.claimed_until, {CLAIM_ACTIVE_SQL},
                (SELECT group_concat(depends_on) FROM task_deps WHERE task_id = t.id),
                (SELECT group_concat(d.depends_on) FROM task_deps d JOIN tasks o ON o.id = d.depends_on
                 WHERE d.task_id = t.id AND o.status != 'done'),
                u.username, t.created_at, t.updated_at, p.agent
         FROM tasks t LEFT JOIN projects p ON p.id = t.project_id
         LEFT JOIN project_tokens pt ON pt.project_id = p.id AND pt.user_id = ?1
         LEFT JOIN users u ON u.id = t.user_id"
    )
}

/// Liste d'ids séparés par des virgules (group_concat), triée.
fn parse_id_list(s: Option<String>) -> Vec<i64> {
    let mut ids: Vec<i64> = s
        .unwrap_or_default()
        .split(',')
        .filter_map(|x| x.trim().parse().ok())
        .collect();
    ids.sort_unstable();
    ids
}

/// Construit une `TaskRow` (sans fichiers ni journal) depuis une ligne de `task_select`.
fn task_from_row(r: &rusqlite::Row) -> rusqlite::Result<TaskRow> {
    let project = match r.get::<_, Option<i64>>(1)? {
        Some(id) => Some(ProjectRow {
            id,
            name: r.get(2)?,
            git_repo: r.get(3)?,
            default_branch: r.get(4)?,
            publish_name: r.get(5)?,
            token: r.get(6)?,
            agent: r.get(21)?,
        }),
        None => None,
    };
    let description: String = r.get(11)?;
    Ok(TaskRow {
        id: r.get(0)?,
        project,
        status: r.get(7)?,
        priority: r.get(8)?,
        branch: r.get(9)?,
        title: r.get(10)?,
        description_preview: first_line_preview(&description),
        description,
        acceptance: r.get(12)?,
        claimed_by: r.get(13)?,
        claimed_until: r.get(14)?,
        claim_active: r.get(15)?,
        depends_on: parse_id_list(r.get(16)?),
        blocked_by: parse_id_list(r.get(17)?),
        created_by: r.get(18)?,
        created_at: r.get(19)?,
        updated_at: r.get(20)?,
        files: vec![],
        log: vec![],
    })
}

/// Première ligne non vide de la description, tronquée à 140 caractères.
fn first_line_preview(description: &str) -> String {
    let Some(line) = description.lines().map(str::trim).find(|l| !l.is_empty()) else {
        return String::new();
    };
    match line.char_indices().nth(140) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_string(),
    }
}

/// Filtre des listes de tâches (tableau, API).
#[derive(Default)]
pub struct TaskFilter<'a> {
    pub project_id: Option<i64>,
    pub status: Option<&'a str>,
}

/// Tâches du tableau (sans leurs fichiers), les plus prioritaires puis les
/// plus récentes d'abord. `user_id` ne sert qu'à charger le token git de
/// l'utilisateur courant.
pub fn fetch_tasks(conn: &Connection, user_id: i64, f: &TaskFilter) -> rusqlite::Result<Vec<TaskRow>> {
    let sql = format!(
        "{} WHERE (?2 IS NULL OR t.project_id = ?2) AND (?3 IS NULL OR t.status = ?3)
         ORDER BY t.priority DESC, t.id DESC",
        task_select()
    );
    conn.prepare(&sql)?
        .query_map(rusqlite::params![user_id, f.project_id, f.status], task_from_row)?
        .collect()
}

fn fetch_files(conn: &Connection, task_id: i64) -> rusqlite::Result<Vec<FileRow>> {
    conn.prepare("SELECT id, name, size, mime FROM files WHERE task_id = ?1 ORDER BY id")?
        .query_map([task_id], |r| {
            Ok(FileRow {
                id: r.get(0)?,
                name: r.get(1)?,
                size: r.get(2)?,
                mime: r.get(3)?,
            })
        })?
        .collect()
}

/// Une tâche avec ses fichiers et son journal ; `None` si elle n'existe pas.
pub fn find_task(conn: &Connection, user_id: i64, id: i64) -> rusqlite::Result<Option<TaskRow>> {
    let sql = format!("{} WHERE t.id = ?2", task_select());
    let Some(mut task) = conn
        .query_row(&sql, rusqlite::params![user_id, id], task_from_row)
        .optional()?
    else {
        return Ok(None);
    };
    task.files = fetch_files(conn, task.id)?;
    task.log = workflow::fetch_log(conn, task.id)?;
    Ok(Some(task))
}

/// Comme `find_task`, avec un 404 si la tâche n'existe pas.
pub fn fetch_one_task(conn: &Connection, user_id: i64, id: i64) -> AppResult<TaskRow> {
    find_task(conn, user_id, id)?.ok_or(AppError::not_found("Task not found."))
}

/// Projet à enregistrer pour l'id reçu du formulaire : `Some(None)` pour
/// « aucun projet » (id <= 0), `Some(Some(id))` s'il existe, `None` s'il est inconnu.
fn resolve_project(conn: &Connection, id: i64) -> rusqlite::Result<Option<Option<i64>>> {
    if id <= 0 {
        return Ok(Some(None));
    }
    let found = conn
        .query_row("SELECT id FROM projects WHERE id = ?1", [id], |r| r.get(0))
        .optional()?;
    Ok(found.map(Some))
}

/// Nom de branche git raisonnablement sûr (ne sert qu'aux refs git2, mais on
/// évite les noms pathologiques : ref parents, slash ou tiret initial, etc.).
pub fn is_safe_branch(b: &str) -> bool {
    let b = b.trim();
    !b.is_empty()
        && b.len() <= 200
        && !b.starts_with(['-', '.', '/'])
        && !b.ends_with('/')
        && !b.contains("..")
        && b.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '_' | '-'))
}

/// Ids de dépendances saisis librement (« 12, #15 16 »).
fn parse_deps(s: &str) -> Vec<i64> {
    let mut ids: Vec<i64> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|x| x.parse().ok())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Vérifie les dépendances de la tâche `task_id` (0 = tâche à créer) :
/// tâches existantes, pas elle-même, pas de cycle.
fn check_deps(conn: &Connection, task_id: i64, deps: &[i64]) -> rusqlite::Result<Result<(), String>> {
    for &d in deps {
        if d == task_id {
            return Ok(Err("A task cannot depend on itself.".into()));
        }
        let exists = conn
            .query_row("SELECT 1 FROM tasks WHERE id = ?1", [d], |_| Ok(()))
            .optional()?
            .is_some();
        if !exists {
            return Ok(Err(format!("Dependency #{d} does not exist.")));
        }
        // Cycle : la tâche est-elle atteignable depuis la dépendance ?
        let cycle = task_id != 0
            && conn
                .query_row(
                    "WITH RECURSIVE r(id) AS (SELECT ?1 UNION SELECT d.depends_on FROM task_deps d JOIN r ON d.task_id = r.id)
                     SELECT 1 FROM r WHERE id = ?2",
                    [d, task_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
        if cycle {
            return Ok(Err(format!("Dependency #{d} would create a cycle.")));
        }
    }
    Ok(Ok(()))
}

/// Remplace les dépendances d'une tâche.
pub fn save_deps(conn: &Connection, task_id: i64, deps: &[i64]) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM task_deps WHERE task_id = ?1", [task_id])?;
    for d in deps {
        conn.execute(
            "INSERT INTO task_deps (task_id, depends_on) VALUES (?1, ?2)",
            [task_id, *d],
        )?;
    }
    Ok(())
}

#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct DashboardTemplate {
    pub user: Option<CurrentUser>,
    pub projects: Vec<ProjectRow>,
    /// Id du projet de filtre sélectionné (0 = tous).
    pub selected_project: i64,
    pub columns: Vec<Column>,
}

/// Le tableau seul (rafraîchissement périodique htmx).
#[derive(Template)]
#[template(path = "board_frag.html")]
pub struct BoardTemplate {
    pub selected_project: i64,
    pub columns: Vec<Column>,
}

#[derive(Template)]
#[template(path = "task_edit.html")]
pub struct TaskEditTemplate {
    pub user: Option<CurrentUser>,
    pub task: TaskRow,
    pub projects: Vec<ProjectRow>,
    pub statuses: &'static [Status],
    pub priorities: &'static [&'static str],
    /// Filtre projet actif du tableau, à préserver au retour.
    pub filter: i64,
    pub error: Option<String>,
}

#[derive(Template)]
#[template(path = "task_new.html")]
pub struct TaskNewTemplate {
    pub user: Option<CurrentUser>,
    pub projects: Vec<ProjectRow>,
    pub priorities: &'static [&'static str],
    /// Projet présélectionné (et filtre à préserver au retour).
    pub selected_project: i64,
    pub error: Option<String>,
}

#[derive(Deserialize)]
pub struct BoardQuery {
    /// Filtre par projet : tolère « project= » (vide, via « All projects ») → None.
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    project: Option<i64>,
    /// Message d'erreur à réafficher (pages de création / d'édition).
    error: Option<String>,
}

/// Option<i64> tolérante : champ absent OU chaîne vide → None.
/// (serde traite par défaut « project= » comme Some("") qui échoue le parse.)
fn deserialize_opt_i64<'de, D>(d: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    if s.is_empty() {
        Ok(None)
    } else {
        s.parse::<i64>().map(Some).map_err(serde::de::Error::custom)
    }
}

/// Colonnes du tableau (filtre projet appliqué s'il existe).
fn board_columns(conn: &Connection, user_id: i64, projects: &[ProjectRow], selected: i64) -> rusqlite::Result<Vec<Column>> {
    let filter = TaskFilter {
        project_id: projects.iter().any(|p| p.id == selected).then_some(selected),
        status: None,
    };
    let mut tasks = fetch_tasks(conn, user_id, &filter)?;
    Ok(STATUSES
        .iter()
        .map(|status| {
            let (col, rest) = tasks.drain(..).partition(|t| t.status == status.value);
            tasks = rest;
            Column { status, tasks: col }
        })
        .collect())
}

/// Tableau de bord : le kanban, filtrable par projet.
pub async fn dashboard(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Query(q): Query<BoardQuery>,
) -> AppResult {
    let selected = q.project.unwrap_or(0);
    let conn = state.conn()?;
    let projects = fetch_projects(&conn, user.id)?;
    let columns = board_columns(&conn, user.id, &projects, selected)?;
    Ok(render(&DashboardTemplate {
        user: Some(user),
        projects,
        selected_project: selected,
        columns,
    }))
}

/// Fragment du tableau, rechargé périodiquement par htmx (suivi en direct
/// du travail des agents).
pub async fn board_fragment(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Query(q): Query<BoardQuery>,
) -> AppResult {
    let selected = q.project.unwrap_or(0);
    let conn = state.conn()?;
    let projects = fetch_projects(&conn, user.id)?;
    let columns = board_columns(&conn, user.id, &projects, selected)?;
    Ok(render(&BoardTemplate { selected_project: selected, columns }))
}

/// Page de création d'une tâche (formulaire sur sa propre page).
pub async fn new_task_page(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Query(q): Query<BoardQuery>,
) -> AppResult {
    let projects = fetch_projects(&*state.conn()?, user.id)?;
    Ok(render(&TaskNewTemplate {
        user: Some(user),
        projects,
        priorities: &PRIORITIES,
        selected_project: q.project.unwrap_or(0),
        error: q.error,
    }))
}

/// Page d'édition d'une tâche : le formulaire complet, la revue (si la
/// tâche attend une revue), le journal, les fichiers joints.
pub async fn edit_task_page(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Query(q): Query<BoardQuery>,
) -> AppResult {
    let conn = state.conn()?;
    let task = fetch_one_task(&conn, user.id, id)?;
    let projects = fetch_projects(&conn, user.id)?;
    Ok(render(&TaskEditTemplate {
        user: Some(user),
        task,
        projects,
        statuses: &STATUSES,
        priorities: &PRIORITIES,
        filter: q.project.unwrap_or(0),
        error: q.error,
    }))
}

/// Redirection vers la page d'édition d'une tâche (avec message d'erreur optionnel).
pub fn redirect_edit(id: i64, filter: i64, error: Option<&str>) -> Response {
    redirect_with_error(&format!("/tasks/{id}/edit?project={filter}"), error)
}

fn redirect_dashboard(filter: i64) -> Response {
    Redirect::to(&format!("/dashboard?project={filter}")).into_response()
}

/// Refus du workflow → retour sur la page d'édition avec le message ; une
/// erreur BDD reste une erreur 500.
pub fn wf_redirect(id: i64, filter: i64, e: WfError) -> AppResult {
    match e {
        WfError::Db(e) => Err(e.into()),
        e => Ok(redirect_edit(id, filter, Some(&e.message()))),
    }
}

#[derive(Deserialize)]
pub struct TaskForm {
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    acceptance: String,
    /// Id du projet (0 = aucun).
    #[serde(default)]
    project: i64,
    #[serde(default = "default_priority")]
    priority: i64,
    /// Branche git optionnelle associée à la tâche.
    #[serde(default)]
    branch: String,
    /// Ids des tâches dont celle-ci dépend (« 12, 15 »).
    #[serde(default)]
    depends_on: String,
    /// État cible (bouton d'état de la page d'édition) ; vide = inchangé.
    #[serde(default)]
    status: String,
    /// Filtre projet actif du tableau, à préserver au retour (0 = tous).
    #[serde(default)]
    filter: i64,
}

fn default_priority() -> i64 {
    DEFAULT_PRIORITY
}

/// Champs d'une tâche validés, prêts à être enregistrés.
pub struct ValidTask {
    pub title: String,
    pub description: String,
    pub acceptance: String,
    pub project_id: Option<i64>,
    pub priority: i64,
    pub branch: String,
    pub depends_on: Vec<i64>,
}

/// Valide les champs d'une tâche (`task_id` = 0 pour une création).
/// `Ok(Err(msg))` : erreur de saisie à afficher ; `Err` : erreur BDD.
pub fn validate_task(
    conn: &Connection,
    task_id: i64,
    title: &str,
    project: Option<i64>,
    priority: i64,
    branch: &str,
    depends_on: &[i64],
) -> rusqlite::Result<Result<(String, Option<i64>, String), String>> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 200 {
        return Ok(Err("Task title must be 1 to 200 characters.".into()));
    }
    let branch = branch.trim();
    if !branch.is_empty() && !is_safe_branch(branch) {
        return Ok(Err("Invalid branch name.".into()));
    }
    if !(0..PRIORITIES.len() as i64).contains(&priority) {
        return Ok(Err("Unknown priority.".into()));
    }
    let Some(project_id) = resolve_project(conn, project.unwrap_or(0))? else {
        return Ok(Err("Unknown project.".into()));
    };
    if let Err(msg) = check_deps(conn, task_id, depends_on)? {
        return Ok(Err(msg));
    }
    Ok(Ok((title.to_string(), project_id, branch.to_string())))
}

fn validate_form(conn: &Connection, task_id: i64, f: &TaskForm) -> rusqlite::Result<Result<ValidTask, String>> {
    if !f.status.is_empty() && !is_valid_status(&f.status) {
        return Ok(Err("Unknown task state.".into()));
    }
    let deps = parse_deps(&f.depends_on);
    let checked = validate_task(conn, task_id, &f.title, Some(f.project), f.priority, &f.branch, &deps)?;
    Ok(checked.map(|(title, project_id, branch)| ValidTask {
        title,
        description: f.description.trim().to_string(),
        acceptance: f.acceptance.trim().to_string(),
        project_id,
        priority: f.priority,
        branch,
        depends_on: deps,
    }))
}

/// Insère une tâche dans l'état initial `status` (`draft`, ou `todo` pour un
/// humain depuis l'UI) et journalise sa création.
pub fn insert_task(conn: &Connection, actor: &Actor, t: &ValidTask, status: &str) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO tasks (user_id, project_id, status, priority, branch, title, description, acceptance)
         VALUES (?1, ?2, ?8, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            actor.user().id,
            t.project_id,
            t.priority,
            t.branch,
            t.title,
            t.description,
            t.acceptance,
            status
        ],
    )?;
    let id = conn.last_insert_rowid();
    save_deps(conn, id, &t.depends_on)?;
    workflow::log(conn, id, actor, "move", &format!("Created in {status}"))?;
    Ok(id)
}

/// Création d'une tâche : en draft, ou directement en todo (bouton
/// « Create in To do », `status=todo`). Redirige vers le tableau.
pub async fn create_task(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Form(f): Form<TaskForm>,
) -> AppResult {
    let conn = state.conn()?;
    let t = match validate_form(&conn, 0, &f)? {
        Ok(t) => t,
        Err(msg) => {
            let url = format!("/tasks/new?project={}", f.filter);
            return Ok(redirect_with_error(&url, Some(&msg)));
        }
    };
    let status = if f.status == "todo" { "todo" } else { "draft" };
    insert_task(&conn, &Actor::Human(user), &t, status)?;
    Ok(redirect_dashboard(f.filter))
}

/// Met à jour une tâche (champs et, via les boutons d'état, son état).
/// Redirige vers la page d'édition.
pub async fn update_task(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Form(f): Form<TaskForm>,
) -> AppResult {
    let conn = state.conn()?;
    let t = match validate_form(&conn, id, &f)? {
        Ok(t) => t,
        Err(msg) => return Ok(redirect_edit(id, f.filter, Some(&msg))),
    };
    let changed = conn.execute(
        "UPDATE tasks SET title = ?1, description = ?2, acceptance = ?3, project_id = ?4,
                priority = ?5, branch = ?6, updated_at = datetime('now')
         WHERE id = ?7",
        rusqlite::params![t.title, t.description, t.acceptance, t.project_id, t.priority, t.branch, id],
    )?;
    if changed == 0 {
        return Err(AppError::not_found("Task not found."));
    }
    save_deps(&conn, id, &t.depends_on)?;
    if !f.status.is_empty() {
        if let Err(e) = workflow::move_task(&conn, &Actor::Human(user), id, &f.status, None) {
            return wf_redirect(id, f.filter, e);
        }
    }
    Ok(redirect_edit(id, f.filter, None))
}

#[derive(Deserialize)]
pub struct MoveForm {
    status: String,
}

/// Déplace une tâche vers un autre état (glisser-déposer du tableau, appelé
/// en fetch) : 204, ou le message d'erreur avec le statut http adéquat.
pub async fn move_task(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Form(f): Form<MoveForm>,
) -> AppResult {
    let conn = state.conn()?;
    match workflow::move_task(&conn, &Actor::Human(user), id, &f.status, None) {
        Ok(()) => Ok(axum::http::StatusCode::NO_CONTENT.into_response()),
        Err(WfError::Db(e)) => Err(e.into()),
        Err(e) => Ok((e.status(), e.message()).into_response()),
    }
}

#[derive(Deserialize)]
pub struct FilterForm {
    /// Filtre projet actif (préserve la vue après l'action).
    #[serde(default)]
    filter: i64,
}

/// Supprime une tâche (fichiers, journal et dépendances en cascade).
/// Redirige vers le tableau (le filtre projet est préservé).
pub async fn delete_task(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<FilterForm>,
) -> AppResult {
    state.conn()?.execute("DELETE FROM tasks WHERE id = ?1", [id])?;
    Ok(redirect_dashboard(f.filter))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_branch_names() {
        for ok in ["main", "feature/my-change", "fix_1.2", "a/b/c"] {
            assert!(is_safe_branch(ok), "{ok}");
        }
        let too_long = "x".repeat(201);
        for bad in ["", "-x", ".x", "/x", "x/", "a..b", "a b", "a~1", "a:b", too_long.as_str()] {
            assert!(!is_safe_branch(bad), "{bad}");
        }
    }

    #[test]
    fn preview_is_first_non_empty_line_truncated() {
        assert_eq!(first_line_preview(""), "");
        assert_eq!(first_line_preview("\n  \n  hello \nworld"), "hello");
        let long = "é".repeat(150);
        assert_eq!(first_line_preview(&long), format!("{}…", "é".repeat(140)));
    }

    #[test]
    fn dependencies_are_parsed_and_cycles_rejected() {
        assert_eq!(parse_deps("12, #15 12;3"), vec![3, 12, 15]);
        assert!(parse_deps(" ").is_empty());
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_app_schema(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO tasks (title) VALUES ('a'), ('b'), ('c');
             INSERT INTO task_deps VALUES (2, 1), (3, 2);",
        )
        .unwrap();
        assert!(check_deps(&conn, 3, &[1]).unwrap().is_ok());
        assert!(check_deps(&conn, 1, &[3]).unwrap().is_err()); // 1 → 3 → 2 → 1
        assert!(check_deps(&conn, 1, &[1]).unwrap().is_err());
        assert!(check_deps(&conn, 0, &[9]).unwrap().is_err());
    }
}
