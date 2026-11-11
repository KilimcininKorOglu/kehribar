//! Azure DevOps Services and Server: `GET /{organization}[/{project}]/
//! _apis/git/repositories`, which answers every repository of the
//! organization or the project at once.

use serde::Deserialize;
use url::Url;

use super::{Api, ProviderError, RemoteRepo};

const API_VERSION: &str = "7.1";

#[derive(Deserialize)]
struct List {
    value: Vec<Repo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Repo {
    remote_url: String,
    ssh_url: Option<String>,
    #[serde(default)]
    is_fork: bool,
    /// A disabled repository cannot be fetched.
    #[serde(default)]
    is_disabled: bool,
}

/// Lists the repositories of `name`, an organization or
/// `organization/project`.
pub async fn list(api: &Api, name: &str) -> Result<Vec<RemoteRepo>, ProviderError> {
    let mut segments: Vec<&str> = name.split('/').collect();
    segments.extend(["_apis", "git", "repositories"]);
    let mut url = api.url(&segments)?;
    url.query_pairs_mut()
        .append_pair("api-version", API_VERSION);
    let list: List = api.json(url).await?;
    Ok(list
        .value
        .into_iter()
        .filter(|repo| !repo.is_disabled)
        .filter_map(remote)
        .collect())
}

/// The clone URL without the organization name Azure DevOps puts in it as
/// the username.
fn remote(repo: Repo) -> Option<RemoteRepo> {
    let mut url = Url::parse(&repo.remote_url).ok()?;
    url.set_username("").ok()?;
    Some(RemoteRepo {
        clone_url: url.to_string(),
        ssh_url: repo.ssh_url,
        fork: repo.is_fork,
        // Azure DevOps has no archived state; disabled ones are left out.
        archived: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_urls_lose_the_organization_username() {
        let repo = Repo {
            remote_url: "https://fabrikam@dev.azure.com/fabrikam/Fiber/_git/App".into(),
            ssh_url: Some("git@ssh.dev.azure.com:v3/fabrikam/Fiber/App".into()),
            is_fork: true,
            is_disabled: false,
        };
        assert_eq!(
            remote(repo),
            Some(RemoteRepo {
                clone_url: "https://dev.azure.com/fabrikam/Fiber/_git/App".into(),
                ssh_url: Some("git@ssh.dev.azure.com:v3/fabrikam/Fiber/App".into()),
                fork: true,
                archived: false,
            })
        );
    }
}
