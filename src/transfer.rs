//! Export and import of the hosts, sources and repositories as JSON.
//!
//! Secrets travel as the base64 of their stored, encrypted bytes, so they
//! work only in an installation with the same secret key. An import adds the
//! records that are missing and leaves every existing record as it is.

use std::collections::HashMap;

use base64::prelude::{BASE64_STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use sqlx::{SqliteConnection, SqlitePool};
use thiserror::Error;

use crate::crypto::Cipher;
use crate::db::hosts::{self, Host, HostInput, ImportedHost, Provider};
use crate::db::repos::{self, NewRepo, Repo};
use crate::db::sources::{self, NewSource, Source, SourceKind, SourceOptions};
use crate::db::unix_now;
use crate::repo_url::{self, GIST_HOST, RepoLocation};
use crate::settings::CONCURRENCY_RANGE;
use crate::web::form::optional_text;
use crate::web::repos::{CheckedSettings, SettingsInput, check_settings};
use crate::{name_filter, providers, ref_filter};

/// The version of the file layout. An import refuses any other version.
pub const FORMAT: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct Export {
    pub format: u32,
    #[serde(default)]
    pub hosts: Vec<HostEntry>,
    #[serde(default)]
    pub sources: Vec<SourceEntry>,
    #[serde(default)]
    pub repos: Vec<RepoEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HostEntry {
    pub host: String,
    pub provider: String,
    #[serde(default)]
    pub api_base_url: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    /// The base64 of the encrypted token.
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub ref_exclude: Option<String>,
    #[serde(default)]
    pub max_concurrency: Option<i64>,
    /// The base64 of the encrypted private SSH key.
    #[serde(default)]
    pub ssh_key: Option<String>,
    #[serde(default)]
    pub ssh_public_key: Option<String>,
}

/// Names a source the way an import matches it: host, kind and name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRef {
    pub host: String,
    pub kind: String,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SourceEntry {
    #[serde(flatten)]
    pub id: SourceRef,
    #[serde(default)]
    pub auto_add: bool,
    #[serde(default)]
    pub include_forks: bool,
    #[serde(default)]
    pub skip_archived: bool,
    #[serde(default)]
    pub include_patterns: Option<String>,
    #[serde(default)]
    pub exclude_patterns: Option<String>,
    #[serde(default)]
    pub use_ssh: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RepoEntry {
    /// The URL as the add form takes it.
    pub url: String,
    /// The source that added the repository.
    #[serde(default)]
    pub source: Option<SourceRef>,
    #[serde(default)]
    pub username: Option<String>,
    /// The base64 of the encrypted token.
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub interval_minutes: Option<i64>,
    #[serde(default)]
    pub wiki_enabled: bool,
    #[serde(default)]
    pub ref_exclude: Option<String>,
    #[serde(default)]
    pub lfs_enabled: bool,
}

/// Reads every host, source and repository.
pub async fn export(pool: &SqlitePool) -> sqlx::Result<Export> {
    let hosts = hosts::list(pool).await?;
    let sources = sources::list(pool).await?;
    let repos = repos::list(pool, None).await?;
    let host_names: HashMap<i64, &str> = hosts.iter().map(|h| (h.id, h.host.as_str())).collect();
    let source_refs: HashMap<i64, SourceRef> = sources
        .iter()
        .filter_map(|s| Some((s.id, source_ref(host_names.get(&s.host_id)?, s))))
        .collect();
    Ok(Export {
        format: FORMAT,
        hosts: hosts.iter().map(host_entry).collect(),
        sources: sources
            .iter()
            .filter_map(|s| Some(source_entry(source_refs.get(&s.id)?.clone(), s)))
            .collect(),
        repos: repos
            .iter()
            .map(|r| repo_entry(r, r.source_id.and_then(|id| source_refs.get(&id))))
            .collect(),
    })
}

fn encode(secret: Option<&Vec<u8>>) -> Option<String> {
    secret.map(|bytes| BASE64_STANDARD.encode(bytes))
}

fn host_entry(host: &Host) -> HostEntry {
    HostEntry {
        host: host.host.clone(),
        provider: host.provider.code().to_owned(),
        api_base_url: host.api_base_url.clone(),
        username: host.username.clone(),
        token: encode(host.token_enc.as_ref()),
        ref_exclude: host.ref_exclude.clone(),
        max_concurrency: host.max_concurrency,
        ssh_key: encode(host.ssh_key_enc.as_ref()),
        ssh_public_key: host.ssh_public_key.clone(),
    }
}

fn source_ref(host: &str, source: &Source) -> SourceRef {
    SourceRef {
        host: host.to_owned(),
        kind: source.kind.code().to_owned(),
        name: source.name.clone(),
    }
}

fn source_entry(id: SourceRef, source: &Source) -> SourceEntry {
    SourceEntry {
        id,
        auto_add: source.auto_add,
        include_forks: source.include_forks,
        skip_archived: source.skip_archived,
        include_patterns: source.include_patterns.clone(),
        exclude_patterns: source.exclude_patterns.clone(),
        use_ssh: source.use_ssh,
    }
}

/// A gist is stored with its fetch URL, which has no owner; the add form
/// takes it with the owner, so the import finds the same archive path.
fn form_url(repo: &Repo) -> String {
    if repo.host == GIST_HOST {
        format!("https://{GIST_HOST}/{}/{}", repo.owner, repo.name)
    } else {
        repo.url.clone()
    }
}

fn repo_entry(repo: &Repo, source: Option<&SourceRef>) -> RepoEntry {
    RepoEntry {
        url: form_url(repo),
        source: source.cloned(),
        username: repo.username.clone(),
        token: encode(repo.token_enc.as_ref()),
        interval_minutes: repo.interval_minutes,
        wiki_enabled: repo.wiki_enabled,
        ref_exclude: repo.ref_exclude.clone(),
        lfs_enabled: repo.lfs_enabled,
    }
}

#[derive(Debug, Error)]
pub enum ImportError {
    #[error("the file is not an export: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unknown export format {0}")]
    Format(u32),
}

impl ImportError {
    pub fn message_key(&self) -> &'static str {
        match self {
            Self::Json(_) => "transfer.bad_file",
            Self::Format(_) => "transfer.bad_format",
        }
    }
}

/// Parses an export file and checks its format version.
pub fn parse(bytes: &[u8]) -> Result<Export, ImportError> {
    let export: Export = serde_json::from_slice(bytes)?;
    if export.format != FORMAT {
        return Err(ImportError::Format(export.format));
    }
    Ok(export)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    Host,
    Source,
    Repo,
}

impl RecordKind {
    pub fn label_key(self) -> &'static str {
        match self {
            Self::Host => "transfer.kind_host",
            Self::Source => "transfer.kind_source",
            Self::Repo => "transfer.kind_repo",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Added,
    /// Added without a secret that did not decrypt with this installation's key.
    AddedWithoutSecret,
    Exists,
    /// Not added; the message key gives the reason.
    Invalid(&'static str),
}

#[derive(Debug, Clone)]
pub struct ImportRow {
    pub kind: RecordKind,
    pub name: String,
    pub outcome: Outcome,
}

/// Adds the records of `export` that are missing, in one transaction:
/// a database error leaves the database as it was.
pub async fn import(
    pool: &SqlitePool,
    cipher: &Cipher,
    export: &Export,
) -> sqlx::Result<Vec<ImportRow>> {
    let now = unix_now();
    let mut tx = pool.begin().await?;
    let mut rows = Vec::new();
    for entry in &export.hosts {
        rows.push(import_host(&mut tx, cipher, entry, now).await?);
    }
    for entry in &export.sources {
        rows.push(import_source(&mut tx, entry, now).await?);
    }
    for entry in &export.repos {
        rows.push(import_repo(&mut tx, cipher, entry, now).await?);
    }
    tx.commit().await?;
    Ok(rows)
}

/// A secret of the file, kept only when it decrypts with this installation's key.
struct Secret {
    bytes: Option<Vec<u8>>,
    dropped: bool,
}

fn secret(cipher: &Cipher, encoded: Option<&str>) -> Secret {
    let Some(encoded) = encoded.filter(|text| !text.is_empty()) else {
        return Secret {
            bytes: None,
            dropped: false,
        };
    };
    let bytes = BASE64_STANDARD
        .decode(encoded)
        .ok()
        .filter(|bytes| cipher.decrypt(bytes).is_ok());
    Secret {
        dropped: bytes.is_none(),
        bytes,
    }
}

fn added(secret_dropped: bool) -> Outcome {
    if secret_dropped {
        Outcome::AddedWithoutSecret
    } else {
        Outcome::Added
    }
}

fn text(value: Option<&String>) -> Option<&str> {
    optional_text(value.map(String::as_str))
}

struct ValidHost {
    host: String,
    provider: Provider,
    api_base_url: Option<String>,
    ref_exclude: Option<String>,
}

/// Applies the checks of the host form. The `Err` is the message key.
fn check_host(entry: &HostEntry) -> Result<ValidHost, &'static str> {
    let host = repo_url::parse_host(&entry.host).ok_or("hosts.invalid_host")?;
    let provider = Provider::parse(&entry.provider).ok_or("hosts.invalid_provider")?;
    let api_base_url = repo_url::parse_api_url(text(entry.api_base_url.as_ref()).unwrap_or(""))
        .map_err(|err| err.message_key())?;
    let ref_exclude = ref_filter::normalize(text(entry.ref_exclude.as_ref()).unwrap_or(""))
        .map_err(|_| "refs.invalid")?;
    if entry
        .max_concurrency
        .is_some_and(|limit| !CONCURRENCY_RANGE.contains(&limit))
    {
        return Err("hosts.invalid_concurrency");
    }
    Ok(ValidHost {
        host,
        provider,
        api_base_url,
        ref_exclude,
    })
}

async fn import_host(
    conn: &mut SqliteConnection,
    cipher: &Cipher,
    entry: &HostEntry,
    now: i64,
) -> sqlx::Result<ImportRow> {
    let row = |outcome| ImportRow {
        kind: RecordKind::Host,
        name: entry.host.clone(),
        outcome,
    };
    let valid = match check_host(entry) {
        Ok(valid) => valid,
        Err(key) => return Ok(row(Outcome::Invalid(key))),
    };
    let token = secret(cipher, entry.token.as_deref());
    let ssh = secret(cipher, entry.ssh_key.as_deref());
    let public_key = text(entry.ssh_public_key.as_ref());
    // A private key is of no use without the public key the page shows.
    let ssh_key = ssh.bytes.as_deref().zip(public_key);
    let ssh_dropped = ssh.dropped || (ssh.bytes.is_some() && public_key.is_none());
    let host = ImportedHost {
        input: HostInput {
            host: &valid.host,
            provider: valid.provider,
            api_base_url: valid.api_base_url.as_deref(),
            username: text(entry.username.as_ref()),
            token_enc: token.bytes.as_deref(),
        },
        ref_exclude: valid.ref_exclude.as_deref(),
        max_concurrency: entry.max_concurrency,
        ssh_key,
    };
    let outcome = match hosts::insert_imported(conn, &host, now).await? {
        Some(_) => added(token.dropped || ssh_dropped),
        None => Outcome::Exists,
    };
    Ok(row(outcome))
}

struct ValidSource {
    host_id: i64,
    kind: SourceKind,
    name: String,
    options: SourceOptions,
}

/// Applies the checks of the source form to an entry of the host `host`.
fn check_source(host: Option<&Host>, entry: &SourceEntry) -> Result<ValidSource, &'static str> {
    let host = host.ok_or("transfer.no_host")?;
    if !providers::can_list(host.provider) {
        return Err("transfer.host_cannot_list");
    }
    let kind = SourceKind::parse(&entry.id.kind).ok_or("sources.invalid_kind")?;
    if !providers::supports(host, kind) {
        return Err("sources.kind_unsupported");
    }
    let depth = providers::name_depth(host.provider, kind);
    let name = repo_url::parse_owner(&entry.id.name, depth > 1)
        .filter(|name| name.split('/').count() <= depth)
        .ok_or("sources.invalid_name")?;
    let patterns = |value: Option<&String>| {
        name_filter::normalize(text(value).unwrap_or("")).map_err(|_| "sources.invalid_pattern")
    };
    Ok(ValidSource {
        host_id: host.id,
        kind,
        name,
        options: SourceOptions {
            auto_add: entry.auto_add,
            include_forks: entry.include_forks,
            skip_archived: entry.skip_archived,
            include_patterns: patterns(entry.include_patterns.as_ref())?,
            exclude_patterns: patterns(entry.exclude_patterns.as_ref())?,
            use_ssh: entry.use_ssh,
        },
    })
}

async fn import_source(
    conn: &mut SqliteConnection,
    entry: &SourceEntry,
    now: i64,
) -> sqlx::Result<ImportRow> {
    let id = &entry.id;
    let row = |outcome| ImportRow {
        kind: RecordKind::Source,
        name: format!("{}: {} ({})", id.host, id.name, id.kind),
        outcome,
    };
    let host = hosts::find_by_host(&mut *conn, &id.host).await?;
    let valid = match check_source(host.as_ref(), entry) {
        Ok(valid) => valid,
        Err(key) => return Ok(row(Outcome::Invalid(key))),
    };
    let source = NewSource {
        host_id: valid.host_id,
        kind: valid.kind,
        name: &valid.name,
        options: valid.options,
    };
    let outcome = match sources::insert(conn, &source, now).await? {
        Some(_) => Outcome::Added,
        None => Outcome::Exists,
    };
    Ok(row(outcome))
}

/// Applies the checks of the add form and the repository settings.
fn check_repo(entry: &RepoEntry) -> Result<(RepoLocation, CheckedSettings), &'static str> {
    let location = repo_url::parse(&entry.url).map_err(|err| err.message_key())?;
    let settings = check_settings(&SettingsInput {
        interval_minutes: entry.interval_minutes,
        wiki_enabled: entry.wiki_enabled,
        lfs_enabled: entry.lfs_enabled,
        ref_exclude: text(entry.ref_exclude.as_ref()).unwrap_or(""),
    })?;
    Ok((location, settings))
}

/// The id of the source the entry names; `None` when it is not in the archive.
async fn linked_source(
    conn: &mut SqliteConnection,
    source: Option<&SourceRef>,
) -> sqlx::Result<Option<i64>> {
    let Some(source) = source else {
        return Ok(None);
    };
    let Some(kind) = SourceKind::parse(&source.kind) else {
        return Ok(None);
    };
    sources::find_id(conn, &source.host, kind, &source.name).await
}

async fn import_repo(
    conn: &mut SqliteConnection,
    cipher: &Cipher,
    entry: &RepoEntry,
    now: i64,
) -> sqlx::Result<ImportRow> {
    let row = |outcome| ImportRow {
        kind: RecordKind::Repo,
        name: entry.url.clone(),
        outcome,
    };
    let (location, settings) = match check_repo(entry) {
        Ok(checked) => checked,
        Err(key) => return Ok(row(Outcome::Invalid(key))),
    };
    let source_id = linked_source(&mut *conn, entry.source.as_ref()).await?;
    let token = secret(cipher, entry.token.as_deref());
    let repo = NewRepo {
        url: &location.url,
        host: &location.host,
        owner: &location.owner,
        name: &location.name,
        source_id,
        username: text(entry.username.as_ref()),
        token_enc: token.bytes.as_deref(),
        interval_minutes: entry.interval_minutes,
        wiki_enabled: entry.wiki_enabled,
    };
    let Some(id) = repos::insert(&mut *conn, &repo, now).await? else {
        return Ok(row(Outcome::Exists));
    };
    repos::update_settings(conn, id, settings.as_settings()).await?;
    Ok(row(added(token.dropped)))
}
