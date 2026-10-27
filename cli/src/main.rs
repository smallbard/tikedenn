//! `tkd` — CLI Tikedenn pour les agents IA (Claude Code, pi) et leur worker :
//! consulter et prendre les tâches du tableau, rendre compte, soumettre en
//! revue, et `tkd worker` pour faire travailler des agents sur les tâches
//! « todo » puis déployer celles qu'un humain a approuvées.

mod agent_log;
mod client;
mod config;
mod deploy;
mod git;
mod logs;
mod render;
mod worker;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use client::Client;
use config::Config;
use serde::Serialize;
use std::io::Read;
use std::path::PathBuf;
use tikedenn_common::api::{ClaimNext, CreateTask, PatchTask, TaskDetail};
use tikedenn_common::is_valid_status;

const SKILL: &str = include_str!("../../agents/tikedenn/SKILL.md");

#[derive(Parser)]
#[command(name = "tkd", version, about = "Tikedenn board CLI for AI agents and their worker")]
struct Cli {
    /// Sortie JSON (pour les scripts).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Save the site URL and API token (created on the site's Agents page).
    Login {
        #[arg(long)]
        url: String,
        #[arg(long)]
        token: String,
    },
    /// Show the identity of the configured token.
    Whoami,
    /// List projects, with their local clone.
    Projects,
    /// List tasks (by default those of the current repository's project).
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        project: Option<String>,
        /// All projects, even inside a repository.
        #[arg(long)]
        all: bool,
    },
    /// Show a task: spec, acceptance criteria, files, log.
    Show { id: i64 },
    /// Claim the next available task and show it (exit code 2 if none).
    Next {
        /// Accepted projects (names or ids); default: the current repository's project.
        #[arg(long)]
        project: Vec<String>,
        /// Any project.
        #[arg(long)]
        all: bool,
        /// Lease duration in minutes.
        #[arg(long)]
        ttl: Option<i64>,
    },
    /// Claim a task (todo → progress) or renew your lease.
    Claim {
        id: i64,
        #[arg(long)]
        ttl: Option<i64>,
    },
    /// Give a task back (to todo by default).
    Release {
        id: i64,
        #[arg(short, long)]
        message: Option<String>,
        /// Keep the current state (only drop the claim).
        #[arg(long)]
        keep_state: bool,
    },
    /// Move a task (agents: todo↔progress, progress→review, deploy→done/review).
    Move {
        id: i64,
        status: String,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Push the task branch if needed, then move the task to review with a report.
    Submit {
        id: i64,
        /// Report for the reviewer ("-" reads stdin).
        #[arg(short, long)]
        message: String,
    },
    /// Add a note to the task log ("-" reads stdin).
    Comment {
        id: i64,
        #[arg(short, long)]
        message: String,
    },
    /// Set the task's git branch.
    Branch { id: i64, name: String },
    /// Propose a new task (created in draft).
    Create {
        title: String,
        #[arg(short, long, default_value = "")]
        description: String,
        #[arg(short, long, default_value = "")]
        acceptance: String,
        #[arg(long)]
        project: Option<String>,
        /// 0 low, 1 normal, 2 high, 3 urgent.
        #[arg(long)]
        priority: Option<i64>,
        /// Ids of tasks this one depends on (e.g. --depends-on 12,15).
        #[arg(long, value_delimiter = ',')]
        depends_on: Vec<i64>,
    },
    /// Update a task: only the given fields change.
    Update {
        id: i64,
        #[arg(long)]
        title: Option<String>,
        /// New description ("-" reads stdin).
        #[arg(short, long)]
        description: Option<String>,
        /// New acceptance criteria ("-" reads stdin).
        #[arg(short, long)]
        acceptance: Option<String>,
        /// Project id or name ("" = no project).
        #[arg(long)]
        project: Option<String>,
        /// 0 low, 1 normal, 2 high, 3 urgent.
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long)]
        branch: Option<String>,
        /// Replaces all dependencies, e.g. --depends-on 12,15 ("" = none).
        #[arg(long)]
        depends_on: Option<String>,
    },
    /// Attach a file to a task.
    Attach { id: i64, file: PathBuf },
    /// Download an attached file.
    Download {
        file_id: i64,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Deploy an approved task now (merge into the base branch, push, publish).
    Deploy {
        id: i64,
        /// Merge locally without pushing nor publishing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Run agents on todo tasks (one git worktree per task) and deploy approved tasks.
    Worker {
        /// claude or pi, for projects without an agent of their own (default from the config: claude).
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        max_parallel: Option<usize>,
        #[arg(long)]
        poll: Option<u64>,
        /// Only these projects (names or ids).
        #[arg(long)]
        project: Vec<String>,
        /// Do not deploy approved tasks.
        #[arg(long)]
        no_deploy: bool,
        /// Only deploy (do not run agents).
        #[arg(long)]
        deploy_only: bool,
        /// One round: process what is available, wait for it, exit.
        #[arg(long)]
        once: bool,
    },
    /// Show the worker log of a task's agent (latest run); without id, list recent logs.
    Logs {
        id: Option<i64>,
        /// Keep printing new lines as the agent works.
        #[arg(short, long)]
        follow: bool,
        /// The deploy log instead of the agent log.
        #[arg(long)]
        deploy: bool,
    },
    /// Install the `tikedenn` skill for Claude Code (~/.claude/skills) and pi (~/.pi/agent/skills).
    InstallSkill,
}

fn print_json<T: Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn print_task(json: bool, t: &TaskDetail) -> Result<()> {
    if json {
        print_json(t)
    } else {
        print!("{}", render::task_markdown(t));
        Ok(())
    }
}

/// Message d'une option `-m` : « - » lit l'entrée standard.
fn message(m: &str) -> Result<String> {
    if m == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        Ok(s)
    } else {
        Ok(m.to_string())
    }
}

/// Liste d'ids séparés par des virgules (« 12, #15 ») ; vide = aucun.
fn parse_ids(s: &str) -> Result<Vec<i64>> {
    s.split(',')
        .map(|p| p.trim().trim_start_matches('#'))
        .filter(|p| !p.is_empty())
        .map(|p| p.parse().with_context(|| format!("Invalid task id '{p}'")))
        .collect()
}

/// Projet du dépôt git courant (d'après son remote origin).
fn current_project(client: &Client) -> Option<tikedenn_common::api::Project> {
    let cwd = std::env::current_dir().ok()?;
    let url = git::origin_url(&cwd)?;
    client.projects(Some(&url)).ok()?.into_iter().next()
}

/// Avant une soumission : pousse la branche courante si c'est celle de la
/// tâche et qu'elle a des commits non poussés.
fn push_if_task_branch(task: &TaskDetail) -> Result<()> {
    let Ok(cwd) = std::env::current_dir() else { return Ok(()) };
    let branch = &task.summary.branch;
    if branch.is_empty() || git::current_branch(&cwd).as_deref() != Some(branch.as_str()) {
        return Ok(());
    }
    if !git::is_clean(&cwd) {
        eprintln!("warning: uncommitted changes in {} are not part of the submission.", cwd.display());
    }
    let pushed = git::remote_branch_exists(&cwd, branch)
        && git::git(&cwd, &["rev-list", "--count", &format!("origin/{branch}..HEAD")])
            .map(|n| n == "0")
            .unwrap_or(false);
    if !pushed {
        eprintln!("Pushing {branch}…");
        git::git(&cwd, &["push", "-u", "origin", &format!("HEAD:refs/heads/{branch}")])?;
    }
    Ok(())
}

fn install_skill() -> Result<()> {
    let home = config::home();
    for dir in [home.join(".claude/skills/tikedenn"), home.join(".pi/agent/skills/tikedenn")] {
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("SKILL.md");
        std::fs::write(&path, SKILL)?;
        println!("Installed {}", path.display());
    }
    Ok(())
}

fn run(cli: Cli) -> Result<()> {
    let json = cli.json;
    let cfg = Config::load()?;
    match cli.cmd {
        Cmd::Login { url, token } => {
            let mut cfg = cfg;
            cfg.url = url.trim_end_matches('/').to_string();
            cfg.token = token.trim().to_string();
            let me = Client::new(&cfg)?.me().context("The site refused this token")?;
            let path = cfg.save()?;
            println!("Logged in as {}/{} ({}) — saved to {}", me.username, me.token, me.scope, path.display());
        }
        Cmd::InstallSkill => install_skill()?,
        Cmd::Logs { id, follow, deploy } => logs::run(&cfg, id, follow, deploy)?,
        cmd => {
            let client = Client::new(&cfg)?;
            run_api(cmd, &cfg, &client, json)?;
        }
    }
    Ok(())
}

fn run_api(cmd: Cmd, cfg: &Config, client: &Client, json: bool) -> Result<()> {
    match cmd {
        Cmd::Whoami => {
            let me = client.me()?;
            if json {
                print_json(&me)?;
            } else {
                println!("{}/{} ({}) on {}", me.username, me.token, me.scope, cfg.url);
            }
        }
        Cmd::Projects => {
            let projects = client.projects(None)?;
            if json {
                return print_json(&projects);
            }
            for p in projects {
                let local = git::find_local_repo(cfg, &p)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "no local clone".into());
                let publish = if p.publish_name.is_empty() { "-".into() } else { format!("publish {}", p.publish_name) };
                let agent = match p.agent.as_str() {
                    "" => "default agent",
                    "none" => "humans only",
                    a => a,
                };
                println!("{:<4} {:<20} {}  [{local}] [{publish}] [{agent}]", p.id, p.name, p.git_repo);
            }
        }
        Cmd::List { status, project, all } => {
            let project = match (project, all) {
                (Some(p), _) => Some(p),
                (None, false) => current_project(client).map(|p| p.name),
                (None, true) => None,
            };
            let tasks = client.tasks(status.as_deref(), project.as_deref())?;
            if json {
                return print_json(&tasks);
            }
            if tasks.is_empty() {
                println!("No task.");
            }
            for t in &tasks {
                println!("{}", render::task_line(t));
            }
        }
        Cmd::Show { id } => print_task(json, &client.task(id)?)?,
        Cmd::Next { project, all, ttl } => {
            let projects = if !project.is_empty() || all {
                project
            } else {
                current_project(client).map(|p| vec![p.name]).unwrap_or_default()
            };
            let req = ClaimNext { projects, status: Some("todo".into()), ttl_minutes: ttl };
            match client.claim_next(&req)? {
                Some(t) => print_task(json, &t)?,
                None => {
                    if json {
                        println!("null");
                    } else {
                        eprintln!("No task available.");
                    }
                    std::process::exit(2);
                }
            }
        }
        Cmd::Claim { id, ttl } => print_task(json, &client.claim(id, ttl)?)?,
        Cmd::Release { id, message: m, keep_state } => {
            let note = m.as_deref().map(message).transpose()?;
            let status = if keep_state { None } else { Some("todo") };
            let t = client.release(id, status, note.as_deref())?;
            println!("Task #{id} released ({}).", t.summary.status);
        }
        Cmd::Move { id, status, message: m } => {
            if !is_valid_status(&status) {
                bail!("Unknown status '{status}'.");
            }
            let note = m.as_deref().map(message).transpose()?;
            let t = client.move_to(id, &status, note.as_deref())?;
            println!("Task #{id} is now {}.", t.summary.status);
        }
        Cmd::Submit { id, message: m } => {
            let report = message(&m)?;
            if report.trim().is_empty() {
                bail!("The report is empty.");
            }
            push_if_task_branch(&client.task(id)?)?;
            client.move_to(id, "review", Some(&report))?;
            println!("Task #{id} submitted for review.");
        }
        Cmd::Comment { id, message: m } => {
            client.comment(id, &message(&m)?, "comment")?;
            println!("Comment added to #{id}.");
        }
        Cmd::Branch { id, name } => {
            let t = client.set_branch(id, &name)?;
            println!("Task #{id} branch: {}", t.summary.branch);
        }
        Cmd::Create { title, description, acceptance, project, priority, depends_on } => {
            let project = project.or_else(|| current_project(client).map(|p| p.name));
            let c = CreateTask {
                title,
                description,
                acceptance,
                project,
                priority,
                branch: String::new(),
                depends_on,
            };
            let t = client.create(&c)?;
            if json {
                print_json(&t)?;
            } else {
                println!("Created #{} in draft.", t.summary.id);
            }
        }
        Cmd::Update { id, title, description, acceptance, project, priority, branch, depends_on } => {
            let p = PatchTask {
                title,
                description: description.as_deref().map(message).transpose()?,
                acceptance: acceptance.as_deref().map(message).transpose()?,
                project,
                priority,
                branch,
                depends_on: depends_on.as_deref().map(parse_ids).transpose()?,
            };
            let t = client.update(id, &p)?;
            if json {
                print_json(&t)?;
            } else {
                println!("Task #{id} updated.");
            }
        }
        Cmd::Attach { id, file } => {
            let data = std::fs::read(&file).with_context(|| format!("Could not read {}", file.display()))?;
            let name = file.file_name().context("Invalid file name")?.to_string_lossy().into_owned();
            client.attach(id, &name, "application/octet-stream", &data)?;
            println!("Attached {name} to #{id}.");
        }
        Cmd::Download { file_id, output } => {
            let data = client.download(file_id)?;
            match output {
                Some(path) => {
                    std::fs::write(&path, data)?;
                    println!("Saved {}", path.display());
                }
                None => {
                    use std::io::Write;
                    std::io::stdout().write_all(&data)?;
                }
            }
        }
        Cmd::Deploy { id, dry_run } => {
            let me = client.me()?;
            let task = client.task(id)?;
            if task.summary.status != "deploy" {
                bail!("Task #{id} is in {}: only approved tasks (deploy) can be deployed.", task.summary.status);
            }
            let project = task.repo.clone().context("The task has no project.")?;
            let repo = git::find_local_repo(cfg, &project)
                .with_context(|| format!("No local clone of project {}.", project.name))?;
            let task = client.claim(id, Some(120))?;
            let ctx = worker::Ctx::new(cfg.clone(), client.clone(), format!("{}/{}", me.username, me.token));
            deploy::deploy_and_report(&ctx, task, &project, &repo, dry_run, &cfg.worker.agent);
        }
        Cmd::Worker { agent, max_parallel, poll, project, no_deploy, deploy_only, once } => {
            let opts = worker::WorkerOpts {
                agent: agent.unwrap_or_else(|| cfg.worker.agent.clone()),
                max_parallel: max_parallel.unwrap_or(cfg.worker.max_parallel).max(1),
                poll_seconds: poll.unwrap_or(cfg.worker.poll_seconds),
                projects: project,
                work: !deploy_only,
                deploy: !no_deploy,
                once,
            };
            worker::run(cfg.clone(), opts)?;
        }
        Cmd::Login { .. } | Cmd::InstallSkill | Cmd::Logs { .. } => unreachable!(),
    }
    Ok(())
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("tkd: {e:#}");
        std::process::exit(1);
    }
}
