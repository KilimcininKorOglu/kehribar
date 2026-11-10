//! Runs `git` without a shell, with a timeout, an isolated configuration and
//! credentials that never appear in the command line.

use std::ffi::OsStr;
use std::fmt;
use std::path::Path;
use std::process::{ExitStatus, Output, Stdio};
use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdout, Command};

/// Environment variables the credential helper reads. Their values reach
/// only the `git` child process, never its arguments.
const ENV_USERNAME: &str = "GIT_ARCHIVE_CRED_USERNAME";
const ENV_PASSWORD: &str = "GIT_ARCHIVE_CRED_PASSWORD";

/// Answers git's `get` request with the two variables above.
const CREDENTIAL_HELPER: &str = "credential.helper=!f() { test \"$1\" = get || exit 0; \
     printf 'username=%s\\npassword=%s\\n' \"$GIT_ARCHIVE_CRED_USERNAME\" \"$GIT_ARCHIVE_CRED_PASSWORD\"; }; f";

/// Longest stderr excerpt kept in an error message.
const MAX_MESSAGE_LEN: usize = 600;

#[derive(Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitErrorKind {
    /// The repository does not exist, or the server hides it.
    NotFound,
    /// The server refused the credentials.
    AuthFailed,
    /// DNS, connection or TLS failure.
    Network,
    Timeout,
    /// `git` could not be started.
    Spawn,
    Other,
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct GitError {
    pub kind: GitErrorKind,
    pub exit_code: Option<i32>,
    pub message: String,
}

impl GitError {
    /// An error raised before or around git, which has no exit code.
    pub(crate) fn local(kind: GitErrorKind, message: String) -> Self {
        Self {
            kind,
            exit_code: None,
            message,
        }
    }
}

/// Runs `git` commands with one timeout.
#[derive(Debug, Clone)]
pub struct Git {
    timeout: Duration,
}

impl Git {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    /// Runs `git <args>` in `dir` and returns its standard output.
    pub async fn run<I, S>(
        &self,
        dir: Option<&Path>,
        args: I,
        credentials: Option<&Credentials>,
    ) -> Result<Vec<u8>, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let name = args
            .first()
            .map(|a| a.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut command = self.command(dir, credentials);
        command.args(&args);
        let output = self.wait(command, &name).await?;
        if output.status.success() {
            return Ok(output.stdout);
        }
        Err(failure(&name, &output.stderr, output.status, credentials))
    }

    /// Runs `git <args>` in `dir` and keeps at most `limit` bytes of its
    /// standard output. The flag is `true` when the output was cut; the
    /// process is stopped then, so a huge diff or file never fills memory.
    pub async fn run_capped(
        &self,
        dir: &Path,
        args: &[&str],
        limit: usize,
    ) -> Result<(Vec<u8>, bool), GitError> {
        let name = args.first().copied().unwrap_or_default().to_owned();
        let mut command = self.command(Some(dir), None);
        command.args(args);
        let child = command.spawn().map_err(spawn_error)?;
        match tokio::time::timeout(self.timeout, read_capped(child, limit)).await {
            Ok(Ok(capped)) if capped.cut || capped.status.success() => {
                Ok((capped.stdout, capped.cut))
            }
            Ok(Ok(capped)) => Err(failure(&name, &capped.stderr, capped.status, None)),
            Ok(Err(err)) => Err(io_error(&name, &err)),
            Err(_) => Err(self.timeout_error(&name)),
        }
    }

    /// Starts `git <args>` in `dir` and returns its standard output to be
    /// streamed. No timeout applies, as a download takes as long as the
    /// client reads; when the reader is dropped, git ends on the closed pipe.
    /// Call it only after a check that the command will succeed.
    pub fn spawn_stdout(&self, dir: &Path, args: &[&str]) -> Result<ChildStdout, GitError> {
        let mut command = self.command(Some(dir), None);
        command.args(args).stderr(Stdio::null());
        let mut child = command.spawn().map_err(spawn_error)?;
        let stdout = child.stdout.take().ok_or_else(|| {
            GitError::local(GitErrorKind::Spawn, "git has no standard output".to_owned())
        })?;
        // The task owns the child, so kill_on_drop does not stop it early,
        // and reaps it when it exits.
        tokio::spawn(async move {
            if let Err(err) = child.wait().await {
                tracing::warn!(%err, "streamed git process failed");
            }
        });
        Ok(stdout)
    }

    fn command(&self, dir: Option<&Path>, credentials: Option<&Credentials>) -> Command {
        let mut command = Command::new("git");
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("LC_ALL", "C")
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        // An empty helper value clears every inherited helper first.
        command.args(["-c", "credential.helper="]);
        if let Some(credentials) = credentials {
            command
                .args(["-c", CREDENTIAL_HELPER])
                .env(ENV_USERNAME, &credentials.username)
                .env(ENV_PASSWORD, &credentials.password);
        }
        command
    }

    async fn wait(&self, mut command: Command, name: &str) -> Result<Output, GitError> {
        let child = command.spawn().map_err(spawn_error)?;
        // kill_on_drop stops the process when the timeout drops the future.
        match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(err)) => Err(io_error(name, &err)),
            Err(_) => Err(self.timeout_error(name)),
        }
    }

    fn timeout_error(&self, name: &str) -> GitError {
        let message = format!("git {name}: timed out after {}s", self.timeout.as_secs());
        GitError::local(GitErrorKind::Timeout, message)
    }
}

/// Output of [`Git::run_capped`] before it is checked.
struct Capped {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    cut: bool,
    status: ExitStatus,
}

async fn read_capped(mut child: Child, limit: usize) -> std::io::Result<Capped> {
    // stderr is read on its own task, so a full stderr pipe never blocks
    // git while stdout is read here.
    let stderr = child.stderr.take();
    let read_err = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut err) = stderr {
            err.read_to_end(&mut buf).await?;
        }
        Ok::<_, std::io::Error>(buf)
    });
    let mut stdout = Vec::new();
    if let Some(out) = child.stdout.take() {
        let max = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
        out.take(max).read_to_end(&mut stdout).await?;
    }
    let cut = stdout.len() > limit;
    if cut {
        // git would wait for a reader forever; stop it.
        stdout.truncate(limit);
        child.start_kill()?;
    }
    let stderr = read_err.await.map_err(std::io::Error::other)??;
    let status = child.wait().await?;
    Ok(Capped {
        stdout,
        stderr,
        cut,
        status,
    })
}

fn spawn_error(err: std::io::Error) -> GitError {
    GitError::local(GitErrorKind::Spawn, format!("cannot start git: {err}"))
}

fn io_error(name: &str, err: &std::io::Error) -> GitError {
    GitError::local(GitErrorKind::Other, format!("git {name}: {err}"))
}

fn failure(
    name: &str,
    stderr: &[u8],
    status: ExitStatus,
    credentials: Option<&Credentials>,
) -> GitError {
    let stderr = String::from_utf8_lossy(stderr);
    GitError {
        kind: classify(&stderr, credentials.is_some()),
        exit_code: status.code(),
        message: format!("git {name}: {}", excerpt(&stderr)),
    }
}

/// Error messages per kind, checked in order. Authentication and missing
/// repositories come before network errors, because git prefixes an HTTP
/// 401 or 404 with "unable to access" too.
const PATTERNS: &[(&str, GitErrorKind)] = &[
    ("authentication failed", GitErrorKind::AuthFailed),
    ("returned error: 401", GitErrorKind::AuthFailed),
    ("returned error: 403", GitErrorKind::AuthFailed),
    ("could not read username", GitErrorKind::AuthFailed),
    ("could not read password", GitErrorKind::AuthFailed),
    ("terminal prompts disabled", GitErrorKind::AuthFailed),
    ("permission denied", GitErrorKind::AuthFailed),
    ("repository not found", GitErrorKind::NotFound),
    ("returned error: 404", GitErrorKind::NotFound),
    ("returned error: 410", GitErrorKind::NotFound),
    (
        "does not appear to be a git repository",
        GitErrorKind::NotFound,
    ),
    ("' not found", GitErrorKind::NotFound),
    ("could not resolve host", GitErrorKind::Network),
    ("failed to connect", GitErrorKind::Network),
    ("connection refused", GitErrorKind::Network),
    ("connection timed out", GitErrorKind::Network),
    ("operation timed out", GitErrorKind::Network),
    ("ssl", GitErrorKind::Network),
    ("unable to access", GitErrorKind::Network),
];

fn classify(stderr: &str, had_credentials: bool) -> GitErrorKind {
    let lower = stderr.to_ascii_lowercase();
    let kind = PATTERNS
        .iter()
        .find(|(needle, _)| lower.contains(needle))
        .map_or(GitErrorKind::Other, |(_, kind)| *kind);
    // GitHub answers 401 instead of 404 for a missing public repository, so
    // without credentials an authentication request means "not available".
    if kind == GitErrorKind::AuthFailed && !had_credentials {
        return GitErrorKind::NotFound;
    }
    kind
}

/// The last lines of stderr, which name the cause, shortened to a safe length.
fn excerpt(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    // git names the cause on `fatal:`/`error:`/`remote:` lines and ends with
    // generic advice, so those lines come first when there are any.
    let tagged: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| {
            ["fatal:", "error:", "remote:"]
                .iter()
                .any(|p| l.starts_with(p))
        })
        .collect();
    let chosen = if tagged.is_empty() { &lines } else { &tagged };
    let text = chosen[..chosen.len().min(3)].join(" | ");
    if text.len() <= MAX_MESSAGE_LEN {
        return text;
    }
    let mut end = MAX_MESSAGE_LEN;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git() -> Git {
        Git::new(Duration::from_secs(30))
    }

    #[test]
    fn classifies_common_git_failures() {
        let cases = [
            (
                "remote: Repository not found.\nfatal: repository 'https://github.com/a/b/' not found",
                true,
                GitErrorKind::NotFound,
            ),
            (
                "fatal: unable to access 'https://x/': The requested URL returned error: 403",
                true,
                GitErrorKind::AuthFailed,
            ),
            (
                "fatal: Authentication failed for 'https://gitlab.com/a/b.git/'",
                true,
                GitErrorKind::AuthFailed,
            ),
            (
                "fatal: could not read Username for 'https://github.com': terminal prompts disabled",
                true,
                GitErrorKind::AuthFailed,
            ),
            (
                "fatal: could not read Username for 'https://github.com': terminal prompts disabled",
                false,
                GitErrorKind::NotFound,
            ),
            (
                "fatal: unable to access 'https://nope.invalid/': Could not resolve host: nope.invalid",
                false,
                GitErrorKind::Network,
            ),
            (
                "fatal: '/tmp/missing' does not appear to be a git repository",
                false,
                GitErrorKind::NotFound,
            ),
            ("fatal: bad object", false, GitErrorKind::Other),
        ];
        for (stderr, creds, expected) in cases {
            assert_eq!(classify(stderr, creds), expected, "{stderr}");
        }
    }

    #[test]
    fn excerpt_keeps_the_cause_and_limits_length() {
        let stderr = "fatal: '/x' does not appear to be a git repository\n\
                      fatal: Could not read from remote repository.\n\n\
                      Please make sure you have the correct access rights\n\
                      and the repository exists.\n";
        assert_eq!(
            excerpt(stderr),
            "fatal: '/x' does not appear to be a git repository | \
             fatal: Could not read from remote repository."
        );
        assert_eq!(excerpt("a\n\nb\nc\nd\n"), "a | b | c");
        let long = "ç".repeat(MAX_MESSAGE_LEN);
        let cut = excerpt(&long);
        assert!(cut.ends_with('…'));
        assert!(cut.len() <= MAX_MESSAGE_LEN + '…'.len_utf8());
    }

    #[test]
    fn credentials_debug_hides_the_password() {
        let creds = Credentials {
            username: "bot".into(),
            password: "hunter2".into(),
        };
        let shown = format!("{creds:?}");
        assert!(shown.contains("bot") && !shown.contains("hunter2"));
    }

    #[tokio::test]
    async fn returns_stdout_on_success() {
        let out = git().run(None, ["--version"], None).await.unwrap();
        assert!(String::from_utf8(out).unwrap().starts_with("git version"));
    }

    #[tokio::test]
    async fn failure_carries_kind_exit_code_and_message() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.git");
        let err = git()
            .run(None, ["ls-remote".as_ref(), missing.as_os_str()], None)
            .await
            .unwrap_err();
        assert_eq!(err.kind, GitErrorKind::NotFound);
        assert_eq!(err.exit_code, Some(128));
        assert!(
            err.message.starts_with("git ls-remote: "),
            "{}",
            err.message
        );
    }

    #[tokio::test]
    async fn timeout_stops_a_slow_command() {
        let err = Git::new(Duration::ZERO)
            .run(None, ["--version"], None)
            .await
            .unwrap_err();
        assert_eq!(err.kind, GitErrorKind::Timeout);
    }

    #[tokio::test]
    async fn capped_output_is_cut_and_the_process_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        git()
            .run(Some(path), ["init", "--quiet"], None)
            .await
            .unwrap();
        // `git help -a` writes several kilobytes to stdout.
        let (full, cut) = git()
            .run_capped(path, &["help", "-a"], usize::MAX >> 1)
            .await
            .unwrap();
        assert!(!cut && full.len() > 1000);
        let (part, cut) = git().run_capped(path, &["help", "-a"], 100).await.unwrap();
        assert!(cut);
        assert_eq!(part, full[..100]);

        let err = git()
            .run_capped(path, &["cat-file", "-t", "nope"], 100)
            .await
            .unwrap_err();
        assert_eq!(err.exit_code, Some(128));
        assert!(err.message.starts_with("git cat-file: "), "{}", err.message);
    }

    #[test]
    fn token_reaches_the_environment_but_never_the_arguments() {
        let creds = Credentials {
            username: "bot".into(),
            password: "s3cret-token".into(),
        };
        let mut command = git().command(None, Some(&creds));
        command.args(["fetch", "https://example.com/o/n"]);
        let std = command.as_std();
        let args: Vec<String> = std
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().all(|a| !a.contains("s3cret-token")), "{args:?}");
        let password = std
            .get_envs()
            .find(|(name, _)| *name == ENV_PASSWORD)
            .and_then(|(_, value)| value);
        assert_eq!(password, Some(std::ffi::OsStr::new("s3cret-token")));
    }

    #[tokio::test]
    async fn credential_helper_answers_from_the_environment() {
        let creds = Credentials {
            username: "bot".into(),
            password: "s3cret".into(),
        };
        let out = Git::new(Duration::from_secs(30))
            .command(None, Some(&creds))
            .args(["credential", "fill"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = out;
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(b"protocol=https\nhost=example.com\n\n")
            .await
            .unwrap();
        drop(stdin);
        let output = child.wait_with_output().await.unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("username=bot"), "{text}");
        assert!(text.contains("password=s3cret"), "{text}");
    }
}
