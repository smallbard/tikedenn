//! Commandes git (via le binaire `git`, pour profiter de la configuration
//! et des identifiants de l'utilisateur) et recherche des clones locaux.

use crate::config::Config;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tikedenn_common::api::Project;
use tikedenn_common::normalize_repo_url;

/// Lance `git -C dir args…` et renvoie sa sortie standard (sans espaces
/// de fin) ; l'erreur contient la sortie d'erreur de git.
pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("Could not run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Verrou des fetch du worker : les tâches parallèles partagent les refs du
/// même dépôt (worktrees), et deux fetch simultanés se disputent la mise à
/// jour d'une même ref (`cannot lock ref`).
static FETCH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Erreur de git due à une ref ou un fichier verrouillé par un autre
/// processus git (passagère : un nouvel essai suffit).
fn is_lock_error(msg: &str) -> bool {
    ["cannot lock ref", "unable to update local ref", ".lock': File exists", "Unable to create '"]
        .iter()
        .any(|p| msg.contains(p))
}

/// `git fetch origin --prune`, un à la fois dans le worker, avec jusqu'à
/// trois essais si une ref a été verrouillée entre-temps (fetch lancé par un
/// agent ou par l'utilisateur dans le même dépôt).
pub fn fetch(dir: &Path) -> Result<()> {
    let _guard = FETCH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut attempt = 1;
    loop {
        match git(dir, &["fetch", "origin", "--prune"]) {
            Ok(_) => return Ok(()),
            Err(e) if attempt < 3 && is_lock_error(&e.to_string()) => {
                std::thread::sleep(std::time::Duration::from_secs(2 * attempt));
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Vrai si la commande git réussit (test d'existence d'une ref, etc.).
pub fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn remote_branch_exists(repo: &Path, branch: &str) -> bool {
    git_ok(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/remotes/origin/{branch}")])
}

/// Branche de base du projet : celle configurée, sinon main, sinon master.
pub fn base_branch(repo: &Path, project: &Project) -> Result<String> {
    let configured = project.default_branch.trim();
    if !configured.is_empty() {
        return Ok(configured.to_string());
    }
    for b in ["main", "master"] {
        if remote_branch_exists(repo, b) {
            return Ok(b.to_string());
        }
    }
    bail!("No base branch (origin/main or origin/master) in {}", repo.display())
}

/// Branche courante (None en HEAD détachée).
pub fn current_branch(dir: &Path) -> Option<String> {
    git(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok()
}

pub fn is_clean(dir: &Path) -> bool {
    git(dir, &["status", "--porcelain"]).map(|s| s.is_empty()).unwrap_or(false)
}

/// URL du remote `origin` du dépôt contenant `dir`.
pub fn origin_url(dir: &Path) -> Option<String> {
    git(dir, &["remote", "get-url", "origin"]).ok().filter(|u| !u.is_empty())
}

/// Clone local d'un projet : chemin explicite de la configuration, sinon
/// le sous-dossier de `repos_root` dont le remote origin correspond au
/// dépôt du projet.
pub fn find_local_repo(cfg: &Config, project: &Project) -> Option<PathBuf> {
    if let Some(p) = cfg.project_path(&project.name) {
        return p.join(".git").exists().then_some(p);
    }
    if project.git_repo.trim().is_empty() {
        return None;
    }
    let wanted = normalize_repo_url(&project.git_repo);
    let entries = std::fs::read_dir(cfg.repos_root()).ok()?;
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join(".git").is_dir())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .find(|d| origin_url(d).map(|u| normalize_repo_url(&u) == wanted).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::is_lock_error;

    #[test]
    fn detects_ref_lock_errors() {
        assert!(is_lock_error("error: cannot lock ref 'refs/remotes/origin/dependabot/cargo/syn-3.0.9': is at 3d47e78 but expected 1a2b3c4"));
        assert!(is_lock_error(" ! 1a2b3c4..3d47e78  x -> origin/x  (unable to update local ref)"));
        assert!(is_lock_error("fatal: Unable to create '/r/.git/shallow.lock': File exists."));
        assert!(!is_lock_error("fatal: could not read Username for 'https://github.com'"));
    }
}
