//! Rendu texte (Markdown) des tâches, lisible par un humain comme par un agent.

use tikedenn_common::api::{TaskDetail, TaskSummary};
use tikedenn_common::priority_label;

fn claim_text(t: &TaskSummary) -> String {
    match (&t.claimed_by, t.claim_active) {
        (Some(by), true) => format!(
            " — claimed by {by} until {} UTC",
            t.claimed_until.as_deref().unwrap_or("?")
        ),
        (Some(by), false) => format!(" — lease of {by} expired"),
        _ => String::new(),
    }
}

/// Une ligne par tâche.
pub fn task_line(t: &TaskSummary) -> String {
    let mut line = format!("#{:<4} {:<8} {:<7} {}", t.id, t.status, priority_label(t.priority), t.title);
    if let Some(p) = &t.project {
        line.push_str(&format!("  [{p}]"));
    }
    if !t.branch.is_empty() {
        line.push_str(&format!("  ⎇ {}", t.branch));
    }
    if !t.blocked_by.is_empty() {
        let ids: Vec<String> = t.blocked_by.iter().map(|i| format!("#{i}")).collect();
        line.push_str(&format!("  blocked by {}", ids.join(" ")));
    }
    line.push_str(&claim_text(t));
    line
}

/// Fiche complète d'une tâche, en Markdown.
pub fn task_markdown(t: &TaskDetail) -> String {
    let s = &t.summary;
    let mut out = format!("# Task #{} — {}\n\n", s.id, s.title);
    out.push_str(&format!("- Status: {}{}\n", s.status, claim_text(s)));
    out.push_str(&format!("- Priority: {}\n", priority_label(s.priority)));
    if let Some(p) = &t.repo {
        out.push_str(&format!("- Project: {} (repo {}", p.name, if p.git_repo.is_empty() { "none" } else { &p.git_repo }));
        if !p.default_branch.is_empty() {
            out.push_str(&format!(", base branch {}", p.default_branch));
        }
        out.push_str(")\n");
    }
    out.push_str(&format!(
        "- Branch: {}\n",
        if s.branch.is_empty() { "(none yet)" } else { &s.branch }
    ));
    if !t.depends_on.is_empty() {
        let deps: Vec<String> = t
            .depends_on
            .iter()
            .map(|d| if s.blocked_by.contains(d) { format!("#{d} (not done)") } else { format!("#{d} (done)") })
            .collect();
        out.push_str(&format!("- Depends on: {}\n", deps.join(", ")));
    }
    if let Some(by) = &t.created_by {
        out.push_str(&format!("- Created by {by} on {} UTC\n", t.created_at));
    }
    out.push_str("\n## Description\n\n");
    out.push_str(if t.description.is_empty() { "(none)" } else { &t.description });
    out.push_str("\n\n## Acceptance criteria\n\n");
    out.push_str(if t.acceptance.is_empty() { "(none given)" } else { &t.acceptance });
    out.push('\n');
    if !t.files.is_empty() {
        out.push_str("\n## Attached files\n\n");
        for f in &t.files {
            out.push_str(&format!("- {} ({} KB) — `tkd download {} -o {}`\n", f.name, f.size / 1024, f.id, f.name));
        }
    }
    let log: Vec<_> = t.log.iter().filter(|l| l.kind != "claim").collect();
    if !log.is_empty() {
        out.push_str("\n## Log (oldest first — review feedback appears here)\n\n");
        for l in log {
            let who = match &l.agent {
                Some(a) => format!("{} 🤖 {a}", l.author),
                None => l.author.clone(),
            };
            if l.kind == "move" {
                out.push_str(&format!("- {} {who}: {}\n", l.created_at, l.body));
            } else {
                let body = l.body.replace('\n', "\n  ");
                out.push_str(&format!("- {} {who} [{}]:\n  {body}\n", l.created_at, l.kind));
            }
        }
    }
    out
}
