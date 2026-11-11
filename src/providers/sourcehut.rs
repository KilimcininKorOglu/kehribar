//! SourceHut (git.sr.ht): the repositories of a user through the GraphQL
//! API, which pages with a cursor and needs a personal access token.

use serde::Deserialize;
use serde_json::json;

use super::{Api, MAX_PAGES, ProviderError, RemoteRepo};

const QUERY: &str = "query ($username: String!, $cursor: Cursor) { \
    user(username: $username) { canonicalName \
    repositories(cursor: $cursor) { cursor results { name } } } }";

#[derive(Deserialize)]
struct Answer {
    data: Option<Data>,
    #[serde(default)]
    errors: Vec<Error>,
}

#[derive(Deserialize)]
struct Error {
    message: String,
}

#[derive(Deserialize)]
struct Data {
    user: Option<User>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct User {
    /// The username with its `~` prefix.
    canonical_name: String,
    repositories: Page,
}

#[derive(Deserialize)]
struct Page {
    results: Vec<Repo>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
struct Repo {
    name: String,
}

/// Lists the repositories of the user `name`, with or without its `~`.
pub async fn list(api: &Api, name: &str) -> Result<Vec<RemoteRepo>, ProviderError> {
    let username = name.trim_start_matches('~');
    let origin = api.base.origin().ascii_serialization();
    let mut repos = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let body = json!({
            "query": QUERY,
            "variables": { "username": username, "cursor": cursor },
        });
        let user = user(api.post_json(&body).await?, username)?;
        repos.extend(
            user.repositories
                .results
                .into_iter()
                .map(|repo| RemoteRepo {
                    clone_url: format!("{origin}/{}/{}", user.canonical_name, repo.name),
                    // SourceHut has neither forks nor an archived state.
                    fork: false,
                    archived: false,
                }),
        );
        cursor = user.repositories.cursor;
        if cursor.is_none() {
            return Ok(repos);
        }
    }
    Err(ProviderError::TooManyPages)
}

/// The user of an answer, or the errors the API sent instead.
fn user(answer: Answer, username: &str) -> Result<User, ProviderError> {
    if !answer.errors.is_empty() {
        let messages: Vec<String> = answer.errors.into_iter().map(|e| e.message).collect();
        return Err(ProviderError::Answer(messages.join("; ")));
    }
    answer
        .data
        .and_then(|data| data.user)
        .ok_or_else(|| ProviderError::Answer(format!("no user named {username}")))
}
