//! GitLab: `GET /groups/{path}/projects` with subgroups,
//! `GET /users/{name}/projects` and `GET /users/{name}/starred_projects`.

use serde::Deserialize;
use serde::de::IgnoredAny;

use super::{Api, ProviderError, RemoteRepo};
use crate::db::hosts::Provider;
use crate::db::sources::SourceKind;

const PER_PAGE: usize = 100;

#[derive(Deserialize)]
struct Project {
    http_url_to_repo: String,
    ssh_url_to_repo: Option<String>,
    /// Present only on forks.
    forked_from_project: Option<IgnoredAny>,
    #[serde(default)]
    archived: bool,
}

pub async fn list(
    api: &Api,
    kind: SourceKind,
    name: &str,
) -> Result<Vec<RemoteRepo>, ProviderError> {
    let mut url = match kind {
        SourceKind::Org => api.url(&["groups", name, "projects"])?,
        SourceKind::User => api.url(&["users", name, "projects"])?,
        SourceKind::Starred => api.url(&["users", name, "starred_projects"])?,
        SourceKind::Gists => return Err(ProviderError::kind(Provider::Gitlab, kind)),
    };
    if kind == SourceKind::Org {
        url.query_pairs_mut()
            .append_pair("include_subgroups", "true");
    }
    let projects: Vec<Project> = api.paged(url, "per_page", PER_PAGE).await?;
    Ok(projects
        .into_iter()
        .map(|project| RemoteRepo {
            clone_url: project.http_url_to_repo,
            ssh_url: project.ssh_url_to_repo,
            fork: project.forked_from_project.is_some(),
            archived: project.archived,
        })
        .collect())
}
