//! Forgejo and Gitea: `GET /users/{name}/repos`, `GET /orgs/{name}/repos`
//! and `GET /users/{name}/starred`.

use serde::Deserialize;

use super::{Api, ProviderError, RemoteRepo};
use crate::db::hosts::Provider;
use crate::db::sources::SourceKind;

/// The default maximum page size of Gitea and Forgejo. A server set to a
/// lower `MAX_RESPONSE_ITEMS` answers with smaller pages, so the list is
/// read until an empty page.
const LIMIT: usize = 50;

#[derive(Deserialize)]
struct Repo {
    clone_url: String,
    ssh_url: Option<String>,
    fork: bool,
    #[serde(default)]
    archived: bool,
}

pub async fn list(
    api: &Api,
    kind: SourceKind,
    name: &str,
) -> Result<Vec<RemoteRepo>, ProviderError> {
    let url = match kind {
        SourceKind::Org => api.url(&["orgs", name, "repos"])?,
        SourceKind::User => api.url(&["users", name, "repos"])?,
        SourceKind::Starred => api.url(&["users", name, "starred"])?,
        SourceKind::Gists => return Err(ProviderError::kind(Provider::Forgejo, kind)),
    };
    let repos: Vec<Repo> = api.pages(url, "limit", LIMIT, |len| len == 0).await?;
    Ok(repos
        .into_iter()
        .map(|repo| RemoteRepo {
            clone_url: repo.clone_url,
            ssh_url: repo.ssh_url,
            fork: repo.fork,
            archived: repo.archived,
        })
        .collect())
}
