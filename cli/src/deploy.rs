//! Déploiement d'une tâche approuvée (état « deploy ») : merge de sa
//! branche dans la branche de base (dans un worktree dédié), push, puis
//! `publish <nom>` si le projet a un nom de publication. Succès → « done »
//! avec le log de publication ; échec → retour en « review » avec l'erreur.

use crate::config::tail_file;
use crate::git::{self, base_branch, git, remote_branch_exists};
use crate::worker::{log_file, task_worktree, Ctx};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tikedenn_common::api::{Project, TaskDetail};

/// Résultat d'un déploiement réussi.
struct Deployed {
    summary: String,
    publish_log: Option<String>,
}

/// Merge la branche de la tâche dans la base, dans un worktree détaché
/// (le clone de l'utilisateur n'est pas touché), et pousse. Renvoie le
/// worktree (au commit poussé).
fn merge_and_push(ctx: &Ctx, task: &TaskDetail, repo: &Path, base: &str, dry_run: bool) -> Result<(PathBuf, String)> {
    let id = task.summary.id;
    let branch = task.summary.branch.trim();
    let project = task.repo.as_ref().context("The task has no project.")?;
    let wt = ctx
        .cfg
        .work_dir()
        .join("worktrees")
        .join("deploy")
        .join(format!("{}-{id}", project.id));
    // Deux essais : si la base a avancé entre le fetch et le push, on refait le merge.
    for attempt in 0..2 {
        git(repo, &["fetch", "origin", "--prune"])?;
        if wt.exists() {
            let _ = git(repo, &["worktree", "remove", "--force", &wt.to_string_lossy()]);
            let _ = std::fs::remove_dir_all(&wt);
        }
        let _ = git(repo, &["worktree", "prune"]);
        std::fs::create_dir_all(wt.parent().unwrap_or(repo))?;
        git(repo, &["worktree", "add", "--detach", &wt.to_string_lossy(), &format!("origin/{base}")])?;
        if !branch.is_empty() {
            if !remote_branch_exists(repo, branch) {
                bail!("Branch `{branch}` is not on the remote (was it pushed?).");
            }
            let message = format!("Merge task #{id}: {}\n\nBranch: {branch}", task.summary.title);
            if let Err(e) = git(&wt, &["merge", "--no-ff", "-m", &message, &format!("origin/{branch}")]) {
                let _ = git(&wt, &["merge", "--abort"]);
                bail!("Merge conflict between `{branch}` and `{base}`: the branch must be rebased on {base}.\n{e}");
            }
        }
        let head = git(&wt, &["rev-parse", "--short", "HEAD"])?;
        if dry_run {
            return Ok((wt, head));
        }
        match git(&wt, &["push", "origin", &format!("HEAD:refs/heads/{base}")]) {
            Ok(_) => return Ok((wt, head)),
            Err(e) if attempt == 0 => eprintln!("[#{id}] push rejected, retrying: {e}"),
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// Lance `publish <nom> --repo <dir>` et renvoie la fin de son log.
fn publish(ctx: &Ctx, id: i64, name: &str, dir: &Path) -> Result<String> {
    let cmd = &ctx.cfg.worker.publish_command;
    if cmd.is_empty() {
        bail!("No publish command configured.");
    }
    let (log_path, log) = log_file(&ctx.cfg, &format!("deploy-{id}"))?;
    println!("[#{id}] publishing with `{} {name}` (log {})", cmd.join(" "), log_path.display());
    let status = Command::new(&cmd[0])
        .args(&cmd[1..])
        .arg(name)
        .arg("--repo")
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .status()
        .with_context(|| format!("Could not run {}", cmd[0]))?;
    let tail = tail_file(&log_path, 150, 15_000);
    if !status.success() {
        bail!("`{} {name}` failed ({status}).\n\n```\n{tail}\n```", cmd.join(" "));
    }
    Ok(tail)
}

fn deploy(ctx: &Ctx, task: &TaskDetail, project: &Project, repo: &Path, dry_run: bool) -> Result<Deployed> {
    let id = task.summary.id;
    let base = base_branch(repo, project)?;
    let (wt, head) = merge_and_push(ctx, task, repo, &base, dry_run)?;
    let branch = task.summary.branch.trim();
    let mut summary = if branch.is_empty() {
        format!("No branch to merge; `{base}` is at {head}.")
    } else if dry_run {
        format!("Dry run: `{branch}` merges cleanly into `{base}` ({head}, not pushed).")
    } else {
        format!("Merged `{branch}` into `{base}` ({head}) and pushed.")
    };
    let mut publish_log = None;
    let name = project.publish_name.trim();
    if !name.is_empty() && !dry_run {
        // Publication depuis le clone de l'utilisateur s'il est propre et sur
        // la base (build incrémental), sinon depuis le worktree de déploiement.
        let dir = if git::current_branch(repo).as_deref() == Some(base.as_str()) && git::is_clean(repo) {
            git(repo, &["merge", "--ff-only", &format!("origin/{base}")])?;
            repo.to_path_buf()
        } else {
            wt.clone()
        };
        let log = publish(ctx, id, name, &dir).map_err(|e| {
            anyhow::anyhow!("The code was merged into `{base}` ({head}) but publishing failed: {e:#}")
        })?;
        summary.push_str(&format!(" Published with `publish {name}`."));
        publish_log = Some(log);
    } else if !name.is_empty() {
        summary.push_str(&format!(" Would publish with `publish {name}`."));
    }
    let _ = git(repo, &["worktree", "remove", "--force", &wt.to_string_lossy()]);
    if !dry_run {
        // Tâche terminée : son worktree de travail et sa branche locale ne servent plus.
        let task_wt = task_worktree(&ctx.cfg, project, id);
        if task_wt.exists() {
            let _ = git(repo, &["worktree", "remove", "--force", &task_wt.to_string_lossy()]);
        }
        if !branch.is_empty() {
            let _ = git(repo, &["branch", "-D", branch]);
        }
    }
    Ok(Deployed { summary, publish_log })
}

/// Déploie une tâche prise en charge (état deploy) et rend compte sur le
/// tableau : done si tout a réussi, review avec l'erreur sinon. En
/// `dry_run`, rien n'est poussé ni publié et la tâche reste en deploy.
pub fn deploy_and_report(ctx: &Ctx, task: TaskDetail, project: &Project, repo: &Path, dry_run: bool) {
    let id = task.summary.id;
    println!("[#{id}] deploying {} ({})", task.summary.title, project.name);
    let result = deploy(ctx, &task, project, repo, dry_run);
    let report = match result {
        Ok(d) => {
            println!("[#{id}] {}", d.summary);
            if dry_run {
                ctx.client.release(id, None, Some(&d.summary)).map(|_| ())
            } else {
                let log = d
                    .publish_log
                    .map(|l| ctx.client.comment(id, &format!("publish output (tail):\n{l}"), "deploy"))
                    .unwrap_or(Ok(()));
                log.and_then(|_| ctx.client.move_to(id, "done", Some(&d.summary)).map(|_| ()))
            }
        }
        Err(e) => {
            eprintln!("[#{id}] deploy failed: {e:#}");
            let note = format!("Deploy failed: {e:#}");
            if dry_run {
                ctx.client.release(id, None, Some(&note)).map(|_| ())
            } else {
                ctx.client.move_to(id, "review", Some(&note)).map(|_| ())
            }
        }
    };
    if let Err(e) = report {
        eprintln!("[#{id}] could not update the board: {e:#}");
    }
}
