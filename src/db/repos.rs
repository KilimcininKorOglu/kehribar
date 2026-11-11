//! Archived repositories and their sync state.

use sqlx::{FromRow, SqlitePool};

use super::like_pattern;

text_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
    #[sqlx(type_name = "TEXT", rename_all = "lowercase")]
    pub enum RepoStatus {
        /// Added, never synced.
        Pending => "pending",
        Syncing => "syncing",
        Ok => "ok",
        Error => "error",
        /// The source repository no longer answers; the archive is kept as is.
        Unreachable => "unreachable",
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Repo {
    pub id: i64,
    pub url: String,
    pub host: String,
    pub owner: String,
    pub name: String,
    pub source_id: Option<i64>,
    pub username: Option<String>,
    pub token_enc: Option<Vec<u8>>,
    pub interval_minutes: Option<i64>,
    pub wiki_enabled: bool,
    pub status: RepoStatus,
    pub last_sync_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub next_sync_at: i64,
    pub last_error: Option<String>,
    pub size_bytes: i64,
    pub default_branch: Option<String>,
    pub has_wiki: bool,
    pub created_at: i64,
    /// Ref patterns left out of this repository's fetches, besides the host's.
    pub ref_exclude: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewRepo<'a> {
    pub url: &'a str,
    pub host: &'a str,
    pub owner: &'a str,
    pub name: &'a str,
    pub source_id: Option<i64>,
    pub username: Option<&'a str>,
    pub token_enc: Option<&'a [u8]>,
    pub interval_minutes: Option<i64>,
    pub wiki_enabled: bool,
}

/// The per-repository sync settings the user can edit.
#[derive(Debug, Clone, Copy)]
pub struct RepoSettings<'a> {
    pub interval_minutes: Option<i64>,
    pub wiki_enabled: bool,
    /// Normalized patterns, see [`crate::ref_filter::normalize`].
    pub ref_exclude: Option<&'a str>,
}

/// The state written when a sync finishes.
#[derive(Debug, Clone)]
pub struct SyncOutcome<'a> {
    pub status: RepoStatus,
    pub finished_at: i64,
    pub next_sync_at: i64,
    pub error: Option<&'a str>,
    pub size_bytes: i64,
    pub default_branch: Option<&'a str>,
    pub has_wiki: bool,
}

/// Builds a `SELECT` of every repository column at compile time.
macro_rules! select_repos {
    ($tail:literal) => {
        concat!(
            "SELECT id, url, host, owner, name, source_id, username, token_enc, \
             interval_minutes, wiki_enabled, status, last_sync_at, last_success_at, \
             next_sync_at, last_error, size_bytes, default_branch, has_wiki, created_at, \
             ref_exclude FROM repos ",
            $tail
        )
    };
}

/// Inserts a repository due for an immediate first sync.
/// Returns `None` when the URL or the host/owner/name path already exists.
pub async fn insert(pool: &SqlitePool, repo: &NewRepo<'_>, now: i64) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar(
        "INSERT INTO repos (url, host, owner, name, source_id, username, token_enc, \
         interval_minutes, wiki_enabled, next_sync_at, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(repo.url)
    .bind(repo.host)
    .bind(repo.owner)
    .bind(repo.name)
    .bind(repo.source_id)
    .bind(repo.username)
    .bind(repo.token_enc)
    .bind(repo.interval_minutes)
    .bind(repo.wiki_enabled)
    .bind(now)
    .bind(now)
    .fetch_optional(pool)
    .await
}

pub async fn get(pool: &SqlitePool, id: i64) -> sqlx::Result<Option<Repo>> {
    sqlx::query_as(select_repos!("WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn find_by_path(
    pool: &SqlitePool,
    host: &str,
    owner: &str,
    name: &str,
) -> sqlx::Result<Option<Repo>> {
    sqlx::query_as(select_repos!("WHERE host = ? AND owner = ? AND name = ?"))
        .bind(host)
        .bind(owner)
        .bind(name)
        .fetch_optional(pool)
        .await
}

/// Lists repositories, optionally filtered by a literal substring of the
/// URL or the owner/name. A blank search lists every repository.
pub async fn list(pool: &SqlitePool, search: Option<&str>) -> sqlx::Result<Vec<Repo>> {
    // The pattern of an empty term is `%%`, which matches every row.
    let pattern = like_pattern(search.map_or("", str::trim));
    sqlx::query_as(select_repos!(
        "WHERE url LIKE ?1 ESCAPE '\\' OR (owner || '/' || name) LIKE ?1 ESCAPE '\\' \
         ORDER BY host, owner, name"
    ))
    .bind(pattern)
    .fetch_all(pool)
    .await
}

/// Repositories whose next sync time has passed and that are not syncing now.
pub async fn due(pool: &SqlitePool, now: i64, limit: i64) -> sqlx::Result<Vec<Repo>> {
    sqlx::query_as(select_repos!(
        "WHERE next_sync_at <= ? AND status != 'syncing' ORDER BY next_sync_at LIMIT ?"
    ))
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Marks the repository as syncing. Returns `false` when another sync holds it,
/// so the same repository never syncs twice at once.
pub async fn claim(pool: &SqlitePool, id: i64, now: i64) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "UPDATE repos SET status = 'syncing', last_sync_at = ? WHERE id = ? AND status != 'syncing'",
    )
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn finish(pool: &SqlitePool, id: i64, outcome: &SyncOutcome<'_>) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE repos SET status = ?1, \
         last_success_at = CASE WHEN ?1 = 'ok' THEN ?2 ELSE last_success_at END, \
         next_sync_at = ?3, last_error = ?4, size_bytes = ?5, \
         default_branch = COALESCE(?6, default_branch), has_wiki = ?7 WHERE id = ?8",
    )
    .bind(outcome.status)
    .bind(outcome.finished_at)
    .bind(outcome.next_sync_at)
    .bind(outcome.error)
    .bind(outcome.size_bytes)
    .bind(outcome.default_branch)
    .bind(outcome.has_wiki)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Makes the repository due now, for the "sync now" button.
pub async fn schedule_now(pool: &SqlitePool, id: i64, now: i64) -> sqlx::Result<bool> {
    let result = sqlx::query("UPDATE repos SET next_sync_at = ? WHERE id = ?")
        .bind(now)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn update_settings(
    pool: &SqlitePool,
    id: i64,
    settings: RepoSettings<'_>,
) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "UPDATE repos SET interval_minutes = ?, wiki_enabled = ?, ref_exclude = ? WHERE id = ?",
    )
    .bind(settings.interval_minutes)
    .bind(settings.wiki_enabled)
    .bind(settings.ref_exclude)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Sets the username. `token_enc` replaces the stored token only when it is
/// `Some`; [`clear_token`] removes it.
pub async fn update_credentials(
    pool: &SqlitePool,
    id: i64,
    username: Option<&str>,
    token_enc: Option<&[u8]>,
) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "UPDATE repos SET username = ?, token_enc = COALESCE(?, token_enc) WHERE id = ?",
    )
    .bind(username)
    .bind(token_enc)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn clear_token(pool: &SqlitePool, id: i64) -> sqlx::Result<bool> {
    let result = sqlx::query("UPDATE repos SET token_enc = NULL WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}

/// Returns repositories left in `syncing` by a crash or restart to `pending`.
pub async fn reset_interrupted(pool: &SqlitePool) -> sqlx::Result<u64> {
    let result = sqlx::query("UPDATE repos SET status = 'pending' WHERE status = 'syncing'")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

pub async fn delete(pool: &SqlitePool, id: i64) -> sqlx::Result<bool> {
    let result = sqlx::query("DELETE FROM repos WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;

    fn new_repo<'a>(owner: &'a str, name: &'a str, url: &'a str) -> NewRepo<'a> {
        NewRepo {
            url,
            host: "github.com",
            owner,
            name,
            source_id: None,
            username: None,
            token_enc: None,
            interval_minutes: None,
            wiki_enabled: true,
        }
    }

    async fn seeded() -> (SqlitePool, i64) {
        let pool = open_in_memory().await.unwrap();
        let id = insert(
            &pool,
            &new_repo("rust-lang", "rust", "https://github.com/rust-lang/rust"),
            100,
        )
        .await
        .unwrap()
        .unwrap();
        (pool, id)
    }

    #[tokio::test]
    async fn new_repository_is_pending_and_due_immediately() {
        let (pool, id) = seeded().await;
        let repo = get(&pool, id).await.unwrap().unwrap();
        assert_eq!(repo.status, RepoStatus::Pending);
        assert_eq!(repo.next_sync_at, 100);
        assert_eq!(due(&pool, 100, 10).await.unwrap().len(), 1);
        assert!(due(&pool, 99, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn duplicate_url_or_path_returns_none() {
        let (pool, _) = seeded().await;
        let same_url = new_repo("other", "repo", "https://github.com/rust-lang/rust");
        assert_eq!(insert(&pool, &same_url, 1).await.unwrap(), None);
        // Same path with different letter case maps to the same archive directory.
        let same_path = new_repo("Rust-Lang", "RUST", "https://github.com/Rust-Lang/RUST");
        assert_eq!(insert(&pool, &same_path, 1).await.unwrap(), None);
        assert!(
            find_by_path(&pool, "github.com", "RUST-LANG", "Rust")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn claim_is_exclusive_until_finish() {
        let (pool, id) = seeded().await;
        assert!(claim(&pool, id, 200).await.unwrap());
        assert!(!claim(&pool, id, 201).await.unwrap());
        assert!(due(&pool, 1000, 10).await.unwrap().is_empty());

        let outcome = SyncOutcome {
            status: RepoStatus::Ok,
            finished_at: 210,
            next_sync_at: 210 + 3600,
            error: None,
            size_bytes: 4096,
            default_branch: Some("main"),
            has_wiki: true,
        };
        finish(&pool, id, &outcome).await.unwrap();
        let repo = get(&pool, id).await.unwrap().unwrap();
        assert_eq!(repo.status, RepoStatus::Ok);
        assert_eq!(repo.last_sync_at, Some(200));
        assert_eq!(repo.last_success_at, Some(210));
        assert_eq!(repo.default_branch.as_deref(), Some("main"));
        assert!(repo.has_wiki);
        assert!(claim(&pool, id, 300).await.unwrap());
    }

    #[tokio::test]
    async fn failed_sync_keeps_last_success_and_default_branch() {
        let (pool, id) = seeded().await;
        let ok = SyncOutcome {
            status: RepoStatus::Ok,
            finished_at: 10,
            next_sync_at: 20,
            error: None,
            size_bytes: 1,
            default_branch: Some("main"),
            has_wiki: false,
        };
        finish(&pool, id, &ok).await.unwrap();
        let failed = SyncOutcome {
            status: RepoStatus::Unreachable,
            finished_at: 30,
            next_sync_at: 40,
            error: Some("repository not found"),
            size_bytes: 1,
            default_branch: None,
            has_wiki: false,
        };
        finish(&pool, id, &failed).await.unwrap();
        let repo = get(&pool, id).await.unwrap().unwrap();
        assert_eq!(repo.status, RepoStatus::Unreachable);
        assert_eq!(repo.last_success_at, Some(10));
        assert_eq!(repo.default_branch.as_deref(), Some("main"));
        assert_eq!(repo.last_error.as_deref(), Some("repository not found"));
    }

    #[tokio::test]
    async fn search_matches_literal_substrings() {
        let (pool, _) = seeded().await;
        insert(
            &pool,
            &new_repo("tokio-rs", "axum", "https://github.com/tokio-rs/axum"),
            1,
        )
        .await
        .unwrap();
        assert_eq!(list(&pool, None).await.unwrap().len(), 2);
        assert_eq!(list(&pool, Some("tokio-rs/ax")).await.unwrap().len(), 1);
        // `_` is a LIKE wildcard; escaped, it matches nothing here.
        assert!(list(&pool, Some("rust_lang")).await.unwrap().is_empty());
        assert_eq!(list(&pool, Some("  ")).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn settings_update_keeps_token_unless_replaced() {
        let pool = open_in_memory().await.unwrap();
        let mut repo = new_repo("o", "n", "https://example.com/o/n");
        repo.token_enc = Some(b"old");
        let id = insert(&pool, &repo, 1).await.unwrap().unwrap();

        let settings = RepoSettings {
            interval_minutes: Some(30),
            wiki_enabled: false,
            ref_exclude: Some("refs/pull/*"),
        };
        assert!(update_settings(&pool, id, settings).await.unwrap());
        let stored = get(&pool, id).await.unwrap().unwrap();
        assert_eq!(stored.token_enc.as_deref(), Some(&b"old"[..]));
        assert_eq!(stored.interval_minutes, Some(30));
        assert!(!stored.wiki_enabled);
        assert_eq!(stored.ref_exclude.as_deref(), Some("refs/pull/*"));

        // A credential update without a token keeps the stored one.
        assert!(
            update_credentials(&pool, id, Some("bot"), None)
                .await
                .unwrap()
        );
        let stored = get(&pool, id).await.unwrap().unwrap();
        assert_eq!(stored.username.as_deref(), Some("bot"));
        assert_eq!(stored.token_enc.as_deref(), Some(&b"old"[..]));
        assert!(
            update_credentials(&pool, id, None, Some(b"new"))
                .await
                .unwrap()
        );
        let stored = get(&pool, id).await.unwrap().unwrap();
        assert_eq!(stored.username, None);
        assert_eq!(stored.token_enc.as_deref(), Some(&b"new"[..]));

        assert!(clear_token(&pool, id).await.unwrap());
        assert!(get(&pool, id).await.unwrap().unwrap().token_enc.is_none());
    }

    #[tokio::test]
    async fn interrupted_syncs_are_reset_and_schedule_now_works() {
        let (pool, id) = seeded().await;
        claim(&pool, id, 5).await.unwrap();
        assert_eq!(reset_interrupted(&pool).await.unwrap(), 1);
        assert_eq!(
            get(&pool, id).await.unwrap().unwrap().status,
            RepoStatus::Pending
        );

        assert!(schedule_now(&pool, id, 7).await.unwrap());
        assert_eq!(get(&pool, id).await.unwrap().unwrap().next_sync_at, 7);
        assert!(delete(&pool, id).await.unwrap());
        assert!(!delete(&pool, id).await.unwrap());
    }
}
