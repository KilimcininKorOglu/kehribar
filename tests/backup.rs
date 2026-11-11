//! Backups: bundles reach S3-compatible storage once per ref change, old
//! bundles are pruned, failures are recorded and notified, and the settings
//! page saves, tests and starts a backup.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::http::{StatusCode, header};
use common::{TestApp, body_text, git_in, header_value};
use git_archive::backup::{self, s3::PART_SIZE};
use git_archive::config::Config;
use git_archive::crypto::Cipher;
use git_archive::db::repos::{self, NewRepo};
use git_archive::db::{self, unix_now};
use git_archive::i18n::Catalog;
use git_archive::sync::SyncContext;
use git_archive::sync::scheduler::Scheduler;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const BUCKET: &str = "archive";
const XMLNS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

#[derive(Default)]
struct S3State {
    objects: BTreeMap<String, Vec<u8>>,
    parts: BTreeMap<u16, Vec<u8>>,
    fail_puts: bool,
    multipart_uploads: usize,
}

/// A path-style S3 server that keeps its objects in memory.
#[derive(Clone, Default)]
struct FakeS3(Arc<Mutex<S3State>>);

impl FakeS3 {
    async fn start() -> (MockServer, Self) {
        let server = MockServer::start().await;
        let fake = Self::default();
        Mock::given(path_regex(format!("^/{BUCKET}(/.*)?$")))
            .respond_with(fake.clone())
            .mount(&server)
            .await;
        (server, fake)
    }

    fn keys(&self, prefix: &str) -> Vec<String> {
        let state = self.0.lock().unwrap();
        state
            .objects
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect()
    }

    fn object(&self, key: &str) -> Vec<u8> {
        self.0.lock().unwrap().objects[key].clone()
    }

    fn insert(&self, key: &str, body: &[u8]) {
        let mut state = self.0.lock().unwrap();
        state.objects.insert(key.to_owned(), body.to_vec());
    }
}

impl Respond for FakeS3 {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let key = request
            .url
            .path()
            .trim_start_matches(&format!("/{BUCKET}"))
            .trim_start_matches('/')
            .to_owned();
        let query: HashMap<String, String> = request.url.query_pairs().into_owned().collect();
        let mut state = self.0.lock().unwrap();
        match (request.method.as_str(), key.is_empty()) {
            ("GET", true) => list(&state, query.get("prefix").map_or("", String::as_str)),
            ("PUT", false) if state.fail_puts => ResponseTemplate::new(500).set_body_string(
                "<Error><Code>InternalError</Code><Message>disk full</Message></Error>",
            ),
            ("PUT", false) => match query.get("partNumber") {
                Some(number) => {
                    let number: u16 = number.parse().unwrap();
                    state.parts.insert(number, request.body.clone());
                    ResponseTemplate::new(200).insert_header("ETag", format!("\"part{number}\""))
                }
                None => {
                    state.objects.insert(key, request.body.clone());
                    ResponseTemplate::new(200)
                }
            },
            ("GET", false) => match state.objects.get(&key) {
                Some(body) => ResponseTemplate::new(200).set_body_bytes(body.clone()),
                None => ResponseTemplate::new(404)
                    .set_body_string("<Error><Code>NoSuchKey</Code></Error>"),
            },
            ("DELETE", false) => {
                state.objects.remove(&key);
                ResponseTemplate::new(204)
            }
            ("POST", false) if query.contains_key("uploads") => xml(&format!(
                "<InitiateMultipartUploadResult xmlns=\"{XMLNS}\"><Bucket>{BUCKET}</Bucket>\
                 <Key>{key}</Key><UploadId>upload-1</UploadId></InitiateMultipartUploadResult>"
            )),
            ("POST", false) => {
                let parts = std::mem::take(&mut state.parts);
                state
                    .objects
                    .insert(key, parts.into_values().flatten().collect());
                state.multipart_uploads += 1;
                xml(&format!(
                    "<CompleteMultipartUploadResult xmlns=\"{XMLNS}\">\
                     <ETag>\"done\"</ETag></CompleteMultipartUploadResult>"
                ))
            }
            _ => ResponseTemplate::new(400),
        }
    }
}

fn list(state: &S3State, prefix: &str) -> ResponseTemplate {
    let contents: String = state
        .objects
        .iter()
        .filter(|(key, _)| key.starts_with(prefix))
        .map(|(key, body)| {
            format!(
                "<Contents><Key>{key}</Key><Size>{}</Size></Contents>",
                body.len()
            )
        })
        .collect();
    xml(&format!(
        "<ListBucketResult xmlns=\"{XMLNS}\"><Name>{BUCKET}</Name><Prefix>{prefix}</Prefix>\
         <IsTruncated>false</IsTruncated>{contents}</ListBucketResult>"
    ))
}

fn xml(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.to_owned(), "application/xml")
}

fn backup_config(data_dir: &Path, endpoint: &str) -> Config {
    let mut config = common::test_config(data_dir);
    config.backup.endpoint = Some(endpoint.to_owned());
    config.backup.bucket = Some(BUCKET.into());
    config.backup.prefix = "backups".into();
    config.backup.access_key_id = Some("key-id".into());
    config.backup.secret_access_key = Some("key-secret".into());
    config.backup.path_style = true;
    config.backup.keep = 1;
    config
}

fn context(config: Config, pool: sqlx::SqlitePool) -> SyncContext {
    SyncContext {
        pool,
        cipher: Cipher::new(&config.secret_key),
        config: Arc::new(config),
        catalog: Arc::new(Catalog::load().unwrap()),
        locks: Default::default(),
    }
}

/// An archived copy of `source`, synced once; returns its id.
async fn archived_repo(ctx: &SyncContext, source: &Path) -> i64 {
    let url = source.to_string_lossy().into_owned();
    let new = NewRepo {
        url: &url,
        host: "local",
        owner: "team",
        name: "project",
        source_id: None,
        username: None,
        token_enc: None,
        interval_minutes: None,
        wiki_enabled: false,
    };
    let id = repos::insert(&ctx.pool, &new, unix_now())
        .await
        .unwrap()
        .unwrap();
    sync(ctx, id).await;
    id
}

async fn sync(ctx: &SyncContext, id: i64) {
    repos::schedule_now(&ctx.pool, id, unix_now())
        .await
        .unwrap();
    for handle in Scheduler::new(ctx.clone()).tick().await.unwrap() {
        handle.await.unwrap();
    }
}

/// Restores `bundle` with `git clone --mirror` and counts its commits.
fn restored_commits(dir: &Path, bundle: &[u8]) -> String {
    let file = dir.join("restore.bundle");
    std::fs::write(&file, bundle).unwrap();
    let target = dir.join("restored.git");
    git_in(
        dir,
        &[
            "clone",
            "--quiet",
            "--mirror",
            file.to_str().unwrap(),
            target.to_str().unwrap(),
        ],
    );
    git_in(&target, &["rev-list", "--all", "--count"])
}

fn commit_file(source: &Path, name: &str, content: &[u8]) {
    std::fs::write(source.join(name), content).unwrap();
    git_in(source, &["add", name]);
    git_in(source, &["commit", "--quiet", "-m", name]);
}

const FOLDER: &str = "backups/local/team/project/";

#[tokio::test]
async fn a_bundle_is_uploaded_once_per_change_and_old_bundles_are_pruned() {
    let (server, s3) = FakeS3::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = backup_config(&dir.path().join("data"), &server.uri());
    let ctx = context(config, db::open_in_memory().await.unwrap());
    let source = common::source_repo(dir.path(), "project");
    let id = archived_repo(&ctx, &source).await;
    assert!(backup::is_due(&ctx).await.unwrap());

    let run = backup::run(&ctx, PART_SIZE).await.unwrap().unwrap();
    assert_eq!((run.uploaded, run.error.as_deref()), (1, None));
    let keys = s3.keys(FOLDER);
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert!(keys[0].ends_with(".bundle"));
    let restore = tempfile::tempdir().unwrap();
    assert_eq!(restored_commits(restore.path(), &s3.object(&keys[0])), "1");
    assert!(!backup::is_due(&ctx).await.unwrap());

    // Unchanged refs: nothing is uploaded again.
    let run = backup::run(&ctx, PART_SIZE).await.unwrap().unwrap();
    assert_eq!((run.uploaded, run.error.as_deref()), (0, None));
    assert_eq!(s3.keys(FOLDER), keys);

    // A new commit: a new bundle, and only the newest one is kept. Other
    // objects in the folder stay.
    s3.insert(&format!("{FOLDER}1.bundle"), b"old");
    s3.insert(&format!("{FOLDER}notes.txt"), b"not a bundle");
    commit_file(&source, "second.txt", b"second\n");
    sync(&ctx, id).await;
    let run = backup::run(&ctx, PART_SIZE).await.unwrap().unwrap();
    assert_eq!((run.uploaded, run.error.as_deref()), (1, None));
    let keys = s3.keys(FOLDER);
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert_eq!(keys[1], format!("{FOLDER}notes.txt"));
    let restore = tempfile::tempdir().unwrap();
    assert_eq!(restored_commits(restore.path(), &s3.object(&keys[0])), "2");
    // No temporary bundle stays behind.
    let tmp = ctx.config.data_dir.join("tmp");
    assert_eq!(std::fs::read_dir(tmp).unwrap().count(), 0);
}

#[tokio::test]
async fn a_failed_upload_is_recorded_and_notified_until_it_works_again() {
    let (server, s3) = FakeS3::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut config = backup_config(&dir.path().join("data"), &server.uri());
    config.notify.webhook_url = Some(format!("{}/hook", server.uri()));
    let ctx = context(config, db::open_in_memory().await.unwrap());
    let source = common::source_repo(dir.path(), "project");
    archived_repo(&ctx, &source).await;
    let events = async || {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/hook")
            .map(|request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                body["event"].as_str().unwrap().to_owned()
            })
            .collect::<Vec<_>>()
    };

    s3.0.lock().unwrap().fail_puts = true;
    for _ in 0..2 {
        let run = backup::run(&ctx, PART_SIZE).await.unwrap().unwrap();
        assert_eq!(run.uploaded, 0);
        assert_eq!(
            run.error.as_deref(),
            Some("local/team/project: S3 HTTP 500: InternalError: disk full")
        );
    }
    assert_eq!(
        events().await,
        ["backup_failed"],
        "a lasting error is sent once"
    );
    assert!(s3.keys(FOLDER).is_empty());

    s3.0.lock().unwrap().fail_puts = false;
    // The failed run stored no hash, so the repository is uploaded now.
    let run = backup::run(&ctx, PART_SIZE).await.unwrap().unwrap();
    assert_eq!((run.uploaded, run.error.as_deref()), (1, None));
    assert_eq!(events().await, ["backup_failed", "backup_recovered"]);
}

#[tokio::test]
async fn a_bundle_larger_than_one_part_goes_up_in_parts() {
    let (server, s3) = FakeS3::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = backup_config(&dir.path().join("data"), &server.uri());
    let ctx = context(config, db::open_in_memory().await.unwrap());
    let source = common::source_repo(dir.path(), "project");
    // Bytes that do not compress, so the bundle spans several parts.
    let mut seed: u32 = 1;
    let noise: Vec<u8> = (0..20_000)
        .map(|_| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (seed >> 16) as u8
        })
        .collect();
    commit_file(&source, "noise.bin", &noise);
    archived_repo(&ctx, &source).await;

    let run = backup::run(&ctx, 4096).await.unwrap().unwrap();
    assert_eq!((run.uploaded, run.error.as_deref()), (1, None));
    assert_eq!(s3.0.lock().unwrap().multipart_uploads, 1);
    let keys = s3.keys(FOLDER);
    let bundle = s3.object(&keys[0]);
    assert!(bundle.len() > 4096 * 4, "{}", bundle.len());
    let restore = tempfile::tempdir().unwrap();
    assert_eq!(restored_commits(restore.path(), &bundle), "2");
}

#[tokio::test]
async fn settings_page_saves_tests_and_starts_the_backup() {
    let (server, s3) = FakeS3::start().await;
    let t = TestApp::new().await;

    let page = body_text(t.get("/settings").await).await;
    assert!(page.contains("Henüz yedekleme yapılmadı."));
    let not_configured = t.post("/settings/backup/run", "").await;
    assert_eq!(not_configured.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body_text(not_configured)
            .await
            .contains("Önce endpoint, bucket")
    );

    let bad = t
        .post("/settings/backup", "s3_endpoint=ftp%3A%2F%2Fx&s3_bucket=b")
        .await;
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_text(bad).await;
    assert!(body.contains("S3 endpoint&#39;i http:// veya https:// ile başlamalı."));
    // The form keeps what was typed.
    assert!(body.contains("value=\"ftp://x\""));
    let bad_keep = t.post("/settings/backup", "keep=0").await;
    assert_eq!(bad_keep.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let form = format!(
        "s3_endpoint={}&s3_bucket={BUCKET}&s3_prefix=backups&s3_access_key_id=id\
         &s3_secret=top-secret&s3_path_style=true&interval_hours=12&keep=3",
        urlencode(&server.uri())
    );
    let saved = t.post("/settings/backup", &form).await;
    assert_eq!(
        header_value(&saved, header::LOCATION),
        "/settings?notice=backup_saved#backup"
    );
    let page = body_text(t.get("/settings?notice=backup_saved").await).await;
    assert!(page.contains("Yedekleme ayarları kaydedildi."));
    assert!(page.contains("Kayıtlı bir secret access key var."));
    assert!(!page.contains("top-secret"));

    let tested = t.post("/settings/backup/test", "").await;
    assert_eq!(tested.status(), StatusCode::OK);
    assert!(body_text(tested).await.contains("Bağlantı çalışıyor."));
    assert!(s3.keys("").is_empty(), "the test object is deleted");

    s3.0.lock().unwrap().fail_puts = true;
    let failed = t.post("/settings/backup/test", "").await;
    assert_eq!(failed.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body_text(failed)
            .await
            .contains("S3 HTTP 500: InternalError: disk full")
    );

    let started = t.post("/settings/backup/run", "").await;
    assert_eq!(
        header_value(&started, header::LOCATION),
        "/settings?notice=backup_started#backup"
    );

    // Clearing the secret turns the backup off.
    t.post("/settings/backup", "clear_s3_secret=1").await;
    let page = body_text(t.get("/settings").await).await;
    assert!(!page.contains("Kayıtlı bir secret access key var."));
}

fn urlencode(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}
