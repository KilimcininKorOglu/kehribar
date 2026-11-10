//! Git servers with their provider type and default credentials.

use sqlx::{FromRow, SqlitePool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
pub enum Provider {
    Github,
    Gitlab,
    Forgejo,
    Bitbucket,
    Generic,
}

impl Provider {
    pub const ALL: [Self; 5] = [
        Self::Github,
        Self::Gitlab,
        Self::Forgejo,
        Self::Bitbucket,
        Self::Generic,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
            Self::Forgejo => "forgejo",
            Self::Bitbucket => "bitbucket",
            Self::Generic => "generic",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == value)
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Host {
    pub id: i64,
    pub host: String,
    pub provider: Provider,
    pub api_base_url: Option<String>,
    pub username: Option<String>,
    pub token_enc: Option<Vec<u8>>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Values written when a host is created or edited.
#[derive(Debug, Clone)]
pub struct HostInput<'a> {
    pub host: &'a str,
    pub provider: Provider,
    pub api_base_url: Option<&'a str>,
    pub username: Option<&'a str>,
    pub token_enc: Option<&'a [u8]>,
}

/// Builds a `SELECT` of every host column at compile time.
macro_rules! select_hosts {
    ($tail:literal) => {
        concat!(
            "SELECT id, host, provider, api_base_url, username, token_enc, created_at, updated_at \
             FROM hosts ",
            $tail
        )
    };
}

pub async fn insert(pool: &SqlitePool, input: &HostInput<'_>, now: i64) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "INSERT INTO hosts (host, provider, api_base_url, username, token_enc, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(input.host)
    .bind(input.provider)
    .bind(input.api_base_url)
    .bind(input.username)
    .bind(input.token_enc)
    .bind(now)
    .bind(now)
    .fetch_one(pool)
    .await
}

/// Updates a host. `token_enc` replaces the stored token only when it is `Some`.
pub async fn update(
    pool: &SqlitePool,
    id: i64,
    input: &HostInput<'_>,
    now: i64,
) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "UPDATE hosts SET host = ?, provider = ?, api_base_url = ?, username = ?, \
         token_enc = COALESCE(?, token_enc), updated_at = ? WHERE id = ?",
    )
    .bind(input.host)
    .bind(input.provider)
    .bind(input.api_base_url)
    .bind(input.username)
    .bind(input.token_enc)
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn clear_token(pool: &SqlitePool, id: i64) -> sqlx::Result<bool> {
    let result = sqlx::query("UPDATE hosts SET token_enc = NULL WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn get(pool: &SqlitePool, id: i64) -> sqlx::Result<Option<Host>> {
    sqlx::query_as(select_hosts!("WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn find_by_host(pool: &SqlitePool, host: &str) -> sqlx::Result<Option<Host>> {
    sqlx::query_as(select_hosts!("WHERE host = ?"))
        .bind(host)
        .fetch_optional(pool)
        .await
}

pub async fn list(pool: &SqlitePool) -> sqlx::Result<Vec<Host>> {
    sqlx::query_as(select_hosts!("ORDER BY host"))
        .fetch_all(pool)
        .await
}

pub async fn delete(pool: &SqlitePool, id: i64) -> sqlx::Result<bool> {
    let result = sqlx::query("DELETE FROM hosts WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;

    fn input<'a>(host: &'a str, token: Option<&'a [u8]>) -> HostInput<'a> {
        HostInput {
            host,
            provider: Provider::Github,
            api_base_url: None,
            username: Some("x-access-token"),
            token_enc: token,
        }
    }

    #[tokio::test]
    async fn crud_round_trip() {
        let pool = open_in_memory().await.unwrap();
        let id = insert(&pool, &input("github.com", Some(b"enc")), 10)
            .await
            .unwrap();

        let host = get(&pool, id).await.unwrap().unwrap();
        assert_eq!(host.provider, Provider::Github);
        assert_eq!(host.token_enc.as_deref(), Some(&b"enc"[..]));

        // Lookup ignores case, because host names are case-insensitive.
        assert!(find_by_host(&pool, "GitHub.com").await.unwrap().is_some());

        // An update without a token keeps the stored one.
        let mut edit = input("github.com", None);
        edit.provider = Provider::Generic;
        assert!(update(&pool, id, &edit, 20).await.unwrap());
        let host = get(&pool, id).await.unwrap().unwrap();
        assert_eq!(host.provider, Provider::Generic);
        assert_eq!(host.token_enc.as_deref(), Some(&b"enc"[..]));
        assert_eq!(host.updated_at, 20);

        assert!(clear_token(&pool, id).await.unwrap());
        assert!(get(&pool, id).await.unwrap().unwrap().token_enc.is_none());

        assert_eq!(list(&pool).await.unwrap().len(), 1);
        assert!(delete(&pool, id).await.unwrap());
        assert!(get(&pool, id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn duplicate_host_is_rejected() {
        let pool = open_in_memory().await.unwrap();
        insert(&pool, &input("gitlab.com", None), 1).await.unwrap();
        assert!(insert(&pool, &input("GITLAB.com", None), 1).await.is_err());
    }

    #[test]
    fn provider_names_round_trip() {
        for provider in Provider::ALL {
            assert_eq!(Provider::parse(provider.as_str()), Some(provider));
        }
        assert_eq!(Provider::parse("svn"), None);
    }
}
