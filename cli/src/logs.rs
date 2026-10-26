//! `tkd logs` : journaux du worker (`<work_dir>/logs/task-<id>-<horodatage>.log`
//! pour un agent, `deploy-<id>-…` pour un déploiement), avec suivi en direct.

use crate::config::Config;
use anyhow::{bail, Result};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

/// Journaux du dossier : (préfixe « task-12 », horodatage, chemin).
fn all_logs(dir: &Path) -> Vec<(String, u64, PathBuf)> {
    let mut out = vec![];
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let Some(stem) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".log")) else {
            continue;
        };
        if let Some((prefix, ts)) = stem.rsplit_once('-') {
            if let Ok(ts) = ts.parse() {
                out.push((prefix.to_string(), ts, path));
            }
        }
    }
    out.sort_by_key(|(_, ts, _)| *ts);
    out
}

/// Journal le plus récent d'une tâche.
fn latest(dir: &Path, prefix: &str) -> Option<PathBuf> {
    all_logs(dir).into_iter().rev().find(|(p, _, _)| p == prefix).map(|(_, _, path)| path)
}

pub fn run(cfg: &Config, id: Option<i64>, follow: bool, deploy: bool) -> Result<()> {
    let dir = cfg.work_dir().join("logs");
    let Some(id) = id else {
        let logs = all_logs(&dir);
        if logs.is_empty() {
            println!("No log in {}.", dir.display());
        }
        let now = SystemTime::now();
        for (prefix, _, path) in logs.iter().rev().take(15) {
            let meta = std::fs::metadata(path).ok();
            let size = meta.as_ref().map_or(0, |m| m.len());
            let age = meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| now.duration_since(t).ok())
                .map_or("?".into(), |d| ago(d.as_secs()));
            println!("{prefix:<14} {size:>9} B  updated {age:<8} {}", path.display());
        }
        return Ok(());
    };
    let prefix = format!("{}-{id}", if deploy { "deploy" } else { "task" });
    let Some(mut path) = latest(&dir, &prefix) else {
        bail!("No {} log for task #{id} in {}.", if deploy { "deploy" } else { "agent" }, dir.display());
    };
    eprintln!("== {}", path.display());
    let mut pos = copy_from(&path, 0)?;
    if !follow {
        return Ok(());
    }
    loop {
        thread::sleep(Duration::from_millis(500));
        // Nouveau passage du worker sur la tâche : nouveau journal.
        if let Some(newer) = latest(&dir, &prefix).filter(|p| *p != path) {
            path = newer;
            pos = 0;
            eprintln!("\n== {}", path.display());
        }
        pos = copy_from(&path, pos)?;
    }
}

/// Affiche le fichier à partir de `pos` ; renvoie la nouvelle position.
fn copy_from(path: &Path, pos: u64) -> Result<u64> {
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(pos))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let mut out = std::io::stdout().lock();
    out.write_all(&buf)?;
    out.flush()?;
    Ok(pos + buf.len() as u64)
}

fn ago(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_latest_log_of_a_task() {
        let dir = std::env::temp_dir().join(format!("tkd-logs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["task-1-100.log", "task-1-300.log", "task-12-400.log", "deploy-1-500.log", "other.txt"] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        assert_eq!(latest(&dir, "task-1"), Some(dir.join("task-1-300.log")));
        assert_eq!(latest(&dir, "deploy-1"), Some(dir.join("deploy-1-500.log")));
        assert_eq!(latest(&dir, "task-2"), None);
        assert_eq!(all_logs(&dir).len(), 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
