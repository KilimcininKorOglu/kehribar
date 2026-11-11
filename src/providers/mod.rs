//! Lists the repositories of a user or an organization, or the ones a user
//! starred, through the API of a hosting provider: GitHub, GitLab,
//! Forgejo/Gitea, Bitbucket Cloud, Gitee, Azure DevOps or SourceHut. Every
//! list is read to its last page.

mod azure;
mod bitbucket;
mod forgejo;
mod gitee;
mod github;
mod gitlab;
mod sourcehut;

use std::time::Duration;

use reqwest::header::HeaderMap;
use reqwest::redirect::Policy;
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use thiserror::Error;
use url::Url;

use crate::db::hosts::{Host, Provider};
use crate::db::sources::SourceKind;
use crate::db::unix_now;
use crate::git::Credentials;
use crate::repo_url::{self, GIST_HOST};

/// A list longer than this many pages counts as broken, not as an archive.
pub const MAX_PAGES: usize = 500;
const TIMEOUT: Duration = Duration::from_secs(60);
/// The wait after a rate limit answer that names no time.
pub const RATE_LIMIT_WAIT_SECONDS: i64 = 15 * 60;

/// One repository a provider lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRepo {
    /// The HTTP(S) clone URL.
    pub clone_url: String,
    /// The SSH clone URL, when the provider names one.
    pub ssh_url: Option<String>,
    pub fork: bool,
    /// The provider marks the repository read-only (archived).
    pub archived: bool,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("the {0} provider has no repository list API")]
    Unsupported(&'static str),
    #[error("the {provider} provider has no {kind} list")]
    KindUnsupported {
        provider: &'static str,
        kind: &'static str,
    },
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
    #[error("the API answered with an error: {0}")]
    Answer(String),
    /// The API refused the request because of its rate limit.
    #[error("the API rate limit is reached until Unix time {until}")]
    RateLimited { until: i64 },
}

impl ProviderError {
    fn kind(provider: Provider, kind: SourceKind) -> Self {
        Self::KindUnsupported {
            provider: provider.code(),
            kind: kind.code(),
        }
    }
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
    /// `Authorization: Bearer <token>` (GitHub, SourceHut).
    Bearer(String),
    /// The `access_token` query parameter (Gitee).
    Query(String),
    /// `PRIVATE-TOKEN: <token>` (GitLab).
    PrivateToken(String),
    /// `Authorization: token <token>` (Forgejo, Gitea).
    Token(String),
    /// HTTP Basic with the account name (Bitbucket, Azure DevOps).
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

    fn get(&self, mut url: Url) -> RequestBuilder {
        if let Auth::Query(token) = &self.auth {
            // Error messages drop the query, so the token stays out of them.
            url.query_pairs_mut().append_pair("access_token", token);
        }
        self.authorized(self.client.get(url))
    }

    fn authorized(&self, request: RequestBuilder) -> RequestBuilder {
        let request = request.header(reqwest::header::ACCEPT, "application/json");
        match &self.auth {
            Auth::None | Auth::Query(_) => request,
            Auth::Bearer(token) => request.bearer_auth(token),
            Auth::PrivateToken(token) => request.header("PRIVATE-TOKEN", token),
            Auth::Token(token) => {
                request.header(reqwest::header::AUTHORIZATION, format!("token {token}"))
            }
            Auth::Basic { username, password } => request.basic_auth(username, Some(password)),
        }
    }

    async fn send(&self, url: Url) -> Result<Response, ProviderError> {
        checked(self.get(url.clone()).send().await?, url)
    }

    /// Posts `body` as JSON to the base URL and reads the JSON answer.
    async fn post_json<T: DeserializeOwned>(
        &self,
        body: &serde_json::Value,
    ) -> Result<T, ProviderError> {
        let request = self.authorized(self.client.post(self.base.clone()).json(body));
        Ok(checked(request.send().await?, self.base.clone())?
            .json()
            .await?)
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
        self.pages(url, size_param, size, |len| len < size).await
    }

    /// Reads pages until `last` says, from the number of items of a page,
    /// that it was the last one.
    async fn pages<T: DeserializeOwned>(
        &self,
        url: Url,
        size_param: &str,
        size: usize,
        last: impl Fn(usize) -> bool,
    ) -> Result<Vec<T>, ProviderError> {
        let mut items = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut page_url = url.clone();
            page_url
                .query_pairs_mut()
                .append_pair(size_param, &size.to_string())
                .append_pair("page", &page.to_string());
            let batch: Vec<T> = self.json(page_url).await?;
            let done = last(batch.len());
            items.extend(batch);
            if done {
                return Ok(items);
            }
        }
        Err(ProviderError::TooManyPages)
    }
}

/// The API base of a host when the host leaves it blank, or `None` for a
/// provider without a list API.
pub fn default_api_base(provider: Provider, host: &str) -> Option<String> {
    match (provider, host) {
        (Provider::Github, "github.com") => Some("https://api.github.com".to_owned()),
        (Provider::Bitbucket, "bitbucket.org") => Some("https://api.bitbucket.org/2.0".to_owned()),
        (Provider::Bitbucket, _) => Some(format!("https://api.{host}/2.0")),
        _ => api_path(provider).map(|path| format!("https://{host}{path}")),
    }
}

/// The API path below the host name of a self-hosted server, as GitHub
/// Enterprise Server, Azure DevOps Server and the others use it.
fn api_path(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::Github => Some("/api/v3"),
        Provider::Gitlab => Some("/api/v4"),
        Provider::Forgejo => Some("/api/v1"),
        Provider::Gitee => Some("/api/v5"),
        Provider::Azure => Some(""),
        Provider::Sourcehut => Some("/query"),
        Provider::Bitbucket | Provider::Generic => None,
    }
}

/// Whether the provider of a host can list repositories.
pub fn can_list(provider: Provider) -> bool {
    provider != Provider::Generic
}

/// Whether the provider of `host` can list sources of `kind`. Azure DevOps
/// lists organizations and projects only, SourceHut users only, and gists
/// exist only on github.com.
pub fn supports(host: &Host, kind: SourceKind) -> bool {
    match kind {
        SourceKind::User => can_list(host.provider) && host.provider != Provider::Azure,
        SourceKind::Org => can_list(host.provider) && host.provider != Provider::Sourcehut,
        SourceKind::Starred => matches!(
            host.provider,
            Provider::Github | Provider::Gitlab | Provider::Forgejo | Provider::Gitee
        ),
        SourceKind::Gists => {
            host.provider == Provider::Github && host.host == repo_url::settings_host(GIST_HOST)
        }
    }
}

/// How many `/`-separated parts a source name of `kind` may have: GitLab
/// groups nest, and an Azure DevOps source is an organization or
/// `organization/project`.
pub fn name_depth(provider: Provider, kind: SourceKind) -> usize {
    match (provider, kind) {
        (Provider::Gitlab, SourceKind::Org) => usize::MAX,
        (Provider::Azure, SourceKind::Org) => 2,
        _ => 1,
    }
}

/// Lists every repository of `name` on `host`.
pub async fn list_repos(
    host: &Host,
    credentials: Option<&Credentials>,
    kind: SourceKind,
    name: &str,
) -> Result<Vec<RemoteRepo>, ProviderError> {
    // A provider without any list keeps its own error below.
    if can_list(host.provider) && !supports(host, kind) {
        return Err(ProviderError::kind(host.provider, kind));
    }
    let api = connect(host, credentials)?;
    list_with(&api, host.provider, kind, name).await
}

async fn list_with(
    api: &Api,
    provider: Provider,
    kind: SourceKind,
    name: &str,
) -> Result<Vec<RemoteRepo>, ProviderError> {
    match provider {
        Provider::Github => github::list(api, kind, name).await,
        Provider::Gitlab => gitlab::list(api, kind, name).await,
        Provider::Forgejo => forgejo::list(api, kind, name).await,
        Provider::Bitbucket => bitbucket::list(api, name).await,
        Provider::Gitee => gitee::list(api, kind, name).await,
        Provider::Azure => azure::list(api, name).await,
        Provider::Sourcehut => sourcehut::list(api, name).await,
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
        .redirect(redirect_policy())
        .build()?;
    Ok(Api {
        client,
        base,
        auth: auth(host.provider, credentials),
    })
}

/// Redirects a list request follows at most.
const MAX_REDIRECTS: usize = 10;

/// Follows a redirect only to the origin of the first request, or to https
/// on its host. reqwest drops the standard credential headers on another
/// host, but not GitLab's `PRIVATE-TOKEN`; a stopped redirect fails the
/// request with its 3xx status.
fn redirect_policy() -> Policy {
    Policy::custom(|attempt| {
        let allowed = attempt
            .previous()
            .first()
            .is_some_and(|first| may_follow(first, attempt.url()));
        if attempt.previous().len() > MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if allowed {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

fn may_follow(first: &Url, next: &Url) -> bool {
    next.host_str() == first.host_str()
        && (next.origin() == first.origin() || next.scheme() == "https")
}

fn auth(provider: Provider, credentials: Option<&Credentials>) -> Auth {
    let Some(credentials) = credentials else {
        return Auth::None;
    };
    let token = credentials.password.clone();
    match provider {
        Provider::Github | Provider::Sourcehut => Auth::Bearer(token),
        Provider::Gitlab => Auth::PrivateToken(token),
        Provider::Forgejo => Auth::Token(token),
        Provider::Gitee => Auth::Query(token),
        // Azure DevOps ignores the username next to a personal access token.
        Provider::Bitbucket | Provider::Azure => Auth::Basic {
            username: credentials.username.clone(),
            password: token,
        },
        Provider::Generic => Auth::None,
    }
}

/// The response when its status is a success, else an error naming `url`.
fn checked(response: Response, url: Url) -> Result<Response, ProviderError> {
    let status = response.status();
    if let Some(until) = rate_limit_until(status, response.headers(), unix_now()) {
        return Err(ProviderError::RateLimited { until });
    }
    if !status.is_success() {
        return Err(ProviderError::Status {
            status: status.as_u16(),
            url: strip_query(url),
        });
    }
    Ok(response)
}

/// The time from which a rate-limited API takes requests again, or `None`
/// when the answer is no rate limit. A 429 is one; a 403 is one when the
/// remaining quota is zero or the server asks to retry later (GitHub's
/// secondary limit). `Retry-After` gives seconds; `X-RateLimit-Reset`
/// (GitHub, Gitea) and `RateLimit-Reset` (GitLab) give a Unix time.
fn rate_limit_until(status: StatusCode, headers: &HeaderMap, now: i64) -> Option<i64> {
    let number =
        |name: &str| -> Option<i64> { headers.get(name)?.to_str().ok()?.trim().parse().ok() };
    let exhausted = number("x-ratelimit-remaining") == Some(0)
        || number("ratelimit-remaining") == Some(0)
        || number("retry-after").is_some();
    let limited =
        status == StatusCode::TOO_MANY_REQUESTS || (status == StatusCode::FORBIDDEN && exhausted);
    if !limited {
        return None;
    }
    let until = number("retry-after")
        .map(|seconds| now + seconds)
        .or_else(|| number("x-ratelimit-reset"))
        .or_else(|| number("ratelimit-reset"))
        .unwrap_or(now + RATE_LIMIT_WAIT_SECONDS);
    Some(until.max(now + 1))
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
            (
                Provider::Gitee,
                "gitee.com",
                Some("https://gitee.com/api/v5"),
            ),
            (
                Provider::Azure,
                "dev.azure.com",
                Some("https://dev.azure.com"),
            ),
            (
                Provider::Sourcehut,
                "git.sr.ht",
                Some("https://git.sr.ht/query"),
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

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn rate_limit_answers_name_the_time_of_the_next_request() {
        let now = 1_000;
        let until = |status: u16, pairs: &[(&'static str, &str)]| {
            rate_limit_until(StatusCode::from_u16(status).unwrap(), &headers(pairs), now)
        };
        // A 429 waits for Retry-After, else the reset time, else the default.
        assert_eq!(until(429, &[("retry-after", "120")]), Some(1_120));
        assert_eq!(until(429, &[("ratelimit-reset", "5000")]), Some(5_000));
        assert_eq!(until(429, &[]), Some(now + RATE_LIMIT_WAIT_SECONDS));
        // GitHub: a 403 with no quota left, waiting for the reset time.
        assert_eq!(
            until(
                403,
                &[
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset", "4000")
                ]
            ),
            Some(4_000)
        );
        // A reset in the past still waits a moment.
        assert_eq!(until(429, &[("x-ratelimit-reset", "10")]), Some(now + 1));
        // A plain 403, a quota left or a success is no rate limit.
        assert_eq!(until(403, &[]), None);
        assert_eq!(until(403, &[("x-ratelimit-remaining", "12")]), None);
        assert_eq!(until(200, &[("x-ratelimit-remaining", "0")]), None);
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
