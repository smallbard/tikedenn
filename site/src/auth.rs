use crate::web::render;
use crate::AppState;
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use askama::Template;
use axum::{
    extract::{Query, State},
    response::{IntoResponse, Redirect, Response},
    Form,
};
use axum_extra::extract::cookie::{Cookie, SameSite, SignedCookieJar};
use serde::Deserialize;

#[derive(Clone)]
pub struct CurrentUser {
    pub id: i64,
    pub username: String,
    pub is_admin: bool,
    /// Tâches en attente de revue (badge de la navigation).
    pub to_review: i64,
}

/// Utilisateur courant d'après le cookie de session signé (uid).
pub fn current_user(state: &AppState, jar: &SignedCookieJar) -> Option<CurrentUser> {
    let uid: i64 = jar.get("uid")?.value().parse().ok()?;
    let conn = state.db.get().ok()?;
    conn.query_row(
        "SELECT id, username, is_admin, (SELECT COUNT(*) FROM tasks WHERE status = 'review')
         FROM users WHERE id = ?1",
        [uid],
        |r| {
            Ok(CurrentUser {
                id: r.get(0)?,
                username: r.get(1)?,
                is_admin: r.get(2)?,
                to_review: r.get(3)?,
            })
        },
    )
    .ok()
}

/// Hachage Argon2 d'un mot de passe (utilisé par l'inscription et
/// l'administration).
pub fn hash_password(password: &str) -> Option<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .ok()
        .map(|h| h.to_string())
}

fn session_cookie(uid: i64) -> Cookie<'static> {
    let mut b = Cookie::build(("uid", uid.to_string()))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .permanent();
    // En production (derrière TLS), définir COOKIE_SECURE=1 dans le service.
    if std::env::var("COOKIE_SECURE").is_ok() {
        b = b.secure(true);
    }
    b.build()
}

#[derive(Template)]
#[template(path = "auth_form.html")]
pub struct AuthFormTemplate {
    pub user: Option<CurrentUser>,
    pub is_register: bool,
    pub next: Option<String>,
    pub error: Option<String>,
}

/// Page de présentation affichée à la racine quand l'utilisateur n'est pas connecté.
#[derive(Template)]
#[template(path = "landing.html")]
pub struct LandingTemplate {
    pub user: Option<CurrentUser>,
    pub registration_enabled: bool,
}

#[derive(Deserialize)]
pub struct NextQuery {
    next: Option<String>,
}

pub async fn login_form(Query(q): Query<NextQuery>) -> Response {
    render(&AuthFormTemplate {
        user: None,
        is_register: false,
        next: q.next,
        error: None,
    })
}

pub async fn register_form(Query(q): Query<NextQuery>) -> Response {
    render(&AuthFormTemplate {
        user: None,
        is_register: true,
        next: q.next,
        error: None,
    })
}

/// Cible de redirection locale uniquement (pas d'open redirect) : un chemin
/// absolu, mais ni `//hote` ni `/\hote` (interprétés comme URL externes).
fn is_local_path(n: &str) -> bool {
    n.starts_with('/') && !n.starts_with("//") && !n.starts_with("/\\")
}

fn auth_redirect(next: &Option<String>) -> Response {
    match next {
        Some(n) if is_local_path(n) => Redirect::to(n),
        _ => Redirect::to("/"),
    }
    .into_response()
}

#[derive(Deserialize)]
pub struct RegisterForm {
    username: String,
    email: String,
    password: String,
    #[serde(default)]
    next: Option<String>,
}

pub async fn register(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(f): Form<RegisterForm>,
) -> Response {
    let username = f.username.trim().to_string();
    let email = f.email.trim().to_string();
    let fail = |msg: &str| {
        render(&AuthFormTemplate {
            user: None,
            is_register: true,
            next: f.next.clone(),
            error: Some(msg.to_string()),
        })
    };
    if username.len() < 3 || username.len() > 32 {
        return fail("Username must be 3 to 32 characters long.");
    }
    if !email.contains('@') {
        return fail("Invalid e-mail address.");
    }
    if f.password.len() < 8 {
        return fail("Password must be at least 8 characters long.");
    }
    let hash = match hash_password(&f.password) {
        Some(h) => h,
        None => return fail("Internal error."),
    };
    let conn = match state.db.get() {
        Ok(c) => c,
        Err(_) => return fail("Internal error."),
    };
    match conn.execute(
        "INSERT INTO users (username, email, pass_hash) VALUES (?1, ?2, ?3)",
        rusqlite::params![username, email, hash],
    ) {
        Ok(_) => {
            let uid = conn.last_insert_rowid();
            let jar = jar.add(session_cookie(uid));
            let target = auth_redirect(&f.next);
            (jar, target).into_response()
        }
        Err(_) => fail("This username or e-mail is already taken."),
    }
}

#[derive(Deserialize)]
pub struct LoginForm {
    username: String,
    password: String,
    #[serde(default)]
    next: Option<String>,
}

pub async fn login(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(f): Form<LoginForm>,
) -> Response {
    let fail = || {
        render(&AuthFormTemplate {
            user: None,
            is_register: false,
            next: f.next.clone(),
            error: Some("Invalid credentials.".to_string()),
        })
    };
    let conn = match state.db.get() {
        Ok(c) => c,
        Err(_) => return fail(),
    };
    let row: Result<(i64, String), _> = conn.query_row(
        "SELECT id, pass_hash FROM users WHERE username = ?1 OR email = ?1",
        [f.username.trim()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    let (uid, hash) = match row {
        Ok(v) => v,
        Err(_) => return fail(),
    };
    let parsed = match PasswordHash::new(&hash) {
        Ok(p) => p,
        Err(_) => return fail(),
    };
    if Argon2::default()
        .verify_password(f.password.as_bytes(), &parsed)
        .is_err()
    {
        return fail();
    }
    let jar = jar.add(session_cookie(uid));
    let target = auth_redirect(&f.next);
    (jar, target).into_response()
}

pub async fn logout(jar: SignedCookieJar) -> Response {
    let jar = jar.remove(Cookie::from("uid"));
    (jar, Redirect::to("/login")).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_paths_are_accepted_as_redirect_targets() {
        assert!(is_local_path("/dashboard"));
        assert!(is_local_path("/tasks/new?project=3"));
        assert!(!is_local_path("//evil.com"));
        assert!(!is_local_path("/\\evil.com"));
        assert!(!is_local_path("https://evil.com"));
        assert!(!is_local_path(""));
    }
}
