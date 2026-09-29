//! Tokens d'API des agents : page de gestion (`/settings/tokens`) et
//! middleware d'authentification `Authorization: Bearer tkd_…` de `/api/v1`
//! et `/mcp`. Seul le SHA-256 du token est stocké : il n'est affiché qu'une
//! fois, à sa création.

use crate::api::ApiError;
use crate::auth::CurrentUser;
use crate::web::{redirect_with_error, render, AppResult};
use crate::workflow::Actor;
use crate::AppState;
use askama::Template;
use axum::{
    extract::{Extension, Path, Query, Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    Form,
};
use rand::RngCore;
use rusqlite::OptionalExtension;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Requêtes permises par token et par minute.
const RATE_LIMIT_PER_MINUTE: u32 = 600;

fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Nouveau token : `tkd_` + 32 octets aléatoires en hexadécimal.
fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("tkd_{hex}")
}

/// Nom de token valide : il sert d'identité d'agent (« utilisateur/nom »).
fn is_valid_token_name(n: &str) -> bool {
    (1..=64).contains(&n.len())
        && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-'))
}

/// URL publique du site d'après la requête (derrière nginx : Host et
/// X-Forwarded-Proto), pour les commandes d'installation affichées.
pub fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("127.0.0.1:3003");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("http");
    format!("{proto}://{host}")
}

pub struct TokenRow {
    pub id: i64,
    pub name: String,
    pub scope: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

#[derive(Template)]
#[template(path = "tokens.html")]
pub struct TokensTemplate {
    pub user: Option<CurrentUser>,
    pub tokens: Vec<TokenRow>,
    /// Token tout juste créé (affiché une seule fois).
    pub new_token: Option<String>,
    pub base_url: String,
    pub error: Option<String>,
}

#[derive(Deserialize)]
pub struct ErrorQuery {
    error: Option<String>,
}

fn tokens_page(
    conn: &rusqlite::Connection,
    user: CurrentUser,
    new_token: Option<String>,
    base_url: String,
    error: Option<String>,
) -> AppResult {
    let tokens = conn
        .prepare(
            "SELECT id, name, scope, created_at, last_used_at FROM api_tokens
             WHERE user_id = ?1 ORDER BY id",
        )?
        .query_map([user.id], |r| {
            Ok(TokenRow {
                id: r.get(0)?,
                name: r.get(1)?,
                scope: r.get(2)?,
                created_at: r.get(3)?,
                last_used_at: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(render(&TokensTemplate {
        user: Some(user),
        tokens,
        new_token,
        base_url,
        error,
    }))
}

/// Liste des tokens de l'utilisateur et instructions d'installation.
pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    Query(q): Query<ErrorQuery>,
) -> AppResult {
    tokens_page(&*state.conn()?, user, None, base_url(&headers), q.error)
}

#[derive(Deserialize)]
pub struct NewTokenForm {
    name: String,
    #[serde(default)]
    scope: String,
}

/// Crée un token : la page est rendue directement (pas de redirection) pour
/// afficher le token une seule fois sans le mettre dans une URL.
pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    Form(f): Form<NewTokenForm>,
) -> AppResult {
    let name = f.name.trim();
    if !is_valid_token_name(name) {
        let msg = "Token name: 1 to 64 letters, digits, '@', '.', '_' or '-'.";
        return Ok(redirect_with_error("/settings/tokens", Some(msg)));
    }
    let scope = if f.scope == "read" { "read" } else { "write" };
    let token = generate_token();
    let conn = state.conn()?;
    let inserted = conn.execute(
        "INSERT INTO api_tokens (user_id, name, token_hash, scope) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![user.id, name, hash_token(&token), scope],
    );
    match inserted {
        Ok(_) => tokens_page(&conn, user, Some(token), base_url(&headers), None),
        Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
            Ok(redirect_with_error("/settings/tokens", Some("You already have a token with this name.")))
        }
        Err(e) => Err(e.into()),
    }
}

/// Révoque un token de l'utilisateur.
pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> AppResult {
    state.conn()?.execute(
        "DELETE FROM api_tokens WHERE id = ?1 AND user_id = ?2",
        rusqlite::params![id, user.id],
    )?;
    Ok(Redirect::to("/settings/tokens").into_response())
}

/// Limitation de débit par token (fenêtre fixe d'une minute, en mémoire).
fn rate_limited(token_id: i64) -> bool {
    static WINDOWS: Mutex<Option<HashMap<i64, (Instant, u32)>>> = Mutex::new(None);
    let mut guard = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    let entry = map.entry(token_id).or_insert((now, 0));
    if now.duration_since(entry.0).as_secs() >= 60 {
        *entry = (now, 0);
    }
    entry.1 += 1;
    entry.1 > RATE_LIMIT_PER_MINUTE
}

/// Middleware de `/api/v1` et `/mcp` : authentifie le token et insère
/// l'`Actor::Agent` correspondant. Un token « read » n'a droit qu'aux GET
/// (et, pour /mcp, aux outils de lecture : voir `mcp`).
pub async fn require_token(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(|t| t.trim().to_string());
    let Some(token) = token else {
        return ApiError::new(StatusCode::UNAUTHORIZED, "Missing 'Authorization: Bearer <token>' header.")
            .into_response();
    };
    let found = (|| -> Result<Option<(i64, String, String, CurrentUser)>, ApiError> {
        let conn = state.conn()?;
        let row = conn
            .query_row(
                "SELECT t.id, t.name, t.scope, u.id, u.username, u.is_admin
                 FROM api_tokens t JOIN users u ON u.id = t.user_id WHERE t.token_hash = ?1",
                [hash_token(&token)],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        CurrentUser { id: r.get(3)?, username: r.get(4)?, is_admin: r.get(5)?, to_review: 0 },
                    ))
                },
            )
            .optional()?;
        if let Some((id, ..)) = &row {
            // Date de dernier usage, écrite au plus une fois par minute.
            conn.execute(
                "UPDATE api_tokens SET last_used_at = datetime('now')
                 WHERE id = ?1 AND (last_used_at IS NULL OR last_used_at < datetime('now', '-1 minute'))",
                [id],
            )?;
        }
        Ok(row)
    })();
    let (token_id, name, scope, user) = match found {
        Ok(Some(row)) => row,
        Ok(None) => return ApiError::new(StatusCode::UNAUTHORIZED, "Invalid or revoked token.").into_response(),
        Err(e) => return e.into_response(),
    };
    if rate_limited(token_id) {
        return ApiError::new(StatusCode::TOO_MANY_REQUESTS, "Rate limit exceeded; slow down.").into_response();
    }
    let read_only = scope == "read";
    if read_only && req.method() != Method::GET && !req.uri().path().starts_with("/mcp") {
        return ApiError::new(StatusCode::FORBIDDEN, "This token is read-only.").into_response();
    }
    req.extensions_mut().insert(TokenScope { read_only });
    req.extensions_mut().insert(Actor::Agent { user, token: name });
    next.run(req).await
}

/// Portée du token de la requête.
#[derive(Clone, Copy)]
pub struct TokenScope {
    pub read_only: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_and_hashed() {
        let (a, b) = (generate_token(), generate_token());
        assert_ne!(a, b);
        assert!(a.starts_with("tkd_") && a.len() == 68);
        assert_eq!(hash_token(&a), hash_token(&a));
        assert_eq!(hash_token(&a).len(), 64);
    }

    #[test]
    fn token_names() {
        assert!(is_valid_token_name("claude@laptop"));
        assert!(is_valid_token_name("pi-worker_2"));
        assert!(!is_valid_token_name(""));
        assert!(!is_valid_token_name("a/b"));
        assert!(!is_valid_token_name("a b"));
    }
}
