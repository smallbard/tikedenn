//! Tableau kanban : projets (nom + dépôt git), tâches dans les états
//! draft / todo / progress / review / deploy / done, description longue et fichiers joints.

use crate::auth::{self, CurrentUser};
use crate::util::urlencode;
use crate::AppState;
use askama::Template;
use axum::{
    body::Body,
    extract::{multipart::Multipart, Extension, Path, Query, State},
    http::header,
    response::{IntoResponse, Redirect, Response},
    Form,
};
use git2::{Cred, DiffFlags, FetchOptions, Patch, RemoteCallbacks, Repository};
use rand::Rng;
use serde::Deserialize;
use std::collections::HashMap;

/// Taille maximale d'une pièce jointe (10 Mo).
const MAX_FILE_SIZE: usize = 10 * 1024 * 1024;

/// États du tableau, dans l'ordre du gauche vers la droite.
const STATUSES: [(&str, &str); 6] = [
    ("draft", "Draft"),
    ("todo", "To do"),
    ("progress", "In progress"),
    ("review", "To review"),
    ("deploy", "To deploy"),
    ("done", "Done"),
];

fn status_choices() -> Vec<StatusChoice> {
    STATUSES
        .iter()
        .map(|(v, l)| StatusChoice {
            value: v.to_string(),
            label: l.to_string(),
        })
        .collect()
}

#[derive(Clone)]
pub struct StatusChoice {
    pub value: String,
    pub label: String,
}

#[derive(Clone)]
pub struct ProjectRow {
    pub id: i64,
    pub name: String,
    pub git_repo: String,
    /// Token optionnel (dépôt privé en https) pour l'utilisateur courant ;
    /// jamais affiché dans l'UI.
    pub token: String,
}

#[derive(Clone)]
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

#[derive(Clone)]
pub struct TaskRow {
    pub id: i64,
    pub project_id: Option<i64>,
    pub project: Option<ProjectRow>,
    pub status: String,
    /// Branche git optionnelle associée à la tâche (diff vs main).
    pub branch: String,
    pub title: String,
    pub description: String,
    /// Première ligne de la description, pour l'affichage compact de la carte.
    pub description_preview: String,
    pub created_at: String,
    pub files: Vec<FileRow>,
}

impl TaskRow {
    /// Id du projet associé (0 si aucun) — pour la comparaison dans les templates.
    pub fn project_id_or_zero(&self) -> i64 {
        self.project_id.unwrap_or(0)
    }
}

pub struct Column {
    pub status: &'static str,
    pub label: &'static str,
    pub count: usize,
    pub cards_html: String,
}

// Projets partagés : pas de filtre par utilisateur ; en revanche le token
// renvoyé est celui du couple (projet, utilisateur courant).
fn fetch_projects(conn: &rusqlite::Connection, user_id: i64) -> Vec<ProjectRow> {
    let mut out = vec![];
    if let Ok(mut st) = conn.prepare(
        "SELECT p.id, p.name, p.git_repo,
                COALESCE((SELECT t.token FROM project_tokens t WHERE t.project_id = p.id AND t.user_id = ?1), '')
         FROM projects p ORDER BY p.name COLLATE NOCASE",
    ) {
        if let Ok(rows) = st.query_map([user_id], |r| {
            Ok(ProjectRow {
                id: r.get(0)?,
                name: r.get(1)?,
                git_repo: r.get(2)?,
                token: r.get(3)?,
            })
        }) {
            out.extend(rows.filter_map(|r| r.ok()));
        }
    }
    out
}

fn fetch_files(conn: &rusqlite::Connection, task_id: i64) -> Vec<FileRow> {
    let mut out = vec![];
    if let Ok(mut st) = conn.prepare("SELECT id, name, size FROM files WHERE task_id = ?1 ORDER BY id")
    {
        if let Ok(rows) = st.query_map([task_id], |r| {
            Ok(FileRow {
                id: r.get(0)?,
                name: r.get(1)?,
                size: r.get(2)?,
            })
        }) {
            out.extend(rows.filter_map(|r| r.ok()));
        }
    }
    out
}

/// Première ligne non vide de la description, tronquée à 140 caractères.
fn first_line_preview(description: &str) -> String {
    let first = description.lines().map(|l| l.trim()).find(|l| !l.is_empty());
    match first {
        Some(l) => match l.char_indices().nth(140) {
            Some((e, _)) => format!("{}…", &l[..e]),
            None => l.to_string(),
        },
        None => String::new(),
    }
}

/// Fichiers de toutes les tâches du tableau en une seule requête (pas de N+1).
/// Même filtre que `fetch_tasks` (?2 IS NULL → tous les projets).
fn fetch_files_for_board(
    conn: &rusqlite::Connection,
    user_id: i64,
    project_id: Option<i64>,
) -> HashMap<i64, Vec<FileRow>> {
    let mut out: HashMap<i64, Vec<FileRow>> = HashMap::new();
    let sql = "SELECT task_id, id, name, size FROM files
               WHERE task_id IN (SELECT id FROM tasks
                                 WHERE user_id = ?1 AND (?2 IS NULL OR project_id = ?2))
               ORDER BY id";
    let mut st = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fetch_files_for_board : {e}");
            return out;
        }
    };
    let rows = match st.query_map(rusqlite::params![user_id, project_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            FileRow {
                id: r.get(1)?,
                name: r.get(2)?,
                size: r.get(3)?,
            },
        ))
    }) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("fetch_files_for_board : {e}");
            return out;
        }
    };
    for r in rows.filter_map(|r| r.ok()) {
        out.entry(r.0).or_default().push(r.1);
    }
    out
}

/// Colonnes communes à `fetch_tasks` et `fetch_one_task` (lues par `task_from_row`).
/// ?3 = user_id : le token (col 5) vient de `project_tokens`, c'est le token
/// de l'utilisateur courant pour le projet de la tâche.
const TASK_SELECT: &str = "SELECT t.id, t.project_id, p.id, p.name, p.git_repo, COALESCE(pt.token, ''), t.status, t.branch, t.title,
                                  t.description, t.created_at
                           FROM tasks t LEFT JOIN projects p ON p.id = t.project_id
                           LEFT JOIN project_tokens pt ON pt.project_id = p.id AND pt.user_id = ?3";

/// Construit une `TaskRow` (sans ses fichiers) depuis une ligne de `TASK_SELECT`.
fn task_from_row(r: &rusqlite::Row) -> rusqlite::Result<TaskRow> {
    // Colonnes : 0=t.id, 1=t.project_id, 2=p.id, 3=p.name, 4=p.git_repo, 5=pt.token,
    //           6=t.status, 7=t.branch, 8=t.title, 9=t.description, 10=t.created_at
    let desc: String = r.get(9)?;
    Ok(TaskRow {
        id: r.get(0)?,
        project_id: r.get::<_, Option<i64>>(1)?,
        project: if r.get::<_, Option<i64>>(2)?.is_some() {
            Some(ProjectRow {
                id: r.get(2)?,
                name: r.get(3)?,
                git_repo: r.get(4)?,
                token: r.get(5)?,
            })
        } else {
            None
        },
        status: r.get(6)?,
        branch: r.get(7)?,
        title: r.get(8)?,
        description_preview: first_line_preview(&desc),
        description: desc,
        created_at: r.get(10)?,
        files: vec![],
    })
}

fn fetch_tasks(conn: &rusqlite::Connection, user_id: i64, project_id: Option<i64>) -> Vec<TaskRow> {
    let mut out = vec![];
    // ?2 IS NULL → pas de filtre projet (tous les projets).
    // Le plus récent d'abord dans chaque colonne.
    let sql = format!(
        "{TASK_SELECT} WHERE t.user_id = ?1 AND (?2 IS NULL OR t.project_id = ?2) ORDER BY t.id DESC"
    );
    let mut st = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return out,
    };
    let rows = match st.query_map(rusqlite::params![user_id, project_id, user_id], task_from_row) {
        Ok(r) => r,
        Err(_) => return out,
    };
    out.extend(rows.filter_map(|r| r.ok()));
    // Fichiers : une seule requête pour toutes les tâches.
    let mut files = fetch_files_for_board(conn, user_id, project_id);
    for t in &mut out {
        t.files = files.remove(&t.id).unwrap_or_default();
    }
    out
}

/// Une seule tâche (pour le re-rendu d'une carte htmx) + ses fichiers.
/// `Ok(None)` : tâche inexistante ; `Err` : vraie erreur BDD.
fn fetch_one_task(
    conn: &rusqlite::Connection,
    user_id: i64,
    id: i64,
) -> rusqlite::Result<Option<TaskRow>> {
    let sql = format!("{TASK_SELECT} WHERE t.user_id = ?1 AND t.id = ?2");
    match conn.query_row(&sql, rusqlite::params![user_id, id, user_id], task_from_row) {
        Ok(mut t) => {
            t.files = fetch_files(conn, t.id);
            Ok(Some(t))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Construit les 4 colonnes du tableau (utilisé par la page et les
/// réponses htmx). `selected` : id du projet de filtre, 0 = tous.
fn build_board(
    state: &AppState,
    user: &CurrentUser,
    selected: i64,
) -> (Vec<ProjectRow>, Vec<Column>) {
    let conn = state.db.get().expect("pool db");
    let projects = fetch_projects(&conn, user.id);
    let filter = if selected > 0 && projects.iter().any(|p| p.id == selected) {
        Some(selected)
    } else {
        None
    };
    let tasks = fetch_tasks(&conn, user.id, filter);
    let mut columns = vec![];
    for (status, label) in STATUSES {
        let mut cards = vec![];
        for t in tasks.iter().filter(|t| t.status == status) {
            let card = TaskCardTemplate {
                task: t,
                filter: selected,
            };
            if let Ok(html) = card.render() {
                cards.push(html);
            }
        }
        columns.push(Column {
            status,
            label,
            count: cards.len(),
            cards_html: cards.join("\n"),
        });
    }
    (projects, columns)
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
#[template(path = "task_card.html")]
pub struct TaskCardTemplate<'a> {
    pub task: &'a TaskRow,
    /// Filtre projet actif, à préserver après l'édition.
    pub filter: i64,
}

#[derive(Template)]
#[template(path = "task_edit.html")]
pub struct TaskEditTemplate<'a> {
    pub user: Option<CurrentUser>,
    pub task: &'a TaskRow,
    pub projects: &'a [ProjectRow],
    pub statuses: &'a [StatusChoice],
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

/// Page de création d'une tâche (formulaire sur sa propre page).
pub async fn new_task_page(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Query(q): Query<BoardQuery>,
) -> Response {
    let selected = q.project.unwrap_or(0);
    let conn = state.db.get().expect("pool db");
    let projects = fetch_projects(&conn, user.id);
    auth::render(&TaskNewTemplate {
        user: Some(user),
        projects,
        selected_project: selected,
        error: q.error,
    })
}

/// Redirection vers la page d'édition d'une tâche (avec message d'erreur optionnel).
fn redirect_edit(id: i64, filter: i64, error: Option<&str>) -> Response {
    let mut url = format!("/tasks/{id}/edit?project={filter}");
    if let Some(e) = error {
        url.push_str(&format!("&error={}", urlencode(e)));
    }
    Redirect::to(&url).into_response()
}

/// Page d'édition d'une tâche : le formulaire complet (titre, projet,
/// description, état, fichiers joints, suppression).
pub async fn edit_task_page(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Query(q): Query<BoardQuery>,
) -> Response {
    let conn = state.db.get().expect("pool db");
    let task = match fetch_one_task(&conn, user.id, id) {
        Ok(Some(t)) => t,
        Ok(None) => return (axum::http::StatusCode::NOT_FOUND, "Task not found.").into_response(),
        Err(_) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load the task.",
            )
                .into_response()
        }
    };
    let projects = fetch_projects(&conn, user.id);
    let statuses = status_choices();
    auth::render(&TaskEditTemplate {
        user: Some(user),
        task: &task,
        projects: &projects,
        statuses: &statuses,
        filter: q.project.unwrap_or(0),
        error: q.error,
    })
}

/// Tableau de bord : le kanban, filtrable par projet.
pub async fn dashboard(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Query(q): Query<BoardQuery>,
) -> Response {
    let selected = q.project.unwrap_or(0);
    let (projects, columns) = build_board(&state, &user, selected);
    auth::render(&DashboardTemplate {
        user: Some(user),
        projects,
        selected_project: selected,
        columns,
    })
}

#[derive(Deserialize)]
pub struct BoardQuery {
    /// Filtre par projet : tolère « project= » (vide, via « All projects ») → None.
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    project: Option<i64>,
    /// Message d'erreur à réafficher (page de création d'une tâche).
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
        s.parse::<i64>()
            .map(Some)
            .map_err(|e| serde::de::Error::custom(e.to_string()))
    }
}

#[derive(Deserialize)]
pub struct TaskForm {
    title: String,
    #[serde(default)]
    description: String,
    /// Id du projet (0 = aucun) pour la nouvelle tâche.
    #[serde(default)]
    project: i64,
    /// Branche git optionnelle associée à la tâche.
    #[serde(default)]
    branch: String,
    /// État cible (bouton d'état de la page d'édition) ; vide = inchangé.
    #[serde(default)]
    status: String,
    /// Filtre projet actif du tableau, à préserver après la création (0 = tous).
    #[serde(default)]
    filter: i64,
}

/// Nom de branche git raisonnablement sûr (ne sert qu'aux refs git2, mais on
/// évite les noms pathologiques : ref parents, slash initial, etc.).
pub fn is_safe_branch(b: &str) -> bool {
    let b = b.trim();
    !b.is_empty()
        && b.len() <= 200
        && !b.starts_with(['-', '.', '/'])
        && !b.ends_with('/')
        && !b.contains("..")
        && b.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '_' | '-'))
}

/// Redirection vers la page de création avec un message d'erreur.
fn redirect_new_task(filter: i64, error: &str) -> Response {
    let mut url = format!("/tasks/new?project={}", filter);
    if !error.is_empty() {
        url.push_str(&format!("&error={}", urlencode(error)));
    }
    Redirect::to(&url).into_response()
}

/// Création d'une tâche : toujours en draft. Redirige vers le tableau.
pub async fn create_task(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Form(f): Form<TaskForm>,
) -> Response {
    let title = f.title.trim().to_string();
    if title.is_empty() {
        return redirect_new_task(f.filter, "Task title is required.");
    }
    let project_id: Option<i64> = if f.project > 0 {
        let conn = state.db.get().expect("pool db");
        match conn.query_row(
            "SELECT id FROM projects WHERE id = ?1",
            [f.project],
            |r| r.get(0),
        ) {
            Ok(id) => Some(id),
            Err(_) => return redirect_new_task(f.filter, "Unknown project."),
        }
    } else {
        None
    };
    let branch = f.branch.trim().to_string();
    if !branch.is_empty() && !is_safe_branch(&branch) {
        return redirect_new_task(f.filter, "Invalid branch name.");
    }
    let conn = state.db.get().expect("pool db");
    if conn
        .execute(
            "INSERT INTO tasks (user_id, project_id, status, branch, title, description)
             VALUES (?1, ?2, 'draft', ?3, ?4, ?5)",
            rusqlite::params![user.id, project_id, branch, title, f.description.trim()],
        )
        .is_err()
    {
        return redirect_new_task(f.filter, "Could not create the task.");
    }
    let url = format!("/dashboard?project={}", f.filter);
    Redirect::to(&url).into_response()
}

/// Déplace une tâche vers un autre état. Redirige vers la page d'édition.
pub async fn move_task(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Form(f): Form<MoveForm>,
) -> Response {
    if !STATUSES.iter().any(|(v, _)| *v == f.status) {
        return redirect_edit(id, f.filter, Some("Unknown task state."));
    }
    let conn = state.db.get().expect("pool db");
    let _ = conn.execute(
        "UPDATE tasks SET status = ?1, updated_at = datetime('now') WHERE id = ?2 AND user_id = ?3",
        rusqlite::params![f.status, id, user.id],
    );
    redirect_edit(id, f.filter, None)
}

#[derive(Deserialize)]
pub struct MoveForm {
    status: String,
    #[serde(default)]
    filter: i64,
}

/// Met à jour une tâche (titre, description, projet). Redirige vers la page d'édition.
pub async fn update_task(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Form(f): Form<TaskForm>,
) -> Response {
    let title = f.title.trim().to_string();
    if title.is_empty() {
        return redirect_edit(id, f.filter, Some("Task title is required."));
    }
    let branch = f.branch.trim().to_string();
    if !branch.is_empty() && !is_safe_branch(&branch) {
        return redirect_edit(id, f.filter, Some("Invalid branch name."));
    }
    if !f.status.is_empty() && !STATUSES.iter().any(|(v, _)| *v == f.status) {
        return redirect_edit(id, f.filter, Some("Unknown task state."));
    }
    let project_id: Option<i64> = if f.project > 0 {
        let conn = state.db.get().expect("pool db");
        match conn.query_row(
            "SELECT id FROM projects WHERE id = ?1",
            [f.project],
            |r| r.get(0),
        ) {
            Ok(p) => Some(p),
            Err(_) => return redirect_edit(id, f.filter, Some("Unknown project.")),
        }
    } else {
        None
    };
    let conn = state.db.get().expect("pool db");
    if conn
        .execute(
            "UPDATE tasks SET title = ?1, description = ?2, project_id = ?3, branch = ?4,
             status = COALESCE(NULLIF(?7, ''), status),
             updated_at = datetime('now')
             WHERE id = ?5 AND user_id = ?6",
            rusqlite::params![title, f.description.trim(), project_id, branch, id, user.id, f.status],
        )
        .is_err()
    {
        return redirect_edit(id, f.filter, Some("Could not update the task."));
    }
    redirect_edit(id, f.filter, None)
}

/// Supprime une tâche (fichiers en cascade). Redirige vers le tableau
/// (le filtre projet est préservé).
pub async fn delete_task(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Form(f): Form<DeleteTaskForm>,
) -> Response {
    // Connexion brève : rendue au pool avant le re-rendu du tableau.
    let deleted = match state.db.get() {
        Ok(conn) => conn
            .execute(
                "DELETE FROM tasks WHERE id = ?1 AND user_id = ?2",
                rusqlite::params![id, user.id],
            )
            .is_ok(),
        Err(_) => false,
    };
    if !deleted {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Could not delete the task.",
        )
            .into_response();
    }
    let url = format!("/dashboard?project={}", f.filter);
    Redirect::to(&url).into_response()
}

#[derive(Deserialize)]
pub struct DeleteTaskForm {
    /// Filtre projet actif (préserve la vue après suppression).
    #[serde(default)]
    filter: i64,
}

/// Téléverse un fichier joint sur une tâche (multipart).
/// La connexion du pool n'est retenue que pour la vérification de propriété
/// et l'INSERT — jamais pendant la lecture du corps (uploads lents).
pub async fn upload_file(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(task_id): Path<i64>,
    mut multipart: Multipart,
) -> Response {
    // La tâche doit exister et appartenir à l'utilisateur (connexion brève).
    let task_ok = {
        let conn = state.db.get().expect("pool db");
        conn.query_row(
            "SELECT id FROM tasks WHERE id = ?1 AND user_id = ?2",
            rusqlite::params![task_id, user.id],
            |r| r.get::<_, i64>(0),
        )
        .is_ok()
    };
    if !task_ok {
        return (axum::http::StatusCode::NOT_FOUND, "Task not found.").into_response();
    }
    let mut filter = 0i64;
    let mut stored = false;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(_) => {
                // Erreur multipart (corps tronqué par la limite de taille, etc.) :
                // ne pas laisser passer comme un upload vide.
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    "Upload failed (request body too large or malformed).",
                )
                    .into_response();
            }
        };
        if field.name() == Some("filter") {
            if let Ok(t) = field.text().await {
                if let Ok(v) = t.parse() {
                    filter = v;
                }
            }
            continue;
        }
        let file_name = match field.file_name().map(|s| s.trim().to_string()) {
            Some(n) if !n.is_empty() => n,
            _ => continue,
        };
        let mime = field.content_type().unwrap_or("").to_string();
        let data = match field.bytes().await {
            Ok(d) => d,
            Err(_) => return (axum::http::StatusCode::BAD_REQUEST, "Upload failed.").into_response(),
        };
        let size = data.len();
        let data = data.to_vec();
        if size > MAX_FILE_SIZE {
            return (
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                "File too large (10 MB maximum).",
            )
                .into_response();
        }
        // Connexion brève, seulement pour l'INSERT.
        match state.db.get() {
            Ok(conn) => {
                if conn
                    .execute(
                        "INSERT INTO files (task_id, name, size, mime, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                        rusqlite::params![task_id, file_name, size as i64, mime, data],
                    )
                    .is_err()
                {
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        "Could not store the file.",
                    )
                        .into_response();
                }
            }
            Err(_) => {
                return (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "Database busy, try again.",
                )
                    .into_response()
            }
        }
        stored = true;
        // Un seul fichier par formulaire.
        break;
    }
    if !stored {
        return (
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "No file uploaded.",
        )
            .into_response();
    }
    // Retour sur la page d'édition de la tâche (filtre du tableau préservé).
    Redirect::to(&format!("/tasks/{task_id}/edit?project={filter}")).into_response()
}

/// En-tête `Content-Disposition` d'un téléchargement : nom ASCII de repli
/// (`filename=`) + nom exact encodé en UTF-8 (`filename*=`, RFC 6266) — un
/// en-tête http ne peut pas contenir de non-ASCII brut.
fn content_disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c == ' ' || (c.is_ascii_graphic() && c != '"' && c != '\\') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
        urlencode(name)
    )
}

/// Téléchargement d'un fichier joint.
pub async fn download_file(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Response {
    let conn = state.db.get().expect("pool db");
    let row: Result<(String, String, i64, Vec<u8>, i64), _> = conn.query_row(
        "SELECT f.name, f.mime, f.size, f.data, t.user_id
         FROM files f JOIN tasks t ON t.id = f.task_id WHERE f.id = ?1",
        [id],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
            ))
        },
    );
    let (name, mime, _size, data, owner) = match row {
        Ok(v) => v,
        Err(_) => return (axum::http::StatusCode::NOT_FOUND, "File not found.").into_response(),
    };
    if owner != user.id {
        return (axum::http::StatusCode::NOT_FOUND, "File not found.").into_response();
    }
    let mime = if mime.is_empty() {
        "application/octet-stream".to_string()
    } else {
        mime
    };
    let mut resp = Body::from(data).into_response();
    if let Ok(hv) = mime.parse() {
        resp.headers_mut().insert(header::CONTENT_TYPE, hv);
    }
    if let Ok(hv) = content_disposition(&name).parse() {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, hv);
    }
    resp
}

/// Supprime un fichier joint. Renvoie du vide (hx-swap=outerHTML).
pub async fn delete_file(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Response {
    if let Ok(conn) = state.db.get() {
        let _ = conn.execute(
            "DELETE FROM files WHERE id = ?1 AND task_id IN (SELECT id FROM tasks WHERE user_id = ?2)",
            rusqlite::params![id, user.id],
        );
    }
    "".into_response()
}

// ---------------------------------------------------------------------------
// Projets

#[derive(Template)]
#[template(path = "projects.html")]
pub struct ProjectsTemplate {
    pub user: Option<CurrentUser>,
    pub projects: Vec<ProjectItemRow>,
    pub error: Option<String>,
}

/// Renvoie vrai si l'URL est sûre pour un attribut `href` :
/// uniquement http(s)://, ssh:// ou la forme scp `git@…`.
fn is_safe_repo_url(url: &str) -> bool {
    let l = url.trim().to_ascii_lowercase();
    l.starts_with("http://")
        || l.starts_with("https://")
        || l.starts_with("ssh://")
        || l.starts_with("git@")
}

pub struct ProjectItemRow {
    pub id: i64,
    pub name: String,
    pub git_repo: String,
    pub tasks: i64,
    /// Un token est configuré pour ce dépôt par l'utilisateur courant
    /// (indicateur, jamais affiché en clair).
    pub has_token: bool,
}

impl ProjectItemRow {
    /// Renvoie vrai si l'URL peut être rendue en <a href> (sinon texte brut).
    pub fn git_repo_is_link(&self) -> bool {
        is_safe_repo_url(&self.git_repo)
    }
}

/// Page de gestion des projets (nom + dépôt git).
pub async fn projects_page(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Query(q): Query<ProjectsQuery>,
) -> Response {
    let conn = state.db.get().expect("pool db");
    let mut projects = vec![];
    if let Ok(mut st) = conn.prepare(
        "SELECT p.id, p.name, p.git_repo,
                (SELECT COUNT(*) FROM tasks t WHERE t.project_id = p.id),
                (EXISTS (SELECT 1 FROM project_tokens t
                         WHERE t.project_id = p.id AND t.user_id = ?1))
         FROM projects p ORDER BY p.name COLLATE NOCASE",
    ) {
        if let Ok(rows) = st.query_map([user.id], |r| {
            Ok(ProjectItemRow {
                id: r.get(0)?,
                name: r.get(1)?,
                git_repo: r.get(2)?,
                tasks: r.get(3)?,
                has_token: r.get(4)?,
            })
        }) {
            projects.extend(rows.filter_map(|r| r.ok()));
        }
    }
    auth::render(&ProjectsTemplate {
        user: Some(user),
        projects,
        error: q.error,
    })
}

#[derive(Deserialize)]
pub struct ProjectsQuery {
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
pub struct ProjectForm {
    name: String,
    #[serde(default)]
    git_repo: String,
    #[serde(default)]
    git_token: String,
}

/// Valide nom, dépôt et token d'un projet ; renvoie le message d'erreur (UI) sinon.
/// La longueur est comptée en caractères, comme le `maxlength` du formulaire.
fn validate_project(name: &str, repo: &str, token: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.chars().count() > 64 {
        return Err("Project name must be 1 to 64 characters.");
    }
    if !repo.is_empty() && !is_safe_repo_url(repo) {
        return Err("Git repo must start with http(s)://, ssh:// or git@.");
    }
    // Token : au plus 256 caractères ASCII imprimables (pas d'espace, pas de retour
    // à la ligne), sinon il serait ambigu en base / dans les en-têtes http.
    if !token.is_empty()
        && (token.chars().count() > 256 || token.chars().any(|c| !c.is_ascii_graphic()))
    {
        return Err("Git token must be 1 to 256 printable ASCII characters.");
    }
    Ok(())
}

/// Message d'erreur (UI) pour un échec d'écriture d'un projet.
fn project_write_error(e: &rusqlite::Error) -> &'static str {
    match e {
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            "A project with this name already exists."
        }
        _ => "Could not save the project.",
    }
}

/// Redirection vers la page des projets avec un message d'erreur
/// (encodé en pourcents pour la query string).
fn projects_error(msg: &str) -> Response {
    let mut q = String::new();
    for b in msg.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            q.push(b as char);
        } else {
            q.push_str(&format!("%{b:02X}"));
        }
    }
    Redirect::to(&format!("/projects?error={q}")).into_response()
}

/// Crée un projet (partagé par tous les utilisateurs ; la page reste
/// protégée par le middleware `require_auth`).
pub async fn create_project(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Form(f): Form<ProjectForm>,
) -> Response {
    let name = f.name.trim().to_string();
    let repo = f.git_repo.trim().to_string();
    let token = f.git_token.trim().to_string();
    if let Err(msg) = validate_project(&name, &repo, &token) {
        return projects_error(msg);
    }
    let conn = state.db.get().expect("pool db");
    match conn.execute(
        "INSERT INTO projects (name, git_repo) VALUES (?1, ?2)",
        rusqlite::params![name, repo],
    ) {
        Err(e) => return projects_error(project_write_error(&e)),
        Ok(_) => {
            // Token (si fourni) : stocké pour le couple (projet, utilisateur
            // courant) — chaque utilisateur a son propre token.
            if !token.is_empty() {
                let _ = conn.execute(
                    "INSERT OR REPLACE INTO project_tokens (project_id, user_id, token)
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![conn.last_insert_rowid(), user.id, token],
                );
            }
        }
    }
    Redirect::to("/projects").into_response()
}

/// Met à jour un projet (nom, dépôt git) et le token de l'appelant.
pub async fn update_project(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Form(f): Form<ProjectForm>,
) -> Response {
    let name = f.name.trim().to_string();
    let repo = f.git_repo.trim().to_string();
    let form_token = f.git_token.trim();
    if let Err(msg) = validate_project(&name, &repo, form_token) {
        return projects_error(msg);
    }
    let conn = state.db.get().expect("pool db");
    if let Err(e) = conn.execute(
        "UPDATE projects SET name = ?1, git_repo = ?2 WHERE id = ?3",
        rusqlite::params![name, repo, id],
    ) {
        return projects_error(project_write_error(&e));
    }
    // Token : le formulaire en contient un → on met à jour le token de
    // l'utilisateur courant pour ce projet ; vide → on conserve le token
    // déjà stocké (l'UI ne le ré-affiche jamais).
    if !form_token.is_empty() {
        let _ = conn.execute(
            "INSERT OR REPLACE INTO project_tokens (project_id, user_id, token)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![id, user.id, form_token.to_string()],
        );
    }
    Redirect::to("/projects").into_response()
}

/// Supprime un projet (ses tâches sont conservées, sans projet ; ses tokens
/// sont supprimés en cascade).
pub async fn delete_project(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    // Projet partagé : les tâches de TOUS les utilisateurs qui y référençaient
    // sont conservées, sans projet.
    if let Ok(conn) = state.db.get() {
        let _ = conn.execute(
            "UPDATE tasks SET project_id = NULL WHERE project_id = ?1",
            [id],
        );
        let _ = conn.execute("DELETE FROM projects WHERE id = ?1", [id]);
    }
    Redirect::to("/projects").into_response()
}

// ===== Diff de la branche d'une tâche vs main (git2) =======================

/// Une ligne d'un diff unifié.
pub struct DiffLine {
    /// "add" | "del" | "ctx" | "hdr".
    pub kind: &'static str,
    pub text: String,
}

/// Un fichier modifié, avec ses lignes de diff.
pub struct DiffFile {
    pub path: String,
    pub added: usize,
    pub deleted: usize,
    pub binary: bool,
    pub lines: Vec<DiffLine>,
}

#[derive(Template)]
#[template(path = "task_diff.html")]
pub struct TaskDiffTemplate<'a> {
    pub user: Option<CurrentUser>,
    pub task: &'a TaskRow,
    /// Branche de base réellement utilisée (main, master…).
    pub base: String,
    pub files: Vec<DiffFile>,
    pub error: Option<String>,
}

/// Trouve la première ref existante parmi `pairs` (nom affiché, ref) et renvoie
/// son commit.
fn find_commit_named<'a>(
    repo: &'a Repository,
    pairs: &[(&str, &str)],
) -> Option<(String, git2::Commit<'a>)> {
    for (name, refname) in pairs {
        if let Ok(r) = repo.find_reference(refname) {
            if let Ok(c) = r.peel_to_commit() {
                return Some((name.to_string(), c));
            }
        }
    }
    None
}

/// Clone le dépôt dans un répertoire temporaire et calcule le diff
/// (arbres de commits) `branch` vs la branche de base (main, sinon master).
/// `token` : token optionnel du projet, utilisé en authentification http(s)
/// (dépôts privés GitHub/GitLab/Gitea). Renvoie (nom de la base, fichiers de la diff).
fn compute_branch_diff(
    repo_url: &str,
    branch: &str,
    token: &str,
    workdir: &std::path::Path,
) -> Result<(String, Vec<DiffFile>), String> {
    // Clone manuel (init + remote + fetch) pour pouvoir injecter un callback
    // d'authentification avant le téléchargement.
    let repo =
        Repository::init(workdir).map_err(|e| format!("Could not initialize the clone: {e}"))?;
    let mut remote = repo
        .remote("origin", repo_url)
        .map_err(|e| format!("Could not add the remote {repo_url}: {e}"))?;
    let mut opts = FetchOptions::new();
    let is_http = {
        let l = repo_url.to_ascii_lowercase();
        l.starts_with("http://") || l.starts_with("https://")
    };
    if is_http && !token.trim().is_empty() {
        let tok = token.trim().to_string();
        let mut cbs = RemoteCallbacks::new();
        // N'importe quel pseudo fonctionne avec les tokens GitHub / GitLab / Gitea ;
        // le token sert de mot de passe (basic auth https).
        cbs.credentials(move |_url, _username, _allowed_types| {
            Cred::userpass_plaintext("x-access-token", &tok)
        });
        opts.remote_callbacks(cbs);
    }
    remote
        .fetch::<&str>(&[], Some(&mut opts), None)
        .map_err(|e| format!("Could not clone {repo_url}: {e}"))?;
    let (base, base_commit) = find_commit_named(
        &repo,
        &[
            ("main", "refs/remotes/origin/main"),
            ("main", "refs/heads/main"),
            ("master", "refs/remotes/origin/master"),
            ("master", "refs/heads/master"),
        ],
    )
    .ok_or_else(|| "Base branch not found (main / master).".to_string())?;
    let (_, head_commit) = find_commit_named(
        &repo,
        &[
            (branch, &format!("refs/heads/{branch}")),
            (branch, &format!("refs/remotes/origin/{branch}")),
        ],
    )
    .ok_or_else(|| format!("Branch '{branch}' not found in the repository."))?;

    let base_tree = base_commit
        .tree()
        .map_err(|e| format!("Could not read the base tree: {e}"))?;
    let head_tree = head_commit
        .tree()
        .map_err(|e| format!("Could not read the branch tree: {e}"))?;
    let diff = repo
        .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), None)
        .map_err(|e| format!("Could not compute the diff: {e}"))?;

    let mut files = vec![];
    for (idx, delta) in diff.deltas().enumerate() {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "?".to_string());
        // Binaire, ou patch texte indisponible : on n'affiche pas le contenu.
        if delta.flags().contains(DiffFlags::BINARY)
            || !matches!(Patch::from_diff(&diff, idx), Ok(Some(_)))
        {
            files.push(DiffFile {
                path,
                added: 0,
                deleted: 0,
                binary: true,
                lines: vec![],
            });
            continue;
        }
        let patch = Patch::from_diff(&diff, idx).unwrap().unwrap();
        let mut lines = vec![];
        let (mut added, mut deleted) = (0usize, 0usize);
        for h in 0..patch.num_hunks() {
            let (_hunk, num_lines) = patch
                .hunk(h)
                .map_err(|e| format!("Could not read a diff hunk: {e}"))?;
            for l in 0..num_lines {
                let line = patch
                    .line_in_hunk(h, l)
                    .map_err(|e| format!("Could not read a diff line: {e}"))?;
                let (kind, is_add, is_del) = match line.origin() {
                    '+' => ("add", true, false),
                    '-' => ("del", false, true),
                    ' ' | '=' | '>' | '<' => ("ctx", false, false),
                    _ => ("hdr", false, false),
                };
                if is_add {
                    added += 1;
                }
                if is_del {
                    deleted += 1;
                }
                lines.push(DiffLine {
                    kind,
                    text: String::from_utf8_lossy(line.content()).trim_end().to_string(),
                });
            }
        }
        files.push(DiffFile {
            path,
            added,
            deleted,
            binary: false,
            lines,
        });
    }
    Ok((base, files))
}

/// Page du diff entre la branche de la tâche et main : clone temporaire du
/// dépôt git du projet (git2), rendu des différentiels par fichier.
pub async fn task_diff_page(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Response {
    // Connexion rendue au pool avant le clone (qui peut durer).
    let task = match fetch_one_task(&state.db.get().expect("pool db"), user.id, id) {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (axum::http::StatusCode::NOT_FOUND, "Task not found.").into_response()
        }
        Err(_) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load the task.",
            )
                .into_response()
        }
    };
    let error_response = |status: axum::http::StatusCode, msg: &str| -> Response {
        let mut resp = auth::render(&TaskDiffTemplate {
            user: Some(user.clone()),
            task: &task,
            base: String::new(),
            files: vec![],
            error: Some(msg.to_string()),
        });
        *resp.status_mut() = status;
        resp
    };

    if task.branch.trim().is_empty() {
        return error_response(
            axum::http::StatusCode::NOT_FOUND,
            "No branch associated with this task. Edit the task to set a branch.",
        );
    }
    if !is_safe_branch(&task.branch) {
        return error_response(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid branch name.",
        );
    }
    let project = match task.project.as_ref() {
        Some(p) => p,
        None => {
            return error_response(
                axum::http::StatusCode::NOT_FOUND,
                "This task has no project — the diff needs a git repository.",
            )
        }
    };
    if project.git_repo.trim().is_empty() || !is_safe_repo_url(&project.git_repo) {
        return error_response(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "The project has no valid git repository.",
        );
    }

    let workdir = std::env::temp_dir().join(format!(
        "tikedenn-git-{id}-{}",
        rand::thread_rng().gen::<u32>()
    ));
    // Clone réseau + diff : bloquant, donc hors des threads du runtime async.
    let (repo_url, branch, token) = (
        project.git_repo.trim().to_string(),
        task.branch.trim().to_string(),
        project.token.clone(),
    );
    let res = tokio::task::spawn_blocking(move || {
        let res = compute_branch_diff(&repo_url, &branch, &token, &workdir);
        let _ = std::fs::remove_dir_all(&workdir); // nettoyage best effort
        res
    })
    .await
    .unwrap_or_else(|e| Err(format!("Diff task failed: {e}")));

    match res {
        Ok((base, files)) => auth::render(&TaskDiffTemplate {
            user: Some(user),
            task: &task,
            base,
            files,
            error: None,
        }),
        Err(e) => error_response(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}
