//! Notifications push via ntfy (https://ntfy.sh ou instance auto-hébergée) :
//! tâche à relire, déploiement échoué ou terminé. Désactivées si `NTFY_URL`
//! (URL complète du topic) n'est pas définie. `NTFY_TOKEN` : jeton d'accès
//! optionnel ; `PUBLIC_URL` : base des liens cliquables des notifications.

use std::sync::OnceLock;

struct Config {
    url: String,
    token: String,
    public_url: String,
}

fn config() -> Option<&'static Config> {
    static CONFIG: OnceLock<Option<Config>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let url = std::env::var("NTFY_URL").ok().filter(|u| !u.trim().is_empty())?;
            Some(Config {
                url,
                token: std::env::var("NTFY_TOKEN").unwrap_or_default(),
                public_url: std::env::var("PUBLIC_URL")
                    .unwrap_or_default()
                    .trim_end_matches('/')
                    .to_string(),
            })
        })
        .as_ref()
}

/// Les en-têtes http n'acceptent pas les retours à la ligne ; ntfy décode
/// l'UTF-8 des en-têtes, on se contente de les aplatir.
fn header_value(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).take(200).collect()
}

/// Envoie une notification en tâche de fond (sans bloquer la requête ; un
/// échec est seulement journalisé). `path` : page de l'app à ouvrir au clic.
pub fn send(title: &str, message: &str, path: &str) {
    let Some(cfg) = config() else { return };
    let title = header_value(title);
    let message = message.to_string();
    let click = if cfg.public_url.is_empty() {
        None
    } else {
        Some(format!("{}{path}", cfg.public_url))
    };
    std::thread::spawn(move || {
        let mut req = ureq::post(&cfg.url)
            .timeout(std::time::Duration::from_secs(10))
            .set("Title", &title)
            .set("Tags", "clipboard");
        if let Some(click) = &click {
            req = req.set("Click", click);
        }
        if !cfg.token.is_empty() {
            req = req.set("Authorization", &format!("Bearer {}", cfg.token));
        }
        if let Err(e) = req.send_string(&message) {
            eprintln!("[tikedenn] ntfy notification failed: {e}");
        }
    });
}
