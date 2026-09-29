//! Actions humaines sur le journal et la revue d'une tâche : commentaire,
//! approbation (review → deploy, le worker publie ensuite), demande de
//! modifications (review → todo, un agent reprend la tâche avec le retour),
//! libération d'une prise en charge.

use crate::auth::CurrentUser;
use crate::board::{redirect_edit, wf_redirect};
use crate::web::AppResult;
use crate::workflow::{self, Actor};
use crate::AppState;
use axum::{
    extract::{Extension, Path, State},
    Form,
};
use rusqlite::OptionalExtension;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct CommentForm {
    #[serde(default)]
    body: String,
    #[serde(default)]
    filter: i64,
}

/// Longueur maximale d'une entrée du journal (caractères).
pub const MAX_LOG_BODY: usize = 20_000;

fn too_long(body: &str) -> bool {
    body.chars().count() > MAX_LOG_BODY
}

/// Ajoute un commentaire au journal de la tâche.
pub async fn add_comment(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Form(f): Form<CommentForm>,
) -> AppResult {
    let body = f.body.trim();
    if body.is_empty() || too_long(body) {
        return Ok(redirect_edit(id, f.filter, Some("A comment must be 1 to 20000 characters.")));
    }
    let conn = state.conn()?;
    if crate::files::task_exists(&conn, id)? {
        workflow::log(&conn, id, &Actor::Human(user), "comment", body)?;
    }
    Ok(redirect_edit(id, f.filter, None))
}

/// État courant d'une tâche (None si elle n'existe pas).
fn status_of(conn: &rusqlite::Connection, id: i64) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT status FROM tasks WHERE id = ?1", [id], |r| r.get(0))
        .optional()
}

/// Approuve la revue : la tâche passe en `deploy` (commentaire optionnel).
pub async fn approve(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Form(f): Form<CommentForm>,
) -> AppResult {
    let conn = state.conn()?;
    if status_of(&conn, id)?.as_deref() != Some("review") {
        return Ok(redirect_edit(id, f.filter, Some("Only a task in review can be approved.")));
    }
    let body = f.body.trim();
    if too_long(body) {
        return Ok(redirect_edit(id, f.filter, Some("The comment is too long.")));
    }
    let actor = Actor::Human(user);
    let text = if body.is_empty() { "Approved".to_string() } else { format!("Approved: {body}") };
    workflow::log(&conn, id, &actor, "review", &text)?;
    if let Err(e) = workflow::move_task(&conn, &actor, id, "deploy", None) {
        return wf_redirect(id, f.filter, e);
    }
    Ok(redirect_edit(id, f.filter, None))
}

/// Demande des modifications : commentaire obligatoire, la tâche repart en
/// `todo` (un agent la reprendra, avec ce retour dans le journal).
pub async fn request_changes(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Form(f): Form<CommentForm>,
) -> AppResult {
    let conn = state.conn()?;
    if status_of(&conn, id)?.as_deref() != Some("review") {
        return Ok(redirect_edit(id, f.filter, Some("Only a task in review can be sent back.")));
    }
    let body = f.body.trim();
    if body.is_empty() || too_long(body) {
        return Ok(redirect_edit(id, f.filter, Some("Explain which changes are needed.")));
    }
    let actor = Actor::Human(user);
    workflow::log(&conn, id, &actor, "review", &format!("Changes requested: {body}"))?;
    if let Err(e) = workflow::move_task(&conn, &actor, id, "todo", None) {
        return wf_redirect(id, f.filter, e);
    }
    Ok(redirect_edit(id, f.filter, None))
}

/// Libère la prise en charge d'une tâche (agent bloqué ou disparu).
pub async fn release(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Form(f): Form<CommentForm>,
) -> AppResult {
    let conn = state.conn()?;
    if let Err(e) = workflow::release(&conn, &Actor::Human(user), id, None, None) {
        return wf_redirect(id, f.filter, e);
    }
    Ok(redirect_edit(id, f.filter, None))
}
