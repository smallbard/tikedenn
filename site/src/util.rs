//! Petits utilitaires partagés.

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
