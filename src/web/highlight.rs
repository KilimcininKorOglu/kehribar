//! Syntax highlighting with CSS classes, so one HTML output serves both the
//! light and the dark theme.

use std::sync::LazyLock;

use syntect::highlighting::ThemeSet;
use syntect::html::{ClassStyle, ClassedHTMLGenerator, css_for_theme_with_class_style};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

const CLASS_STYLE: ClassStyle = ClassStyle::SpacedPrefixed { prefix: "hl-" };
/// Larger files are shown as plain text: highlighting is slow on them.
const MAX_HIGHLIGHT_BYTES: usize = 256 * 1024;
const LIGHT_THEME: &str = "InspiredGitHub";
const DARK_THEME: &str = "base16-ocean.dark";

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// The stylesheet for both themes. The dark rules apply under
/// `data-theme="dark"`, and under the system dark setting when the user
/// chose no theme. CSS nesting scopes the generated rules.
pub static CSS: LazyLock<String> = LazyLock::new(|| {
    let themes = ThemeSet::load_defaults();
    let css = |name: &str| {
        themes
            .themes
            .get(name)
            .and_then(|theme| css_for_theme_with_class_style(theme, CLASS_STYLE).ok())
            .unwrap_or_default()
    };
    let (light, dark) = (css(LIGHT_THEME), css(DARK_THEME));
    format!(
        ":root:not([data-theme=\"dark\"]) {{\n{light}\n}}\n\
         :root[data-theme=\"dark\"] {{\n{dark}\n}}\n\
         @media (prefers-color-scheme: dark) {{\n:root:not([data-theme]) {{\n{dark}\n}}\n}}\n"
    )
});

fn syntax_for(path: &str, text: &str) -> &'static SyntaxReference {
    let name = path.rsplit('/').next().unwrap_or(path);
    let extension = name.rsplit_once('.').map_or(name, |(_, ext)| ext);
    SYNTAXES
        .find_syntax_by_extension(extension)
        .or_else(|| SYNTAXES.find_syntax_by_extension(name))
        .or_else(|| {
            text.lines()
                .next()
                .and_then(|first| SYNTAXES.find_syntax_by_first_line(first))
        })
        .unwrap_or_else(|| SYNTAXES.find_syntax_plain_text())
}

/// Highlighted HTML of `text`, safe to embed: every text character is
/// escaped and only `<span class>` elements are added. Text that is too
/// large or fails to parse is escaped without highlighting.
pub fn to_html(path: &str, text: &str) -> String {
    if text.len() > MAX_HIGHLIGHT_BYTES {
        return escape(text);
    }
    let syntax = syntax_for(path, text);
    let mut generator = ClassedHTMLGenerator::new_with_class_style(syntax, &SYNTAXES, CLASS_STYLE);
    for line in LinesWithEndings::from(text) {
        if generator
            .parse_html_for_line_which_includes_newline(line)
            .is_err()
        {
            return escape(text);
        }
    }
    generator.finalize()
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_highlighted_and_escaped() {
        let html = to_html("src/main.rs", "fn main() { let s = \"<script>\"; }\n");
        assert!(html.contains("hl-"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    fn unknown_and_large_files_are_plain_escaped_text() {
        let html = to_html("notes.unknown-ext", "<b>bold</b>\n");
        assert!(html.contains("&lt;b&gt;bold&lt;/b&gt;"), "{html}");
        let big = "<".repeat(MAX_HIGHLIGHT_BYTES + 1);
        assert_eq!(
            to_html("a.rs", &big),
            "&lt;".repeat(MAX_HIGHLIGHT_BYTES + 1)
        );
    }

    #[test]
    fn stylesheet_has_both_themes() {
        assert!(CSS.contains(":root[data-theme=\"dark\"]"));
        assert!(CSS.contains(".hl-"));
    }
}
