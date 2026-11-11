//! GitHub and GitHub Enterprise Server: `GET /users/{name}/repos`,
//! `GET /orgs/{name}/repos` and `GET /users/{name}/starred`.

use serde::Deserialize;
use url::Url;

use super::{Api, Auth, ProviderError, RemoteRepo};
use crate::db::sources::SourceKind;

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

pub async fn list(
    api: &Api,
    kind: SourceKind,
    name: &str,
) -> Result<Vec<RemoteRepo>, ProviderError> {
    let url = list_url(api, kind, name).await?;
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

/// The first page URL of the list of `kind`. The public lists of a user
/// hold public repositories only; for the token's own account the `/user`
/// lists hold its private ones too.
async fn list_url(api: &Api, kind: SourceKind, name: &str) -> Result<Url, ProviderError> {
    let own = kind != SourceKind::Org && is_token_owner(api, name).await?;
    let (mut url, (key, value)) = match (kind, own) {
        (SourceKind::Org, _) => (api.url(&["orgs", name, "repos"])?, ("type", "all")),
        (SourceKind::User, true) => (api.url(&["user", "repos"])?, ("affiliation", "owner")),
        (SourceKind::User, false) => (api.url(&["users", name, "repos"])?, ("type", "owner")),
        (SourceKind::Starred, true) => (api.url(&["user", "starred"])?, ("sort", "created")),
        (SourceKind::Starred, false) => {
            (api.url(&["users", name, "starred"])?, ("sort", "created"))
        }
    };
    url.query_pairs_mut().append_pair(key, value);
    Ok(url)
}

async fn is_token_owner(api: &Api, name: &str) -> Result<bool, ProviderError> {
    if matches!(api.auth, Auth::None) {
        return Ok(false);
    }
    let user: User = api.json(api.url(&["user"])?).await?;
    Ok(user.login.eq_ignore_ascii_case(name))
}
