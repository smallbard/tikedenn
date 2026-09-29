//! Éléments partagés entre le serveur (`tikedenn-site`) et le CLI des agents
//! (`tkd`) : états du tableau, priorités, règles de transition des agents,
//! normalisation des URL git et types JSON de l'API `/api/v1`.

pub mod api;

/// États du tableau, dans l'ordre de gauche à droite.
pub const STATUSES: [&str; 6] = ["draft", "todo", "progress", "review", "deploy", "done"];

pub fn is_valid_status(s: &str) -> bool {
    STATUSES.contains(&s)
}

/// Priorités (0 = basse … 3 = urgente) ; les agents prennent d'abord les
/// tâches les plus prioritaires, puis les plus anciennes.
pub const PRIORITIES: [&str; 4] = ["low", "normal", "high", "urgent"];
pub const DEFAULT_PRIORITY: i64 = 1;

pub fn priority_label(p: i64) -> &'static str {
    PRIORITIES.get(p as usize).copied().unwrap_or("normal")
}

/// Transitions permises à un agent (un humain peut tout faire depuis l'UI) :
/// prendre une tâche prête, la soumettre en revue ou l'abandonner, et —
/// pour le déploiement — la terminer ou la renvoyer en revue en cas d'échec.
/// `draft → todo` (tâche prête) et `review → deploy` (approbation) restent
/// des décisions humaines.
pub fn agent_may_move(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("todo", "progress")
            | ("progress", "review")
            | ("progress", "todo")
            | ("deploy", "done")
            | ("deploy", "review")
    )
}

/// Forme canonique d'une URL de dépôt git, pour comparer une URL https, ssh
/// ou scp (`git@hote:chemin`) : `hote/chemin` en minuscules, sans `.git`,
/// sans identifiants ni slash final.
pub fn normalize_repo_url(url: &str) -> String {
    let mut s = url.trim().to_ascii_lowercase();
    if let Some(i) = s.find("://") {
        s = s[i + 3..].to_string();
    } else if let Some((user_host, path)) = s.split_once(':') {
        // Forme scp : git@github.com:owner/repo
        s = format!("{user_host}/{path}");
    }
    if let Some(i) = s.find('@') {
        if s[..i].find('/').is_none() {
            s = s[i + 1..].to_string();
        }
    }
    let s = s.trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    // Port éventuel du ssh (hote:22/chemin) retiré pour comparer les hôtes.
    match s.split_once('/') {
        Some((host, path)) => {
            let host = host.split(':').next().unwrap_or(host);
            format!("{host}/{path}")
        }
        None => s.to_string(),
    }
}

/// Nom de branche dérivé d'une tâche : `task/<id>-<titre-en-slug>`.
pub fn task_branch_name(id: i64, title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        format!("task/{id}")
    } else {
        format!("task/{id}-{slug}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_urls_are_normalized() {
        let canon = "github.com/smallbard/tikedenn";
        for url in [
            "https://github.com/smallbard/tikedenn",
            "https://github.com/smallbard/tikedenn.git",
            "https://x-access-token:abc@github.com/Smallbard/Tikedenn/",
            "git@github.com:smallbard/tikedenn.git",
            "ssh://git@github.com:22/smallbard/tikedenn.git",
            "ssh://git@github.com/smallbard/tikedenn",
        ] {
            assert_eq!(normalize_repo_url(url), canon, "{url}");
        }
        assert_ne!(normalize_repo_url("https://gitlab.com/smallbard/tikedenn"), canon);
    }

    #[test]
    fn agent_transitions() {
        assert!(agent_may_move("todo", "progress"));
        assert!(agent_may_move("progress", "review"));
        assert!(agent_may_move("deploy", "done"));
        assert!(!agent_may_move("draft", "todo"));
        assert!(!agent_may_move("review", "deploy"));
        assert!(!agent_may_move("review", "done"));
        assert!(!agent_may_move("todo", "done"));
    }

    #[test]
    fn branch_names() {
        assert_eq!(task_branch_name(42, "Add a /login page!"), "task/42-add-a-login-page");
        assert_eq!(task_branch_name(7, "Édition"), "task/7-dition");
        assert_eq!(task_branch_name(3, "!!!"), "task/3");
        assert!(task_branch_name(1, &"a".repeat(100)).len() <= 48);
    }
}
