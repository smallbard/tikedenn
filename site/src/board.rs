//! Tableau kanban : tâches dans les états draft / todo / progress / review /
//! deploy / done, filtrables par projet, avec description longue, branche git
//! optionnelle et fichiers joints (voir `files`).

use crate::auth::CurrentUser;
use crate::projects::{fetch_projects, ProjectRow};
use crate::web::{redirect_with_error, render, AppError, AppResult};
use crate::AppState;
use askama::Template;
use axum::{
    extract::{Extension, Path, Query, State},
    response::{IntoResponse, Redirect, Response},
    Form,
};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;

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

fn is_valid_status(s: &str) -> bool {
    STATUSES.iter().any(|st| st.value == s)
}

pub struct FileRow {
    pub id: i64,
    pub name: String,
    pub size: i64,
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
    /// Branche git optionnelle associée à la tâche (diff vs main).
    pub branch: String,
    pub title: String,
    pub description: String,
    /// Première ligne de la description, pour l'affichage compact de la carte.
    pub description_preview: String,
    /// Fichiers joints (chargés seulement pour une tâche seule, pas le tableau).
    pub files: Vec<FileRow>,
}

impl TaskRow {
    /// Id du projet associé (0 si aucun) — pour la comparaison dans les templates.
    pub fn project_id_or_zero(&self) -> i64 {
        self.project.as_ref().map_or(0, |p| p.id)
    }
}

/// Une colonne du tableau et ses tâches.
pub struct Column {
    pub status: &'static Status,
    pub tasks: Vec<TaskRow>,
}

/// Colonnes communes à `fetch_tasks` et `fetch_one_task` (lues par `task_from_row`).
/// ?3 = user_id : le token (col 4) vient de `project_tokens`, c'est le token
/// de l'utilisateur courant pour le projet de la tâche.
const TASK_SELECT: &str = "SELECT t.id, p.id, p.name, p.git_repo, COALESCE(pt.token, ''),
                                  t.status, t.branch, t.title, t.description
                           FROM tasks t LEFT JOIN projects p ON p.id = t.project_id
                           LEFT JOIN project_tokens pt ON pt.project_id = p.id AND pt.user_id = ?3";

/// Construit une `TaskRow` (sans ses fichiers) depuis une ligne de `TASK_SELECT`.
fn task_from_row(r: &rusqlite::Row) -> rusqlite::Result<TaskRow> {
    let project = match r.get::<_, Option<i64>>(1)? {
        Some(id) => Some(ProjectRow {
            id,
            name: r.get(2)?,
            git_repo: r.get(3)?,
            token: r.get(4)?,
        }),
        None => None,
    };
    let description: String = r.get(8)?;
    Ok(TaskRow {
        id: r.get(0)?,
        project,
        status: r.get(5)?,
        branch: r.get(6)?,
        title: r.get(7)?,
        description_preview: first_line_preview(&description),
        description,
        files: vec![],
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

/// Tâches de l'utilisateur (sans leurs fichiers), la plus récente d'abord.
/// `project_id` : filtre optionnel (`None` → tous les projets).
fn fetch_tasks(
    conn: &Connection,
    user_id: i64,
    project_id: Option<i64>,
) -> rusqlite::Result<Vec<TaskRow>> {
    let sql = format!(
        "{TASK_SELECT} WHERE t.user_id = ?1 AND (?2 IS NULL OR t.project_id = ?2) ORDER BY t.id DESC"
    );
    conn.prepare(&sql)?
        .query_map(rusqlite::params![user_id, project_id, user_id], task_from_row)?
        .collect()
}

fn fetch_files(conn: &Connection, task_id: i64) -> rusqlite::Result<Vec<FileRow>> {
    conn.prepare("SELECT id, name, size FROM files WHERE task_id = ?1 ORDER BY id")?
        .query_map([task_id], |r| {
            Ok(FileRow {
                id: r.get(0)?,
                name: r.get(1)?,
                size: r.get(2)?,
            })
        })?
        .collect()
}

/// Une tâche de l'utilisateur avec ses fichiers ; 404 si elle n'existe pas
/// (ou appartient à un autre utilisateur).
pub fn fetch_one_task(conn: &Connection, user_id: i64, id: i64) -> AppResult<TaskRow> {
    let sql = format!("{TASK_SELECT} WHERE t.user_id = ?1 AND t.id = ?2");
    let mut task = conn
        .query_row(&sql, rusqlite::params![user_id, id, user_id], task_from_row)
        .optional()?
        .ok_or(AppError::not_found("Task not found."))?;
    task.files = fetch_files(conn, task.id)?;
    Ok(task)
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

#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct DashboardTemplate {
    pub user: Option<CurrentUser>,
    pub projects: Vec<ProjectRow>,
    /// Id du projet de filtre sélectionné (0 = tous).
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
    /// Filtre projet actif du tableau, à préserver au retour.
    pub filter: i64,
    pub error: Option<String>,
}

#[derive(Template)]
#[template(path = "task_new.html")]
pub struct TaskNewTemplate {
    pub user: Option<CurrentUser>,
    pub projects: Vec<ProjectRow>,
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

/// Tableau de bord : le kanban, filtrable par projet.
pub async fn dashboard(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Query(q): Query<BoardQuery>,
) -> AppResult {
    let selected = q.project.unwrap_or(0);
    let conn = state.conn()?;
    let projects = fetch_projects(&conn, user.id)?;
    let filter = projects.iter().any(|p| p.id == selected).then_some(selected);
    let mut tasks = fetch_tasks(&conn, user.id, filter)?;
    let columns = STATUSES
        .iter()
        .map(|status| {
            let (col, rest) = tasks.drain(..).partition(|t| t.status == status.value);
            tasks = rest;
            Column { status, tasks: col }
        })
        .collect();
    Ok(render(&DashboardTemplate {
        user: Some(user),
        projects,
        selected_project: selected,
        columns,
    }))
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
        selected_project: q.project.unwrap_or(0),
        error: q.error,
    }))
}

/// Page d'édition d'une tâche : le formulaire complet (titre, projet,
/// description, état, fichiers joints, suppression).
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
        filter: q.project.unwrap_or(0),
        error: q.error,
    }))
}

/// Redirection vers la page d'édition d'une tâche (avec message d'erreur optionnel).
fn redirect_edit(id: i64, filter: i64, error: Option<&str>) -> Response {
    redirect_with_error(&format!("/tasks/{id}/edit?project={filter}"), error)
}

fn redirect_dashboard(filter: i64) -> Response {
    Redirect::to(&format!("/dashboard?project={filter}")).into_response()
}

#[derive(Deserialize)]
pub struct TaskForm {
    title: String,
    #[serde(default)]
    description: String,
    /// Id du projet (0 = aucun).
    #[serde(default)]
    project: i64,
    /// Branche git optionnelle associée à la tâche.
    #[serde(default)]
    branch: String,
    /// État cible (bouton d'état de la page d'édition) ; vide = inchangé.
    #[serde(default)]
    status: String,
    /// Filtre projet actif du tableau, à préserver au retour (0 = tous).
    #[serde(default)]
    filter: i64,
}

/// Champs d'une tâche validés, prêts à être enregistrés.
struct ValidTask {
    title: String,
    description: String,
    project_id: Option<i64>,
    branch: String,
}

/// Valide le formulaire d'une tâche. `Ok(Err(msg))` : erreur de saisie à
/// afficher ; `Err` : erreur BDD.
fn validate_task(conn: &Connection, f: &TaskForm) -> AppResult<Result<ValidTask, &'static str>> {
    let title = f.title.trim();
    if title.is_empty() {
        return Ok(Err("Task title is required."));
    }
    let branch = f.branch.trim();
    if !branch.is_empty() && !is_safe_branch(branch) {
        return Ok(Err("Invalid branch name."));
    }
    if !f.status.is_empty() && !is_valid_status(&f.status) {
        return Ok(Err("Unknown task state."));
    }
    let Some(project_id) = resolve_project(conn, f.project)? else {
        return Ok(Err("Unknown project."));
    };
    Ok(Ok(ValidTask {
        title: title.to_string(),
        description: f.description.trim().to_string(),
        project_id,
        branch: branch.to_string(),
    }))
}

/// Création d'une tâche : toujours en draft. Redirige vers le tableau.
pub async fn create_task(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Form(f): Form<TaskForm>,
) -> AppResult {
    let conn = state.conn()?;
    let t = match validate_task(&conn, &f)? {
        Ok(t) => t,
        Err(msg) => {
            let url = format!("/tasks/new?project={}", f.filter);
            return Ok(redirect_with_error(&url, Some(msg)));
        }
    };
    conn.execute(
        "INSERT INTO tasks (user_id, project_id, status, branch, title, description)
         VALUES (?1, ?2, 'draft', ?3, ?4, ?5)",
        rusqlite::params![user.id, t.project_id, t.branch, t.title, t.description],
    )?;
    Ok(redirect_dashboard(f.filter))
}

/// Met à jour une tâche (titre, description, projet, branche et, via les
/// boutons d'état, son état). Redirige vers la page d'édition.
pub async fn update_task(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Form(f): Form<TaskForm>,
) -> AppResult {
    let conn = state.conn()?;
    let t = match validate_task(&conn, &f)? {
        Ok(t) => t,
        Err(msg) => return Ok(redirect_edit(id, f.filter, Some(msg))),
    };
    conn.execute(
        "UPDATE tasks SET title = ?1, description = ?2, project_id = ?3, branch = ?4,
         status = COALESCE(NULLIF(?7, ''), status),
         updated_at = datetime('now')
         WHERE id = ?5 AND user_id = ?6",
        rusqlite::params![t.title, t.description, t.project_id, t.branch, id, user.id, f.status],
    )?;
    Ok(redirect_edit(id, f.filter, None))
}

#[derive(Deserialize)]
pub struct MoveForm {
    status: String,
    #[serde(default)]
    filter: i64,
}

/// Déplace une tâche vers un autre état (glisser-déposer du tableau).
/// Redirige vers la page d'édition.
pub async fn move_task(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Form(f): Form<MoveForm>,
) -> AppResult {
    if !is_valid_status(&f.status) {
        return Ok(redirect_edit(id, f.filter, Some("Unknown task state.")));
    }
    state.conn()?.execute(
        "UPDATE tasks SET status = ?1, updated_at = datetime('now') WHERE id = ?2 AND user_id = ?3",
        rusqlite::params![f.status, id, user.id],
    )?;
    Ok(redirect_edit(id, f.filter, None))
}

#[derive(Deserialize)]
pub struct FilterForm {
    /// Filtre projet actif (préserve la vue après l'action).
    #[serde(default)]
    filter: i64,
}

/// Supprime une tâche (fichiers en cascade). Redirige vers le tableau
/// (le filtre projet est préservé).
pub async fn delete_task(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Form(f): Form<FilterForm>,
) -> AppResult {
    state.conn()?.execute(
        "DELETE FROM tasks WHERE id = ?1 AND user_id = ?2",
        rusqlite::params![id, user.id],
    )?;
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
}
