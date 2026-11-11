//! Shared page layout, rendering and static assets.

use std::sync::LazyLock;

use askama::Template;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use sha2::{Digest, Sha256};

use super::auth::CurrentSession;
use super::prefs::{Prefs, Theme};
use super::{AppState, format, highlight};
use crate::db::hosts::Provider;
use crate::db::repos::RepoStatus;
use crate::db::unix_now;
use crate::error::AppError;
use crate::i18n::{Lang, Tr};

const APP_CSS: &str = include_str!("../../static/app.css");
const HTMX_JS: &str = include_str!("../../static/htmx.min.js");
const FAVICON_SVG: &str = include_str!("../../static/favicon.svg");

/// Short content hash appended to asset URLs, so a new build is never served
/// from a stale browser cache.
static ASSET_VERSION: LazyLock<String> = LazyLock::new(|| {
    let digest = Sha256::new()
        .chain_update(APP_CSS)
        .chain_update(HTMX_JS)
        .chain_update(FAVICON_SVG)
        .chain_update(highlight::CSS.as_str())
        .finalize();
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
});

/// Main menu entries; the current one is highlighted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    None,
    Repos,
    Sources,
    Hosts,
    Settings,
}

/// Data every page passes to `base.html`.
#[derive(Debug, Clone)]
pub struct Layout {
    pub tr: Tr,
    pub theme: Theme,
    /// Set only on signed-in pages; it also turns the menu on.
    pub csrf: Option<String>,
    pub nav: Nav,
    /// Path of this page, where the preference form returns to.
    pub path: String,
    /// Render time, the reference point of relative times.
    pub now: i64,
}

impl Layout {
    pub fn public(state: &AppState, prefs: Prefs, path: &str) -> Self {
        Self {
            tr: Tr::new(state.catalog.clone(), prefs.lang),
            theme: prefs.theme,
            csrf: None,
            nav: Nav::None,
            path: path.to_owned(),
            now: unix_now(),
        }
    }

    pub fn tf(&self, key: &str, args: &[(&str, &str)]) -> String {
        self.tr.tf(key, args)
    }

    // The helpers below take references, because askama passes every
    // template argument by reference.

    /// The message for `key` with `{n}` replaced by `n`, singular for one.
    pub fn tf_number(&self, key: &str, n: &i64) -> String {
        self.tr.tf_count(key, *n)
    }

    /// The CSRF token for hidden form fields; empty on public pages.
    pub fn csrf_token(&self) -> &str {
        self.csrf.as_deref().unwrap_or_default()
    }

    pub fn status_label(&self, status: &RepoStatus) -> &str {
        self.t(match status {
            RepoStatus::Pending => "status.pending",
            RepoStatus::Syncing => "status.syncing",
            RepoStatus::Ok => "status.ok",
            RepoStatus::Error => "status.error",
            RepoStatus::Unreachable => "status.unreachable",
        })
    }

    pub fn provider_label(&self, provider: &Provider) -> &str {
        self.t(match provider {
            Provider::Github => "provider.github",
            Provider::Gitlab => "provider.gitlab",
            Provider::Forgejo => "provider.forgejo",
            Provider::Bitbucket => "provider.bitbucket",
            Provider::Generic => "provider.generic",
        })
    }

    /// `2026-11-10 19:04 UTC`.
    pub fn absolute(&self, ts: &i64) -> String {
        format::absolute(*ts)
    }

    /// `5 dk önce`, relative to the render time.
    pub fn ago(&self, ts: &i64) -> String {
        format::relative(&self.tr, self.now, *ts)
    }

    pub fn bytes(&self, size: &i64) -> String {
        format::bytes(self.tr.lang(), *size)
    }

    /// Takes a value: askama passes a computed expression such as
    /// `end - start` by value.
    pub fn duration(&self, seconds: i64) -> String {
        format::duration(&self.tr, seconds)
    }

    pub fn private(
        state: &AppState,
        prefs: Prefs,
        session: &CurrentSession,
        nav: Nav,
        path: &str,
    ) -> Self {
        Self {
            csrf: Some(session.csrf_token.clone()),
            nav,
            ..Self::public(state, prefs, path)
        }
    }

    pub fn t<'a>(&'a self, key: &'a str) -> &'a str {
        self.tr.t(key)
    }

    pub fn lang_code(&self) -> &'static str {
        self.tr.lang().code()
    }

    /// The `data-theme` value, or `None` to follow the system setting.
    pub fn theme_attr(&self) -> Option<&'static str> {
        (self.theme != Theme::System).then(|| self.theme.code())
    }

    pub fn themes(&self) -> [Theme; 3] {
        Theme::ALL
    }

    pub fn langs(&self) -> [Lang; 2] {
        Lang::ALL
    }

    pub fn is_theme(&self, theme: &Theme) -> bool {
        self.theme == *theme
    }

    pub fn is_lang(&self, lang: &Lang) -> bool {
        self.tr.lang() == *lang
    }

    pub fn theme_label(&self, theme: &Theme) -> &str {
        self.t(match theme {
            Theme::System => "theme.system",
            Theme::Light => "theme.light",
            Theme::Dark => "theme.dark",
        })
    }

    pub fn lang_label(&self, lang: &Lang) -> &str {
        self.t(match lang {
            Lang::Tr => "lang.tr",
            Lang::En => "lang.en",
        })
    }

    /// `aria-current` value for a menu entry.
    pub fn current(&self, nav: &str) -> &'static str {
        let active = matches!(
            (self.nav, nav),
            (Nav::Repos, "repos")
                | (Nav::Sources, "sources")
                | (Nav::Hosts, "hosts")
                | (Nav::Settings, "settings")
        );
        if active { "page" } else { "false" }
    }

    pub fn asset_version(&self) -> &str {
        &ASSET_VERSION
    }
}

pub fn render<T: Template>(page: &T) -> Result<Html<String>, AppError> {
    page.render().map(Html).map_err(AppError::internal)
}

pub async fn static_file(Path(file): Path<String>) -> Response {
    let (body, content_type) = match file.as_str() {
        "app.css" => (APP_CSS, "text/css; charset=utf-8"),
        "htmx.min.js" => (HTMX_JS, "text/javascript; charset=utf-8"),
        "favicon.svg" => (FAVICON_SVG, "image/svg+xml"),
        "highlight.css" => (highlight::CSS.as_str(), "text/css; charset=utf-8"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            // URLs carry the content hash, so the files can be cached for long.
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        body,
    )
        .into_response()
}
