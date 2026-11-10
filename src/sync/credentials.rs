//! Chooses the credentials a sync sends to the source server.

use crate::crypto::Cipher;
use crate::db::hosts::Host;
use crate::db::repos::Repo;
use crate::git::Credentials;

/// Username sent with a token when none is configured. GitHub, GitLab,
/// Forgejo and Gitea accept any username next to a personal access token.
pub const DEFAULT_TOKEN_USERNAME: &str = "git";

/// Where the token of a repository comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// The repository's own token, which overrides the host token.
    Repo,
    /// The default token of the repository's host.
    Host,
    /// No token: the repository is fetched anonymously.
    Anonymous,
}

pub fn source(repo: &Repo, host: Option<&Host>) -> CredentialSource {
    if repo.token_enc.is_some() {
        CredentialSource::Repo
    } else if host.is_some_and(|h| h.token_enc.is_some()) {
        CredentialSource::Host
    } else {
        CredentialSource::Anonymous
    }
}

/// The repository's own token when it has one, else the host's default
/// token. The username comes from the same place as the token.
pub fn resolve(
    cipher: &Cipher,
    repo: &Repo,
    host: Option<&Host>,
) -> Result<Option<Credentials>, String> {
    let (token, username) = match (source(repo, host), host) {
        (CredentialSource::Repo, _) => (&repo.token_enc, &repo.username),
        (CredentialSource::Host, Some(host)) => (&host.token_enc, &host.username),
        _ => return Ok(None),
    };
    let Some(token) = token.as_deref() else {
        return Ok(None);
    };
    let password = cipher
        .decrypt(token)
        .map_err(|err| format!("cannot decrypt the stored token: {err}"))?;
    Ok(Some(Credentials {
        username: username
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| DEFAULT_TOKEN_USERNAME.to_owned()),
        password,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::hosts::Provider;
    use crate::db::repos::RepoStatus;

    fn cipher() -> Cipher {
        Cipher::new(&[3; 32])
    }

    fn repo(token: Option<&str>, username: Option<&str>) -> Repo {
        Repo {
            id: 1,
            url: "https://example.com/o/n".into(),
            host: "example.com".into(),
            owner: "o".into(),
            name: "n".into(),
            source_id: None,
            username: username.map(Into::into),
            token_enc: token.map(|t| cipher().encrypt(t).unwrap()),
            interval_minutes: None,
            wiki_enabled: true,
            status: RepoStatus::Pending,
            last_sync_at: None,
            last_success_at: None,
            next_sync_at: 0,
            last_error: None,
            size_bytes: 0,
            default_branch: None,
            has_wiki: false,
            created_at: 0,
        }
    }

    fn host(token: Option<&str>, username: Option<&str>) -> Host {
        Host {
            id: 1,
            host: "example.com".into(),
            provider: Provider::Generic,
            api_base_url: None,
            username: username.map(Into::into),
            token_enc: token.map(|t| cipher().encrypt(t).unwrap()),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn pair(creds: Option<Credentials>) -> Option<(String, String)> {
        creds.map(|c| (c.username, c.password))
    }

    #[test]
    fn repository_token_overrides_the_host_token() {
        let host = host(Some("host-token"), Some("host-user"));
        let own = repo(Some("repo-token"), None);
        assert_eq!(source(&own, Some(&host)), CredentialSource::Repo);
        assert_eq!(
            pair(resolve(&cipher(), &own, Some(&host)).unwrap()),
            Some(("git".into(), "repo-token".into()))
        );
    }

    #[test]
    fn host_token_is_the_default() {
        let host = host(Some("host-token"), Some("host-user"));
        // The repository username is ignored: it belongs to its own token.
        let plain = repo(None, Some("repo-user"));
        assert_eq!(source(&plain, Some(&host)), CredentialSource::Host);
        assert_eq!(
            pair(resolve(&cipher(), &plain, Some(&host)).unwrap()),
            Some(("host-user".into(), "host-token".into()))
        );
    }

    #[test]
    fn without_tokens_the_fetch_is_anonymous() {
        let plain = repo(None, None);
        assert_eq!(source(&plain, None), CredentialSource::Anonymous);
        let empty_host = host(None, Some("user"));
        assert_eq!(
            source(&plain, Some(&empty_host)),
            CredentialSource::Anonymous
        );
        assert!(
            resolve(&cipher(), &plain, Some(&empty_host))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_token_from_another_key_is_an_error() {
        let mut own = repo(None, None);
        own.token_enc = Some(Cipher::new(&[9; 32]).encrypt("x").unwrap());
        let err = resolve(&cipher(), &own, None).unwrap_err();
        assert!(err.starts_with("cannot decrypt"), "{err}");
    }
}
