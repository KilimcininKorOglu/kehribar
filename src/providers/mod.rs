//! Lists the repositories of a user or an organization through the REST
//! API of a hosting provider: GitHub, GitLab, Forgejo/Gitea or Bitbucket
//! Cloud. Every list is read to its last page.

mod bitbucket;
mod forgejo;
mod github;
mod gitlab;

use std::time::Duration;

use reqwest::{Client, RequestBuilder, Response};
use serde::de::DeserializeOwned;
use thiserror::Error;
use url::Url;

use crate::db::hosts::{Host, Provider};
use crate::db::sources::SourceKind;
use crate::git::Credentials;

/// A list longer than this many pages counts as broken, not as an archive.
pub const MAX_PAGES: usize = 500;
const TIMEOUT: Duration = Duration::from_secs(60);

/// One repository a provider lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRepo {
    /// The HTTP(S) clone URL.
    pub clone_url: String,
    pub fork: bool,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("the {0} provider has no repository list API")]
    Unsupported(&'static str),
    #[error("invalid API URL: {0}")]
    BadUrl(String),
    #[error("{url} answered HTTP {status}")]
    Status { status: u16, url: String },
    #[error("API request failed: {0}")]
    Request(String),
    #[error("the list has more than {MAX_PAGES} pages")]
    TooManyPages,
    #[error("the API sent a next page on another server: {0}")]
    ForeignPage(String),
}

impl From<reqwest::Error> for ProviderError {
    fn from(err: reqwest::Error) -> Self {
        // Without the URL: a next-page link can carry anything.
        Self::Request(err.without_url().to_string())
    }
}

/// How a request proves who it is.
enum Auth {
    None,
    /// `Authorization: Bearer <token>` (GitHub).
    Bearer(String),
    /// `PRIVATE-TOKEN: <token>` (GitLab).
    PrivateToken(String),
    /// `Authorization: token <token>` (Forgejo, Gitea).
    Token(String),
    /// HTTP Basic with the account name (Bitbucket).
    Basic {
        username: String,
        password: String,
    },
}

/// A client bound to one API base URL and its credentials.
pub struct Api {
    client: Client,
    base: Url,
    auth: Auth,
}

impl Api {
    /// The URL of `segments` below the base; each segment is escaped, so a
    /// GitLab group path `a/b` becomes `a%2Fb`.
    fn url(&self, segments: &[&str]) -> Result<Url, ProviderError> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|()| ProviderError::BadUrl(self.base.to_string()))?
            .pop_if_empty()
            .extend(segments);
        Ok(url)
    }

    fn get(&self, url: Url) -> RequestBuilder {
        let request = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json");
        match &self.auth {
            Auth::None => request,
            Auth::Bearer(token) => request.bearer_auth(token),
            Auth::PrivateToken(token) => request.header("PRIVATE-TOKEN", token),
            Auth::Token(token) => {
                request.header(reqwest::header::AUTHORIZATION, format!("token {token}"))
            }
            Auth::Basic { username, password } => request.basic_auth(username, Some(password)),
        }
    }

    async fn send(&self, url: Url) -> Result<Response, ProviderError> {
        let response = self.get(url.clone()).send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(ProviderError::Status {
                status: status.as_u16(),
                url: strip_query(url),
            });
        }
        Ok(response)
    }

    async fn json<T: DeserializeOwned>(&self, url: Url) -> Result<T, ProviderError> {
        Ok(self.send(url).await?.json().await?)
    }

    /// Reads a list that pages with `page=N` and a page size parameter,
    /// until a page comes back shorter than the page size.
    async fn paged<T: DeserializeOwned>(
        &self,
        url: Url,
        size_param: &str,
        size: usize,
    ) -> Result<Vec<T>, ProviderError> {
        let mut items = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut page_url = url.clone();
            page_url
                .query_pairs_mut()
                .append_pair(size_param, &size.to_string())
                .append_pair("page", &page.to_string());
            let batch: Vec<T> = self.json(page_url).await?;
            let last = batch.len() < size;
            items.extend(batch);
            if last {
                return Ok(items);
            }
        }
        Err(ProviderError::TooManyPages)
    }
}

/// The API base of a host when the host leaves it blank, or `None` for a
/// provider without a list API.
pub fn default_api_base(provider: Provider, host: &str) -> Option<String> {
    match provider {
        Provider::Github if host == "github.com" => Some("https://api.github.com".to_owned()),
        // GitHub Enterprise Server.
        Provider::Github => Some(format!("https://{host}/api/v3")),
        Provider::Gitlab => Some(format!("https://{host}/api/v4")),
        Provider::Forgejo => Some(format!("https://{host}/api/v1")),
        Provider::Bitbucket if host == "bitbucket.org" => {
            Some("https://api.bitbucket.org/2.0".to_owned())
        }
        Provider::Bitbucket => Some(format!("https://api.{host}/2.0")),
        Provider::Generic => None,
    }
}

/// Whether the provider of a host can list repositories.
pub fn can_list(provider: Provider) -> bool {
    provider != Provider::Generic
}

/// Lists every repository of `name` on `host`.
pub async fn list_repos(
    host: &Host,
    credentials: Option<&Credentials>,
    kind: SourceKind,
    name: &str,
) -> Result<Vec<RemoteRepo>, ProviderError> {
    let api = connect(host, credentials)?;
    match host.provider {
        Provider::Github => github::list(&api, kind, name).await,
        Provider::Gitlab => gitlab::list(&api, kind, name).await,
        Provider::Forgejo => forgejo::list(&api, kind, name).await,
        Provider::Bitbucket => bitbucket::list(&api, name).await,
        Provider::Generic => Err(ProviderError::Unsupported(Provider::Generic.code())),
    }
}

fn connect(host: &Host, credentials: Option<&Credentials>) -> Result<Api, ProviderError> {
    let base = host
        .api_base_url
        .clone()
        .or_else(|| default_api_base(host.provider, &host.host))
        .ok_or(ProviderError::Unsupported(host.provider.code()))?;
    let base = Url::parse(&base).map_err(|_| ProviderError::BadUrl(base.clone()))?;
    let client = Client::builder()
        .user_agent(concat!("git-archive/", env!("CARGO_PKG_VERSION")))
        .timeout(TIMEOUT)
        .build()?;
    Ok(Api {
        client,
        base,
        auth: auth(host.provider, credentials),
    })
}

fn auth(provider: Provider, credentials: Option<&Credentials>) -> Auth {
    let Some(credentials) = credentials else {
        return Auth::None;
    };
    let token = credentials.password.clone();
    match provider {
        Provider::Github => Auth::Bearer(token),
        Provider::Gitlab => Auth::PrivateToken(token),
        Provider::Forgejo => Auth::Token(token),
        Provider::Bitbucket => Auth::Basic {
            username: credentials.username.clone(),
            password: token,
        },
        Provider::Generic => Auth::None,
    }
}

/// The URL without its query, for error messages.
fn strip_query(mut url: Url) -> String {
    url.set_query(None);
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_bases_follow_the_host() {
        let cases = [
            (
                Provider::Github,
                "github.com",
                Some("https://api.github.com"),
            ),
            (
                Provider::Github,
                "git.corp",
                Some("https://git.corp/api/v3"),
            ),
            (
                Provider::Gitlab,
                "gitlab.com",
                Some("https://gitlab.com/api/v4"),
            ),
            (
                Provider::Forgejo,
                "codeberg.org",
                Some("https://codeberg.org/api/v1"),
            ),
            (
                Provider::Bitbucket,
                "bitbucket.org",
                Some("https://api.bitbucket.org/2.0"),
            ),
            (Provider::Generic, "git.example", None),
        ];
        for (provider, host, expected) in cases {
            assert_eq!(
                default_api_base(provider, host).as_deref(),
                expected,
                "{host}"
            );
        }
    }

    #[test]
    fn path_segments_are_escaped() {
        let api = Api {
            client: Client::new(),
            base: Url::parse("https://gitlab.example/api/v4/").unwrap(),
            auth: Auth::None,
        };
        assert_eq!(
            api.url(&["groups", "a/b c", "projects"]).unwrap().as_str(),
            "https://gitlab.example/api/v4/groups/a%2Fb%20c/projects"
        );
    }
}
