//! Briques communes des handlers : rendu des templates, erreur applicative
//! (convertible en réponse http) et redirections avec message d'erreur.

use crate::util::urlencode;
use askama::Template;
use axum::{
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};

pub fn render<T: Template>(t: &T) -> Response {
    match t.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => AppError::Internal(format!("template error: {e}")).into_response(),
    }
}

/// Erreur d'un handler. Les erreurs BDD / pool se propagent avec `?` et
/// deviennent un 500 journalisé (message générique côté client).
pub enum AppError {
    /// Réponse http explicite (404, 400…) avec un message pour l'utilisateur.
    Http(StatusCode, &'static str),
    /// Erreur interne : journalisée, jamais détaillée au client.
    Internal(String),
}

pub type AppResult<T = Response> = Result<T, AppError>;

impl AppError {
    pub fn not_found(msg: &'static str) -> Self {
        AppError::Http(StatusCode::NOT_FOUND, msg)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::Http(status, msg) => (status, msg).into_response(),
            AppError::Internal(e) => {
                eprintln!("[tikedenn] internal error: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal error.").into_response()
            }
        }
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        AppError::Internal(format!("sqlite: {e}"))
    }
}

impl From<r2d2::Error> for AppError {
    fn from(e: r2d2::Error) -> Self {
        AppError::Internal(format!("db pool: {e}"))
    }
}

/// Redirection (303) vers `url`, avec `error=` ajouté à la query si fourni.
pub fn redirect_with_error(url: &str, error: Option<&str>) -> Response {
    match error {
        Some(e) => {
            let sep = if url.contains('?') { '&' } else { '?' };
            Redirect::to(&format!("{url}{sep}error={}", urlencode(e))).into_response()
        }
        None => Redirect::to(url).into_response(),
    }
}
