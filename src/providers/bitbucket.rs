//! Bitbucket Cloud: `GET /repositories/{workspace}`, which pages through a
//! `next` link. Users and teams are both workspaces.

use serde::Deserialize;
use serde::de::IgnoredAny;
use url::Url;

use super::{Api, MAX_PAGES, ProviderError, RemoteRepo};

const PAGE_LEN: &str = "100";

#[derive(Deserialize)]
struct Page {
    values: Vec<Repo>,
    next: Option<String>,
}

#[derive(Deserialize)]
struct Repo {
    links: Links,
    /// Present only on forks.
    parent: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct Links {
    clone: Vec<CloneLink>,
}

#[derive(Deserialize)]
struct CloneLink {
    name: String,
    href: String,
}

pub async fn list(api: &Api, workspace: &str) -> Result<Vec<RemoteRepo>, ProviderError> {
    let mut url = api.url(&["repositories", workspace])?;
    url.query_pairs_mut().append_pair("pagelen", PAGE_LEN);
    let mut repos = Vec::new();
    for _ in 0..MAX_PAGES {
        let page: Page = api.json(url).await?;
        repos.extend(page.values.into_iter().filter_map(remote));
        let Some(next) = page.next else {
            return Ok(repos);
        };
        url = same_origin(&api.base, &next)?;
    }
    Err(ProviderError::TooManyPages)
}

/// The HTTPS clone link without the account name Bitbucket puts in it.
fn remote(repo: Repo) -> Option<RemoteRepo> {
    let link = repo.links.clone.into_iter().find(|l| l.name == "https")?;
    let mut url = Url::parse(&link.href).ok()?;
    url.set_username("").ok()?;
    Some(RemoteRepo {
        clone_url: url.to_string(),
        fork: repo.parent.is_some(),
    })
}

/// The credentials go only to the API server, so a next link must stay on it.
fn same_origin(base: &Url, next: &str) -> Result<Url, ProviderError> {
    let url = Url::parse(next).map_err(|_| ProviderError::ForeignPage(next.to_owned()))?;
    if url.origin() == base.origin() {
        Ok(url)
    } else {
        Err(ProviderError::ForeignPage(
            url.origin().ascii_serialization(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_links_lose_the_account_name() {
        let repo = Repo {
            links: Links {
                clone: vec![
                    CloneLink {
                        name: "ssh".into(),
                        href: "git@bitbucket.org:team/app.git".into(),
                    },
                    CloneLink {
                        name: "https".into(),
                        href: "https://alice@bitbucket.org/team/app.git".into(),
                    },
                ],
            },
            parent: None,
        };
        assert_eq!(
            remote(repo),
            Some(RemoteRepo {
                clone_url: "https://bitbucket.org/team/app.git".into(),
                fork: false,
            })
        );
    }

    #[test]
    fn next_links_must_stay_on_the_api_server() {
        let base = Url::parse("https://api.bitbucket.org/2.0").unwrap();
        assert!(same_origin(&base, "https://api.bitbucket.org/2.0/repositories/t?page=2").is_ok());
        assert!(matches!(
            same_origin(&base, "https://evil.example/2.0/repositories/t?page=2"),
            Err(ProviderError::ForeignPage(_))
        ));
        assert!(same_origin(&base, "http://api.bitbucket.org/2.0/x").is_err());
    }
}
