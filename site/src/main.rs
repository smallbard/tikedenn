mod admin;
mod auth;
mod board;
mod db;

use axum::{
    extract::{DefaultBodyLimit, Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use axum_extra::extract::cookie::{Key, SignedCookieJar};
use r2d2_sqlite::SqliteConnectionManager;
use rand::RngCore;
use std::path::PathBuf;
use tower_http::services::ServeDir;

pub type Pool = r2d2::Pool<SqliteConnectionManager>;

#[derive(Clone)]
pub struct AppState {
    pub db: Pool,
    pub key: Key,
}

impl axum::extract::FromRef<AppState> for Key {
    fn from_ref(s: &AppState) -> Key {
        s.key.clone()
    }
}

fn env_or(name: &str, default: PathBuf) -> PathBuf {
    std::env::var(name).map(PathBuf::from).unwrap_or(default)
}

/// Charge la clé de signature des cookies ; elle est générée et persistée
/// si le fichier n'existe pas (les sessions survivent ainsi aux redéploiements).
fn load_key(path: &PathBuf) -> anyhow::Result<Key> {
    if let Ok(bytes) = std::fs::read(path) {
        if bytes.len() >= 64 {
            return Ok(Key::from(&bytes));
        }
    }
    let mut bytes = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut bytes);
    std::fs::write(path, bytes)?;
    Ok(Key::from(&bytes))
}

/// Garde d'authentification : toute route sous ce middleware exige un cookie
/// de session valide, sinon redirection vers /login (avec ?next= pour y
/// revenir après). Les routes /login, /register, /logout et /static restent
/// publiques.
async fn require_auth(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    req: Request,
    next: Next,
) -> Response {
    if let Some(u) = auth::current_user(&state, &jar) {
        let mut req = req;
        req.extensions_mut().insert(u);
        return next.run(req).await;
    }
    let uri = req.uri().path().to_string();
    let dest = if uri == "/" {
        "/login".into()
    } else {
        format!("/login?next={uri}")
    };
    (jar, Redirect::to(&dest)).into_response()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let app_db = env_or("APP_DB", root.join("app.sqlite"));
    let static_dir = env_or("STATIC_DIR", root.join("static"));
    let key = load_key(&env_or("SECRET_KEY_FILE", root.join("secret.key")))?;

    let pool = r2d2::Pool::builder()
        .max_size(8)
        .build(SqliteConnectionManager::file(&app_db))?;
    {
        let conn = pool.get()?;
        db::init_app_schema(&conn)?;
        seed_default_admin(&conn);
    }

    let state = AppState { db: pool, key };

    let public = Router::new()
        .route("/login", get(auth::login_form).post(auth::login))
        .route("/register", get(auth::register_form).post(auth::register))
        .route("/logout", post(auth::logout))
        .nest_service("/static", ServeDir::new(static_dir));

    // Zone protégée : seule l'accueil (/) décide entre connexion et tableau
    // de bord, tout le reste est derrière require_auth.
    // 11 Mo : fichier de 10 Mo + overhead multipart (axum limite à 2 Mo par défaut).
    let protected = Router::new()
        .route("/dashboard", get(board::dashboard))
        .route("/projects", get(board::projects_page).post(board::create_project))
        .route("/projects/{id}", post(board::update_project))
        .route("/projects/{id}/delete", post(board::delete_project))
        .route("/tasks", post(board::create_task))
        .route("/tasks/{id}", post(board::update_task))
        .route("/tasks/{id}/move", post(board::move_task))
        .route("/tasks/{id}/delete", post(board::delete_task))
        .route("/tasks/{id}/files", post(board::upload_file))
        .route("/files/{id}", get(board::download_file))
        .route("/files/{id}/delete", post(board::delete_file))
        .route("/admin/users", get(admin::users))
        .route("/admin/users/{id}/delete", post(admin::delete_user))
        .route("/admin/users/{id}/password", post(admin::set_password))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ))
        .layer(DefaultBodyLimit::max(11 * 1024 * 1024));

    let router = Router::new()
        .route("/", get(index))
        .merge(public)
        .merge(protected)
        .with_state(state);

    let addr = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:3002".into());
    println!("Tikedenn on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}

/// Au tout premier lancement (aucun compte), crée un compte administrateur
/// par défaut avec un mot de passe aléatoire affiché dans la console
/// (journal systemd en production). À changer ensuite depuis /admin/users.
fn seed_default_admin(conn: &rusqlite::Connection) {
    let count: i64 = match conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0)) {
        Ok(n) => n,
        Err(_) => return,
    };
    if count != 0 {
        return;
    }
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    let password: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let Some(hash) = auth::hash_password(&password) else {
        return;
    };
    if conn
        .execute(
            "INSERT INTO users (username, email, pass_hash, is_admin) VALUES ('admin', 'olivier.leguyader@smallbard.com', ?1, 1)",
            [&hash],
        )
        .is_ok()
    {
        eprintln!(
            "[tikedenn] Default admin account created — username: \"admin\", password: \"{password}\". Change it from /admin/users after logging in."
        );
    }
}

/// Racine : tableau de bord si connecté, formulaire de connexion sinon.
async fn index(
    State(state): State<AppState>,
    jar: SignedCookieJar,
) -> Response {
    if auth::current_user(&state, &jar).is_some() {
        Redirect::to("/dashboard").into_response()
    } else {
        auth::login_form().await
    }
}
