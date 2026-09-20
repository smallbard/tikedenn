//! Tableau kanban : projets (nom + dépôt git), tâches dans les états
//! draft / todo / progress / done, description longue et fichiers joints.

use crate::auth::{self, CurrentUser};
use crate::AppState;
use askama::Template;
use axum::{
    body::Body,
    extract::{multipart::Multipart, Path, Query, State},
    http::header,
    response::{IntoResponse, Redirect, Response},
    Form,
};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;

/// Taille maximale d'une pièce jointe (10 Mo).
const MAX_FILE_SIZE: usize = 10 * 1024 * 1024;

/// États du tableau, dans l'ordre du gauche vers la droite.
const STATUSES: [(&str, &str); 4] = [
    ("draft", "Draft"),
    ("todo", "To do"),
    ("progress", "In progress"),
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

/// Extrait l'utilisateur connecté (la garde require_auth garantit sa présence).
fn user(state: &AppState, jar: &SignedCookieJar) -> CurrentUser {
    auth::current_user(state, jar)
        .unwrap_or_else(|| panic!("require_auth a laissé passer une requête non authentifiée"))
}

#[derive(Clone)]
pub struct ProjectRow {
    pub id: i64,
    pub name: String,
    pub git_repo: String,
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

fn fetch_projects(conn: &rusqlite::Connection, user_id: i64) -> Vec<ProjectRow> {
    let mut out = vec![];
    if let Ok(mut st) = conn
        .prepare("SELECT id, name, git_repo FROM projects WHERE user_id = ?1 ORDER BY name COLLATE NOCASE")
    {
        if let Ok(rows) = st.query_map([user_id], |r| {
            Ok(ProjectRow {
                id: r.get(0)?,
                name: r.get(1)?,
                git_repo: r.get(2)?,
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

fn fetch_tasks(conn: &rusqlite::Connection, user_id: i64, project_id: Option<i64>) -> Vec<TaskRow> {
    let mut out = vec![];
    // ?2 IS NULL → pas de filtre projet (tous les projets).
    let sql = "SELECT t.id, t.project_id, p.id, p.name, p.git_repo, t.status, t.title,
                      t.description, t.created_at
               FROM tasks t LEFT JOIN projects p ON p.id = t.project_id
               WHERE t.user_id = ?1 AND (?2 IS NULL OR t.project_id = ?2)";
    let mut st = match conn.prepare(sql) {
        Ok(s) => s,
        Err(_) => return out,
    };
    let rows = match st.query_map(rusqlite::params![user_id, project_id], |r| {
        // Colonnes : 0=t.id, 1=t.project_id, 2=p.id, 3=p.name, 4=p.git_repo,
        //           5=t.status, 6=t.title, 7=t.description, 8=t.created_at
        Ok(TaskRow {
            id: r.get(0)?,
            project_id: r.get::<_, Option<i64>>(1)?,
            project: if r.get::<_, Option<i64>>(2)?.is_some() {
                Some(ProjectRow {
                    id: r.get(2)?,
                    name: r.get(3)?,
                    git_repo: r.get(4)?,
                })
            } else {
                None
            },
            status: r.get(5)?,
            title: r.get(6)?,
            description: r.get(7)?,
            description_preview: String::new(),
            created_at: r.get(8)?,
            files: vec![],
        })
    }) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for row in rows {
        let mut row = match row {
            Ok(r) => r,
            Err(_) => continue,
        };
        // Aperçu : première ligne non vide de la description, tronquée.
        let first = row
            .description
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty());
        if let Some(l) = first {
            row.description_preview = if l.len() > 140 {
                let mut e = l.char_indices().take(140).last().map(|(i, _)| i).unwrap_or(0);
                while !l.is_char_boundary(e) {
                    e -= 1;
                }
                format!("{}…", &l[..e])
            } else {
                l.to_string()
            };
        }
        row.files = fetch_files(conn, row.id);
        out.push(row);
    }
    out
}

/// Construit les 4 colonnes du tableau (utilisé par la page et les
/// réponses htmx). `selected` : id du projet de filtre, 0 = tous.
fn build_board(
    state: &AppState,
    jar: &SignedCookieJar,
    selected: i64,
) -> (Vec<ProjectRow>, Vec<Column>) {
    let user = user(state, jar);
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
                task: t.clone(),
                projects: projects.clone(),
                statuses: status_choices(),
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

/// Rendu du fragment de tableau (réponse htmx des déplacements/créations).
fn render_board_fragment(state: &AppState, jar: &SignedCookieJar, selected: i64) -> Response {
    let (projects, columns) = build_board(state, jar, selected);
    auth::render(&BoardFragment {
        columns,
        user: Some(user(state, jar)),
        _projects: projects,
    })
}

#[derive(Template)]
#[template(path = "board_frag.html")]
pub struct BoardFragment {
    pub user: Option<CurrentUser>,
    pub columns: Vec<Column>,
    // Non utilisé par le fragment, mais partagé avec la page (contexte askama).
    pub _projects: Vec<ProjectRow>,
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
pub struct TaskCardTemplate {
    pub task: TaskRow,
    pub projects: Vec<ProjectRow>,
    pub statuses: Vec<StatusChoice>,
    /// Filtre projet actif, à préserver lors des déplacements.
    pub filter: i64,
}

/// Tableau de bord : le kanban, filtrable par projet.
pub async fn dashboard(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Query(q): Query<BoardQuery>,
) -> Response {
    let selected = q.project.unwrap_or(0);
    let (projects, columns) = build_board(&state, &jar, selected);
    auth::render(&DashboardTemplate {
        user: Some(user(&state, &jar)),
        projects,
        selected_project: selected,
        columns,
    })
}

#[derive(Deserialize)]
pub struct BoardQuery {
    project: Option<i64>,
}

#[derive(Deserialize)]
pub struct TaskForm {
    title: String,
    #[serde(default)]
    description: String,
    /// Id du projet (0 = aucun). Doublé : aussi utilisé comme filtre à préserver.
    project: i64,
}

/// Création d'une tâche : toujours en draft. Renvoie le fragment de tableau.
pub async fn create_task(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(f): Form<TaskForm>,
) -> Response {
    let user = user(&state, &jar);
    let title = f.title.trim().to_string();
    if title.is_empty() {
        return (
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "Task title is required.",
        )
            .into_response();
    }
    let project_id: Option<i64> = if f.project > 0 {
        let conn = state.db.get().expect("pool db");
        match conn.query_row(
            "SELECT id FROM projects WHERE id = ?1 AND user_id = ?2",
            rusqlite::params![f.project, user.id],
            |r| r.get(0),
        ) {
            Ok(id) => Some(id),
            Err(_) => {
                return (
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                    "Unknown project.",
                )
                    .into_response()
            }
        }
    } else {
        None
    };
    let conn = state.db.get().expect("pool db");
    if conn
        .execute(
            "INSERT INTO tasks (user_id, project_id, status, title, description)
             VALUES (?1, ?2, 'draft', ?3, ?4)",
            rusqlite::params![user.id, project_id, title, f.description.trim()],
        )
        .is_err()
    {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Could not create the task.",
        )
            .into_response();
    }
    render_board_fragment(&state, &jar, f.project)
}

/// Déplace une tâche vers un autre état. Renvoie le fragment de tableau.
pub async fn move_task(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    Form(f): Form<MoveForm>,
) -> Response {
    let user = user(&state, &jar);
    if !STATUSES.iter().any(|(v, _)| *v == f.status) {
        return (
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "Unknown task state.",
        )
            .into_response();
    }
    let conn = state.db.get().expect("pool db");
    let _ = conn.execute(
        "UPDATE tasks SET status = ?1, updated_at = datetime('now') WHERE id = ?2 AND user_id = ?3",
        rusqlite::params![f.status, id, user.id],
    );
    render_board_fragment(&state, &jar, f.filter)
}

#[derive(Deserialize)]
pub struct MoveForm {
    status: String,
    #[serde(default)]
    filter: i64,
}

/// Met à jour une tâche (titre, description, projet). Renvoie la carte.
pub async fn update_task(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    Form(f): Form<TaskForm>,
) -> Response {
    let user = user(&state, &jar);
    let title = f.title.trim().to_string();
    if title.is_empty() {
        return (
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "Task title is required.",
        )
            .into_response();
    }
    let project_id: Option<i64> = if f.project > 0 {
        let conn = state.db.get().expect("pool db");
        match conn.query_row(
            "SELECT id FROM projects WHERE id = ?1 AND user_id = ?2",
            rusqlite::params![f.project, user.id],
            |r| r.get(0),
        ) {
            Ok(p) => Some(p),
            Err(_) => return render_task_card(&state, &jar, id), // garde le projet d'origine
        }
    } else {
        None
    };
    let conn = state.db.get().expect("pool db");
    let _ = conn.execute(
        "UPDATE tasks SET title = ?1, description = ?2, project_id = ?3,
         updated_at = datetime('now')
         WHERE id = ?4 AND user_id = ?5",
        rusqlite::params![title, f.description.trim(), project_id, id, user.id],
    );
    render_task_card(&state, &jar, id)
}

/// Rendu d'une carte individuelle (réponse htmx après édition).
fn render_task_card(state: &AppState, jar: &SignedCookieJar, id: i64) -> Response {
    let user = user(state, jar);
    let conn = state.db.get().expect("pool db");
    let projects = fetch_projects(&conn, user.id);
    let mut resp = (
        axum::http::StatusCode::NOT_FOUND,
        "Task not found.",
    )
        .into_response();
    if let Some(t) = fetch_tasks(&conn, user.id, None).into_iter().find(|t| t.id == id) {
        let card = TaskCardTemplate {
            task: t,
            projects,
            statuses: status_choices(),
            filter: 0,
        };
        match card.render() {
            Ok(html) => resp = html.into_response(),
            Err(_) => {
                resp = (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "Template error.",
                )
                .into_response()
            }
        }
    }
    resp
}

/// Supprime une tâche (fichiers en cascade). Renvoie du vide (hx-swap=outerHTML).
pub async fn delete_task(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> Response {
    let user = user(&state, &jar);
    if let Ok(conn) = state.db.get() {
        let _ = conn.execute(
            "DELETE FROM tasks WHERE id = ?1 AND user_id = ?2",
            rusqlite::params![id, user.id],
        );
    }
    "".into_response()
}

/// Téléverse un fichier joint sur une tâche (multipart).
pub async fn upload_file(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(task_id): Path<i64>,
    mut multipart: Multipart,
) -> Response {
    let user = user(&state, &jar);
    let conn = state.db.get().expect("pool db");
    // La tâche doit exister et appartenir à l'utilisateur.
    let task_ok = conn
        .query_row(
            "SELECT id FROM tasks WHERE id = ?1 AND user_id = ?2",
            rusqlite::params![task_id, user.id],
            |r| r.get::<_, i64>(0),
        )
        .is_ok();
    if !task_ok {
        return Redirect::to("/dashboard").into_response();
    }
    let mut filter = 0i64;
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
        // Un seul fichier par formulaire.
        break;
    }
    if filter > 0 {
        Redirect::to(&format!("/dashboard?project={filter}")).into_response()
    } else {
        Redirect::to("/dashboard").into_response()
    }
}

/// Téléchargement d'un fichier joint.
pub async fn download_file(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> Response {
    let user = user(&state, &jar);
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
    let safe_name: String = name.chars().filter(|c| *c != '"' && *c != '\n' && *c != '\r').collect();
    let mut resp = Body::from(data).into_response();
    if let Ok(hv) = mime.parse() {
        resp.headers_mut().insert(header::CONTENT_TYPE, hv);
    }
    resp.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{safe_name}\"").parse().unwrap(),
    );
    resp
}

/// Supprime un fichier joint. Renvoie du vide (hx-swap=outerHTML).
pub async fn delete_file(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> Response {
    let user = user(&state, &jar);
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
    jar: SignedCookieJar,
    Query(q): Query<ProjectsQuery>,
) -> Response {
    let user = user(&state, &jar);
    let conn = state.db.get().expect("pool db");
    let mut projects = vec![];
    if let Ok(mut st) = conn.prepare(
        "SELECT p.id, p.name, p.git_repo,
                (SELECT COUNT(*) FROM tasks t WHERE t.project_id = p.id)
         FROM projects p WHERE p.user_id = ?1 ORDER BY p.name COLLATE NOCASE",
    ) {
        if let Ok(rows) = st.query_map([user.id], |r| {
            Ok(ProjectItemRow {
                id: r.get(0)?,
                name: r.get(1)?,
                git_repo: r.get(2)?,
                tasks: r.get(3)?,
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
}

/// Crée un projet.
pub async fn create_project(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(f): Form<ProjectForm>,
) -> Response {
    let user = user(&state, &jar);
    let name = f.name.trim().to_string();
    let repo = f.git_repo.trim().to_string();
    if name.is_empty() || name.len() > 64 {
        return Redirect::to("/projects?error=Project%20name%20must%20be%201%20to%2064%20characters.").into_response();
    }
    if !repo.is_empty() && !is_safe_repo_url(&repo) {
        return Redirect::to("/projects?error=Git%20repo%20must%20start%20with%20http%28s%29%3A%2F%2F%2C%20ssh%3A%2F%2F%20or%20git%40.").into_response();
    }
    let conn = state.db.get().expect("pool db");
    let _ = conn.execute(
        "INSERT INTO projects (user_id, name, git_repo) VALUES (?1, ?2, ?3)",
        rusqlite::params![user.id, name, repo],
    );
    Redirect::to("/projects").into_response()
}

/// Met à jour un projet (nom, dépôt git).
pub async fn update_project(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    Form(f): Form<ProjectForm>,
) -> Response {
    let user = user(&state, &jar);
    let name = f.name.trim().to_string();
    let repo = f.git_repo.trim().to_string();
    if !name.is_empty() && name.len() <= 64 {
        if repo.is_empty() || is_safe_repo_url(&repo) {
            let conn = state.db.get().expect("pool db");
            let _ = conn.execute(
                "UPDATE projects SET name = ?1, git_repo = ?2 WHERE id = ?3 AND user_id = ?4",
                rusqlite::params![name, repo, id, user.id],
            );
        }
    }
    Redirect::to("/projects").into_response()
}

/// Supprime un projet (ses tâches sont conservées, sans projet).
pub async fn delete_project(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> Response {
    let user = user(&state, &jar);
    if let Ok(conn) = state.db.get() {
        let _ = conn.execute(
            "UPDATE tasks SET project_id = NULL WHERE project_id = ?1 AND user_id = ?2",
            rusqlite::params![id, user.id],
        );
        let _ = conn.execute(
            "DELETE FROM projects WHERE id = ?1 AND user_id = ?2",
            rusqlite::params![id, user.id],
        );
    }
    Redirect::to("/projects").into_response()
}
