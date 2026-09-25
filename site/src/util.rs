//! Petits utilitaires partagés.

use std::sync::OnceLock;

/// Encode une chaîne en pourcents (caractères non réservés conservés), pour
/// un paramètre de query ou un `filename*` RFC 5987.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Auto-inscription activée ? `REGISTRATION=0` la désactive (routes
/// /register absentes, liens « Sign up » masqués) ; ouverte par défaut.
/// Lu une fois au démarrage ; appelé aussi depuis les templates.
pub fn registration_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("REGISTRATION").unwrap_or_default() != "0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_keeps_unreserved_and_encodes_the_rest() {
        assert_eq!(urlencode("a-Z_0.~"), "a-Z_0.~");
        assert_eq!(urlencode("a b/é?"), "a%20b%2F%C3%A9%3F");
        assert_eq!(urlencode(""), "");
    }
}
