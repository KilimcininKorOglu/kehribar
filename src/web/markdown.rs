//! Markdown rendering for README files. The archived content is untrusted,
//! so the HTML is sanitised, and relative links point into the archive.

use std::borrow::Cow;

use ammonia::{Builder, UrlRelative, UrlRelativeEvaluate};
use pulldown_cmark::{Options, Parser, html};

/// Extensions that a relative link opens as a raw file, so images load.
const RAW_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "svg", "ico", "bmp", "avif",
];

/// Builds the archive URL of a relative link in a README.
pub trait LinkTarget: Send + Sync {
    /// URL of the file page (`raw` false) or the raw file of `path`.
    fn file_url(&self, path: &str, raw: bool) -> String;
}

struct Rewriter<'a> {
    /// Directory of the README, from the repository root.
    base_dir: String,
    target: &'a dyn LinkTarget,
}

impl<'a> UrlRelativeEvaluate<'a> for Rewriter<'a> {
    fn evaluate<'url>(&self, url: &'url str) -> Option<Cow<'url, str>> {
        if url.starts_with('#') {
            return Some(Cow::Borrowed(url));
        }
        let (link, fragment) = url.split_once('#').unwrap_or((url, ""));
        let link = link.split_once('?').map_or(link, |(path, _)| path);
        let path = join(&self.base_dir, link)?;
        let raw = path
            .rsplit_once('.')
            .is_some_and(|(_, ext)| RAW_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()));
        let mut out = self.target.file_url(&path, raw);
        if !fragment.is_empty() && !raw {
            out.push('#');
            out.push_str(fragment);
        }
        Some(Cow::Owned(out))
    }
}

/// Resolves `link` against `base_dir`. `None` when it leaves the root.
fn join(base_dir: &str, link: &str) -> Option<String> {
    let mut parts: Vec<&str> = if link.starts_with('/') {
        Vec::new()
    } else {
        base_dir.split('/').filter(|p| !p.is_empty()).collect()
    };
    for part in link.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            name => parts.push(name),
        }
    }
    Some(parts.join("/"))
}

/// Sanitised HTML of a Markdown document found in `base_dir`.
pub fn render(text: &str, base_dir: &str, target: &dyn LinkTarget) -> String {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let mut unsafe_html = String::new();
    html::push_html(&mut unsafe_html, Parser::new_ext(text, options));
    let rewriter = Rewriter {
        base_dir: base_dir.to_owned(),
        target,
    };
    Builder::default()
        .url_relative(UrlRelative::Custom(Box::new(rewriter)))
        .link_rel(Some("noopener noreferrer nofollow"))
        .clean(&unsafe_html)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Target;

    impl LinkTarget for Target {
        fn file_url(&self, path: &str, raw: bool) -> String {
            format!("/{}/{path}", if raw { "raw" } else { "blob" })
        }
    }

    #[test]
    fn scripts_and_event_handlers_are_removed() {
        let html = render(
            "# Title\n\n<script>alert(1)</script>\n\n<img src=x onerror=alert(2)>\n\n[x](javascript:alert(3))",
            "",
            &Target,
        );
        assert!(html.contains("<h1>Title</h1>"), "{html}");
        for bad in ["<script", "onerror", "javascript:"] {
            assert!(!html.contains(bad), "{bad} in {html}");
        }
    }

    #[test]
    fn relative_links_point_into_the_archive() {
        let html = render(
            "[guide](guide.md#setup) ![logo](../img/logo.PNG) [top](#title) [out](../../x) [web](https://example.com)",
            "docs",
            &Target,
        );
        assert!(
            html.contains("href=\"/blob/docs/guide.md#setup\""),
            "{html}"
        );
        assert!(html.contains("src=\"/raw/img/logo.PNG\""), "{html}");
        assert!(html.contains("href=\"#title\""), "{html}");
        assert!(html.contains("href=\"https://example.com\""), "{html}");
        // A link above the repository root loses its target.
        assert!(
            html.contains("<a rel=\"noopener noreferrer nofollow\">out</a>"),
            "{html}"
        );
    }

    #[test]
    fn paths_are_joined_without_leaving_the_root() {
        assert_eq!(join("a/b", "../c").as_deref(), Some("a/c"));
        assert_eq!(join("a", "/top.md").as_deref(), Some("top.md"));
        assert_eq!(join("", "../x"), None);
    }
}
