//! Runs `git http-backend` as a CGI program for one smart HTTP request.
//!
//! The backend handles the protocol details (gzip request bodies, protocol
//! version 2, the ref advertisement). Pushing is turned off in its
//! configuration, and the caller passes only upload-pack requests.

use std::path::Path;
use std::process::Stdio;

use futures_util::{Stream, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdout, Command};

use super::command::{GitError, GitErrorKind};

/// Lines of CGI headers read before the backend counts as broken.
const MAX_HEADER_LINES: usize = 64;

/// The parts of the HTTP request the backend reads.
#[derive(Debug, Clone, Default)]
pub struct CgiRequest {
    pub method: String,
    /// `/<name>.git/info/refs` or `/<name>.git/git-upload-pack`.
    pub path_info: String,
    pub query: String,
    pub content_type: String,
    pub content_encoding: String,
    /// The `Git-Protocol` header, passed on only when it is well formed.
    pub git_protocol: String,
}

/// The start of the backend's answer; `body` streams the rest.
pub struct CgiResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: BufReader<ChildStdout>,
}

/// `version=2` and the like; anything else is dropped.
fn valid_protocol(value: &str) -> bool {
    value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'=' | b':' | b'.'))
}

/// Runs the backend with `root` as its project root. The request body is
/// written to the backend while its answer is read.
pub async fn run<S, B, E>(
    root: &Path,
    request: &CgiRequest,
    body: S,
) -> Result<CgiResponse, GitError>
where
    S: Stream<Item = Result<B, E>> + Send + Unpin + 'static,
    B: AsRef<[u8]> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let mut command = Command::new("git");
    command
        .arg("http-backend")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.receivepack")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", &request.method)
        .env("PATH_INFO", &request.path_info)
        .env("QUERY_STRING", &request.query)
        .env("CONTENT_TYPE", &request.content_type)
        .env("HTTP_CONTENT_ENCODING", &request.content_encoding)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if valid_protocol(&request.git_protocol) {
        command.env("GIT_PROTOCOL", &request.git_protocol);
    }
    let mut child = command.spawn().map_err(|err| spawn_failed(&err))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| broken("no standard input"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| broken("no standard output"))?;
    tokio::spawn(feed(stdin, body));
    // The task owns the child, so the backend runs while the body streams,
    // and it is reaped when it ends.
    tokio::spawn(async move {
        if let Err(err) = child.wait().await {
            tracing::warn!(%err, "git http-backend failed");
        }
    });
    let mut reader = BufReader::new(stdout);
    let (status, headers) = read_headers(&mut reader).await?;
    Ok(CgiResponse {
        status,
        headers,
        body: reader,
    })
}

/// Copies the request body to the backend. A client that stops sending
/// ends the request; the backend then sees a short body and fails.
async fn feed<S, B, E>(mut stdin: tokio::process::ChildStdin, mut body: S)
where
    S: Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    while let Some(chunk) = body.next().await {
        let written = match chunk {
            Ok(bytes) => stdin.write_all(bytes.as_ref()).await,
            Err(err) => {
                tracing::warn!(%err, "request body to git http-backend failed");
                return;
            }
        };
        if let Err(err) = written {
            tracing::warn!(%err, "git http-backend stopped reading the request");
            return;
        }
    }
}

async fn read_headers(
    reader: &mut BufReader<ChildStdout>,
) -> Result<(u16, Vec<(String, String)>), GitError> {
    let mut status = 200;
    let mut headers = Vec::new();
    for _ in 0..MAX_HEADER_LINES {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|err| broken(&err.to_string()))?;
        let line = line.trim_end();
        if read == 0 || line.is_empty() {
            return Ok((status, headers));
        }
        match parse_header(line) {
            Some(("status", value)) => status = parse_status(value),
            Some((name, value)) => headers.push((name.to_owned(), value.to_owned())),
            None => return Err(broken("malformed CGI header")),
        }
    }
    Err(broken("too many CGI headers"))
}

/// `Name: value`, with the name in lower case for `Status`.
fn parse_header(line: &str) -> Option<(&str, &str)> {
    let (name, value) = line.split_once(':')?;
    let name = if name.eq_ignore_ascii_case("status") {
        "status"
    } else {
        name
    };
    Some((name, value.trim()))
}

/// `404 Not Found` gives 404; a broken value gives 502.
fn parse_status(value: &str) -> u16 {
    value
        .split_whitespace()
        .next()
        .and_then(|code| code.parse().ok())
        .filter(|code| (100..600).contains(code))
        .unwrap_or(502)
}

fn spawn_failed(err: &std::io::Error) -> GitError {
    GitError::local(
        GitErrorKind::Spawn,
        format!("cannot start git http-backend: {err}"),
    )
}

fn broken(reason: &str) -> GitError {
    GitError::local(GitErrorKind::Other, format!("git http-backend: {reason}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_and_status_are_parsed() {
        assert_eq!(
            parse_header("Status: 404 Not Found"),
            Some(("status", "404 Not Found"))
        );
        assert_eq!(
            parse_header("Content-Type: application/x-git-upload-pack-result"),
            Some(("Content-Type", "application/x-git-upload-pack-result"))
        );
        assert_eq!(parse_header("garbage"), None);
        assert_eq!(parse_status("403 Forbidden"), 403);
        assert_eq!(parse_status("abc"), 502);
        assert_eq!(parse_status("99"), 502);
    }

    #[test]
    fn only_well_formed_protocol_values_pass() {
        assert!(valid_protocol("version=2"));
        assert!(valid_protocol(""));
        assert!(!valid_protocol("version=2\nX=1"));
        assert!(!valid_protocol("a b"));
    }
}
