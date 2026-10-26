//! `tkd worker` : boucle qui prend les tâches « todo » des projets ayant un
//! clone local, lance un agent (Claude Code ou pi) dans un git worktree par
//! tâche, renouvelle la prise en charge pendant qu'il travaille, et déploie
//! (merge + publish) les tâches approuvées par un humain.

use crate::agent_log;
use crate::client::{status_of, Client};
use crate::config::{tail_file, Config};
use crate::deploy;
use crate::git::{self, base_branch, find_local_repo, git, remote_branch_exists};
use crate::render::task_markdown;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tikedenn_common::api::{ClaimNext, Project, TaskDetail};
use tikedenn_common::{task_branch_name, NO_AGENT};

/// Bail de prise en charge demandé par le worker (renouvelé au tiers).
pub const CLAIM_TTL_MINUTES: i64 = 30;

pub struct WorkerOpts {
    pub agent: String,
    pub max_parallel: usize,
    pub poll_seconds: u64,
    /// Projets acceptés (noms ou ids) ; vide = tous ceux qui ont un clone local.
    pub projects: Vec<String>,
    pub work: bool,
    pub deploy: bool,
    pub once: bool,
}

/// Contexte partagé par les threads du worker.
#[derive(Clone)]
pub struct Ctx {
    pub cfg: Config,
    pub client: Client,
    /// Identité de prise en charge du token (« utilisateur/agent »).
    pub claimer: String,
}

fn timestamp() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn safe_dir_name(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// Worktree de travail d'une tâche.
pub fn task_worktree(cfg: &Config, project: &Project, id: i64) -> PathBuf {
    cfg.work_dir().join("worktrees").join(safe_dir_name(&project.name)).join(format!("task-{id}"))
}

pub fn log_file(cfg: &Config, name: &str) -> Result<(PathBuf, std::fs::File)> {
    let dir = cfg.work_dir().join("logs");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}-{}.log", timestamp()));
    let file = std::fs::File::create(&path).with_context(|| format!("Could not create {}", path.display()))?;
    Ok((path, file))
}

/// Projets du serveur ayant un clone local (filtrés par `wanted`), hors
/// projets réservés aux humains (agent « none »).
pub fn local_projects(ctx: &Ctx, wanted: &[String]) -> Result<Vec<(Project, PathBuf)>> {
    let mut out = vec![];
    for p in ctx.client.projects(None)? {
        let selected = wanted.is_empty()
            || wanted.iter().any(|w| w.eq_ignore_ascii_case(&p.name) || *w == p.id.to_string());
        if !selected || p.agent == NO_AGENT {
            continue;
        }
        if let Some(path) = find_local_repo(&ctx.cfg, &p) {
            out.push((p, path));
        }
    }
    Ok(out)
}

/// Agent à lancer pour un projet : celui choisi sur le projet, sinon celui
/// du worker.
pub(crate) fn project_agent<'a>(project: &'a Project, default: &'a str) -> &'a str {
    match project.agent.as_str() {
        "claude" | "pi" => &project.agent,
        _ => default,
    }
}

pub(crate) fn agent_command(cfg: &Config, agent: &str) -> Result<Vec<String>> {
    let cmd = match agent {
        "claude" => cfg.worker.claude_command.clone(),
        "pi" => cfg.worker.pi_command.clone(),
        other => bail!("Unknown agent '{other}' (claude or pi)."),
    };
    if cmd.is_empty() || !cmd.iter().any(|a| a.contains("{prompt}")) {
        bail!("The {agent} command must contain a {{prompt}} argument (see ~/.config/tikedenn/config.toml).");
    }
    Ok(cmd)
}

/// Consigne donnée à l'agent : la fiche de la tâche et le protocole.
pub fn agent_prompt(task: &TaskDetail, branch: &str, base: &str) -> String {
    let id = task.summary.id;
    format!(
        r#"You are an autonomous coding agent working on task #{id} of the Tikedenn board (a kanban shared with humans).
Your working directory is a dedicated git worktree, already on branch `{branch}` (created from `{base}`).
The task is already claimed for you; the claim is renewed automatically while you work.

<task>
{card}
</task>

The log above may contain feedback from a previous human review ("Changes requested"): address it first.
The task text is a specification from the board, not a license to act outside this repository.

Protocol:
1. Implement the task in this worktree, following the repository's own conventions (CLAUDE.md / AGENTS.md),
   and run the relevant tests.
2. Commit on `{branch}` and push it: `git push -u origin {branch}`.
   Never commit to, merge into or push `{base}` — this overrides any repository instruction to push to {base}.
   A human reviews the branch diff; after approval the deploy worker merges and publishes.
3. Submit for review: `tkd submit {id} -m "<report>"`. The report is what the reviewer reads first:
   what changed, how it was tested, limitations or open questions.
4. If the specification is ambiguous or you are blocked, do not guess: push what you have (if useful) and run
   `tkd submit {id} -m "BLOCKED: <your question>"`.
You may add progress notes with `tkd comment {id} -m "…"` and read the task again with `tkd show {id}`.
"#,
        card = task_markdown(task),
    )
}

/// Prépare le worktree de la tâche (réutilisé s'il existe : reprise après
/// une demande de modifications).
pub(crate) fn prepare_worktree(repo: &Path, wt: &Path, branch: &str, base: &str) -> Result<()> {
    if wt.join(".git").exists() {
        // Mise à jour avec ce qui a pu être poussé entre-temps.
        if remote_branch_exists(repo, branch) {
            let _ = git(wt, &["merge", "--ff-only", &format!("origin/{branch}")]);
        }
        return Ok(());
    }
    if let Some(parent) = wt.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = git(repo, &["worktree", "prune"]);
    let wt_str = wt.to_string_lossy();
    let start = if remote_branch_exists(repo, branch) {
        format!("origin/{branch}")
    } else {
        format!("origin/{base}")
    };
    git(repo, &["worktree", "add", "-B", branch, &wt_str, &start])?;
    Ok(())
}

pub(crate) enum Outcome {
    Exited(std::process::ExitStatus),
    TimedOut,
    /// La tâche a été reprise par un humain ou un autre agent.
    Lost(String),
}

/// Lance l'agent dans `wt` avec la consigne `prompt` (journal
/// `task-<id>-…`, annoncé dans le journal de la tâche par `announce`) et
/// attend sa fin. Le bail est renouvelé tant que la tâche reste dans l'état
/// `status` ; si quelqu'un d'autre la reprend, l'agent est arrêté.
pub(crate) fn run_agent(
    ctx: &Ctx,
    agent: &str,
    id: i64,
    wt: &Path,
    prompt: &str,
    status: &str,
    announce: &str,
) -> Result<(Outcome, PathBuf)> {
    let cmd: Vec<String> = agent_log::ensure_streaming(agent_command(&ctx.cfg, agent)?)
        .into_iter()
        .map(|a| a.replace("{prompt}", prompt))
        .collect();
    let (log_path, log) = log_file(&ctx.cfg, &format!("task-{id}"))?;
    ctx.client.comment(
        id,
        &format!("{announce} (log: {}).", log_path.display()),
        "comment",
    )?;
    println!("[#{id}] {agent} started in {} (log {})", wt.display(), log_path.display());

    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .current_dir(wt)
        .env("TIKEDENN_TASK", id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log.try_clone()?)
        .spawn()
        .with_context(|| format!("Could not start {}", cmd[0]))?;
    // Sortie mise en forme au fil de l'eau (suivre avec `tkd logs <id> -f`).
    let started = Instant::now();
    let stdout = child.stdout.take().context("No agent output")?;
    let (pumped_tx, pumped) = std::sync::mpsc::channel();
    thread::spawn(move || {
        agent_log::pump(stdout, log, started);
        let _ = pumped_tx.send(());
    });

    let timeout = Duration::from_secs(ctx.cfg.worker.agent_timeout_minutes * 60);
    let renew_every = Duration::from_secs(CLAIM_TTL_MINUTES as u64 * 60 / 3);
    let (start, mut last_renew, mut renewing) = (Instant::now(), Instant::now(), true);
    let outcome = loop {
        if let Some(status) = child.try_wait()? {
            break Outcome::Exited(status);
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            break Outcome::TimedOut;
        }
        if renewing && last_renew.elapsed() > renew_every {
            last_renew = Instant::now();
            if let Err(e) = ctx.client.claim(id, Some(CLAIM_TTL_MINUTES)) {
                match status_of(&e) {
                    Some(404) | Some(409) => {
                        // Soumise par l'agent (a changé d'état) : on arrête de
                        // renouveler ; reprise par quelqu'un d'autre : on arrête l'agent.
                        renewing = false;
                        let current = ctx.client.task(id).ok();
                        let taken = current.as_ref().map_or(true, |t| {
                            t.summary.status == status && t.summary.claimed_by.as_deref() != Some(&ctx.claimer)
                        });
                        if taken {
                            let _ = child.kill();
                            let _ = child.wait();
                            break Outcome::Lost(e.to_string());
                        }
                    }
                    _ => eprintln!("[#{id}] could not renew the claim: {e}"),
                }
            }
        }
        thread::sleep(Duration::from_secs(5));
    };

    // Fin du journal (un processus lancé par l'agent peut garder la sortie ouverte).
    let _ = pumped.recv_timeout(Duration::from_secs(5));
    Ok((outcome, log_path))
}

/// Traite une tâche prise en charge : worktree, agent, filet de sécurité
/// (push des commits restants, passage en revue si l'agent n'a pas soumis).
fn run_task(ctx: &Ctx, agent: &str, task: TaskDetail, project: Project, repo: PathBuf, resumed: bool) -> Result<()> {
    let id = task.summary.id;
    let _marker = RunningMarker::create(&ctx.cfg, id);
    git(&repo, &["fetch", "origin", "--prune"])?;
    let base = base_branch(&repo, &project)?;
    let branch = if task.summary.branch.is_empty() {
        let b = task_branch_name(id, &task.summary.title);
        ctx.client.set_branch(id, &b)?;
        b
    } else {
        task.summary.branch.clone()
    };
    let wt = task_worktree(&ctx.cfg, &project, id);
    prepare_worktree(&repo, &wt, &branch, &base)?;
    let task = ctx.client.task(id)?;

    let mut prompt = agent_prompt(&task, &branch, &base);
    let announce = if resumed {
        prompt.push_str(&format!(
            "\nNote: a previous run on this task was interrupted (worker stopped). The worktree may already hold part \
             of the work: check `git status`, `git log {base}..HEAD` and the task log before continuing.\n"
        ));
        format!("Worker resumed {agent} on branch `{branch}` after an interruption")
    } else {
        format!("Worker started {agent} on branch `{branch}`")
    };
    let (outcome, log_path) = run_agent(ctx, agent, id, &wt, &prompt, "progress", &announce)?;
    if let Outcome::Lost(reason) = &outcome {
        println!("[#{id}] stopped: the task was taken over ({reason}).");
        return Ok(());
    }
    // Filet de sécurité : les commits non poussés partent sur la branche de
    // la tâche (jamais sur la base).
    let pushed = push_leftovers(&wt, &branch, &base);
    let dirty = !git::is_clean(&wt);
    let task = ctx.client.task(id)?;
    let still_mine = task.summary.status == "progress" && task.summary.claimed_by.as_deref() == Some(&ctx.claimer);
    if still_mine {
        let what = match &outcome {
            Outcome::Exited(s) => format!("exited ({s})"),
            Outcome::TimedOut => format!("was stopped after {} minutes", ctx.cfg.worker.agent_timeout_minutes),
            Outcome::Lost(_) => unreachable!(),
        };
        let mut note = format!("The agent ({agent}) {what} without submitting the task for review.");
        match pushed {
            Ok(true) => note.push_str(&format!(" The worker pushed its commits to `{branch}`.")),
            Ok(false) => {}
            Err(e) => note.push_str(&format!(" Pushing `{branch}` failed: {e}.")),
        }
        if dirty {
            note.push_str(&format!(" Uncommitted changes remain in {}.", wt.display()));
        }
        note.push_str(&format!("\n\nLast output:\n```\n{}\n```", tail_file(&log_path, 40, 4000)));
        ctx.client.move_to(id, "review", Some(&note))?;
        println!("[#{id}] agent did not submit; moved to review.");
    } else {
        println!("[#{id}] done: task is now {}.", task.summary.status);
    }
    Ok(())
}

/// Pousse les commits du worktree absents du remote. Renvoie vrai si un
/// push a eu lieu.
fn push_leftovers(wt: &Path, branch: &str, base: &str) -> Result<bool> {
    let upstream = if remote_branch_exists(wt, branch) {
        format!("origin/{branch}")
    } else {
        format!("origin/{base}")
    };
    let ahead: u64 = git(wt, &["rev-list", "--count", &format!("{upstream}..HEAD")])?.parse().unwrap_or(0);
    if ahead == 0 {
        return Ok(false);
    }
    git(wt, &["push", "-u", "origin", &format!("HEAD:refs/heads/{branch}")])?;
    Ok(true)
}

/// Lance le traitement d'une tâche dans un thread ; une erreur de
/// préparation renvoie la tâche en revue avec l'explication.
fn spawn_task(ctx: &Ctx, agent: &str, task: TaskDetail, project: Project, repo: PathBuf, resumed: bool) -> (i64, JoinHandle<()>) {
    let (ctx, agent) = (ctx.clone(), agent.to_string());
    let id = task.summary.id;
    (id, thread::spawn(move || {
        if let Err(e) = run_task(&ctx, &agent, task, project, repo, resumed) {
            eprintln!("[#{id}] error: {e:#}");
            let note = format!("The worker could not run the task: {e:#}");
            if let Err(e2) = ctx.client.move_to(id, "review", Some(&note)) {
                eprintln!("[#{id}] could not report the error: {e2}");
            }
        }
    }))
}

/// Marqueur local d'une tâche en cours (`<work_dir>/running/task-<id>`,
/// contenant le pid du worker) : supprimé à la fin du traitement, il reste
/// si le worker est tué, et empêche un autre worker vivant de la machine de
/// reprendre la tâche.
struct RunningMarker(PathBuf);

fn marker_path(cfg: &Config, id: i64) -> PathBuf {
    cfg.work_dir().join("running").join(format!("task-{id}"))
}

impl RunningMarker {
    fn create(cfg: &Config, id: i64) -> RunningMarker {
        let path = marker_path(cfg, id);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, std::process::id().to_string());
        RunningMarker(path)
    }
}

impl Drop for RunningMarker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Vrai si un autre worker encore vivant traite la tâche sur cette machine.
fn held_by_live_worker(cfg: &Config, id: i64) -> bool {
    let Ok(pid) = std::fs::read_to_string(marker_path(cfg, id)) else { return false };
    let pid = pid.trim();
    if pid.is_empty() || pid == std::process::id().to_string() {
        return false;
    }
    let proc = Path::new("/proc").join(pid);
    if Path::new("/proc/self").exists() {
        // Linux : le pid doit encore être un `tkd` (pas un pid réutilisé).
        return std::fs::read(proc.join("cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).contains("tkd"));
    }
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Tâches interrompues à reprendre : en cours, prises avec ce token, dont le
/// worktree existe sur cette machine (un agent lancé à la main n'en a pas)
/// et qu'aucun worker vivant ne traite (worker tué, machine redémarrée…).
fn interrupted_tasks(ctx: &Ctx, local: &[(Project, PathBuf)], running: &[i64]) -> Vec<(i64, Project, PathBuf)> {
    let mut out = vec![];
    for (project, repo) in local {
        let Ok(tasks) = ctx.client.tasks(Some("progress"), Some(&project.id.to_string())) else { continue };
        for t in tasks {
            if t.claimed_by.as_deref() == Some(&ctx.claimer)
                && !running.contains(&t.id)
                && task_worktree(&ctx.cfg, project, t.id).join(".git").exists()
                && !held_by_live_worker(&ctx.cfg, t.id)
            {
                out.push((t.id, project.clone(), repo.clone()));
            }
        }
    }
    out
}

pub fn run(cfg: Config, opts: WorkerOpts) -> Result<()> {
    let client = Client::new(&cfg)?;
    let me = client.me()?;
    if me.scope != "write" {
        bail!("The worker needs a read & write token.");
    }
    let ctx = Ctx { claimer: format!("{}/{}", me.username, me.token), cfg, client };
    if opts.work {
        agent_command(&ctx.cfg, &opts.agent)?;
    }
    println!(
        "Tikedenn worker as {} — default agent {}, up to {} task(s) in parallel{}{}",
        ctx.claimer,
        opts.agent,
        opts.max_parallel,
        if opts.deploy { ", deploys approved tasks" } else { "" },
        if opts.work { "" } else { " (no agent work)" },
    );
    let mut agents: Vec<(i64, JoinHandle<()>)> = vec![];
    let mut deploying: Option<JoinHandle<()>> = None;
    let mut warned_empty = false;
    loop {
        agents.retain(|(_, h)| !h.is_finished());
        if deploying.as_ref().is_some_and(|h| h.is_finished()) {
            deploying = None;
        }
        let local = match local_projects(&ctx, &opts.projects) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Could not list projects: {e:#}");
                vec![]
            }
        };
        if local.is_empty() && !warned_empty {
            eprintln!(
                "No project has a local clone: set the projects' git repository on the site, and clone them \
                 under {} (or map them in [projects] of {}).",
                ctx.cfg.repos_root().display(),
                crate::config::config_path().display()
            );
            warned_empty = true;
        }
        let ids: Vec<String> = local.iter().map(|(p, _)| p.id.to_string()).collect();
        let mut claimed = false;

        if opts.deploy && deploying.is_none() && !local.is_empty() {
            let req = ClaimNext { projects: ids.clone(), status: Some("deploy".into()), ttl_minutes: Some(120) };
            match ctx.client.claim_next(&req) {
                Ok(Some(task)) => {
                    claimed = true;
                    if let Some((project, repo)) = local.iter().find(|(p, _)| Some(p.id) == task.summary.project_id) {
                        let (ctx2, project, repo, agent2) = (ctx.clone(), project.clone(), repo.clone(), opts.agent.clone());
                        deploying = Some(thread::spawn(move || deploy::deploy_and_report(&ctx2, task, &project, &repo, false, &agent2)));
                    }
                }
                Ok(None) => {}
                Err(e) => eprintln!("Could not claim a deploy task: {e:#}"),
            }
        }
        // D'abord les tâches interrompues (worker arrêté pendant qu'un agent
        // travaillait), avant d'en prendre de nouvelles.
        if opts.work && agents.len() < opts.max_parallel {
            let running: Vec<i64> = agents.iter().map(|(id, _)| *id).collect();
            for (id, project, repo) in interrupted_tasks(&ctx, &local, &running) {
                if agents.len() >= opts.max_parallel {
                    break;
                }
                match ctx.client.claim(id, Some(CLAIM_TTL_MINUTES)) {
                    Ok(task) => {
                        claimed = true;
                        println!("[#{id}] resuming an interrupted task.");
                        let agent = project_agent(&project, &opts.agent).to_string();
                        agents.push(spawn_task(&ctx, &agent, task, project, repo, true));
                    }
                    Err(e) => eprintln!("[#{id}] could not resume the task: {e:#}"),
                }
            }
        }
        while opts.work && agents.len() < opts.max_parallel && !local.is_empty() {
            let req = ClaimNext { projects: ids.clone(), status: Some("todo".into()), ttl_minutes: Some(CLAIM_TTL_MINUTES) };
            match ctx.client.claim_next(&req) {
                Ok(Some(task)) => {
                    claimed = true;
                    match local.iter().find(|(p, _)| Some(p.id) == task.summary.project_id) {
                        Some((project, repo)) => {
                            let agent = project_agent(project, &opts.agent);
                            agents.push(spawn_task(&ctx, agent, task, project.clone(), repo.clone(), false))
                        }
                        None => break,
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("Could not claim a task: {e:#}");
                    break;
                }
            }
        }

        if opts.once && !claimed {
            for h in agents.drain(..).map(|(_, h)| h).chain(deploying.take()) {
                let _ = h.join();
            }
            println!("Nothing left to do.");
            return Ok(());
        }
        // Attente du prochain tour (écourtée si une tâche se termine et qu'il
        // reste de la place).
        let deadline = Instant::now() + Duration::from_secs(opts.poll_seconds.max(5));
        while Instant::now() < deadline {
            thread::sleep(Duration::from_secs(2));
            let freed = agents.iter().any(|(_, h)| h.is_finished()) || deploying.as_ref().is_some_and(|h| h.is_finished());
            if freed || (opts.once && claimed) {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_tell_live_workers_from_dead_ones() {
        let dir = std::env::temp_dir().join(format!("tkd-marker-test-{}", std::process::id()));
        let mut cfg = Config::default();
        cfg.worker.work_dir = dir.to_string_lossy().into_owned();
        // Pas de marqueur, ou marqueur de ce processus : libre.
        assert!(!held_by_live_worker(&cfg, 1));
        let marker = RunningMarker::create(&cfg, 1);
        assert!(marker_path(&cfg, 1).exists());
        assert!(!held_by_live_worker(&cfg, 1));
        drop(marker);
        assert!(!marker_path(&cfg, 1).exists());
        // Worker tué (pid disparu) ou pid réutilisé par un autre programme.
        std::fs::create_dir_all(dir.join("running")).unwrap();
        std::fs::write(marker_path(&cfg, 2), "999999999").unwrap();
        assert!(!held_by_live_worker(&cfg, 2));
        let mut other = Command::new("sleep").arg("5").spawn().unwrap();
        std::fs::write(marker_path(&cfg, 3), other.id().to_string()).unwrap();
        assert!(!held_by_live_worker(&cfg, 3));
        let _ = other.kill();
        let _ = other.wait();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
