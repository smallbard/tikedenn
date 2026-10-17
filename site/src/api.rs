//! API JSON `/api/v1` des agents (authentification par token, voir
//! `tokens`). Les fonctions `op_*` portent la logique, partagée avec le
//! serveur MCP (`mcp`) ; les handlers ne font que (dé)sérialiser.

use crate::board::{self, find_task, insert_task, TaskFilter, ValidTask};
use crate::files::{self, MAX_FILE_SIZE};
use crate::projects::{fetch_projects, find_project};
use crate::review::MAX_LOG_BODY;
use crate::web::AppError;
use crate::workflow::{self, Actor, WfError};
use crate::AppState;
use axum::{
    body::Bytes,
    extract::{Extension, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::Connection;
use serde::Deserialize;
use tikedenn_common::api::{
    ApiErrorBody, Claim, ClaimNext, CreateTask, Me, MoveTask, NewComment, PatchTask, Project, Release,
    TaskDetail, TaskSummary,
};
use tikedenn_common::{is_valid_status, normalize_repo_url, DEFAULT_PRIORITY};

/// Erreur de l'API : statut http + message, rendue en JSON `{"error": …}`.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        ApiError { status, message: message.into() }
    }

    fn invalid(message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, message)
    }

    fn not_found(what: &str) -> Self {
        ApiError::new(StatusCode::NOT_FOUND, format!("{what} not found."))
    }

    fn internal(e: impl std::fmt::Display) -> Self {
        eprintln!("[tikedenn] api internal error: {e}");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Internal error.")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(ApiErrorBody { error: self.message })).into_response()
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        ApiError::internal(format!("sqlite: {e}"))
    }
}

impl From<AppError> for ApiError {
    fn from(e: AppError) -> Self {
        match e {
            AppError::Http(status, msg) => ApiError::new(status, msg),
            AppError::Internal(e) => ApiError::internal(e),
        }
    }
}

impl From<WfError> for ApiError {
    fn from(e: WfError) -> Self {
        match e {
            WfError::Db(e) => e.into(),
            e => ApiError::new(e.status(), e.message()),
        }
    }
}

// ---------------------------------------------------------------------------
// Opérations (API et MCP)
// ---------------------------------------------------------------------------

/// Projets ; `repo` : ne garder que celui dont le dépôt correspond (URL
/// https, ssh ou scp, comparées sous forme normalisée).
pub fn op_projects(conn: &Connection, actor: &Actor, repo: Option<&str>) -> ApiResult<Vec<Project>> {
    let wanted = repo.map(normalize_repo_url).filter(|r| !r.is_empty());
    Ok(fetch_projects(conn, actor.user().id)?
        .iter()
        .filter(|p| match &wanted {
            Some(w) => !p.git_repo.is_empty() && &normalize_repo_url(&p.git_repo) == w,
            None => true,
        })
        .map(|p| p.to_api())
        .collect())
}

/// Id du projet désigné par son id ou son nom.
fn project_id(conn: &Connection, actor: &Actor, key: &str) -> ApiResult<i64> {
    let projects = fetch_projects(conn, actor.user().id)?;
    find_project(&projects, key)
        .map(|p| p.id)
        .ok_or_else(|| ApiError::not_found(&format!("Project '{key}'")))
}

pub fn op_list_tasks(
    conn: &Connection,
    actor: &Actor,
    status: Option<&str>,
    project: Option<&str>,
) -> ApiResult<Vec<TaskSummary>> {
    let status = status.filter(|s| !s.is_empty());
    if let Some(s) = status {
        if !is_valid_status(s) {
            return Err(ApiError::invalid(format!("Unknown status '{s}'.")));
        }
    }
    let project_id = match project.filter(|p| !p.is_empty()) {
        Some(p) => Some(project_id(conn, actor, p)?),
        None => None,
    };
    let tasks = board::fetch_tasks(conn, actor.user().id, &TaskFilter { project_id, status })?;
    Ok(tasks.iter().map(|t| t.to_summary()).collect())
}

pub fn op_get_task(conn: &Connection, actor: &Actor, id: i64) -> ApiResult<TaskDetail> {
    find_task(conn, actor.user().id, id)?
        .map(|t| t.to_detail())
        .ok_or_else(|| ApiError::not_found("Task"))
}

/// Crée une tâche en draft (suite de travail proposée par un agent : c'est
/// un humain qui la passera en todo).
pub fn op_create_task(conn: &Connection, actor: &Actor, c: &CreateTask) -> ApiResult<TaskDetail> {
    let project = match c.project.as_deref().filter(|p| !p.is_empty()) {
        Some(p) => Some(project_id(conn, actor, p)?),
        None => None,
    };
    let priority = c.priority.unwrap_or(DEFAULT_PRIORITY);
    let (title, project_id, branch) =
        board::validate_task(conn, 0, &c.title, project, priority, &c.branch, &[])?.map_err(ApiError::invalid)?;
    let id = insert_task(
        conn,
        actor,
        &ValidTask {
            title,
            description: c.description.trim().to_string(),
            acceptance: c.acceptance.trim().to_string(),
            project_id,
            priority,
            branch,
            depends_on: vec![],
        },
        "draft",
    )?;
    op_get_task(conn, actor, id)
}

/// Seule la branche git est modifiable par un agent (la spécification
/// reste du ressort des humains).
pub fn op_patch_task(conn: &Connection, actor: &Actor, id: i64, p: &PatchTask) -> ApiResult<TaskDetail> {
    let task = op_get_task(conn, actor, id)?;
    if task.summary.claim_active && task.summary.claimed_by.as_deref() != Some(&actor.claimer()) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("The task is claimed by {}.", task.summary.claimed_by.unwrap_or_default()),
        ));
    }
    if let Some(branch) = &p.branch {
        let branch = branch.trim();
        if !branch.is_empty() && !board::is_safe_branch(branch) {
            return Err(ApiError::invalid("Invalid branch name."));
        }
        if branch != task.summary.branch {
            conn.execute(
                "UPDATE tasks SET branch = ?1, updated_at = datetime('now') WHERE id = ?2",
                rusqlite::params![branch, id],
            )?;
            workflow::log(conn, id, actor, "comment", &format!("Branch set to `{branch}`"))?;
        }
    }
    op_get_task(conn, actor, id)
}

pub fn op_claim(conn: &Connection, actor: &Actor, id: i64, c: &Claim) -> ApiResult<TaskDetail> {
    workflow::claim(conn, actor, id, c.ttl_minutes)?;
    op_get_task(conn, actor, id)
}

pub fn op_claim_next(conn: &Connection, actor: &Actor, c: &ClaimNext) -> ApiResult<Option<TaskDetail>> {
    let mut ids = vec![];
    for p in &c.projects {
        ids.push(project_id(conn, actor, p)?);
    }
    let status = c.status.as_deref().unwrap_or("todo");
    match workflow::claim_next(conn, actor, &ids, status, c.ttl_minutes)? {
        Some(id) => Ok(Some(op_get_task(conn, actor, id)?)),
        None => Ok(None),
    }
}

fn check_note(note: Option<&str>) -> ApiResult<()> {
    match note {
        Some(n) if n.chars().count() > MAX_LOG_BODY => {
            Err(ApiError::invalid(format!("Note too long ({MAX_LOG_BODY} characters max).")))
        }
        _ => Ok(()),
    }
}

pub fn op_release(conn: &Connection, actor: &Actor, id: i64, r: &Release) -> ApiResult<TaskDetail> {
    check_note(r.note.as_deref())?;
    workflow::release(conn, actor, id, r.status.as_deref(), r.note.as_deref())?;
    op_get_task(conn, actor, id)
}

pub fn op_move(conn: &Connection, actor: &Actor, id: i64, m: &MoveTask) -> ApiResult<TaskDetail> {
    check_note(m.note.as_deref())?;
    workflow::move_task(conn, actor, id, &m.status, m.note.as_deref())?;
    op_get_task(conn, actor, id)
}

pub fn op_comment(conn: &Connection, actor: &Actor, id: i64, c: &NewComment) -> ApiResult<()> {
    let body = c.body.trim();
    if body.is_empty() {
        return Err(ApiError::invalid("Empty comment."));
    }
    check_note(Some(body))?;
    let kind = c.kind.as_deref().unwrap_or("comment");
    if !matches!(kind, "comment" | "report" | "deploy") {
        return Err(ApiError::invalid("Comment kind must be comment, report or deploy."));
    }
    if !files::task_exists(conn, id)? {
        return Err(ApiError::not_found("Task"));
    }
    workflow::log(conn, id, actor, kind, body)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers http
// ---------------------------------------------------------------------------

pub async fn me(
    Extension(actor): Extension<Actor>,
    Extension(scope): Extension<crate::tokens::TokenScope>,
) -> Json<Me> {
    Json(Me {
        username: actor.user().username.clone(),
        token: actor.agent_name().to_string(),
        scope: if scope.read_only { "read" } else { "write" }.into(),
    })
}

#[derive(Deserialize)]
pub struct ProjectsQuery {
    repo: Option<String>,
}

pub async fn projects(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<ProjectsQuery>,
) -> ApiResult<Json<Vec<Project>>> {
    Ok(Json(op_projects(&*state.conn()?, &actor, q.repo.as_deref())?))
}

#[derive(Deserialize)]
pub struct TasksQuery {
    status: Option<String>,
    project: Option<String>,
}

pub async fn list_tasks(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<TasksQuery>,
) -> ApiResult<Json<Vec<TaskSummary>>> {
    Ok(Json(op_list_tasks(&*state.conn()?, &actor, q.status.as_deref(), q.project.as_deref())?))
}

pub async fn create_task(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(c): Json<CreateTask>,
) -> ApiResult<(StatusCode, Json<TaskDetail>)> {
    Ok((StatusCode::CREATED, Json(op_create_task(&*state.conn()?, &actor, &c)?)))
}

pub async fn get_task(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> ApiResult<Json<TaskDetail>> {
    Ok(Json(op_get_task(&*state.conn()?, &actor, id)?))
}

pub async fn patch_task(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(p): Json<PatchTask>,
) -> ApiResult<Json<TaskDetail>> {
    Ok(Json(op_patch_task(&*state.conn()?, &actor, id, &p)?))
}

pub async fn claim(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    body: Option<Json<Claim>>,
) -> ApiResult<Json<TaskDetail>> {
    let c = body.map(|Json(c)| c).unwrap_or_default();
    Ok(Json(op_claim(&*state.conn()?, &actor, id, &c)?))
}

/// Prochaine tâche disponible, prise en charge ; 204 s'il n'y en a pas.
pub async fn claim_next(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    body: Option<Json<ClaimNext>>,
) -> ApiResult<Response> {
    let c = body.map(|Json(c)| c).unwrap_or_default();
    Ok(match op_claim_next(&*state.conn()?, &actor, &c)? {
        Some(task) => Json(task).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

pub async fn release(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    body: Option<Json<Release>>,
) -> ApiResult<Json<TaskDetail>> {
    let r = body.map(|Json(r)| r).unwrap_or_default();
    Ok(Json(op_release(&*state.conn()?, &actor, id, &r)?))
}

pub async fn move_task(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(m): Json<MoveTask>,
) -> ApiResult<Json<TaskDetail>> {
    Ok(Json(op_move(&*state.conn()?, &actor, id, &m)?))
}

pub async fn add_comment(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(c): Json<NewComment>,
) -> ApiResult<StatusCode> {
    op_comment(&*state.conn()?, &actor, id, &c)?;
    Ok(StatusCode::CREATED)
}

pub async fn download_file(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult<Response> {
    files::file_response(&*state.conn()?, id)?.ok_or_else(|| ApiError::not_found("File"))
}

/// Joint un fichier (corps brut de la requête, nom dans le chemin).
pub async fn upload_file(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((id, name)): Path<(i64, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<TaskDetail>)> {
    let name = name.trim();
    if name.is_empty() || name.len() > 255 || name.contains(['/', '\\']) {
        return Err(ApiError::invalid("Invalid file name."));
    }
    if body.len() > MAX_FILE_SIZE {
        return Err(ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "File too large (10 MB maximum)."));
    }
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("application/octet-stream");
    let conn = state.conn()?;
    if !files::task_exists(&conn, id)? {
        return Err(ApiError::not_found("Task"));
    }
    files::insert_file(&conn, id, name, mime, &body)?;
    workflow::log(&conn, id, &actor, "comment", &format!("Attached file `{name}`"))?;
    Ok((StatusCode::CREATED, Json(op_get_task(&conn, &actor, id)?)))
}

/// Route inconnue sous /api : 404 en JSON.
pub async fn not_found() -> ApiError {
    ApiError::not_found("API route")
}
