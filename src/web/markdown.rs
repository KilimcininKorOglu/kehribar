//! Markdown rendering for README files. The archived content is untrusted,
//! so the HTML is sanitised, and relative links point into the archive.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use ammonia::{Builder, UrlRelative, UrlRelativeEvaluate};
use pulldown_cmark::{Options, Parser, html};

/// Extensions that a relative link opens as a raw file, so images load.
const RAW_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "svg", "ico", "bmp", "avif",
];

/// Archived copies of external images: the image URL a document names,
/// and the local URL of its copy.
pub type AssetUrls = Arc<HashMap<String, String>>;

/// Builds the archive URL of a relative link in a README.
pub trait LinkTarget: Send + Sync {
    /// URL of the file page (`raw` false) or the raw file of `path`.
    fn file_url(&self, path: &str, raw: bool) -> String;

    /// The archived images to show instead of their external URLs.
    fn assets(&self) -> Option<AssetUrls> {
        None
    }
}

struct Rewriter<'a> {
    /// Directory of the README, from the repository root.
    base_dir: String,
    target: &'a dyn LinkTarget,
    /// Local image URLs the sanitiser itself wrote; they stay as they are.
    local: Vec<String>,
}

impl<'a> UrlRelativeEvaluate<'a> for Rewriter<'a> {
    fn evaluate<'url>(&self, url: &'url str) -> Option<Cow<'url, str>> {
        if url.starts_with('#') || self.local.iter().any(|local| local == url) {
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

/// Whether `path` names a Markdown file.
pub fn is_markdown(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown")
}

/// The unsanitised HTML of a Markdown document.
fn to_html(text: &str) -> String {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let mut unsafe_html = String::new();
    html::push_html(&mut unsafe_html, Parser::new_ext(text, options));
    unsafe_html
}

/// Sanitised HTML of a Markdown document found in `base_dir`.
pub fn render(text: &str, base_dir: &str, target: &dyn LinkTarget) -> String {
    clean(&to_html(text), base_dir, target)
}

/// Sanitises HTML that an archived document produced: scripts, event
/// handlers and unsafe URLs go, relative links point into the archive and
/// archived images replace their external URLs.
pub fn clean(unsafe_html: &str, base_dir: &str, target: &dyn LinkTarget) -> String {
    let assets = target.assets();
    let rewriter = Rewriter {
        base_dir: base_dir.to_owned(),
        target,
        local: assets
            .as_ref()
            .map(|map| map.values().cloned().collect())
            .unwrap_or_default(),
    };
    let mut builder = Builder::default();
    builder
        .url_relative(UrlRelative::Custom(Box::new(rewriter)))
        .link_rel(Some("noopener noreferrer nofollow"));
    if let Some(assets) = assets {
        builder.attribute_filter(move |element, attribute, value| {
            match (element, attribute, assets.get(value)) {
                ("img", "src", Some(local)) => Some(Cow::Owned(local.clone())),
                _ => Some(Cow::Borrowed(value)),
            }
        });
    }
    builder.clean(unsafe_html).to_string()
}

/// The external `http(s)` image URLs of a Markdown document, in order and
/// without repeats: Markdown images and HTML `<img>` elements, as the
/// sanitiser leaves them for the page.
pub fn image_urls(text: &str) -> Vec<String> {
    let found = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&found);
    Builder::default()
        .url_relative(UrlRelative::Deny)
        .attribute_filter(move |element, attribute, value| {
            let external = value.starts_with("https://") || value.starts_with("http://");
            if element == "img" && attribute == "src" && external {
                let mut urls = sink.lock().unwrap_or_else(PoisonError::into_inner);
                if !urls.iter().any(|url| url == value) {
                    urls.push(value.to_owned());
                }
            }
            Some(Cow::Borrowed(value))
        })
        .clean(&to_html(text));
    let mut urls = found.lock().unwrap_or_else(PoisonError::into_inner);
    std::mem::take(&mut *urls)
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

    struct WithAssets(AssetUrls);

    impl LinkTarget for WithAssets {
        fn file_url(&self, path: &str, _raw: bool) -> String {
            format!("/raw/{path}")
        }

        fn assets(&self) -> Option<AssetUrls> {
            Some(self.0.clone())
        }
    }

    #[test]
    fn external_images_are_found_and_replaced_by_their_copies() {
        let text = "![a](https://img.example/a.png?x=1&y=2) ![b](local.png)\n\n\
                    <p><img src=\"http://img.example/b.svg\" onerror=x></p>\n\n\
                    ![again](https://img.example/a.png?x=1&y=2) ![js](javascript:alert(1))\n\n\
                    [link](https://img.example/c.png)";
        assert_eq!(
            image_urls(text),
            [
                "https://img.example/a.png?x=1&y=2",
                "http://img.example/b.svg"
            ]
        );
        let target = WithAssets(Arc::new(HashMap::from([(
            "https://img.example/a.png?x=1&y=2".to_owned(),
            "/repos/1/asset/abc".to_owned(),
        )])));
        let html = render(text, "", &target);
        assert!(html.contains("src=\"/repos/1/asset/abc\""), "{html}");
        assert!(html.contains("src=\"http://img.example/b.svg\""), "{html}");
        assert!(html.contains("src=\"/raw/local.png\""), "{html}");
        // Only images are replaced, not links to the same address.
        assert!(
            html.contains("href=\"https://img.example/c.png\""),
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
