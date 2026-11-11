//! GitHub and GitHub Enterprise Server: `GET /users/{name}/repos`,
//! `GET /orgs/{name}/repos`, `GET /users/{name}/starred` and, on
//! github.com, `GET /users/{name}/gists`.

use serde::Deserialize;
use url::Url;

use super::{Api, Auth, ProviderError, RemoteRepo};
use crate::db::sources::SourceKind;
use crate::repo_url::GIST_HOST;

const PER_PAGE: usize = 100;

#[derive(Deserialize)]
struct Repo {
    clone_url: String,
    fork: bool,
    #[serde(default)]
    archived: bool,
}

#[derive(Deserialize)]
struct User {
    login: String,
}

#[derive(Deserialize)]
struct Gist {
    id: String,
    /// Missing on anonymous gists.
    owner: Option<User>,
}

pub async fn list(
    api: &Api,
    kind: SourceKind,
    name: &str,
) -> Result<Vec<RemoteRepo>, ProviderError> {
    let url = list_url(api, kind, name).await?;
    if kind == SourceKind::Gists {
        let gists: Vec<Gist> = api.paged(url, "per_page", PER_PAGE).await?;
        return Ok(gists.into_iter().filter_map(gist_repo).collect());
    }
    let repos: Vec<Repo> = api.paged(url, "per_page", PER_PAGE).await?;
    Ok(repos
        .into_iter()
        .map(|repo| RemoteRepo {
            clone_url: repo.clone_url,
            fork: repo.fork,
            archived: repo.archived,
        })
        .collect())
}

/// A gist as its page URL, which names the owner the archive path needs.
fn gist_repo(gist: Gist) -> Option<RemoteRepo> {
    let owner = gist.owner?;
    Some(RemoteRepo {
        clone_url: format!("https://{GIST_HOST}/{}/{}", owner.login, gist.id),
        fork: false,
        archived: false,
    })
}

/// The first page URL of the list of `kind`. The public lists of a user
/// hold public repositories only; for the token's own account the `/user`
/// lists (and `/gists`) hold its private ones too.
async fn list_url(api: &Api, kind: SourceKind, name: &str) -> Result<Url, ProviderError> {
    let own = kind != SourceKind::Org && is_token_owner(api, name).await?;
    let (segments, query) = list_path(kind, own, name);
    let mut url = api.url(&segments)?;
    if let Some((key, value)) = query {
        url.query_pairs_mut().append_pair(key, value);
    }
    Ok(url)
}

/// Path segments and the optional query pair of the list of `kind`;
/// `own` is whether `name` is the token's account.
fn list_path(kind: SourceKind, own: bool, name: &str) -> (Vec<&str>, Option<(&str, &str)>) {
    match (kind, own) {
        (SourceKind::Org, _) => (vec!["orgs", name, "repos"], Some(("type", "all"))),
        (SourceKind::User, true) => (vec!["user", "repos"], Some(("affiliation", "owner"))),
        (SourceKind::User, false) => (vec!["users", name, "repos"], Some(("type", "owner"))),
        (SourceKind::Starred, true) => (vec!["user", "starred"], None),
        (SourceKind::Starred, false) => (vec!["users", name, "starred"], None),
        (SourceKind::Gists, true) => (vec!["gists"], None),
        (SourceKind::Gists, false) => (vec!["users", name, "gists"], None),
    }
}

async fn is_token_owner(api: &Api, name: &str) -> Result<bool, ProviderError> {
    if matches!(api.auth, Auth::None) {
        return Ok(false);
    }
    let user: User = api.json(api.url(&["user"])?).await?;
    Ok(user.login.eq_ignore_ascii_case(name))
}
