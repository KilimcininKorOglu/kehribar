//! Archives the external images that the Markdown files of a repository
//! and the Markdown pages of its wiki show, so the README keeps its
//! pictures when their server goes away.
//!
//! The files come from servers the archived content names, so every
//! request is checked: only `http(s)`, only public addresses, and the
//! connection goes to the address that was checked. Redirects are followed
//! by hand and checked the same way.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use reqwest::header::{CONTENT_TYPE, LOCATION};
use reqwest::redirect::Policy;
use sha2::{Digest, Sha256};
use url::{Host, Url};

use super::SyncContext;
use crate::config::Config;
use crate::db::assets::{self, Asset, Check};
use crate::db::repos::Repo;
use crate::git::{Git, mirror, read};
use crate::web::{markdown, wiki};

/// Images archived per repository at most; the repository page says when
/// its files name more.
pub const MAX_IMAGES: usize = 200;
/// Bytes of one image at most.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// An archived image is downloaded again after this many seconds.
pub const REFRESH_SECONDS: i64 = 86_400;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REDIRECTS: usize = 3;
/// A file younger than this is never removed as unused: a sync that runs
/// at the same time may have written it and not recorded it yet.
const UNUSED_GRACE: Duration = Duration::from_secs(3600);

/// Address blocks that are never public: `(network, prefix length)`.
const BLOCKED_V4: &[(Ipv4Addr, u8)] = &[
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(192, 0, 2, 0), 24),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(198, 51, 100, 0), 24),
    (Ipv4Addr::new(203, 0, 113, 0), 24),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
];
const BLOCKED_V6: &[(Ipv6Addr, u8)] = &[
    (Ipv6Addr::UNSPECIFIED, 128),
    (Ipv6Addr::LOCALHOST, 128),
    (Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),
    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
];
/// NAT64 addresses carry an IPv4 address in their last 32 bits.
const NAT64: (Ipv6Addr, u8) = (Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96);

fn in_block(address: u128, network: u128, prefix: u8, bits: u32) -> bool {
    let shift = bits - u32::from(prefix);
    address.checked_shr(shift).unwrap_or(0) == network.checked_shr(shift).unwrap_or(0)
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let address = u128::from(u32::from(ip));
    !BLOCKED_V4
        .iter()
        .any(|(net, prefix)| in_block(address, u128::from(u32::from(*net)), *prefix, 32))
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let address = u128::from(ip);
    if in_block(address, u128::from(NAT64.0), NAT64.1, 128) {
        // The low 32 bits are the IPv4 address; the cast keeps exactly them.
        #[allow(clippy::cast_possible_truncation)]
        return is_public_v4(Ipv4Addr::from(address as u32));
    }
    !BLOCKED_V6
        .iter()
        .any(|(net, prefix)| in_block(address, u128::from(*net), *prefix, 128))
}

/// Whether `ip` is a public address: not loopback, private, link-local,
/// shared (CGNAT), multicast, documentation or otherwise reserved.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

/// Decides which addresses an image request may connect to.
#[derive(Debug, Clone, Default)]
pub struct AddressGuard {
    /// Addresses allowed although they are not public. Empty in the
    /// application; tests add the address of their local image server.
    allowed: Arc<Vec<IpAddr>>,
}

impl AddressGuard {
    /// Allows public addresses only.
    pub fn public_only() -> Self {
        Self::default()
    }

    /// Allows `addresses` too. For tests, whose image server is local.
    pub fn allowing(addresses: Vec<IpAddr>) -> Self {
        Self {
            allowed: Arc::new(addresses),
        }
    }

    pub fn permits(&self, ip: IpAddr) -> bool {
        self.allowed.contains(&ip) || is_public(ip)
    }
}

/// A downloaded image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub data: Vec<u8>,
    pub content_type: String,
}

/// The address to connect to for `url`, after the guard checked every
/// address the host name resolves to.
async fn checked_address(guard: &AddressGuard, url: &Url) -> Result<SocketAddr, String> {
    let port = url.port_or_known_default().ok_or("the URL has no port")?;
    let addresses: Vec<SocketAddr> = match url.host() {
        Some(Host::Ipv4(ip)) => vec![SocketAddr::new(IpAddr::V4(ip), port)],
        Some(Host::Ipv6(ip)) => vec![SocketAddr::new(IpAddr::V6(ip), port)],
        Some(Host::Domain(name)) => tokio::net::lookup_host((name, port))
            .await
            .map_err(|err| format!("cannot resolve {name}: {err}"))?
            .collect(),
        None => return Err("the URL has no host".to_owned()),
    };
    if let Some(blocked) = addresses.iter().find(|a| !guard.permits(a.ip())) {
        return Err(format!("{} is not a public address", blocked.ip()));
    }
    addresses
        .first()
        .copied()
        .ok_or_else(|| "the host has no address".to_owned())
}

/// One request without redirects, connected to the checked address.
async fn request(guard: &AddressGuard, url: &Url) -> Result<reqwest::Response, String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("{} URLs are not fetched", url.scheme()));
    }
    let address = checked_address(guard, url).await?;
    let mut builder = reqwest::Client::builder()
        .redirect(Policy::none())
        .no_proxy()
        .timeout(REQUEST_TIMEOUT);
    if let Some(Host::Domain(name)) = url.host() {
        builder = builder.resolve(name, address);
    }
    let client = builder.build().map_err(|err| err.to_string())?;
    let response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|err| err.to_string())?;
    let status = response.status();
    if status.is_success() || status.is_redirection() {
        Ok(response)
    } else {
        Err(format!("the server answered {status}"))
    }
}

/// Downloads the image at `url`, following at most three redirects.
pub async fn download(guard: &AddressGuard, url: &str) -> Result<Image, String> {
    let mut current = Url::parse(url).map_err(|err| format!("invalid URL: {err}"))?;
    for _ in 0..=MAX_REDIRECTS {
        let response = request(guard, &current).await?;
        if !response.status().is_redirection() {
            return read_image(response).await;
        }
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or("a redirect without a location")?;
        current = current
            .join(location)
            .map_err(|err| format!("invalid redirect: {err}"))?;
    }
    Err(format!("more than {MAX_REDIRECTS} redirects"))
}

/// The `type/subtype` of an image response; `None` for any other type.
fn image_type(value: &str) -> Option<String> {
    let mime = value.split(';').next()?.trim().to_ascii_lowercase();
    let valid = mime.starts_with("image/")
        && mime
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '+' | '-'));
    valid.then_some(mime)
}

async fn read_image(mut response: reqwest::Response) -> Result<Image, String> {
    let declared = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let content_type =
        image_type(declared).ok_or_else(|| format!("not an image: \"{declared}\""))?;
    let too_large = || format!("larger than {} MB", MAX_IMAGE_BYTES / (1024 * 1024));
    if response
        .content_length()
        .is_some_and(|length| length > MAX_IMAGE_BYTES as u64)
    {
        return Err(too_large());
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|err| err.to_string())? {
        if data.len() + chunk.len() > MAX_IMAGE_BYTES {
            return Err(too_large());
        }
        data.extend_from_slice(&chunk);
    }
    Ok(Image { data, content_type })
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Whether `name` is a stored image's name: 64 lowercase hex digits.
pub fn is_hash(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Stores `image` in `dir` under its hash, unless a file with that content
/// exists.
async fn store(dir: &Path, image: &Image) -> std::io::Result<String> {
    let hash = sha256_hex(&image.data);
    let file = dir.join(&hash);
    match touch(&file).await {
        Ok(()) => return Ok(hash),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    tokio::fs::create_dir_all(dir).await?;
    // Written beside its final name and renamed, so a reader never sees a
    // partial file. Each writer has its own part file: two syncs that store
    // the same image at once never write into one file.
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix).map_err(std::io::Error::other)?;
    let part = dir.join(format!(".{hash}.{}.part", sha256_hex(&suffix)));
    tokio::fs::write(&part, &image.data).await?;
    tokio::fs::rename(&part, &file).await?;
    Ok(hash)
}

/// Sets the modification time of a stored image to now. A sync that
/// removes unused images may have read the used ones before this sync
/// records the image; the fresh time keeps the file for [`UNUSED_GRACE`].
async fn touch(file: &Path) -> std::io::Result<()> {
    let handle = tokio::fs::OpenOptions::new()
        .write(true)
        .open(file)
        .await?
        .into_std()
        .await;
    tokio::task::spawn_blocking(move || handle.set_modified(SystemTime::now()))
        .await
        .map_err(std::io::Error::other)?
}

/// External image URLs in the order they were found, without repeats.
#[derive(Default)]
struct Found {
    urls: Vec<String>,
    seen: HashSet<String>,
}

/// The external image URLs of the Markdown files at the archive's `HEAD`,
/// then of the Markdown pages of its wiki when `with_wiki`.
async fn collect(git: &Git, dir: &Path, with_wiki: bool) -> Result<Vec<String>, String> {
    let mut found = Found::default();
    collect_from(git, dir, markdown::is_markdown, &mut found).await?;
    if with_wiki {
        let wiki = mirror::wiki_dir(dir);
        collect_from(git, &wiki, wiki::is_markdown_page, &mut found).await?;
    }
    Ok(found.urls)
}

/// Adds the image URLs of the files at `HEAD` of `dir` that `is_page`
/// accepts.
async fn collect_from(
    git: &Git,
    dir: &Path,
    is_page: fn(&str) -> bool,
    found: &mut Found,
) -> Result<(), String> {
    let Some(commit) = read::resolve_commit(git, dir, "HEAD")
        .await
        .map_err(|err| err.message)?
    else {
        return Ok(());
    };
    let files = read::files(git, dir, &commit)
        .await
        .map_err(|err| err.message)?;
    for file in files.iter().filter(|f| !f.symlink && is_page(&f.path)) {
        let blob = read::blob(git, dir, &commit, &file.path, read::MAX_BLOB_BYTES)
            .await
            .map_err(|err| err.message)?;
        if blob.too_large || blob.is_binary() {
            continue;
        }
        let text = String::from_utf8_lossy(&blob.data);
        for url in markdown::image_urls(&text) {
            if found.seen.insert(url.clone()) {
                found.urls.push(url);
            }
        }
    }
    Ok(())
}

/// Downloads the images of a repository, and of its wiki when `with_wiki`,
/// that have no copy or an old one, after a successful sync. A failed
/// download keeps the copy it had and does not fail the sync; it is logged.
pub async fn refresh(
    ctx: &SyncContext,
    git: &Git,
    repo: &Repo,
    dir: &Path,
    with_wiki: bool,
    now: i64,
) -> sqlx::Result<()> {
    let urls = match collect(git, dir, with_wiki).await {
        Ok(urls) => urls,
        Err(err) => {
            tracing::warn!(repo = %repo.url, %err, "cannot list the images of the Markdown files");
            return Ok(());
        }
    };
    let found = urls.len();
    let wanted: Vec<String> = urls.into_iter().take(MAX_IMAGES).collect();
    let known: HashMap<String, Asset> = assets::for_repo(&ctx.pool, repo.id)
        .await?
        .into_iter()
        .map(|asset| (asset.url.clone(), asset))
        .collect();
    for url in known.keys().filter(|url| !wanted.contains(url)) {
        assets::remove(&ctx.pool, repo.id, url).await?;
    }
    for url in &wanted {
        let fresh = known
            .get(url)
            .is_some_and(|asset| now - asset.fetched_at < REFRESH_SECONDS);
        if !fresh {
            fetch(ctx, repo, url, now).await?;
        }
    }
    let check = Check {
        found: i64::try_from(found).unwrap_or(i64::MAX),
        skipped: i64::try_from(found - wanted.len()).unwrap_or(i64::MAX),
        checked_at: now,
    };
    assets::record_check(&ctx.pool, repo.id, check).await?;
    remove_unused(&ctx.pool, &ctx.config).await
}

/// Downloads and records one image; a failure is logged.
async fn fetch(ctx: &SyncContext, repo: &Repo, url: &str, now: i64) -> sqlx::Result<()> {
    let image = match download(&ctx.guard, url).await {
        Ok(image) => image,
        Err(err) => {
            tracing::warn!(repo = %repo.url, %url, %err, "cannot download an image");
            return Ok(());
        }
    };
    let sha256 = match store(&ctx.config.assets_dir(), &image).await {
        Ok(hash) => hash,
        Err(err) => {
            tracing::warn!(repo = %repo.url, %url, %err, "cannot store an image");
            return Ok(());
        }
    };
    let asset = Asset {
        url: url.to_owned(),
        sha256,
        content_type: image.content_type,
        bytes: i64::try_from(image.data.len()).unwrap_or(i64::MAX),
        fetched_at: now,
    };
    assets::upsert(&ctx.pool, repo.id, &asset).await
}

/// Deletes the stored images that no repository uses any more. Errors are
/// logged: a file left behind takes space but breaks nothing.
pub async fn remove_unused(pool: &sqlx::SqlitePool, config: &Config) -> sqlx::Result<()> {
    let used = assets::used_hashes(pool).await?;
    let dir = config.assets_dir();
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            tracing::warn!(dir = %dir.display(), %err, "cannot list the stored images");
            return Ok(());
        }
    };
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => remove_if_unused(&entry, &used).await,
            Ok(None) => return Ok(()),
            Err(err) => {
                tracing::warn!(dir = %dir.display(), %err, "cannot list the stored images");
                return Ok(());
            }
        }
    }
}

async fn remove_if_unused(entry: &tokio::fs::DirEntry, used: &HashSet<String>) {
    let name = entry.file_name().to_string_lossy().into_owned();
    if !is_hash(&name) || used.contains(&name) {
        return;
    }
    let path = entry.path();
    let recent = match entry.metadata().await.and_then(|m| m.modified()) {
        Ok(modified) => SystemTime::now()
            .duration_since(modified)
            .map_or(true, |age| age < UNUSED_GRACE),
        Err(err) => {
            tracing::warn!(file = %path.display(), %err, "cannot read a stored image");
            return;
        }
    };
    if recent {
        return;
    }
    if let Err(err) = tokio::fs::remove_file(&path).await {
        tracing::warn!(file = %path.display(), %err, "cannot remove an unused image");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn only_public_addresses_are_permitted() {
        let guard = AddressGuard::public_only();
        for blocked in [
            "127.0.0.1",
            "10.1.2.3",
            "172.31.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "ff02::1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
        ] {
            assert!(!guard.permits(ip(blocked)), "{blocked}");
        }
        for public in [
            "93.184.216.34",
            "1.1.1.1",
            "2606:4700::1111",
            "64:ff9b::101:101",
        ] {
            assert!(guard.permits(ip(public)), "{public}");
        }
        let test = AddressGuard::allowing(vec![ip("127.0.0.1")]);
        assert!(test.permits(ip("127.0.0.1")));
        assert!(!test.permits(ip("::1")));
    }

    #[test]
    fn image_types_and_hashes_are_checked() {
        assert_eq!(
            image_type("image/PNG; charset=x").as_deref(),
            Some("image/png")
        );
        assert_eq!(
            image_type("image/svg+xml").as_deref(),
            Some("image/svg+xml")
        );
        assert_eq!(image_type("text/html"), None);
        assert_eq!(image_type("image/png\"x"), None);
        let hash = sha256_hex(b"abc");
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(is_hash(&hash));
        assert!(!is_hash("../etc/passwd"));
    }

    #[tokio::test]
    async fn requests_to_private_addresses_are_refused_before_connecting() {
        let guard = AddressGuard::public_only();
        for url in [
            "http://127.0.0.1/a.png",
            "http://10.0.0.1/a.png",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]/a.png",
            "http://localhost/a.png",
        ] {
            let err = download(&guard, url).await.unwrap_err();
            assert!(err.contains("is not a public address"), "{url}: {err}");
        }
        let err = download(&guard, "file:///etc/passwd").await.unwrap_err();
        assert!(err.contains("file URLs are not fetched"), "{err}");
    }

    #[tokio::test]
    async fn storing_does_not_touch_the_part_file_of_another_writer() {
        let dir = tempfile::tempdir().unwrap();
        let image = Image {
            data: b"png bytes".to_vec(),
            content_type: "image/png".to_owned(),
        };
        let hash = sha256_hex(&image.data);
        // Another sync is writing the same image now.
        let other = dir.path().join(format!(".{hash}.part"));
        std::fs::write(&other, b"png").unwrap();

        assert_eq!(store(dir.path(), &image).await.unwrap(), hash);
        assert_eq!(std::fs::read(dir.path().join(&hash)).unwrap(), image.data);
        assert_eq!(std::fs::read(&other).unwrap(), b"png");
    }

    #[tokio::test]
    async fn storing_an_existing_image_renews_its_time() {
        let dir = tempfile::tempdir().unwrap();
        let image = Image {
            data: b"png bytes".to_vec(),
            content_type: "image/png".to_owned(),
        };
        let file = dir.path().join(sha256_hex(&image.data));
        std::fs::write(&file, &image.data).unwrap();
        let old = SystemTime::now() - 2 * UNUSED_GRACE;
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(old)
            .unwrap();

        store(dir.path(), &image).await.unwrap();
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default();
        assert!(age < UNUSED_GRACE, "{age:?}");
    }
}
