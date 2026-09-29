//! Gestion des utilisateurs, réservée aux comptes avec le drapeau is_admin.

use askama::Template;
use axum::{
    extract::{Extension, Path, Query, Request, State},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    Form,
};
use serde::Deserialize;

use crate::auth::{hash_password, CurrentUser};
use crate::web::{redirect_with_error, render, AppError, AppResult};
use crate::AppState;

/// Middleware de la zone /admin : 404 pour les non-administrateurs (la page
/// n'existe pas pour eux). L'authentification a déjà été faite par require_auth.
pub async fn require_admin(
    Extension(user): Extension<CurrentUser>,
    req: Request,
    next: Next,
) -> Response {
    if user.is_admin {
        next.run(req).await
    } else {
        AppError::not_found("Not found.").into_response()
    }
}

pub struct UserRow {
    pub id: i64,
    pub username: String,
    pub email: String,
    pub created_at: String,
    pub tasks: i64,
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

#[derive(Deserialize)]
pub struct ErrorQuery {
    error: Option<String>,
}

/// Liste des utilisateurs (avec le nombre de tâches qu'ils ont créées).
pub async fn users(
    State(state): State<AppState>,
    Extension(admin): Extension<CurrentUser>,
    Query(q): Query<ErrorQuery>,
) -> AppResult {
    let conn = state.conn()?;
    let mut st = conn.prepare(
        "SELECT u.id, u.username, u.email, u.created_at,
                (SELECT COUNT(*) FROM tasks t WHERE t.user_id = u.id),
                u.is_admin
         FROM users u ORDER BY u.created_at DESC, u.id DESC",
    )?;
    let users = st
        .query_map([], |r| {
            let id = r.get(0)?;
            Ok(UserRow {
                id,
                username: r.get(1)?,
                email: r.get(2)?,
                created_at: r.get(3)?,
                tasks: r.get(4)?,
                is_admin: r.get(5)?,
                is_self: id == admin.id,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(render(&AdminUsersTemplate {
        user: Some(admin),
        users,
        error: q.error,
    }))
}

/// Supprime un utilisateur (tokens en cascade ; ses tâches restent sur le
/// tableau partagé, sans créateur). L'administrateur ne
/// peut pas se supprimer.
pub async fn delete_user(
    State(state): State<AppState>,
    Extension(admin): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> AppResult {
    if id != admin.id {
        state.conn()?.execute("DELETE FROM users WHERE id = ?1", [id])?;
    }
    Ok(Redirect::to("/admin/users").into_response())
}

#[derive(Deserialize)]
pub struct PasswordForm {
    password: String,
}

/// Définit un nouveau mot de passe (l'administrateur peut le faire pour tout
/// compte, y compris le sien).
pub async fn set_password(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<PasswordForm>,
) -> AppResult {
    let password = f.password.trim();
    if password.len() < 8 {
        return Ok(redirect_with_error(
            "/admin/users",
            Some("Password must be at least 8 characters."),
        ));
    }
    let hash = hash_password(password)
        .ok_or_else(|| AppError::Internal("password hashing failed".into()))?;
    state.conn()?.execute(
        "UPDATE users SET pass_hash = ?1 WHERE id = ?2",
        rusqlite::params![hash, id],
    )?;
    Ok(Redirect::to("/admin/users").into_response())
}
