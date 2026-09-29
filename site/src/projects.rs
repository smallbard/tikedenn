//! Projets : nom + dépôt git, partagés par tous les utilisateurs. Chaque
//! utilisateur peut y associer son propre token git (table `project_tokens`).

use crate::auth::CurrentUser;
use crate::board::is_safe_branch;
use crate::web::{redirect_with_error, render, AppResult};
use crate::AppState;
use askama::Template;
use axum::{
    extract::{Extension, Path, Query, State},
    response::{IntoResponse, Redirect, Response},
    Form,
};
use rusqlite::Connection;
use serde::Deserialize;

pub struct ProjectRow {
    pub id: i64,
    pub name: String,
    pub git_repo: String,
    /// Branche de base des diffs et des déploiements (vide = main, sinon master).
    pub default_branch: String,
    /// Nom du projet pour `publish <nom>` (vide = pas de publication).
    pub publish_name: String,
    /// Token optionnel (dépôt privé en https) de l'utilisateur courant ;
    /// jamais affiché dans l'UI.
    pub token: String,
}

impl ProjectRow {
    /// Vue JSON (API) : sans le token.
    pub fn to_api(&self) -> tikedenn_common::api::Project {
        tikedenn_common::api::Project {
            id: self.id,
            name: self.name.clone(),
            git_repo: self.git_repo.clone(),
            default_branch: self.default_branch.clone(),
            publish_name: self.publish_name.clone(),
        }
    }
}

/// Tous les projets (partagés : pas de filtre par utilisateur), avec le
/// token de l'utilisateur courant.
pub fn fetch_projects(conn: &Connection, user_id: i64) -> rusqlite::Result<Vec<ProjectRow>> {
    conn.prepare(
        "SELECT p.id, p.name, p.git_repo, p.default_branch, p.publish_name,
                COALESCE((SELECT t.token FROM project_tokens t WHERE t.project_id = p.id AND t.user_id = ?1), '')
         FROM projects p ORDER BY p.name COLLATE NOCASE",
    )?
    .query_map([user_id], |r| {
        Ok(ProjectRow {
            id: r.get(0)?,
            name: r.get(1)?,
            git_repo: r.get(2)?,
            default_branch: r.get(3)?,
            publish_name: r.get(4)?,
            token: r.get(5)?,
        })
    })?
    .collect()
}

/// Projet désigné par son id ou son nom (API, CLI des agents).
pub fn find_project<'a>(projects: &'a [ProjectRow], key: &str) -> Option<&'a ProjectRow> {
    let key = key.trim();
    match key.parse::<i64>() {
        Ok(id) => projects.iter().find(|p| p.id == id),
        Err(_) => projects.iter().find(|p| p.name.eq_ignore_ascii_case(key)),
    }
}

/// Renvoie vrai si l'URL est sûre pour un attribut `href` :
/// uniquement http(s)://, ssh:// ou la forme scp `git@…`.
pub fn is_safe_repo_url(url: &str) -> bool {
    is_http_url(url) || {
        let l = url.trim().to_ascii_lowercase();
        l.starts_with("ssh://") || l.starts_with("git@")
    }
}

/// URL http(s) (seules à pouvoir utiliser le token en basic auth).
pub fn is_http_url(url: &str) -> bool {
    let l = url.trim().to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

pub struct ProjectItemRow {
    pub id: i64,
    pub name: String,
    pub git_repo: String,
    pub default_branch: String,
    pub publish_name: String,
    pub tasks: i64,
    /// Un token est configuré pour ce dépôt par l'utilisateur courant
    /// (indicateur, jamais affiché en clair).
    pub has_token: bool,
}

impl ProjectItemRow {
    /// Renvoie vrai si l'URL peut être rendue en <a href> (sinon texte brut).
    pub fn git_repo_is_link(&self) -> bool {
        is_safe_repo_url(&self.git_repo)
    }
}

#[derive(Template)]
#[template(path = "projects.html")]
pub struct ProjectsTemplate {
    pub user: Option<CurrentUser>,
    pub projects: Vec<ProjectItemRow>,
    pub error: Option<String>,
}

#[derive(Deserialize)]
pub struct ProjectsQuery {
    error: Option<String>,
}

/// Page de gestion des projets (nom + dépôt git).
pub async fn projects_page(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Query(q): Query<ProjectsQuery>,
) -> AppResult {
    let conn = state.conn()?;
    let projects = conn
        .prepare(
            "SELECT p.id, p.name, p.git_repo, p.default_branch, p.publish_name,
                    (SELECT COUNT(*) FROM tasks t WHERE t.project_id = p.id),
                    (EXISTS (SELECT 1 FROM project_tokens t
                             WHERE t.project_id = p.id AND t.user_id = ?1))
             FROM projects p ORDER BY p.name COLLATE NOCASE",
        )?
        .query_map([user.id], |r| {
            Ok(ProjectItemRow {
                id: r.get(0)?,
                name: r.get(1)?,
                git_repo: r.get(2)?,
                default_branch: r.get(3)?,
                publish_name: r.get(4)?,
                tasks: r.get(5)?,
                has_token: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(render(&ProjectsTemplate {
        user: Some(user),
        projects,
        error: q.error,
    }))
}

#[derive(Deserialize)]
pub struct ProjectForm {
    name: String,
    #[serde(default)]
    git_repo: String,
    #[serde(default)]
    git_token: String,
    #[serde(default)]
    default_branch: String,
    #[serde(default)]
    publish_name: String,
}

impl ProjectForm {
    /// Champs nettoyés (nom, dépôt, token).
    fn trimmed(&self) -> (&str, &str, &str) {
        (self.name.trim(), self.git_repo.trim(), self.git_token.trim())
    }

    /// Branche de base et nom de publication, validés.
    fn extras(&self) -> Result<(&str, &str), &'static str> {
        let branch = self.default_branch.trim();
        if !branch.is_empty() && !is_safe_branch(branch) {
            return Err("Invalid default branch name.");
        }
        let publish = self.publish_name.trim();
        if publish.len() > 64
            || !publish.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err("Publish name: letters, digits, '-', '_' or '.' only.");
        }
        Ok((branch, publish))
    }
}

/// Valide nom, dépôt et token d'un projet ; renvoie le message d'erreur (UI) sinon.
/// La longueur est comptée en caractères, comme le `maxlength` du formulaire.
fn validate_project(name: &str, repo: &str, token: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.chars().count() > 64 {
        return Err("Project name must be 1 to 64 characters.");
    }
    if !repo.is_empty() && !is_safe_repo_url(repo) {
        return Err("Git repo must start with http(s)://, ssh:// or git@.");
    }
    // Token : au plus 256 caractères ASCII imprimables (pas d'espace, pas de retour
    // à la ligne), sinon il serait ambigu en base / dans les en-têtes http.
    if !token.is_empty() && (token.len() > 256 || !token.chars().all(|c| c.is_ascii_graphic())) {
        return Err("Git token must be 1 to 256 printable ASCII characters.");
    }
    Ok(())
}

/// Message d'erreur (UI) pour un échec d'écriture d'un projet, ou `Err` si
/// l'erreur n'est pas due à la saisie.
fn project_write_error(e: rusqlite::Error) -> AppResult<&'static str> {
    match e {
        rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation => {
            Ok("A project with this name already exists.")
        }
        e => Err(e.into()),
    }
}

fn projects_error(msg: &str) -> Response {
    redirect_with_error("/projects", Some(msg))
}

/// Enregistre le token de l'utilisateur pour le projet ; un token vide
/// conserve celui déjà stocké (l'UI ne le ré-affiche jamais).
fn save_token(conn: &Connection, project_id: i64, user_id: i64, token: &str) -> rusqlite::Result<()> {
    if !token.is_empty() {
        conn.execute(
            "INSERT OR REPLACE INTO project_tokens (project_id, user_id, token) VALUES (?1, ?2, ?3)",
            rusqlite::params![project_id, user_id, token],
        )?;
    }
    Ok(())
}

/// Crée un projet (partagé par tous les utilisateurs) avec, si fourni, le
/// token de l'utilisateur courant.
pub async fn create_project(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Form(f): Form<ProjectForm>,
) -> AppResult {
    let (name, repo, token) = f.trimmed();
    let extras = validate_project(name, repo, token).and_then(|_| f.extras());
    let (branch, publish) = match extras {
        Ok(e) => e,
        Err(msg) => return Ok(projects_error(msg)),
    };
    let conn = state.conn()?;
    if let Err(e) = conn.execute(
        "INSERT INTO projects (name, git_repo, default_branch, publish_name) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![name, repo, branch, publish],
    ) {
        return Ok(projects_error(project_write_error(e)?));
    }
    save_token(&conn, conn.last_insert_rowid(), user.id, token)?;
    Ok(Redirect::to("/projects").into_response())
}

/// Met à jour un projet (nom, dépôt git) et le token de l'appelant.
pub async fn update_project(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Form(f): Form<ProjectForm>,
) -> AppResult {
    let (name, repo, token) = f.trimmed();
    let extras = validate_project(name, repo, token).and_then(|_| f.extras());
    let (branch, publish) = match extras {
        Ok(e) => e,
        Err(msg) => return Ok(projects_error(msg)),
    };
    let conn = state.conn()?;
    match conn.execute(
        "UPDATE projects SET name = ?1, git_repo = ?2, default_branch = ?3, publish_name = ?4 WHERE id = ?5",
        rusqlite::params![name, repo, branch, publish, id],
    ) {
        Err(e) => return Ok(projects_error(project_write_error(e)?)),
        // Projet inexistant : ne pas créer de token orphelin (FK).
        Ok(0) => return Ok(Redirect::to("/projects").into_response()),
        Ok(_) => {}
    }
    save_token(&conn, id, user.id, token)?;
    Ok(Redirect::to("/projects").into_response())
}

/// Supprime un projet : les tâches de tous les utilisateurs qui y
/// référençaient sont conservées sans projet, les tokens supprimés (FK).
pub async fn delete_project(State(state): State<AppState>, Path(id): Path<i64>) -> AppResult {
    state.conn()?.execute("DELETE FROM projects WHERE id = ?1", [id])?;
    Ok(Redirect::to("/projects").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_urls() {
        for ok in ["https://github.com/a/b", "HTTP://x", "ssh://git@x/y", "git@github.com:a/b"] {
            assert!(is_safe_repo_url(ok), "{ok}");
        }
        for bad in ["javascript:alert(1)", "file:///etc", "/local/path", ""] {
            assert!(!is_safe_repo_url(bad), "{bad}");
        }
        assert!(!is_http_url("git@github.com:a/b"));
    }

    #[test]
    fn project_validation() {
        assert!(validate_project("P", "", "").is_ok());
        assert!(validate_project("P", "https://x/y", "ghp_abc").is_ok());
        assert!(validate_project("", "", "").is_err());
        assert!(validate_project(&"é".repeat(64), "", "").is_ok());
        assert!(validate_project(&"é".repeat(65), "", "").is_err());
        assert!(validate_project("P", "ftp://x", "").is_err());
        assert!(validate_project("P", "", "a b").is_err());
        assert!(validate_project("P", "", &"a".repeat(257)).is_err());
    }
}
