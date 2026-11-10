//! Chooses the credentials a sync sends to the source server.

use crate::crypto::Cipher;
use crate::db::repos::Repo;
use crate::git::Credentials;

/// Username sent with a token when none is configured. GitHub, GitLab,
/// Forgejo and Gitea accept any username next to a personal access token.
pub const DEFAULT_TOKEN_USERNAME: &str = "git";

/// The repository's own token, when it has one.
pub fn for_repo(cipher: &Cipher, repo: &Repo) -> Result<Option<Credentials>, String> {
    let Some(stored) = repo.token_enc.as_deref() else {
        return Ok(None);
    };
    let password = cipher.decrypt(stored).map_err(|err| err.to_string())?;
    Ok(Some(Credentials {
        username: repo
            .username
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| DEFAULT_TOKEN_USERNAME.to_owned()),
        password,
    }))
}
