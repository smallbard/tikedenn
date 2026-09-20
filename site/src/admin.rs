//! Gestion des utilisateurs, réservée aux comptes avec le drapeau is_admin.

use askama::Template;
use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Redirect, Response},
    Form,
};
use axum_extra::extract::SignedCookieJar;
use serde::Deserialize;
use std::collections::HashMap;

use crate::auth::{current_user, hash_password, render, CurrentUser};
use crate::AppState;

/// Utilisateur courant s'il est administrateur, sinon 404 (la page n'existe
/// pas pour les autres) ou redirection vers la connexion.
fn require_admin(state: &AppState, jar: &SignedCookieJar) -> Result<CurrentUser, Response> {
    match current_user(state, jar) {
        None => Err(Redirect::to("/login").into_response()),
        Some(u) if u.is_admin => Ok(u),
        Some(_) => Err(axum::http::StatusCode::NOT_FOUND.into_response()),
    }
}

pub struct UserRow {
    pub id: i64,
    pub username: String,
    pub email: String,
    pub created_at: String,
    pub notes: i64,
    pub is_admin: bool,
    pub is_self: bool,
}

#[derive(Template)]
#[template(path = "admin_users.html")]
pub struct AdminUsersTemplate {
    pub user: Option<CurrentUser>,
    pub users: Vec<UserRow>,
    pub error: Option<String>,
}

/// Liste des utilisateurs (avec leur nombre de notes).
pub async fn users(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let admin = match require_admin(&state, &jar) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let mut users = vec![];
    if let Ok(conn) = state.db.get() {
        if let Ok(mut st) = conn.prepare(
            "SELECT u.id, u.username, u.email, u.created_at,
                    (SELECT COUNT(*) FROM tasks t WHERE t.user_id = u.id),
                    u.is_admin
             FROM users u ORDER BY u.created_at DESC, u.id DESC",
        ) {
            if let Ok(rows) = st.query_map([], |r| {
                Ok(UserRow {
                    id: r.get(0)?,
                    username: r.get(1)?,
                    email: r.get(2)?,
                    created_at: r.get(3)?,
                    notes: r.get(4)?,
                    is_admin: r.get::<_, i32>(5)? != 0,
                    is_self: false,
                })
            }) {
                for mut u in rows.flatten() {
                    u.is_self = u.id == admin.id;
                    users.push(u);
                }
            }
        }
    }
    let error = params.get("error").cloned();
    render(&AdminUsersTemplate {
        user: Some(admin),
        users,
        error,
    })
}

/// Supprime un utilisateur et ses notes (cascade). L'administrateur ne peut
/// pas se supprimer.
pub async fn delete_user(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> Response {
    let admin = match require_admin(&state, &jar) {
        Ok(u) => u,
        Err(r) => return r,
    };
    if id != admin.id {
        if let Ok(conn) = state.db.get() {
            let _ = conn.execute("DELETE FROM users WHERE id = ?1", [id]);
        }
    }
    Redirect::to("/admin/users").into_response()
}

#[derive(Deserialize)]
pub struct PasswordForm {
    password: String,
}

/// Définit un nouveau mot de passe (l'administrateur peut le faire pour tout
/// compte, y compris le sien).
pub async fn set_password(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    Form(f): Form<PasswordForm>,
) -> Response {
    let _admin = match require_admin(&state, &jar) {
        Ok(u) => u,
        Err(r) => return r,
    };
    if f.password.trim().len() < 8 {
        return Redirect::to("/admin/users?error=Password%20must%20be%20at%20least%208%20characters.")
            .into_response();
    }
    if let Some(hash) = hash_password(f.password.trim()) {
        if let Ok(conn) = state.db.get() {
            let _ = conn.execute("UPDATE users SET pass_hash = ?1 WHERE id = ?2", rusqlite::params![&hash, id]);
        }
    }
    Redirect::to("/admin/users").into_response()
}
