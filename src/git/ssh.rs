//! SSH keys of hosts and the stored host keys, handled with `ssh-keygen`.

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

/// Whether a `known_hosts` line belongs to `host` on any port. The file is
/// read directly, because `ssh-keygen -F` finds a host only on one port.
/// Fetches write plain, unhashed names (`host` or `[host]:port`), since
/// they run without the system SSH configuration.
fn is_entry_of(line: &str, host: &str) -> bool {
    let line = line.trim();
    if line.starts_with('#') {
        return false;
    }
    let Some(names) = line.split_whitespace().next() else {
        return false;
    };
    names.split(',').any(|name| {
        name.eq_ignore_ascii_case(host)
            || name
                .strip_prefix('[')
                .and_then(|rest| rest.split_once("]:"))
                .is_some_and(|(name, _port)| name.eq_ignore_ascii_case(host))
    })
}

async fn read_known_hosts(known_hosts: &Path) -> Result<String, SshError> {
    match tokio::fs::read_to_string(known_hosts).await {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(err.into()),
    }
}

/// The stored keys of `host` in `known_hosts`, as whole lines.
pub async fn known_keys(known_hosts: &Path, host: &str) -> Result<Vec<String>, SshError> {
    let text = read_known_hosts(known_hosts).await?;
    Ok(text
        .lines()
        .filter(|line| is_entry_of(line, host))
        .map(|line| line.trim().to_owned())
        .collect())
}

/// Removes the stored keys of `host`, so the next connection stores the
/// key the server shows then. Gives the number of removed lines.
pub async fn forget(known_hosts: &Path, host: &str) -> Result<usize, SshError> {
    let text = read_known_hosts(known_hosts).await?;
    let mut kept = String::with_capacity(text.len());
    let mut removed = 0;
    for line in text.lines() {
        if is_entry_of(line, host) {
            removed += 1;
        } else {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    if removed == 0 {
        return Ok(0);
    }
    // A rename replaces the file in one step, so a fetch that reads it at
    // the same time sees the old or the new list, never half of one.
    let next = known_hosts.with_extension(format!("new-{}", random_name()?));
    tokio::fs::write(&next, kept).await?;
    tokio::fs::rename(&next, known_hosts).await?;
    Ok(removed)
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
    fn known_host_lines_match_the_host_on_every_port() {
        assert!(is_entry_of("git.example ssh-ed25519 AAAA", "git.example"));
        assert!(is_entry_of(
            "[git.example]:2222 ssh-ed25519 AAAA",
            "git.example"
        ));
        assert!(is_entry_of(
            "other,GIT.example ssh-ed25519 AAAA",
            "git.example"
        ));
        assert!(!is_entry_of(
            "git.example.org ssh-ed25519 AAAA",
            "git.example"
        ));
        assert!(!is_entry_of(
            "[git.example.org]:22 ssh-ed25519 AAAA",
            "git.example"
        ));
        assert!(!is_entry_of(
            "# git.example ssh-ed25519 AAAA",
            "git.example"
        ));
        assert!(!is_entry_of("   ", "git.example"));
    }

    #[tokio::test]
    async fn known_host_keys_are_listed_and_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let known_hosts = dir.path().join("known_hosts");
        assert!(
            known_keys(&known_hosts, "git.example")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(forget(&known_hosts, "git.example").await.unwrap(), 0);

        std::fs::write(
            &known_hosts,
            "git.example ssh-ed25519 AAAA1\n[git.example]:2222 ssh-ed25519 AAAA2\nother.example ssh-ed25519 AAAA3\n",
        )
        .unwrap();
        let keys = known_keys(&known_hosts, "git.example").await.unwrap();
        assert_eq!(
            keys,
            [
                "git.example ssh-ed25519 AAAA1",
                "[git.example]:2222 ssh-ed25519 AAAA2"
            ]
        );

        assert_eq!(forget(&known_hosts, "git.example").await.unwrap(), 2);
        assert!(
            known_keys(&known_hosts, "git.example")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(&known_hosts).unwrap(),
            "other.example ssh-ed25519 AAAA3\n"
        );
        // Only the list itself stays in the folder.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
