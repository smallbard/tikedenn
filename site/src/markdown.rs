//! Rendu Markdown des descriptions et critères d'acceptation (CommonMark +
//! tableaux, listes de tâches, barré). Le HTML produit est nettoyé par
//! ammonia : les textes viennent aussi des agents, aucun script ne doit passer.
//! Les blocs ```mermaid deviennent `<pre class="mermaid">`, dessinés côté
//! navigateur par `static/app.js`.

use std::sync::OnceLock;

use axum::response::{Html, IntoResponse};
use axum::Form;
use pulldown_cmark::{html, CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd};
use serde::Deserialize;

/// Markdown → HTML sûr.
pub fn render(src: &str) -> String {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
    let mut in_mermaid = false;
    let events = Parser::new_ext(src, opts).map(|ev| match ev {
        Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(ref lang))) if lang.trim() == "mermaid" => {
            in_mermaid = true;
            Event::Html(CowStr::Borrowed("<pre class=\"mermaid\">"))
        }
        Event::End(TagEnd::CodeBlock) if in_mermaid => {
            in_mermaid = false;
            Event::Html(CowStr::Borrowed("</pre>"))
        }
        // Texte du graphe échappé : mermaid lit le textContent.
        Event::Text(t) if in_mermaid => Event::Html(CowStr::from(escape(&t))),
        ev => ev,
    });
    let mut out = String::new();
    html::push_html(&mut out, events);
    sanitizer().clean(&out).to_string()
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn sanitizer() -> &'static ammonia::Builder<'static> {
    static B: OnceLock<ammonia::Builder<'static>> = OnceLock::new();
    B.get_or_init(|| {
        let mut b = ammonia::Builder::default();
        b.add_allowed_classes("pre", &["mermaid"])
            // Cases à cocher des listes de tâches (désactivées par pulldown-cmark).
            .add_tags(&["input"])
            .add_tag_attributes("input", &["type", "checked", "disabled"])
            .link_rel(Some("noopener noreferrer nofollow"));
        b
    })
}

#[derive(Deserialize)]
pub struct PreviewForm {
    #[serde(default)]
    text: String,
}

/// Aperçu du texte en cours d'édition (onglet « Preview »).
pub async fn preview(Form(f): Form<PreviewForm>) -> impl IntoResponse {
    Html(render(&f.text))
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn renders_markdown_and_mermaid_safely() {
        let html = render("# Title\n\n- [x] done\n\n| a |\n|---|\n| b |\n\n<script>alert(1)</script>\n[x](javascript:alert(1))");
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<table>"));
        assert!(html.contains("type=\"checkbox\""));
        assert!(!html.contains("<script"));
        assert!(!html.contains("javascript:"));
        let html = render("```mermaid\ngraph TD; A-->B<script>\n```");
        assert!(html.contains("<pre class=\"mermaid\">graph TD; A--&gt;B&lt;script&gt;\n</pre>"), "{html}");
        // Un autre bloc de code garde son rendu habituel.
        assert!(render("```rust\nfn x() {}\n```").contains("<pre><code"));
    }
}
