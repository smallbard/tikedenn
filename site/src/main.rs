mod admin;
mod api;
mod auth;
mod board;
mod db;
mod files;
mod git_diff;
mod highlight;
mod markdown;
mod mcp;
mod notify;
mod order;
mod projects;
mod review;
mod tokens;
mod util;
mod web;
mod workflow;

use axum::{
    extract::{DefaultBodyLimit, Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{any, get, post, put},
    Router,
};
use axum_extra::extract::cookie::{Key, SignedCookieJar};
use r2d2_sqlite::SqliteConnectionManager;
use rand::RngCore;
use std::path::{Path, PathBuf};
use tower_http::services::ServeDir;

pub type Pool = r2d2::Pool<SqliteConnectionManager>;
pub type Conn = r2d2::PooledConnection<SqliteConnectionManager>;

#[derive(Clone)]
pub struct AppState {
    pub db: Pool,
    pub key: Key,
}

impl AppState {
    /// Connexion du pool ; l'échec (pool épuisé) devient une erreur 500.
    pub fn conn(&self) -> Result<Conn, web::AppError> {
        Ok(self.db.get()?)
    }
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
fn load_key(path: &Path) -> anyhow::Result<Key> {
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
/// revenir après). Les routes /login, /register (si activée), /logout et
/// /static restent publiques.
async fn require_auth(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    mut req: Request,
    next: Next,
) -> Response {
    if let Some(u) = auth::current_user(&state, &jar) {
        req.extensions_mut().insert(u);
        return next.run(req).await;
    }
    let dest = match req.uri().path_and_query().map(|pq| pq.as_str()) {
        None | Some("/") => "/login".into(),
        Some(target) => format!("/login?next={}", util::urlencode(target)),
    };
    (jar, Redirect::to(&dest)).into_response()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let app_db = env_or("APP_DB", root.join("app.sqlite"));
    let static_dir = env_or("STATIC_DIR", root.join("static"));
    let key = load_key(&env_or("SECRET_KEY_FILE", root.join("secret.key")))?;

    // Les PRAGMA foreign_keys / busy_timeout valent par connexion : on les
    // applique à chaque connexion ouverte par le pool (sinon pas de cascade).
    let manager = SqliteConnectionManager::file(&app_db).with_init(|c| {
        c.execute_batch("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")
    });
    let pool = r2d2::Pool::builder().max_size(8).build(manager)?;
    {
        let conn = pool.get()?;
        db::init_app_schema(&conn)?;
        seed_default_admin(&conn)?;
    }

    let state = AppState { db: pool, key };

    // REGISTRATION=0 désactive l'auto-inscription (routes /register) ;
    // activée par défaut (utile pour le dev local).
    let mut public = Router::new()
        .route("/login", get(auth::login_form).post(auth::login))
        .route("/logout", post(auth::logout))
        .nest_service("/static", ServeDir::new(static_dir));
    if util::registration_enabled() {
        public = public.route(
            "/register",
            get(auth::register_form).post(auth::register),
        );
    }

    // Zone admin : 404 pour les non-admins (require_admin), en plus de
    // l'authentification appliquée à toute la zone protégée.
    let admin = Router::new()
        .route("/admin/users", get(admin::users))
        .route("/admin/users/{id}/delete", post(admin::delete_user))
        .route("/admin/users/{id}/password", post(admin::set_password))
        .route_layer(middleware::from_fn(admin::require_admin));

    // Zone protégée : seule l'accueil (/) décide entre présentation et
    // tableau de bord, tout le reste est derrière require_auth.
    // 11 Mo : fichier de 10 Mo + overhead multipart (axum limite à 2 Mo par défaut).
    let protected = Router::new()
        .route("/dashboard", get(board::dashboard))
        .route("/dashboard/board", get(board::board_fragment))
        .route("/projects", get(projects::projects_page).post(projects::create_project))
        .route("/projects/{id}", post(projects::update_project))
        .route("/projects/{id}/delete", post(projects::delete_project))
        .route("/tasks", post(board::create_task))
        .route("/tasks/new", get(board::new_task_page))
        .route("/markdown", post(markdown::preview))
        .route("/tasks/{id}/edit", get(board::edit_task_page))
        .route("/tasks/{id}/diff", get(git_diff::task_diff_page))
        .route("/tasks/{id}", post(board::update_task))
        .route("/tasks/{id}/move", post(board::move_task))
        .route("/tasks/{id}/delete", post(board::delete_task))
        .route("/tasks/{id}/comments", post(review::add_comment))
        .route("/tasks/{id}/approve", post(review::approve))
        .route("/tasks/{id}/request-changes", post(review::request_changes))
        .route("/tasks/{id}/release", post(review::release))
        .route("/tasks/{id}/files", post(files::upload_file))
        .route("/files/{id}", get(files::download_file))
        .route("/files/{id}/delete", post(files::delete_file))
        .route("/settings/tokens", get(tokens::list).post(tokens::create))
        .route("/settings/tokens/{id}/delete", post(tokens::delete))
        .merge(admin)
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ))
        .layer(DefaultBodyLimit::max(11 * 1024 * 1024));

    // API des agents (JSON) et serveur MCP : authentification par token
    // (Authorization: Bearer), pas par cookie.
    let agents = Router::new()
        .route("/api/v1/me", get(api::me))
        .route("/api/v1/projects", get(api::projects))
        .route("/api/v1/tasks", get(api::list_tasks).post(api::create_task))
        .route("/api/v1/tasks/claim-next", post(api::claim_next))
        .route("/api/v1/tasks/{id}", get(api::get_task).patch(api::patch_task))
        .route("/api/v1/tasks/{id}/claim", post(api::claim))
        .route("/api/v1/tasks/{id}/release", post(api::release))
        .route("/api/v1/tasks/{id}/move", post(api::move_task))
        .route("/api/v1/tasks/{id}/comments", post(api::add_comment))
        .route("/api/v1/tasks/{id}/files/{name}", put(api::upload_file))
        .route("/api/v1/files/{id}", get(api::download_file))
        .route("/mcp", post(mcp::post).get(mcp::get))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            tokens::require_token,
        ))
        .route("/api/{*rest}", any(api::not_found))
        .layer(DefaultBodyLimit::max(11 * 1024 * 1024));

    let router = Router::new()
        .route("/", get(index))
        .merge(public)
        .merge(protected)
        .merge(agents)
        .with_state(state);

    let addr = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:3003".into());
    println!("Tikedenn on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}

/// Au tout premier lancement (aucun compte), crée un compte administrateur
/// par défaut avec un mot de passe aléatoire affiché dans la console
/// (journal systemd en production). À changer ensuite depuis /admin/users.
fn seed_default_admin(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
    if count != 0 {
        return Ok(());
    }
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    let password: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let hash = auth::hash_password(&password)
        .ok_or_else(|| anyhow::anyhow!("could not hash the admin password"))?;
    conn.execute(
        "INSERT INTO users (username, email, pass_hash, is_admin) VALUES ('admin', 'admin@localhost', ?1, 1)",
        [&hash],
    )?;
    eprintln!(
        "[tikedenn] Default admin account created — username: \"admin\", password: \"{password}\". Change it from /admin/users after logging in."
    );
    Ok(())
}

/// Racine : tableau de bord si connecté, page de présentation sinon.
async fn index(State(state): State<AppState>, jar: SignedCookieJar) -> Response {
    if auth::current_user(&state, &jar).is_some() {
        Redirect::to("/dashboard").into_response()
    } else {
        web::render(&auth::LandingTemplate {
            user: None,
            registration_enabled: util::registration_enabled(),
        })
    }
}
