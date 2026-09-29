//! Diff de la branche d'une tâche vs la branche de base du projet : clone en
//! cache du dépôt git du projet (git2), diff d'arbres depuis le point de
//! divergence, rendu par fichier. C'est la page de revue humaine.

use crate::auth::CurrentUser;
use crate::board::{fetch_one_task, is_safe_branch, TaskRow};
use crate::projects::{is_http_url, is_safe_repo_url};
use crate::web::{render, AppResult};
use crate::AppState;
use askama::Template;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::Response,
};
use git2::{Cred, DiffFlags, FetchOptions, FetchPrune, Patch, RemoteCallbacks, Repository};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Une ligne d'un diff unifié.
pub struct DiffLine {
    /// "add" | "del" | "ctx" | "hdr".
    pub kind: &'static str,
    pub text: String,
}

/// Un fichier modifié, avec ses lignes de diff.
pub struct DiffFile {
    pub path: String,
    pub added: usize,
    pub deleted: usize,
    pub binary: bool,
    pub lines: Vec<DiffLine>,
}

#[derive(Template)]
#[template(path = "task_diff.html")]
pub struct TaskDiffTemplate {
    pub user: Option<CurrentUser>,
    pub task: TaskRow,
    /// Branche de base réellement utilisée (main, master…).
    pub base: String,
    pub files: Vec<DiffFile>,
    pub error: Option<String>,
    /// Filtre projet du tableau à préserver (formulaires de revue).
    pub filter: i64,
}

/// Trouve la première ref existante parmi `pairs` (nom affiché, ref) et renvoie
/// son commit.
fn find_commit_named<'a>(
    repo: &'a Repository,
    pairs: &[(&str, &str)],
) -> Option<(String, git2::Commit<'a>)> {
    pairs.iter().find_map(|(name, refname)| {
        let commit = repo.find_reference(refname).ok()?.peel_to_commit().ok()?;
        Some((name.to_string(), commit))
    })
}

/// Lignes d'un patch texte, avec le nombre de lignes ajoutées / supprimées.
fn patch_lines(patch: &Patch) -> Result<(Vec<DiffLine>, usize, usize), String> {
    let mut lines = vec![];
    let (mut added, mut deleted) = (0, 0);
    for h in 0..patch.num_hunks() {
        let (_hunk, num_lines) = patch
            .hunk(h)
            .map_err(|e| format!("Could not read a diff hunk: {e}"))?;
        for l in 0..num_lines {
            let line = patch
                .line_in_hunk(h, l)
                .map_err(|e| format!("Could not read a diff line: {e}"))?;
            let kind = match line.origin() {
                '+' => {
                    added += 1;
                    "add"
                }
                '-' => {
                    deleted += 1;
                    "del"
                }
                ' ' | '=' | '>' | '<' => "ctx",
                _ => "hdr",
            };
            lines.push(DiffLine {
                kind,
                text: String::from_utf8_lossy(line.content()).trim_end().to_string(),
            });
        }
    }
    Ok((lines, added, deleted))
}

/// Répertoire des clones en cache (un dépôt nu par projet), réutilisés d'un
/// affichage de diff à l'autre : seul un `fetch` incrémental est refait.
fn cache_root() -> PathBuf {
    std::env::var("GIT_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("tikedenn-git-cache"))
}

/// Verrou par projet : deux diffs simultanés ne fetchent pas le même cache.
fn project_lock(project_id: i64) -> Arc<Mutex<()>> {
    static LOCKS: Mutex<Option<HashMap<i64, Arc<Mutex<()>>>>> = Mutex::new(None);
    let mut guard = LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    guard.get_or_insert_with(HashMap::new).entry(project_id).or_default().clone()
}

/// Ouvre (ou crée) le dépôt nu en cache du projet, avec `origin` pointant
/// vers `repo_url`.
fn open_cache(path: &std::path::Path, repo_url: &str) -> Result<Repository, String> {
    let repo = match Repository::open_bare(path) {
        Ok(r) => r,
        Err(_) => {
            let _ = std::fs::remove_dir_all(path);
            Repository::init_bare(path).map_err(|e| format!("Could not initialize the clone: {e}"))?
        }
    };
    let same_url = repo
        .find_remote("origin")
        .ok()
        .and_then(|r| r.url().ok().map(|u| u == repo_url))
        .unwrap_or(false);
    if !same_url {
        let _ = repo.remote_delete("origin");
        repo.remote("origin", repo_url)
            .map_err(|e| format!("Could not add the remote {repo_url}: {e}"))?;
    }
    Ok(repo)
}

/// Met à jour le clone en cache du projet et calcule le diff de `branch`
/// depuis son point de divergence avec la branche de base (`base_pref`, ou
/// main puis master) — comme la vue « three-dot » des forges.
/// `token` : token optionnel de l'utilisateur, utilisé en authentification
/// http(s) (dépôts privés GitHub/GitLab/Gitea). Le fetch est refait à chaque
/// fois (et doit réussir) : le cache ne donne jamais accès à un dépôt privé
/// sans token valide. Renvoie (nom de la base, fichiers).
fn compute_branch_diff(
    project_id: i64,
    repo_url: &str,
    branch: &str,
    base_pref: &str,
    token: &str,
) -> Result<(String, Vec<DiffFile>), String> {
    let lock = project_lock(project_id);
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let repo = open_cache(&cache_root().join(format!("project-{project_id}")), repo_url)?;
    let mut remote = repo
        .find_remote("origin")
        .map_err(|e| format!("Could not open the remote: {e}"))?;
    let mut opts = FetchOptions::new();
    opts.prune(FetchPrune::On);
    if is_http_url(repo_url) && !token.is_empty() {
        let tok = token.to_string();
        let mut cbs = RemoteCallbacks::new();
        // N'importe quel pseudo fonctionne avec les tokens GitHub / GitLab / Gitea ;
        // le token sert de mot de passe (basic auth https).
        cbs.credentials(move |_url, _username, _allowed_types| {
            Cred::userpass_plaintext("x-access-token", &tok)
        });
        opts.remote_callbacks(cbs);
    }
    remote
        .fetch(&["+refs/heads/*:refs/remotes/origin/*"], Some(&mut opts), None)
        .map_err(|e| format!("Could not fetch {repo_url}: {e}"))?;
    let preferred = format!("refs/remotes/origin/{base_pref}");
    let bases: Vec<(&str, &str)> = if base_pref.is_empty() {
        vec![("main", "refs/remotes/origin/main"), ("master", "refs/remotes/origin/master")]
    } else {
        vec![(base_pref, preferred.as_str())]
    };
    let (base, base_commit) = find_commit_named(&repo, &bases).ok_or_else(|| {
        if base_pref.is_empty() {
            "Base branch not found (main / master).".to_string()
        } else {
            format!("Base branch '{base_pref}' not found.")
        }
    })?;
    let (_, head_commit) = find_commit_named(&repo, &[(branch, &format!("refs/remotes/origin/{branch}"))])
        .ok_or_else(|| format!("Branch '{branch}' not found in the repository (not pushed yet?)."))?;
    // Point de divergence : les commits arrivés sur la base depuis ne
    // polluent pas le diff.
    let base_commit = match repo.merge_base(base_commit.id(), head_commit.id()) {
        Ok(oid) => repo.find_commit(oid).map_err(|e| format!("Could not read the merge base: {e}"))?,
        Err(_) => base_commit,
    };

    let base_tree = base_commit
        .tree()
        .map_err(|e| format!("Could not read the base tree: {e}"))?;
    let head_tree = head_commit
        .tree()
        .map_err(|e| format!("Could not read the branch tree: {e}"))?;
    let diff = repo
        .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), None)
        .map_err(|e| format!("Could not compute the diff: {e}"))?;

    let mut files = vec![];
    for (idx, delta) in diff.deltas().enumerate() {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "?".to_string());
        // Binaire, ou patch texte indisponible : on n'affiche pas le contenu.
        let patch = match Patch::from_diff(&diff, idx) {
            Ok(Some(p)) if !delta.flags().contains(DiffFlags::BINARY) => p,
            _ => {
                files.push(DiffFile {
                    path,
                    added: 0,
                    deleted: 0,
                    binary: true,
                    lines: vec![],
                });
                continue;
            }
        };
        let (lines, added, deleted) = patch_lines(&patch)?;
        files.push(DiffFile {
            path,
            added,
            deleted,
            binary: false,
            lines,
        });
    }
    Ok((base, files))
}

/// Page du diff entre la branche de la tâche et la branche de base, avec le
/// panneau de revue (approuver / demander des modifications) si la tâche
/// attend une revue.
pub async fn task_diff_page(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> AppResult {
    // Connexion rendue au pool avant le clone (qui peut durer).
    let task = fetch_one_task(&*state.conn()?, user.id, id)?;
    let page = |task: TaskRow, status, result: Result<(String, Vec<DiffFile>), String>| -> Response {
        let (base, files, error) = match result {
            Ok((base, files)) => (base, files, None),
            Err(e) => (String::new(), vec![], Some(e)),
        };
        let filter = task.project_id_or_zero();
        let mut resp = render(&TaskDiffTemplate {
            user: Some(user.clone()),
            filter,
            task,
            base,
            files,
            error,
        });
        *resp.status_mut() = status;
        resp
    };

    let branch = task.branch.trim().to_string();
    if branch.is_empty() {
        let msg = "No branch associated with this task. Edit the task to set a branch.";
        return Ok(page(task, StatusCode::NOT_FOUND, Err(msg.into())));
    }
    if !is_safe_branch(&branch) {
        return Ok(page(task, StatusCode::UNPROCESSABLE_ENTITY, Err("Invalid branch name.".into())));
    }
    let Some(project) = task.project.as_ref() else {
        let msg = "This task has no project — the diff needs a git repository.";
        return Ok(page(task, StatusCode::NOT_FOUND, Err(msg.into())));
    };
    let repo_url = project.git_repo.trim().to_string();
    if !is_safe_repo_url(&repo_url) {
        let msg = "The project has no valid git repository.";
        return Ok(page(task, StatusCode::UNPROCESSABLE_ENTITY, Err(msg.into())));
    }
    let token = project.token.trim().to_string();
    let base_pref = project.default_branch.trim().to_string();
    let project_id = project.id;

    // Fetch réseau + diff : bloquant, donc hors des threads du runtime async.
    let result = tokio::task::spawn_blocking(move || {
        compute_branch_diff(project_id, &repo_url, &branch, &base_pref, &token)
    })
    .await
    .unwrap_or_else(|e| Err(format!("Diff task failed: {e}")));
    let status = if result.is_ok() { StatusCode::OK } else { StatusCode::INTERNAL_SERVER_ERROR };
    Ok(page(task, status, result))
}
