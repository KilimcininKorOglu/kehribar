//! GitHub and GitHub Enterprise Server: `GET /users/{name}/repos` and
//! `GET /orgs/{name}/repos`.

use serde::Deserialize;

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
    let (mut url, (key, value)) = match kind {
        SourceKind::Org => (api.url(&["orgs", name, "repos"])?, ("type", "all")),
        // `/users/{name}/repos` lists public repositories only; the token's
        // own account lists its private ones through `/user/repos`.
        SourceKind::User if is_token_owner(api, name).await? => {
            (api.url(&["user", "repos"])?, ("affiliation", "owner"))
        }
        SourceKind::User => (api.url(&["users", name, "repos"])?, ("type", "owner")),
    };
    url.query_pairs_mut().append_pair(key, value);
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

async fn is_token_owner(api: &Api, name: &str) -> Result<bool, ProviderError> {
    if matches!(api.auth, Auth::None) {
        return Ok(false);
    }
    let user: User = api.json(api.url(&["user"])?).await?;
    Ok(user.login.eq_ignore_ascii_case(name))
}
