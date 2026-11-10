//! Shared page layout, rendering and static assets.

use std::sync::LazyLock;

use askama::Template;
use axum::Extension;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use sha2::{Digest, Sha256};

use super::AppState;
use super::auth::CurrentSession;
use super::prefs::{Prefs, Theme};
use crate::error::AppError;
use crate::i18n::{Lang, Tr};

const APP_CSS: &str = include_str!("../../static/app.css");
const HTMX_JS: &str = include_str!("../../static/htmx.min.js");

/// Short content hash appended to asset URLs, so a new build is never served
/// from a stale browser cache.
static ASSET_VERSION: LazyLock<String> = LazyLock::new(|| {
    let digest = Sha256::new()
        .chain_update(APP_CSS)
        .chain_update(HTMX_JS)
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
}

impl Layout {
    pub fn public(state: &AppState, prefs: Prefs, path: &str) -> Self {
        Self {
            tr: Tr::new(state.catalog.clone(), prefs.lang),
            theme: prefs.theme,
            csrf: None,
            nav: Nav::None,
            path: path.to_owned(),
        }
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

#[derive(Template)]
#[template(path = "home.html")]
struct HomePage {
    layout: Layout,
}

pub async fn home(
    State(state): State<AppState>,
    prefs: Prefs,
    Extension(session): Extension<CurrentSession>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response, AppError> {
    let page = HomePage {
        layout: Layout::private(&state, prefs, &session, Nav::Repos, uri.path()),
    };
    Ok(render(&page)?.into_response())
}

pub async fn static_file(Path(file): Path<String>) -> Response {
    let (body, content_type) = match file.as_str() {
        "app.css" => (APP_CSS, "text/css; charset=utf-8"),
        "htmx.min.js" => (HTMX_JS, "text/javascript; charset=utf-8"),
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
