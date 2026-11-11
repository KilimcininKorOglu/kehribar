//! Writes the ref history of one fetch: what changed, and which branches
//! and tags the source deleted or brought back.

use std::collections::BTreeMap;
use std::path::Path;

use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::db::ref_events::{self, EventKind, NewEvent};
use crate::git::Git;
use crate::git::mirror::{self, ChangeKind, RescuedRef, SyncReport};

/// One fetch of a repository, as the history records it.
pub struct Fetch<'a> {
    pub repo_id: i64,
    pub run_id: i64,
    pub now: i64,
    pub report: &'a SyncReport,
}

/// Records the history of `fetch` in one transaction. The first record of
/// a repository writes a `baseline` event for each branch and tag, after a
/// `rewritten` event for each commit the archive kept before the history
/// began.
pub async fn record(
    pool: &SqlitePool,
    git: &Git,
    dir: &Path,
    exclude: &[String],
    fetch: &Fetch<'_>,
) -> Result<(), String> {
    let first = !ref_events::has_any(pool, fetch.repo_id)
        .await
        .map_err(|err| err.to_string())?;
    let rescued = if first {
        mirror::rescued_refs(git, dir, exclude)
            .await
            .map_err(|err| err.message)?
    } else {
        Vec::new()
    };
    write(pool, fetch, first, &rescued)
        .await
        .map_err(|err| err.to_string())
}

async fn write(
    pool: &SqlitePool,
    fetch: &Fetch<'_>,
    first: bool,
    rescued: &[RescuedRef],
) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    if first {
        write_start(&mut tx, fetch, rescued).await?;
    } else {
        write_changes(&mut tx, fetch).await?;
    }
    write_source_state(&mut tx, fetch).await?;
    tx.commit().await
}

/// The kept commits of the time before the history, then the current state.
async fn write_start(
    tx: &mut Transaction<'_, Sqlite>,
    fetch: &Fetch<'_>,
    rescued: &[RescuedRef],
) -> sqlx::Result<()> {
    for kept in rescued {
        let event = NewEvent {
            name: &kept.name,
            old: Some(&kept.id),
            new: None,
            kind: EventKind::Rewritten,
            at: kept.at,
        };
        // The fetch that rewrote it is not this one.
        ref_events::insert(&mut **tx, fetch.repo_id, None, &event).await?;
    }
    for (name, id) in &fetch.report.archive_refs {
        let event = NewEvent {
            name,
            old: None,
            new: Some(id),
            kind: EventKind::Baseline,
            at: fetch.now,
        };
        ref_events::insert(&mut **tx, fetch.repo_id, Some(fetch.run_id), &event).await?;
    }
    Ok(())
}

async fn write_changes(tx: &mut Transaction<'_, Sqlite>, fetch: &Fetch<'_>) -> sqlx::Result<()> {
    for change in &fetch.report.changes {
        let event = NewEvent {
            name: &change.name,
            old: change.old.as_deref(),
            new: Some(&change.new),
            kind: match change.kind {
                ChangeKind::Created => EventKind::Created,
                ChangeKind::Moved => EventKind::Moved,
                ChangeKind::Rewritten => EventKind::Rewritten,
            },
            at: fetch.now,
        };
        ref_events::insert(&mut **tx, fetch.repo_id, Some(fetch.run_id), &event).await?;
    }
    Ok(())
}

/// Marks the branches and tags the source deleted, and clears the ones it
/// has again. A mark whose ref is no longer tracked (an exclude pattern
/// matches it now) is dropped without an event.
async fn write_source_state(
    tx: &mut Transaction<'_, Sqlite>,
    fetch: &Fetch<'_>,
) -> sqlx::Result<()> {
    let marked = ref_events::missing(&mut **tx, fetch.repo_id).await?;
    mark_deleted(tx, fetch, &marked).await?;
    clear_returned(tx, fetch, &marked).await
}

/// Marks the refs the source lost since the last fetch.
async fn mark_deleted(
    tx: &mut Transaction<'_, Sqlite>,
    fetch: &Fetch<'_>,
    marked: &BTreeMap<String, i64>,
) -> sqlx::Result<()> {
    for (name, id) in fetch.report.missing_at_source() {
        if marked.contains_key(name) {
            continue;
        }
        ref_events::add_missing(&mut **tx, fetch.repo_id, name, fetch.now).await?;
        let event = source_event(name, id, EventKind::DeletedAtSource, fetch.now);
        ref_events::insert(&mut **tx, fetch.repo_id, Some(fetch.run_id), &event).await?;
    }
    Ok(())
}

/// Clears the marks of refs the source has again, and of refs no longer
/// tracked.
async fn clear_returned(
    tx: &mut Transaction<'_, Sqlite>,
    fetch: &Fetch<'_>,
    marked: &BTreeMap<String, i64>,
) -> sqlx::Result<()> {
    let report = fetch.report;
    for name in marked.keys() {
        if let Some(id) = report.source_refs.get(name) {
            let event = source_event(name, id, EventKind::BackAtSource, fetch.now);
            ref_events::insert(&mut **tx, fetch.repo_id, Some(fetch.run_id), &event).await?;
        } else if report.archive_refs.contains_key(name) {
            continue;
        }
        ref_events::remove_missing(&mut **tx, fetch.repo_id, name).await?;
    }
    Ok(())
}

fn source_event<'a>(name: &'a str, id: &'a str, kind: EventKind, at: i64) -> NewEvent<'a> {
    NewEvent {
        name,
        old: None,
        new: Some(id),
        kind,
        at,
    }
}
