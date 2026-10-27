//! Journal des agents lisible en direct. Claude Code est lancé avec
//! `--output-format stream-json` (sinon `claude -p` n'écrit rien avant la
//! fin) ; chaque événement JSON devient une ligne horodatée : message de
//! l'agent, outil appelé, résultat. Les lignes qui ne sont pas du JSON (pi,
//! autres commandes) sont recopiées telles quelles.

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::time::Instant;

/// Limite d'utilisation de l'agent atteinte (session, quota…).
#[derive(Debug, Clone, PartialEq)]
pub struct Limit {
    pub message: String,
    /// Heure de remise à zéro (secondes Unix), si l'agent la donne.
    pub resets_at: Option<u64>,
}

/// Repère une limite d'utilisation dans un événement : `rate_limit_event`
/// refusé, ou résultat final du type « You've hit your session limit ».
pub fn limit_in(v: &Value, current: Option<Limit>) -> Option<Limit> {
    match v.get("type").and_then(Value::as_str) {
        Some("rate_limit_event") => {
            let info = v.get("rate_limit_info")?;
            if info.get("status").and_then(Value::as_str) != Some("rejected") {
                return current;
            }
            let resets_at = info.get("resetsAt").and_then(Value::as_u64);
            Some(Limit { message: "Usage limit reached".into(), resets_at: resets_at.or(current.and_then(|l| l.resets_at)) })
        }
        Some("result") => {
            let text = v.get("result").and_then(Value::as_str).unwrap_or("");
            if is_limit_message(text) {
                Some(Limit { message: text.trim().to_string(), resets_at: current.and_then(|l| l.resets_at) })
            } else {
                current
            }
        }
        _ => current,
    }
}

/// Message de limite (sortie de Claude Code, ou d'un autre agent en texte).
pub fn is_limit_message(text: &str) -> bool {
    let t = text.to_lowercase();
    t.len() < 400
        && ((t.contains("hit your") && t.contains("limit"))
            || t.contains("usage limit reached")
            || t.contains("rate limit exceeded")
            || t.contains("quota exceeded"))
}

/// Complète une commande `claude -p` sans format de sortie explicite pour
/// qu'elle écrive ses événements au fil de l'eau (configurations existantes).
pub fn ensure_streaming(mut cmd: Vec<String>) -> Vec<String> {
    let is_claude = cmd.first().is_some_and(|c| c.rsplit('/').next() == Some("claude"));
    let printing = cmd.iter().any(|a| a == "-p" || a == "--print");
    let has_format = cmd.iter().any(|a| a.starts_with("--output-format"));
    if is_claude && printing && !has_format {
        cmd.extend(["--output-format", "stream-json", "--verbose"].map(String::from));
    }
    cmd
}

/// Recopie la sortie de l'agent dans le journal, ligne par ligne, mise en
/// forme ; renvoie la limite d'utilisation rencontrée, le cas échéant.
pub fn pump(input: impl Read, mut out: impl Write, start: Instant) -> Option<Limit> {
    let mut limit = None;
    for line in BufReader::new(input).split(b'\n') {
        let Ok(line) = line else { break };
        let line = String::from_utf8_lossy(&line);
        let text = match serde_json::from_str::<Value>(&line) {
            Ok(v) if v.get("type").is_some() => {
                limit = limit_in(&v, limit);
                match format_event(&v) {
                    Some(t) => t,
                    None => continue,
                }
            }
            _ => {
                if is_limit_message(&line) {
                    limit = Some(Limit { message: line.trim().to_string(), resets_at: None });
                }
                line.into_owned()
            }
        };
        let secs = start.elapsed().as_secs();
        let stamp = format!("[{:02}:{:02}:{:02}]", secs / 3600, secs / 60 % 60, secs % 60);
        for (i, l) in text.lines().enumerate() {
            let prefix = if i == 0 { stamp.as_str() } else { "          " };
            if writeln!(out, "{prefix} {l}").is_err() {
                return limit;
            }
        }
        let _ = out.flush();
    }
    limit
}

/// Une ligne (ou plusieurs) par événement intéressant ; `None` pour le reste.
pub fn format_event(v: &Value) -> Option<String> {
    let s = |key: &str| v.get(key).and_then(Value::as_str).unwrap_or("");
    // Messages d'un sous-agent : décalés.
    let indent = if v.get("parent_tool_use_id").is_some_and(|p| !p.is_null()) { "  │ " } else { "" };
    let content = || v.pointer("/message/content").and_then(Value::as_array).cloned().unwrap_or_default();
    let lines: Vec<String> = match s("type") {
        "system" if s("subtype") == "init" => vec![format!("Session started (model {}, in {})", s("model"), s("cwd"))],
        "assistant" => content()
            .iter()
            .filter_map(|c| match c.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let t = c.get("text").and_then(Value::as_str).unwrap_or("").trim();
                    (!t.is_empty()).then(|| format!("💬 {t}"))
                }
                Some("tool_use") => Some(format!(
                    "→ {} {}",
                    c.get("name").and_then(Value::as_str).unwrap_or("?"),
                    tool_summary(c.get("input").unwrap_or(&Value::Null))
                )),
                _ => None,
            })
            .collect(),
        "user" => content()
            .iter()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("tool_result"))
            .map(|c| {
                let text = result_text(c.get("content").unwrap_or(&Value::Null));
                let n = text.lines().count();
                let first = one_line(text.lines().find(|l| !l.trim().is_empty()).unwrap_or(""), 160);
                if c.get("is_error").and_then(Value::as_bool) == Some(true) {
                    format!("  ✗ {}", one_line(&text, 400))
                } else if n > 1 {
                    format!("  ← {first} (+{} lines)", n - 1)
                } else {
                    format!("  ← {first}")
                }
            })
            .collect(),
        "result" => {
            let mut head = format!(
                "Finished ({}): {} turns, {}s",
                s("subtype"),
                v.get("num_turns").and_then(Value::as_u64).unwrap_or(0),
                v.get("duration_ms").and_then(Value::as_u64).unwrap_or(0) / 1000
            );
            if let Some(cost) = v.get("total_cost_usd").and_then(Value::as_f64) {
                head.push_str(&format!(", ${cost:.2}"));
            }
            vec![head, s("result").trim().to_string()]
        }
        _ => vec![],
    };
    let lines: Vec<String> = lines.into_iter().filter(|l| !l.is_empty()).collect();
    (!lines.is_empty()).then(|| {
        lines.iter().flat_map(|l| l.lines().map(|x| format!("{indent}{x}")).collect::<Vec<_>>()).collect::<Vec<_>>().join("\n")
    })
}

/// Argument le plus parlant d'un appel d'outil (commande, fichier, motif…).
fn tool_summary(input: &Value) -> String {
    for key in ["command", "file_path", "path", "pattern", "url", "query", "description", "prompt"] {
        if let Some(x) = input.get(key).and_then(Value::as_str) {
            return one_line(x, 200);
        }
    }
    one_line(&input.to_string(), 200)
}

fn result_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Texte sur une ligne, tronqué à `max` caractères.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.trim().replace('\n', " ⏎ ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn adds_streaming_only_to_claude_print_without_format() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let out = ensure_streaming(v(&["claude", "-p", "{prompt}"]));
        assert_eq!(out, v(&["claude", "-p", "{prompt}", "--output-format", "stream-json", "--verbose"]));
        let kept = v(&["claude", "-p", "x", "--output-format", "text"]);
        assert_eq!(ensure_streaming(kept.clone()), kept);
        assert_eq!(ensure_streaming(v(&["pi", "-p", "x"])), v(&["pi", "-p", "x"]));
    }

    #[test]
    fn formats_stream_json_events() {
        let tool = json!({"type": "assistant", "parent_tool_use_id": null, "message": {"content": [
            {"type": "text", "text": "Let me check."},
            {"type": "tool_use", "name": "Bash", "input": {"command": "cargo test\n--all", "description": "Run"}}]}});
        assert_eq!(format_event(&tool).unwrap(), "💬 Let me check.\n→ Bash cargo test ⏎ --all");
        let res = json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "content": "ok\nline 2\nline 3", "is_error": false}]}});
        assert_eq!(format_event(&res).unwrap(), "  ← ok (+2 lines)");
        let sub = json!({"type": "user", "parent_tool_use_id": "t1", "message": {"content": [
            {"type": "tool_result", "content": [{"type": "text", "text": "boom"}], "is_error": true}]}});
        assert_eq!(format_event(&sub).unwrap(), "  │   ✗ boom");
        let done = json!({"type": "result", "subtype": "success", "num_turns": 3, "duration_ms": 61000,
                          "total_cost_usd": 0.1234, "result": "All done."});
        assert_eq!(format_event(&done).unwrap(), "Finished (success): 3 turns, 61s, $0.12\nAll done.");
        assert!(format_event(&json!({"type": "rate_limit_event"})).is_none());
    }

    #[test]
    fn detects_usage_limits() {
        let limited = "{\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"rejected\",\"resetsAt\":1793034000}}\n\
            {\"type\":\"result\",\"subtype\":\"success\",\"result\":\"You've hit your session limit · resets 11pm (Europe/Paris)\"}\n";
        let limit = pump(limited.as_bytes(), Vec::new(), Instant::now()).unwrap();
        assert_eq!(limit.resets_at, Some(1793034000));
        assert!(limit.message.contains("session limit"));
        // Événement « allowed » ou agent qui parle de limites : pas de limite.
        let fine = "{\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"allowed\",\"resetsAt\":1}}\n\
            {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"You've hit your session limit\"}]}}\n\
            {\"type\":\"result\",\"subtype\":\"success\",\"result\":\"Added a rate limiter; the limit is 600 req/min.\"}\n";
        assert_eq!(pump(fine.as_bytes(), Vec::new(), Instant::now()), None);
        // Agent en texte (pi…).
        assert!(pump("Error: usage limit reached\n".as_bytes(), Vec::new(), Instant::now()).is_some());
    }

    #[test]
    fn pump_stamps_lines_and_keeps_plain_text() {
        let input = "plain output\n{\"type\":\"rate_limit_event\"}\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"a\\nb\"}]}}\n";
        let mut out = Vec::new();
        assert_eq!(pump(input.as_bytes(), &mut out, Instant::now()), None);
        assert_eq!(String::from_utf8(out).unwrap(), "[00:00:00] plain output\n[00:00:00] 💬 a\n           b\n");
    }
}
