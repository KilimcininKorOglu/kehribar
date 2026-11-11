//! SSH keys of hosts and the stored host keys, handled with `ssh-keygen`.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use thiserror::Error;
use tokio::process::Command;

const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum SshError {
    #[error("ssh-keygen: {0}")]
    Keygen(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// A new key: the private key in OpenSSH format and its public line.
#[derive(Debug)]
pub struct KeyPair {
    pub private: String,
    pub public: String,
}

/// A private key in a file only its owner can read. The file is removed
/// when the value is dropped, also after an error.
#[derive(Debug)]
pub struct KeyFile(PathBuf);

impl KeyFile {
    pub fn write(dir: &Path, key: &str) -> Result<Self, SshError> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("ssh-key-{}", random_name()?));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let guard = Self(path);
        file.write_all(key.as_bytes())?;
        // OpenSSH refuses a key file without its final newline.
        if !key.ends_with('\n') {
            file.write_all(b"\n")?;
        }
        Ok(guard)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for KeyFile {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.0) {
            tracing::warn!(file = %self.0.display(), %err, "cannot remove a temporary key file");
        }
    }
}

/// Removes the key files and key folders that a killed process left in
/// `tmp`: [`KeyFile`] removes its file on drop, which a kill skips. Call it
/// at startup, before anything writes a key. Gives the number removed.
pub fn remove_leftovers(tmp: &Path) -> std::io::Result<usize> {
    let entries = match std::fs::read_dir(tmp) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("ssh-keygen-") {
            std::fs::remove_dir_all(entry.path())?;
        } else if name.starts_with("ssh-key-") {
            std::fs::remove_file(entry.path())?;
        } else {
            continue;
        }
        removed += 1;
    }
    Ok(removed)
}

fn random_name() -> Result<String, SshError> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|err| SshError::Keygen(err.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

async fn keygen(args: &[&OsStr]) -> Result<Output, SshError> {
    let child = Command::new("ssh-keygen")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    match tokio::time::timeout(TIMEOUT, child.wait_with_output()).await {
        Ok(output) => Ok(output?),
        Err(_) => Err(SshError::Keygen("timed out".to_owned())),
    }
}

/// The output of a successful run, else its error message.
fn checked(output: Output) -> Result<Output, SshError> {
    if output.status.success() {
        return Ok(output);
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(SshError::Keygen(message))
}

/// Creates an ed25519 key without a passphrase in a private folder below
/// `tmp`, which is removed again.
pub async fn generate(tmp: &Path, comment: &str) -> Result<KeyPair, SshError> {
    let dir = tmp.join(format!("ssh-keygen-{}", random_name()?));
    std::fs::create_dir_all(&dir)?;
    let result = generate_in(&dir, comment).await;
    if let Err(err) = tokio::fs::remove_dir_all(&dir).await {
        tracing::warn!(dir = %dir.display(), %err, "cannot remove a temporary key folder");
    }
    result
}

async fn generate_in(dir: &Path, comment: &str) -> Result<KeyPair, SshError> {
    let file = dir.join("key");
    let args: [&OsStr; 9] = [
        "-q".as_ref(),
        "-t".as_ref(),
        "ed25519".as_ref(),
        "-N".as_ref(),
        "".as_ref(),
        "-C".as_ref(),
        comment.as_ref(),
        "-f".as_ref(),
        file.as_os_str(),
    ];
    checked(keygen(&args).await?)?;
    Ok(KeyPair {
        private: tokio::fs::read_to_string(&file).await?,
        public: tokio::fs::read_to_string(file.with_extension("pub"))
            .await?
            .trim()
            .to_owned(),
    })
}

/// The public line of `private`. Fails for text that is no private key and
/// for a key with a passphrase, which a fetch could not use.
pub async fn public_key(tmp: &Path, private: &str) -> Result<String, SshError> {
    let file = KeyFile::write(tmp, private)?;
    let args: [&OsStr; 5] = [
        "-y".as_ref(),
        "-P".as_ref(),
        "".as_ref(),
        "-f".as_ref(),
        file.path().as_os_str(),
    ];
    let output = checked(keygen(&args).await?)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The file in `dir` that holds the stored host keys of `host`. Each host
/// has its own file: forgetting the keys of one host removes one file, so
/// it never loses a key that a fetch stores for another host at the same
/// time. Characters other than letters, digits, `-` and an inner `.` are
/// written as `%XX`, so the name never leaves `dir`.
pub fn known_hosts_file(dir: &Path, host: &str) -> PathBuf {
    let mut name = String::with_capacity(host.len());
    for (at, byte) in host.to_ascii_lowercase().bytes().enumerate() {
        if byte.is_ascii_alphanumeric() || byte == b'-' || (byte == b'.' && at > 0) {
            name.push(char::from(byte));
        } else {
            name.push_str(&format!("%{byte:02X}"));
        }
    }
    if name.is_empty() {
        name.push('%');
    }
    dir.join(name)
}

async fn read_known_hosts(file: &Path) -> Result<String, SshError> {
    match tokio::fs::read_to_string(file).await {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(err.into()),
    }
}

/// The key lines of a host file, without blank lines and comments.
fn key_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

/// The stored keys of the host file `file`, as whole lines.
pub async fn known_keys(file: &Path) -> Result<Vec<String>, SshError> {
    let text = read_known_hosts(file).await?;
    Ok(key_lines(&text).map(str::to_owned).collect())
}

/// Removes the host file `file`, so the next connection stores the key
/// the server shows then. Gives the number of keys it held.
pub async fn forget(file: &Path) -> Result<usize, SshError> {
    let count = key_lines(&read_known_hosts(file).await?).count();
    match tokio::fs::remove_file(file).await {
        Ok(()) => Ok(count),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(err.into()),
    }
}

/// Moves the keys of `legacy`, the single `known_hosts` file of earlier
/// versions, into the host files of `dir`, then removes it. A key already
/// in its host file is not added again, so a run that stopped half way
/// can run again. Call it at startup, before a fetch runs. Gives the
/// number of keys moved.
pub fn split_legacy(legacy: &Path, dir: &Path) -> std::io::Result<usize> {
    let text = match std::fs::read_to_string(legacy) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    let mut by_host: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for line in key_lines(&text) {
        match line_host(line) {
            Some(host) => by_host.entry(host).or_default().push(line),
            None => tracing::warn!(line, "known_hosts line without a plain host name dropped"),
        }
    }
    std::fs::create_dir_all(dir)?;
    let mut moved = 0;
    for (host, lines) in by_host {
        moved += append_missing(&known_hosts_file(dir, &host), &lines)?;
    }
    std::fs::remove_file(legacy)?;
    Ok(moved)
}

/// The host whose file a `known_hosts` line (`host` or `[host]:port`)
/// belongs in: the name the settings of a repository use, so a gist key
/// goes with github.com. Hashed names give `None`; fetches never write
/// them.
fn line_host(line: &str) -> Option<String> {
    let first = line.split_whitespace().next()?.split(',').next()?;
    if first.starts_with('|') {
        return None;
    }
    let bare = first
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("]:"))
        .map_or(first, |(host, _port)| host)
        .to_ascii_lowercase();
    Some(crate::repo_url::settings_host(&bare).to_owned())
}

/// Adds the lines of `lines` that `file` lacks. Gives how many it added.
fn append_missing(file: &Path, lines: &[&str]) -> std::io::Result<usize> {
    let existing = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    let mut text = existing.clone();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    let mut added = 0;
    for line in lines {
        if !key_lines(&existing).any(|old| old == *line) {
            text.push_str(line);
            text.push('\n');
            added += 1;
        }
    }
    if added > 0 {
        std::fs::write(file, text)?;
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn generated_keys_give_back_their_public_line() {
        let dir = tempfile::tempdir().unwrap();
        let pair = generate(dir.path(), "git-archive@example").await.unwrap();
        assert!(pair.private.contains("OPENSSH PRIVATE KEY"));
        assert!(pair.public.starts_with("ssh-ed25519 "), "{}", pair.public);
        assert!(pair.public.ends_with(" git-archive@example"));
        // Nothing stays in the temporary folder.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

        let public = public_key(dir.path(), pair.private.trim_end())
            .await
            .unwrap();
        assert_eq!(public, pair.public);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(public_key(dir.path(), "not a key").await.is_err());
    }

    #[test]
    fn key_files_are_private_and_removed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = {
            let file = KeyFile::write(dir.path(), "secret").unwrap();
            let mode = std::fs::metadata(file.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            assert_eq!(std::fs::read_to_string(file.path()).unwrap(), "secret\n");
            file.path().to_owned()
        };
        assert!(!path.exists());
    }

    #[test]
    fn keys_a_killed_process_left_are_removed_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(remove_leftovers(&dir.path().join("missing")).unwrap(), 0);
        // A drop that never ran, as after a kill.
        std::mem::forget(KeyFile::write(dir.path(), "secret").unwrap());
        let folder = dir.path().join("ssh-keygen-00");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("key"), "secret").unwrap();
        std::fs::write(dir.path().join("bundle-1.bundle"), "keep").unwrap();

        assert_eq!(remove_leftovers(dir.path()).unwrap(), 2);
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["bundle-1.bundle"]);
    }

    #[test]
    fn host_file_names_stay_inside_the_folder() {
        let dir = Path::new("/k");
        assert_eq!(
            known_hosts_file(dir, "Git.Example"),
            dir.join("git.example")
        );
        assert_eq!(known_hosts_file(dir, "h:8080"), dir.join("h%3A8080"));
        assert_eq!(known_hosts_file(dir, ".."), dir.join("%2E."));
        assert_eq!(known_hosts_file(dir, "a/b"), dir.join("a%2Fb"));
        assert_eq!(known_hosts_file(dir, ""), dir.join("%"));
    }

    #[tokio::test]
    async fn forgetting_one_host_keeps_the_keys_of_every_other() {
        let dir = tempfile::tempdir().unwrap();
        let git = known_hosts_file(dir.path(), "git.example");
        let other = known_hosts_file(dir.path(), "other.example");
        assert!(known_keys(&git).await.unwrap().is_empty());
        assert_eq!(forget(&git).await.unwrap(), 0);

        std::fs::write(
            &git,
            "git.example ssh-ed25519 AAAA1\n\n[git.example]:2222 ssh-ed25519 AAAA2\n",
        )
        .unwrap();
        std::fs::write(&other, "other.example ssh-ed25519 AAAA3\n").unwrap();
        assert_eq!(
            known_keys(&git).await.unwrap(),
            [
                "git.example ssh-ed25519 AAAA1",
                "[git.example]:2222 ssh-ed25519 AAAA2"
            ]
        );
        assert_eq!(forget(&git).await.unwrap(), 2);
        assert!(!git.exists());
        // A fetch that stores the key of another host meanwhile writes into
        // a file the removal never reads or replaces.
        assert_eq!(
            known_keys(&other).await.unwrap(),
            ["other.example ssh-ed25519 AAAA3"]
        );
    }

    #[test]
    fn the_single_file_of_earlier_versions_is_split_by_host() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("known_hosts");
        let dir = root.path().join("known_hosts.d");
        assert_eq!(split_legacy(&legacy, &dir).unwrap(), 0);
        std::fs::write(
            &legacy,
            "git.example ssh-ed25519 A1\n[GIT.example]:2222 ssh-ed25519 A2\n\
             # comment\ngist.github.com ssh-ed25519 G1\ngithub.com ssh-ed25519 G2\n\
             |1|hashed= ssh-ed25519 H\n",
        )
        .unwrap();
        // A run that stopped half way had written one line already.
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            known_hosts_file(&dir, "git.example"),
            "git.example ssh-ed25519 A1\n",
        )
        .unwrap();

        assert_eq!(split_legacy(&legacy, &dir).unwrap(), 3);
        assert!(!legacy.exists());
        let read = |host: &str| std::fs::read_to_string(known_hosts_file(&dir, host)).unwrap();
        assert_eq!(
            read("git.example"),
            "git.example ssh-ed25519 A1\n[GIT.example]:2222 ssh-ed25519 A2\n"
        );
        // A gist uses the settings of github.com, so its key goes there.
        assert_eq!(
            read("github.com"),
            "gist.github.com ssh-ed25519 G1\ngithub.com ssh-ed25519 G2\n"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
    }
}
