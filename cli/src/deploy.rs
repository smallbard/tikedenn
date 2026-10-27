//! Déploiement d'une tâche approuvée (état « deploy ») : merge de sa
//! branche dans la branche de base (dans un worktree dédié), push, puis
//! `publish <nom>` si le projet a un nom de publication. Succès → « done »
//! avec le log de publication ; échec → retour en « review » avec l'erreur.
//! En cas de conflit de merge, l'agent du projet tente d'abord de rebaser la
//! branche sur la base ; la tâche revient en revue dans tous les cas (la
//! résolution des conflits doit être relue avant le déploiement).

use crate::config::tail_file;
use crate::git::{self, base_branch, git, git_ok, remote_branch_exists};
use crate::render::task_markdown;
use crate::worker::{log_file, prepare_worktree, project_agent, run_agent, task_worktree, Ctx, Outcome};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tikedenn_common::api::{Project, TaskDetail};
use tikedenn_common::NO_AGENT;

/// La branche de la tâche ne se merge plus proprement dans la base.
#[derive(Debug)]
pub struct MergeConflict {
    pub branch: String,
    pub base: String,
    pub detail: String,
}

impl std::fmt::Display for MergeConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Merge conflict between `{}` and `{}`: the branch must be rebased on {}.\n{}",
            self.branch, self.base, self.base, self.detail
        )
    }
}

impl std::error::Error for MergeConflict {}

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
                bail!("Branch `{branch}` is not on the remote: nothing was pushed, so there is nothing to deploy. Move the task back to To do.");
            }
            let message = format!("Merge task #{id}: {}\n\nBranch: {branch}", task.summary.title);
            if let Err(e) = git(&wt, &["merge", "--no-ff", "-m", &message, &format!("origin/{branch}")]) {
                let files = git(&wt, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
                let _ = git(&wt, &["merge", "--abort"]);
                let detail = if files.trim().is_empty() {
                    format!("{e:#}")
                } else {
                    format!("Conflicting files: {}", files.split_whitespace().collect::<Vec<_>>().join(", "))
                };
                return Err(MergeConflict { branch: branch.into(), base: base.into(), detail }.into());
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

/// Consigne donnée à l'agent pour rebaser la branche d'une tâche approuvée.
fn rebase_prompt(task: &TaskDetail, c: &MergeConflict) -> String {
    let (id, branch, base) = (task.summary.id, &c.branch, &c.base);
    format!(
        r#"You are an autonomous coding agent. Task #{id} of the Tikedenn board was approved by a human, but its branch
`{branch}` no longer merges into `{base}` (other work landed on `{base}` since). Your working directory is a dedicated
git worktree on `{branch}`, at the approved commit.

<task>
{card}
</task>

<merge-error>
{detail}
</merge-error>

Your only job is to rebase `{branch}` on `{base}`:
1. `git fetch origin && git rebase origin/{base}`. Resolve each conflict keeping the intent of both sides: the task's
   change and what landed on `{base}`. Do not add unrelated changes.
2. Build and run the relevant tests (following the repository's CLAUDE.md / AGENTS.md).
3. Push the rebased branch: `git push --force-with-lease origin {branch}`. Never push to `{base}`.
4. Do not change the task's status (no `tkd submit`, `tkd release`): the worker checks the result and sends the task
   back to human review, who will read your resolution.
If you cannot resolve the conflicts safely, run `git rebase --abort` and explain why.
End with a short report: the conflicting files, how you resolved each one, and the tests you ran."#,
        card = task_markdown(task),
        detail = c.detail.trim(),
    )
}

/// Vrai si une opération git (rebase, merge) est restée en cours dans `wt`.
fn operation_in_progress(wt: &Path) -> bool {
    ["rebase-merge", "rebase-apply", "MERGE_HEAD"].iter().any(|p| {
        git(wt, &["rev-parse", "--git-path", p]).map(|path| wt.join(path).exists()).unwrap_or(false)
    })
}

/// Fait rebaser la branche par l'agent du projet. Renvoie le compte rendu à
/// joindre au retour en revue si la branche poussée contient désormais la
/// base, une erreur sinon.
fn rebase_with_agent(ctx: &Ctx, agent: &str, task: &TaskDetail, project: &Project, repo: &Path, c: &MergeConflict) -> Result<String> {
    let id = task.summary.id;
    let (branch, base) = (c.branch.as_str(), c.base.as_str());
    let wt = task_worktree(&ctx.cfg, project, id);
    prepare_worktree(repo, &wt, branch, base)?;
    // On repart exactement de la branche approuvée (les restes locaux de la
    // phase de travail ont été poussés ou relus avant l'approbation).
    let _ = git(&wt, &["rebase", "--abort"]);
    let _ = git(&wt, &["merge", "--abort"]);
    git(&wt, &["checkout", "-B", branch, &format!("origin/{branch}")])?;
    git(&wt, &["reset", "--hard", &format!("origin/{branch}")])?;

    println!("[#{id}] merge conflict: asking {agent} to rebase `{branch}` on `{base}`");
    let announce = format!("Merge conflict with `{base}`: the worker asked {agent} to rebase `{branch}`");
    let (outcome, log_path, _limit) = run_agent(ctx, agent, id, &wt, &rebase_prompt(task, c), "deploy", &announce)?;
    let tail = tail_file(&log_path, 30, 3000);
    let what = match &outcome {
        Outcome::Lost(reason) => bail!("the task was taken over ({reason})."),
        Outcome::TimedOut => format!("was stopped after {} minutes", ctx.cfg.worker.agent_timeout_minutes),
        Outcome::Exited(s) => format!("exited ({s})"),
    };
    if operation_in_progress(&wt) {
        let _ = git(&wt, &["rebase", "--abort"]);
        let _ = git(&wt, &["merge", "--abort"]);
        bail!("{agent} {what} with the rebase unfinished.\n\nLast output:\n```\n{tail}\n```");
    }
    git(&wt, &["fetch", "origin", "--prune"])?;
    let has_base = |rev: &str| git_ok(&wt, &["merge-base", "--is-ancestor", &format!("origin/{base}"), rev]);
    let remote = format!("origin/{branch}");
    // Rebasée localement mais pas poussée : le worker pousse.
    if !has_base(&remote) && has_base("HEAD") && git::is_clean(&wt) {
        git(&wt, &["push", "--force-with-lease", "origin", &format!("HEAD:refs/heads/{branch}")])?;
        git(&wt, &["fetch", "origin", "--prune"])?;
    }
    if !has_base(&remote) {
        bail!("{agent} {what} without rebasing `{branch}` on `{base}`.\n\nLast output:\n```\n{tail}\n```");
    }
    let head = git(&wt, &["rev-parse", "--short", &remote])?;
    Ok(format!(
        "{agent} rebased `{branch}` on `{base}` and pushed it ({head}). Review the conflict resolution in the diff, \
         then approve again to deploy.\n\nAgent report (end of its log):\n```\n{tail}\n```"
    ))
}

/// Déploie une tâche prise en charge (état deploy) et rend compte sur le
/// tableau : done si tout a réussi, review avec l'erreur sinon. En
/// `dry_run`, rien n'est poussé ni publié et la tâche reste en deploy. En
/// cas de conflit de merge, `default_agent` (ou l'agent du projet) tente un
/// rebase avant le retour en revue.
pub fn deploy_and_report(ctx: &Ctx, task: TaskDetail, project: &Project, repo: &Path, dry_run: bool, default_agent: &str) {
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
            let mut note = format!("Deploy failed: {e:#}");
            if let Some(conflict) = e.downcast_ref::<MergeConflict>().filter(|_| !dry_run && project.agent != NO_AGENT) {
                let agent = project_agent(project, default_agent);
                match rebase_with_agent(ctx, agent, &task, project, repo, conflict) {
                    Ok(report) => {
                        println!("[#{id}] rebased by {agent}; back to review");
                        note.push_str(&format!("\n\n{report}"));
                    }
                    Err(e) => {
                        eprintln!("[#{id}] rebase by {agent} failed: {e:#}");
                        note.push_str(&format!("\n\nAutomatic rebase failed: {e:#}"));
                    }
                }
            }
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
