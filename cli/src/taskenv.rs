//! Environnement d'une session d'agent : dossier temporaire dans le worktree
//! (au lieu de /tmp, jamais nettoyé), cache de compilation gardé d'une
//! session à l'autre, arrêt des processus laissés par l'agent (serveurs de
//! test…) et ménage des worktrees des tâches finies.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// Dossier temporaire de l'agent, à la racine du worktree (vidé à la fin de
/// chaque session).
pub const TMP_DIR: &str = ".tikedenn-tmp";
/// Cache de compilation du worktree (gardé d'une session à l'autre,
/// supprimé avec le worktree ou après `cache_max_idle_days`).
pub const CACHE_DIR: &str = ".tikedenn-cache";
/// Date de la dernière session d'agent, dans le cache.
const LAST_USED: &str = ".last-used";

pub fn tmp_dir(wt: &Path) -> PathBuf {
    wt.join(TMP_DIR)
}

pub fn cache_dir(wt: &Path) -> PathBuf {
    wt.join(CACHE_DIR)
}

/// Crée un dossier ignoré par git (un `.gitignore` `*` à l'intérieur : rien
/// à ajouter au dépôt, `git status` reste propre).
fn ignored_dir(dir: PathBuf) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(".gitignore"), "*\n")?;
    Ok(dir)
}

/// Crée le dossier temporaire du worktree.
pub fn prepare_tmp(wt: &Path) -> std::io::Result<PathBuf> {
    ignored_dir(tmp_dir(wt))
}

/// Crée le cache de compilation du worktree et note son utilisation.
pub fn prepare_cache(wt: &Path) -> std::io::Result<PathBuf> {
    let dir = ignored_dir(cache_dir(wt))?;
    touch_cache(wt);
    Ok(dir)
}

/// Note l'utilisation du cache (début et fin de session).
pub fn touch_cache(wt: &Path) {
    let _ = std::fs::write(cache_dir(wt).join(LAST_USED), "");
}

/// Temps écoulé depuis la dernière session qui a utilisé le cache (None :
/// pas de cache).
pub fn cache_idle(wt: &Path) -> Option<Duration> {
    let dir = cache_dir(wt);
    let meta = std::fs::metadata(dir.join(LAST_USED)).or_else(|_| std::fs::metadata(&dir)).ok()?;
    Some(SystemTime::now().duration_since(meta.modified().ok()?).unwrap_or_default())
}

/// Supprime le cache de compilation du worktree.
pub fn clear_cache(wt: &Path) {
    let _ = std::fs::remove_dir_all(cache_dir(wt));
}

/// Vrai si `program` est dans le PATH.
pub fn in_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}

/// Vide le dossier temporaire (fin de session de l'agent).
pub fn clear_tmp(wt: &Path) {
    let _ = std::fs::remove_dir_all(tmp_dir(wt));
}

/// Processus encore vivants lancés pendant la session de l'agent : ceux dont
/// l'environnement contient `TIKEDENN_TMP=<dossier du worktree>` (hérité de
/// l'agent, propre à la tâche). Linux seulement (`/proc`) ; ailleurs, rien.
pub fn leftover_processes(wt: &Path) -> Vec<(u32, String)> {
    let marker = format!("TIKEDENN_TMP={}", tmp_dir(wt).display());
    let me = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else { return vec![] };
    let mut out = vec![];
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        if pid == me {
            continue;
        }
        let Ok(env) = std::fs::read(e.path().join("environ")) else { continue };
        if !env.split(|&b| b == 0).any(|v| v == marker.as_bytes()) {
            continue;
        }
        let cmd = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
        let cmd = String::from_utf8_lossy(&cmd).replace('\0', " ").trim().to_string();
        out.push((pid, cmd));
    }
    out.sort();
    out
}

fn signal(pids: &[u32], sig: &str) {
    if pids.is_empty() {
        return;
    }
    let _ = Command::new("kill")
        .arg(sig)
        .args(pids.iter().map(|p| p.to_string()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Arrête les processus laissés par l'agent (SIGTERM, puis SIGKILL après
/// 10 s) et renvoie ceux qui ont été trouvés.
pub fn stop_leftovers(wt: &Path) -> Vec<(u32, String)> {
    let found = leftover_processes(wt);
    let pids: Vec<u32> = found.iter().map(|(p, _)| *p).collect();
    signal(&pids, "-TERM");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) && !leftover_processes(wt).is_empty() {
        std::thread::sleep(Duration::from_millis(500));
    }
    let rest: Vec<u32> = leftover_processes(wt).iter().map(|(p, _)| *p).collect();
    signal(&rest, "-KILL");
    found
}

/// Résumé lisible pour le journal de la tâche.
pub fn describe(procs: &[(u32, String)]) -> String {
    procs
        .iter()
        .map(|(pid, cmd)| {
            let cmd: String = cmd.chars().take(160).collect();
            format!("- {pid}: {cmd}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Numéro de tâche d'un dossier de worktree (`task-<id>`).
pub fn worktree_task_id(dir: &Path) -> Option<i64> {
    dir.file_name()?.to_str()?.strip_prefix("task-")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmp_dir_is_ignored_by_git() {
        let base = std::env::temp_dir().join(format!("tkd-taskenv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        assert!(Command::new("git").arg("-C").arg(&base).args(["init", "-q"]).status().unwrap().success());
        let tmp = prepare_tmp(&base).unwrap();
        std::fs::write(tmp.join("test.db"), "x").unwrap();
        let st = Command::new("git").arg("-C").arg(&base).args(["status", "--porcelain"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&st.stdout), "");
        let cache = prepare_cache(&base).unwrap();
        std::fs::create_dir_all(cache.join("target/debug")).unwrap();
        let st = Command::new("git").arg("-C").arg(&base).args(["status", "--porcelain"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&st.stdout), "");
        assert!(cache_idle(&base).unwrap() < Duration::from_secs(60));
        clear_tmp(&base);
        assert!(!tmp.exists());
        assert!(cache.join("target/debug").exists(), "the build cache survives the session");
        clear_cache(&base);
        assert!(cache_idle(&base).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn finds_and_stops_processes_of_the_session() {
        let wt = std::env::temp_dir().join(format!("tkd-taskenv-procs-{}", std::process::id()));
        let mut child = Command::new("sleep")
            .arg("60")
            .env("TIKEDENN_TMP", tmp_dir(&wt))
            .spawn()
            .unwrap();
        let found = leftover_processes(&wt);
        assert!(found.iter().any(|(p, _)| *p == child.id()), "{found:?}");
        let other = std::env::temp_dir().join("tkd-taskenv-other");
        assert!(leftover_processes(&other).is_empty());
        stop_leftovers(&wt);
        // Le processus est arrêté (on récupère son statut de fin).
        let status = child.wait().unwrap();
        assert!(!status.success());
    }

    #[test]
    fn parses_worktree_task_ids() {
        assert_eq!(worktree_task_id(Path::new("/w/Emdrein/task-200")), Some(200));
        assert_eq!(worktree_task_id(Path::new("/w/Emdrein/other")), None);
    }
}
