//! Forgejo and Gitea: `GET /users/{name}/repos` and `GET /orgs/{name}/repos`.

use serde::Deserialize;

use super::{Api, ProviderError, RemoteRepo};
use crate::db::sources::SourceKind;

/// The default maximum page size of Gitea and Forgejo.
const LIMIT: usize = 50;

#[derive(Deserialize)]
struct Repo {
    clone_url: String,
    fork: bool,
}

pub async fn list(
    api: &Api,
    kind: SourceKind,
    name: &str,
) -> Result<Vec<RemoteRepo>, ProviderError> {
    let owner = match kind {
        SourceKind::Org => "orgs",
        SourceKind::User => "users",
    };
    let url = api.url(&[owner, name, "repos"])?;
    let repos: Vec<Repo> = api.paged(url, "limit", LIMIT).await?;
    Ok(repos
        .into_iter()
        .map(|repo| RemoteRepo {
            clone_url: repo.clone_url,
            fork: repo.fork,
        })
        .collect())
}
