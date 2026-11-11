//! Access tokens: clone tokens for /git/ and API tokens for /api/v1. Only
//! the SHA-256 of a token is stored; the token itself is shown once.

use sqlx::{FromRow, SqlitePool};

text_enum! {
    /// What a token opens.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
    #[sqlx(type_name = "TEXT", rename_all = "snake_case")]
    pub enum TokenKind {
        Clone => "clone",
        Api => "api",
    }
}

/// `last_used_at` is written at most once per this many seconds.
const TOUCH_SECONDS: i64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct AccessToken {
    pub id: i64,
    pub kind: TokenKind,
    pub name: String,
    /// Clone tokens: every repository, else the linked ones.
    pub all_repos: bool,
    /// API tokens: changes allowed, else reading only.
    pub can_write: bool,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub last_used_at: Option<i64>,
}

impl AccessToken {
    pub fn is_expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|at| at <= now)
    }
}

/// A token to store.
#[derive(Debug, Clone, Copy)]
pub struct NewToken<'a> {
    pub kind: TokenKind,
    pub name: &'a str,
    pub hash: &'a [u8],
    pub all_repos: bool,
    pub can_write: bool,
    /// The repositories of a clone token without `all_repos`.
    pub repo_ids: &'a [i64],
    pub expires_at: Option<i64>,
}

/// Builds a `SELECT` of every token column except the hash at compile time.
macro_rules! select_tokens {
    ($tail:literal) => {
        concat!(
            "SELECT id, kind, name, all_repos, can_write, created_at, expires_at, last_used_at \
             FROM access_tokens ",
            $tail
        )
    };
}

/// Stores the token and its repositories in one transaction.
pub async fn create(pool: &SqlitePool, token: &NewToken<'_>, now: i64) -> sqlx::Result<i64> {
    let mut tx = pool.begin().await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO access_tokens (kind, name, token_hash, all_repos, can_write, created_at, \
         expires_at) VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(token.kind)
    .bind(token.name)
    .bind(token.hash)
    .bind(token.all_repos)
    .bind(token.can_write)
    .bind(now)
    .bind(token.expires_at)
    .fetch_one(&mut *tx)
    .await?;
    for repo_id in token.repo_ids {
        sqlx::query("INSERT OR IGNORE INTO access_token_repos (token_id, repo_id) VALUES (?, ?)")
            .bind(id)
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(id)
}

/// Every token, clone tokens first, newest first within each kind.
pub async fn list(pool: &SqlitePool) -> sqlx::Result<Vec<AccessToken>> {
    sqlx::query_as(select_tokens!(
        "ORDER BY kind DESC, created_at DESC, id DESC"
    ))
    .fetch_all(pool)
    .await
}

/// `(token id, host/owner/name)` of every repository linked to a token.
pub async fn repo_names(pool: &SqlitePool) -> sqlx::Result<Vec<(i64, String)>> {
    sqlx::query_as(
        "SELECT t.token_id, r.host || '/' || r.owner || '/' || r.name FROM access_token_repos t \
         JOIN repos r ON r.id = t.repo_id ORDER BY t.token_id, r.host, r.owner, r.name",
    )
    .fetch_all(pool)
    .await
}

/// Deletes a token; `false` when there was none.
pub async fn revoke(pool: &SqlitePool, id: i64) -> sqlx::Result<bool> {
    let result = sqlx::query("DELETE FROM access_tokens WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}

/// The token of `kind` with this hash, unless it expired.
pub async fn find_valid(
    pool: &SqlitePool,
    hash: &[u8],
    kind: TokenKind,
    now: i64,
) -> sqlx::Result<Option<AccessToken>> {
    sqlx::query_as(select_tokens!(
        "WHERE token_hash = ? AND kind = ? AND (expires_at IS NULL OR expires_at > ?)"
    ))
    .bind(hash)
    .bind(kind)
    .bind(now)
    .fetch_optional(pool)
    .await
}

/// Whether the clone token may fetch the repository.
pub async fn allows_repo(
    pool: &SqlitePool,
    token: &AccessToken,
    repo_id: i64,
) -> sqlx::Result<bool> {
    if token.all_repos {
        return Ok(true);
    }
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM access_token_repos WHERE token_id = ? AND repo_id = ?)",
    )
    .bind(token.id)
    .bind(repo_id)
    .fetch_one(pool)
    .await
}

/// Records a use of the token, at most once a minute.
pub async fn touch_used(pool: &SqlitePool, id: i64, now: i64) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE access_tokens SET last_used_at = ?1 WHERE id = ?2 \
         AND (last_used_at IS NULL OR last_used_at <= ?1 - ?3)",
    )
    .bind(now)
    .bind(id)
    .bind(TOUCH_SECONDS)
    .execute(pool)
    .await?;
    Ok(())
}
