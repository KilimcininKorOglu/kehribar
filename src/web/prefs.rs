//! Theme and language preferences, stored in cookies.

use std::convert::Infallible;

use axum::Form;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::Deserialize;

use super::AppState;
use super::auth::local_path;
use crate::i18n::Lang;

pub const THEME_COOKIE: &str = "ga_theme";
pub const LANG_COOKIE: &str = "ga_lang";

text_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Theme {
        System => "system",
        Light => "light",
        Dark => "dark",
    }
}

/// The preferences of the current request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prefs {
    pub lang: Lang,
    pub theme: Theme,
}

impl Prefs {
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let jar = CookieJar::from_headers(headers);
        let cookie_lang = jar.get(LANG_COOKIE).and_then(|c| Lang::parse(c.value()));
        let header_lang = || {
            headers
                .get(header::ACCEPT_LANGUAGE)
                .and_then(|value| value.to_str().ok())
                .and_then(Lang::from_accept_language)
        };
        Self {
            lang: cookie_lang.or_else(header_lang).unwrap_or(Lang::Tr),
            theme: jar
                .get(THEME_COOKIE)
                .and_then(|c| Theme::parse(c.value()))
                .unwrap_or(Theme::System),
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Prefs {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::from_headers(&parts.headers))
    }
}

#[derive(Debug, Deserialize)]
pub struct PrefsForm {
    theme: Option<String>,
    lang: Option<String>,
    back: Option<String>,
}

/// Saves the chosen theme and language, then returns to the page the form was on.
pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    Form(form): Form<PrefsForm>,
) -> Response {
    let secure = state.config.cookie_secure;
    let mut jar = jar;
    if let Some(theme) = form.theme.as_deref().and_then(Theme::parse) {
        jar = jar.add(pref_cookie(THEME_COOKIE, theme.code(), secure));
    }
    if let Some(lang) = form.lang.as_deref().and_then(Lang::parse) {
        jar = jar.add(pref_cookie(LANG_COOKIE, lang.code(), secure));
    }
    if headers.contains_key("hx-request") {
        // htmx submits the form on change; a full reload applies the new values.
        let refresh = [("hx-refresh", HeaderValue::from_static("true"))];
        return (jar, refresh).into_response();
    }
    let back = local_path(form.back.as_deref()).to_owned();
    (jar, Redirect::to(&back)).into_response()
}

fn pref_cookie(name: &'static str, value: &'static str, secure: bool) -> Cookie<'static> {
    Cookie::build((name, value))
        .path("/")
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::days(365))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn defaults_to_turkish_and_system_theme() {
        let prefs = Prefs::from_headers(&HeaderMap::new());
        assert_eq!(
            prefs,
            Prefs {
                lang: Lang::Tr,
                theme: Theme::System
            }
        );
    }

    #[test]
    fn cookie_wins_over_accept_language() {
        let map = headers(&[
            (header::COOKIE, "ga_lang=tr; ga_theme=dark"),
            (header::ACCEPT_LANGUAGE, "en-US"),
        ]);
        assert_eq!(
            Prefs::from_headers(&map),
            Prefs {
                lang: Lang::Tr,
                theme: Theme::Dark
            }
        );
    }

    #[test]
    fn accept_language_applies_without_cookie_and_bad_values_are_ignored() {
        let map = headers(&[
            (header::COOKIE, "ga_lang=xx; ga_theme=neon"),
            (header::ACCEPT_LANGUAGE, "en"),
        ]);
        assert_eq!(
            Prefs::from_headers(&map),
            Prefs {
                lang: Lang::En,
                theme: Theme::System
            }
        );
    }
}
