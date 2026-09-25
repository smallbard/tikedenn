//! Diff de la branche d'une tâche vs main : clone temporaire du dépôt git du
//! projet (git2), diff d'arbres, rendu par fichier.

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
use git2::{Cred, DiffFlags, FetchOptions, Patch, RemoteCallbacks, Repository};
use rand::Rng;

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

/// Clone le dépôt dans `workdir` et calcule le diff (arbres de commits)
/// `branch` vs la branche de base (main, sinon master).
/// `token` : token optionnel de l'utilisateur, utilisé en authentification
/// http(s) (dépôts privés GitHub/GitLab/Gitea). Renvoie (nom de la base, fichiers).
fn compute_branch_diff(
    repo_url: &str,
    branch: &str,
    token: &str,
    workdir: &std::path::Path,
) -> Result<(String, Vec<DiffFile>), String> {
    // Clone manuel (init + remote + fetch) pour pouvoir injecter un callback
    // d'authentification avant le téléchargement.
    let repo =
        Repository::init(workdir).map_err(|e| format!("Could not initialize the clone: {e}"))?;
    let mut remote = repo
        .remote("origin", repo_url)
        .map_err(|e| format!("Could not add the remote {repo_url}: {e}"))?;
    let mut opts = FetchOptions::new();
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
        .fetch::<&str>(&[], Some(&mut opts), None)
        .map_err(|e| format!("Could not clone {repo_url}: {e}"))?;
    let (base, base_commit) = find_commit_named(
        &repo,
        &[
            ("main", "refs/remotes/origin/main"),
            ("main", "refs/heads/main"),
            ("master", "refs/remotes/origin/master"),
            ("master", "refs/heads/master"),
        ],
    )
    .ok_or_else(|| "Base branch not found (main / master).".to_string())?;
    let (_, head_commit) = find_commit_named(
        &repo,
        &[
            (branch, &format!("refs/heads/{branch}")),
            (branch, &format!("refs/remotes/origin/{branch}")),
        ],
    )
    .ok_or_else(|| format!("Branch '{branch}' not found in the repository."))?;

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

/// Page du diff entre la branche de la tâche et main.
pub async fn task_diff_page(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> AppResult {
    // Connexion rendue au pool avant le clone (qui peut durer).
    let task = fetch_one_task(&*state.conn()?, user.id, id)?;
    let page = |task, status, result: Result<(String, Vec<DiffFile>), String>| -> Response {
        let (base, files, error) = match result {
            Ok((base, files)) => (base, files, None),
            Err(e) => (String::new(), vec![], Some(e)),
        };
        let mut resp = render(&TaskDiffTemplate {
            user: Some(user.clone()),
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

    // Clone réseau + diff : bloquant, donc hors des threads du runtime async.
    let workdir = std::env::temp_dir().join(format!(
        "tikedenn-git-{id}-{}",
        rand::thread_rng().gen::<u32>()
    ));
    let result = tokio::task::spawn_blocking(move || {
        let res = compute_branch_diff(&repo_url, &branch, &token, &workdir);
        let _ = std::fs::remove_dir_all(&workdir); // nettoyage best effort
        res
    })
    .await
    .unwrap_or_else(|e| Err(format!("Diff task failed: {e}")));
    let status = if result.is_ok() { StatusCode::OK } else { StatusCode::INTERNAL_SERVER_ERROR };
    Ok(page(task, status, result))
}
