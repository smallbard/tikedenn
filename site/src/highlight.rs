//! Coloration syntaxique des fichiers du diff (syntect, avec les grammaires
//! et thèmes de bat fournis par two-face). On colore la version complète du
//! fichier (avant ou après) jusqu'à la dernière ligne affichée, pour que
//! chaînes et commentaires sur plusieurs lignes restent justes, puis le diff
//! reprend les lignes dont il a besoin par leur numéro.

use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color, FontStyle, Style, Theme};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

/// Au-delà, le fichier est affiché sans couleurs (temps de rendu).
pub const MAX_FILE_BYTES: usize = 512 * 1024;

fn syntaxes() -> &'static SyntaxSet {
    static S: OnceLock<SyntaxSet> = OnceLock::new();
    S.get_or_init(two_face::syntax::extra_newlines)
}

fn theme() -> &'static Theme {
    static T: OnceLock<Theme> = OnceLock::new();
    T.get_or_init(|| two_face::theme::extra().get(two_face::theme::EmbeddedThemeName::OneHalfDark).clone())
}

/// Échappement HTML d'un texte.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Grammaire d'un fichier : par nom (Dockerfile, Makefile…), extension, puis
/// première ligne (#!/bin/sh…). `None` pour du texte brut.
fn syntax_for(path: &str, text: &str) -> Option<&'static SyntaxReference> {
    let ss = syntaxes();
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e);
    ss.find_syntax_by_extension(name)
        .or_else(|| ext.and_then(|e| ss.find_syntax_by_extension(e)))
        .or_else(|| ss.find_syntax_by_first_line(text.lines().next().unwrap_or("")))
        .filter(|s| s.name != "Plain Text")
}

/// Lignes HTML colorées de `text` (au plus `upto` lignes) ; `None` si le
/// langage est inconnu ou le fichier trop gros.
pub fn highlight_lines(path: &str, text: &str, upto: usize) -> Option<Vec<String>> {
    if text.len() > MAX_FILE_BYTES || upto == 0 {
        return None;
    }
    let syntax = syntax_for(path, text)?;
    let theme = theme();
    let default_fg = theme.settings.foreground.unwrap_or(Color::WHITE);
    let mut h = HighlightLines::new(syntax, theme);
    let mut out = Vec::with_capacity(upto);
    for line in LinesWithEndings::from(text).take(upto) {
        let regions = h.highlight_line(line, syntaxes()).ok()?;
        out.push(line_html(&regions, default_fg));
    }
    Some(out)
}

fn line_html(regions: &[(Style, &str)], default_fg: Color) -> String {
    let mut html = String::new();
    // Fusion des morceaux voisins de même style.
    let mut merged: Vec<(Style, String)> = vec![];
    for (style, piece) in regions {
        let piece = piece.trim_end_matches(['\n', '\r']);
        if piece.is_empty() {
            continue;
        }
        match merged.last_mut() {
            Some((s, text)) if s.foreground == style.foreground && s.font_style == style.font_style => text.push_str(piece),
            _ => merged.push((*style, piece.to_string())),
        }
    }
    for (style, piece) in merged {
        let mut css = String::new();
        let c = style.foreground;
        if c != default_fg {
            css.push_str(&format!("color:#{:02x}{:02x}{:02x}", c.r, c.g, c.b));
        }
        if style.font_style.contains(FontStyle::ITALIC) {
            css.push_str(if css.is_empty() { "font-style:italic" } else { ";font-style:italic" });
        }
        if style.font_style.contains(FontStyle::BOLD) {
            css.push_str(if css.is_empty() { "font-weight:600" } else { ";font-weight:600" });
        }
        if css.is_empty() || piece.trim().is_empty() {
            html.push_str(&escape(&piece));
        } else {
            html.push_str(&format!("<span style=\"{css}\">{}</span>", escape(&piece)));
        }
    }
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_known_languages_and_escapes() {
        let src = "fn main() {\n    let s = \"<b>\";\n}\n";
        let lines = highlight_lines("src/main.rs", src, 10).unwrap();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("<span style=\"color:#"), "{}", lines[0]);
        assert!(lines[1].contains("&lt;b&gt;") && !lines[1].contains("<b>"));
        // Arrêt à `upto` lignes.
        assert_eq!(highlight_lines("a.rs", src, 2).unwrap().len(), 2);
        // Par nom de fichier, extension ajoutée par two-face, première ligne.
        assert!(highlight_lines("deploy/Dockerfile", "FROM rust\n", 1).is_some());
        assert!(highlight_lines("Cargo.toml", "[package]\nname = \"x\"\n", 2).is_some());
        assert!(highlight_lines("bin/run", "#!/bin/sh\necho hi\n", 2).is_some());
        assert!(highlight_lines("notes.txt", "hello\n", 1).is_none());
    }

    #[test]
    fn multiline_comments_keep_their_state() {
        let src = "/* start\n   still a comment\n*/\nlet x = 1;\n";
        let lines = highlight_lines("a.js", src, 4).unwrap();
        assert_eq!(lines[1].matches("<span").count(), 1, "{}", lines[1]);
        assert!(lines[1].starts_with("<span style=\"color:#"));
    }
}
