//! Gitee: `GET /users/{name}/repos`, `GET /orgs/{name}/repos` and
//! `GET /users/{name}/starred` of API v5, with the token in the
//! `access_token` query parameter.

use serde::Deserialize;
use url::Url;

use super::{Api, Auth, ProviderError, RemoteRepo};
use crate::db::hosts::Provider;
use crate::db::sources::SourceKind;

/// The largest page size of Gitee API v5.
const PER_PAGE: usize = 100;

#[derive(Deserialize)]
struct Repo {
    /// The HTTPS clone URL, `https://gitee.com/<owner>/<name>.git`.
    html_url: String,
    fork: bool,
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
            clone_url: repo.html_url,
            fork: repo.fork,
            // Gitee API v5 has no archived state.
            archived: false,
        })
        .collect())
}

/// The first page URL of the list of `kind`. For the token's own account
/// the `/user` lists hold its private repositories too.
async fn list_url(api: &Api, kind: SourceKind, name: &str) -> Result<Url, ProviderError> {
    if kind == SourceKind::Gists {
        return Err(ProviderError::kind(Provider::Gitee, kind));
    }
    let own = kind != SourceKind::Org && is_token_owner(api, name).await?;
    let (segments, owner_only) = match (kind, own) {
        (SourceKind::User, true) => (vec!["user", "repos"], true),
        (SourceKind::User, false) => (vec!["users", name, "repos"], true),
        (SourceKind::Starred, true) => (vec!["user", "starred"], false),
        (SourceKind::Starred, false) => (vec!["users", name, "starred"], false),
        _ => (vec!["orgs", name, "repos"], false),
    };
    let mut url = api.url(&segments)?;
    if owner_only {
        // The default `all` adds the repositories the user is a member of.
        url.query_pairs_mut().append_pair("type", "owner");
    }
    Ok(url)
}

async fn is_token_owner(api: &Api, name: &str) -> Result<bool, ProviderError> {
    if matches!(api.auth, Auth::None) {
        return Ok(false);
    }
    let user: User = api.json(api.url(&["user"])?).await?;
    Ok(user.login.eq_ignore_ascii_case(name))
}
