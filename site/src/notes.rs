use crate::auth::{self, CurrentUser};
use crate::AppState;
use askama::Template;
use axum::{
    extract::{Path, State},
    response::{IntoResponse, Response},
    Form,
};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;

/// Extrait l'utilisateur connecté (la garde require_auth garantit sa présence).
fn user(state: &AppState, jar: &SignedCookieJar) -> CurrentUser {
    auth::current_user(state, jar)
        .unwrap_or_else(|| panic!("require_auth a laissé passer une requête non authentifiée"))
}

#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct DashboardTemplate {
    pub user: Option<CurrentUser>,
    pub notes_html: String,
    pub is_empty: bool,
}

#[derive(Template, Clone)]
#[template(path = "note_item.html")]
pub struct NoteItemTemplate {
    pub note: NoteRow,
}

#[derive(Clone)]
pub struct NoteRow {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub created_at: String,
}

fn fetch_notes(conn: &rusqlite::Connection, user_id: i64) -> Vec<NoteRow> {
    let mut stmt = conn
        .prepare("SELECT id, title, body, created_at FROM notes WHERE user_id = ?1 ORDER BY id DESC")
        .unwrap();
    let rows = stmt
        .query_map([user_id], |r| {
            Ok(NoteRow {
                id: r.get(0)?,
                title: r.get(1)?,
                body: r.get(2)?,
                created_at: r.get(3)?,
            })
        })
        .unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

pub async fn dashboard(
    State(state): State<AppState>,
    jar: SignedCookieJar,
) -> Response {
    let user = user(&state, &jar);
    let uid = user.id;
    let conn = state.db.get().expect("pool db");
    let notes = fetch_notes(&conn, uid);
    let notes_html = notes
        .iter()
        .filter_map(|n| NoteItemTemplate { note: n.clone() }.render().ok())
        .collect::<Vec<_>>()
        .join("\n");
    let is_empty = notes.is_empty();
    auth::render(&DashboardTemplate {
        user: Some(user),
        notes_html,
        is_empty,
    })
}

/// Fragment htmx : la liste des notes (utilisé pour la page, remplaçable tel quel).
pub async fn list(State(state): State<AppState>, jar: SignedCookieJar) -> Response {
    let user = user(&state, &jar);
    let conn = state.db.get().expect("pool db");
    let notes = fetch_notes(&conn, user.id);
    let mut body = String::new();
    for n in notes {
        if let Ok(html) = (NoteItemTemplate { note: n }).render() {
            body.push_str(&html);
        }
    }
    body.into_response()
}

#[derive(Deserialize)]
pub struct NoteForm {
    title: String,
    #[serde(default)]
    body: String,
}

/// Création d'une note : renvoie le fragment HTML de la nouvelle note
/// (hx-swap="beforeend" dans le formulaire du tableau de bord).
pub async fn create(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(f): Form<NoteForm>,
) -> Response {
    let title = f.title.trim().to_string();
    if title.is_empty() {
        return (
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "Note title cannot be empty.",
        )
            .into_response();
    }
    let user = user(&state, &jar);
    let conn = state.db.get().expect("pool db");
    let note_id = match conn.execute(
        "INSERT INTO notes (user_id, title, body) VALUES (?1, ?2, ?3)",
        rusqlite::params![user.id, title, f.body.trim()],
    ) {
        Ok(_) => conn.last_insert_rowid(),
        Err(e) => {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    };
    let note = NoteRow {
        id: note_id,
        title,
        body: f.body.trim().to_string(),
        created_at: conn
            .query_row(
                "SELECT created_at FROM notes WHERE id = ?1",
                [note_id],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| String::new()),
    };
    match (NoteItemTemplate { note }).render() {
        Ok(html) => axum::response::Html(html).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("template error: {e}"),
        )
            .into_response(),
    }
}

/// Suppression : renvoie vide, le client retire l'élément (hx-swap="outerHTML").
pub async fn delete(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> Response {
    let user = user(&state, &jar);
    let conn = state.db.get().expect("pool db");
    match conn.execute(
        "DELETE FROM notes WHERE id = ?1 AND user_id = ?2",
        rusqlite::params![id, user.id],
    ) {
        Ok(_) => "".into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
