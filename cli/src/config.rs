//! Configuration de `tkd` : `~/.config/tikedenn/config.toml` (ou
//! `$TIKEDENN_CONFIG`), surchargée par `TIKEDENN_URL` / `TIKEDENN_TOKEN`.
//!
//! ```toml
//! url = "https://tikedenn.example.com"
//! token = "tkd_…"
//! repos_root = "~/Repos"          # clones locaux, trouvés par leur remote origin
//!
//! [projects]                      # chemins explicites (prioritaires sur la recherche)
//! tikedenn = "~/Repos/tikedenn"
//!
//! [worker]
//! agent = "claude"                # ou "pi"
//! max_parallel = 1
//! poll_seconds = 60
//! agent_timeout_minutes = 120
//! claude_command = ["claude", "-p", "{prompt}", "--dangerously-skip-permissions",
//!                   "--output-format", "stream-json", "--verbose"]   # journal en direct
//! pi_command = ["pi", "-p", "{prompt}"]
//! publish_command = ["publish"]
//! ```

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Config {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repos_root: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub projects: BTreeMap<String, String>,
    #[serde(default)]
    pub worker: WorkerConfig,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct WorkerConfig {
    pub agent: String,
    pub max_parallel: usize,
    pub poll_seconds: u64,
    pub agent_timeout_minutes: u64,
    /// Répertoire des worktrees (un par tâche) et des journaux des agents.
    pub work_dir: String,
    /// Commandes des agents ; `{prompt}` est remplacé par la consigne.
    pub claude_command: Vec<String>,
    pub pi_command: Vec<String>,
    /// Commande de publication ; reçoit `<nom> --repo <dépôt>`.
    pub publish_command: Vec<String>,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        WorkerConfig {
            agent: "claude".into(),
            max_parallel: 1,
            poll_seconds: 60,
            agent_timeout_minutes: 120,
            work_dir: "~/.cache/tikedenn".into(),
            // Headless sans aucune demande de permission : l'agent doit
            // pouvoir lancer les tests, les builds… dans son worktree. Pour
            // restreindre : "--permission-mode", "acceptEdits",
            // "--allowedTools", "Bash(git:*),Bash(tkd:*)".
            claude_command: vec![
                "claude".into(),
                "-p".into(),
                "{prompt}".into(),
                "--dangerously-skip-permissions".into(),
                // Événements au fil de l'eau, mis en forme dans le journal
                // (ajoutés aussi aux anciennes configurations, voir agent_log).
                "--output-format".into(),
                "stream-json".into(),
                "--verbose".into(),
            ],
            pi_command: vec!["pi".into(), "-p".into(), "{prompt}".into()],
            publish_command: vec!["publish".into()],
        }
    }
}

/// Développe `~/` en chemin absolu.
pub fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(path),
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("TIKEDENN_CONFIG") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    base.join("tikedenn").join("config.toml")
}

impl Config {
    /// Configuration du fichier (vide s'il n'existe pas) + variables d'environnement.
    pub fn load() -> Result<Config> {
        let path = config_path();
        let mut cfg: Config = match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("Invalid config file {}", path.display()))?,
            Err(_) => Config::default(),
        };
        if let Ok(url) = std::env::var("TIKEDENN_URL") {
            cfg.url = url;
        }
        if let Ok(token) = std::env::var("TIKEDENN_TOKEN") {
            cfg.token = token;
        }
        cfg.url = cfg.url.trim_end_matches('/').to_string();
        Ok(cfg)
    }

    /// Vérifie qu'un serveur et un token sont configurés.
    pub fn require_server(&self) -> Result<()> {
        if self.url.is_empty() || self.token.is_empty() {
            bail!(
                "Tikedenn is not configured: run `tkd login --url <site> --token <token>` \
                 (token created on the site's Agents page), or set TIKEDENN_URL and TIKEDENN_TOKEN."
            );
        }
        Ok(())
    }

    /// Écrit le fichier de configuration (droits 0600 : il contient le token).
    pub fn save(&self) -> Result<PathBuf> {
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(path)
    }

    pub fn repos_root(&self) -> PathBuf {
        expand(self.repos_root.as_deref().unwrap_or("~/Repos"))
    }

    pub fn work_dir(&self) -> PathBuf {
        expand(&self.worker.work_dir)
    }

    /// Chemin local explicitement configuré pour un projet.
    pub fn project_path(&self, name: &str) -> Option<PathBuf> {
        self.projects
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| expand(v))
    }
}

/// Dernières lignes d'un fichier texte (journal d'un agent, de publish…),
/// limitées à `max_chars` caractères.
pub fn tail_file(path: &Path, lines: usize, max_chars: usize) -> String {
    let content = std::fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    tail(&content, lines, max_chars)
}

pub fn tail(content: &str, lines: usize, max_chars: usize) -> String {
    let all: Vec<&str> = content.lines().collect();
    let text = all[all.len().saturating_sub(lines)..].join("\n");
    let count = text.chars().count();
    if count <= max_chars {
        text
    } else {
        let skip = count - max_chars;
        format!("…{}", text.chars().skip(skip).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_last_lines_and_chars() {
        assert_eq!(tail("a\nb\nc", 2, 100), "b\nc");
        assert_eq!(tail("abcdef", 5, 3), "…def");
        assert_eq!(tail("", 5, 3), "");
    }

    #[test]
    fn config_defaults_and_partial_files() {
        let cfg: Config = toml::from_str("url = \"https://x\"\n[worker]\nagent = \"pi\"\n").unwrap();
        assert_eq!(cfg.worker.agent, "pi");
        assert_eq!(cfg.worker.max_parallel, 1);
        assert!(cfg.worker.claude_command.iter().any(|a| a == "{prompt}"));
    }
}
