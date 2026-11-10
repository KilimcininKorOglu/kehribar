//! Validation of repository URLs and the archive path derived from them.

use std::path::{Path, PathBuf};

use thiserror::Error;
use url::Url;

/// A validated repository URL split into the parts the archive path uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoLocation {
    /// The URL to fetch, without a trailing slash.
    pub url: String,
    /// Host name, with `:port` when the URL names one.
    pub host: String,
    /// Every path segment before the name, joined with `/` (GitLab groups
    /// can nest).
    pub owner: String,
    /// Last path segment without `.git`.
    pub name: String,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum UrlError {
    #[error("not a valid URL")]
    Invalid,
    #[error("only http and https URLs are supported")]
    Scheme,
    #[error("credentials belong in the token fields, not in the URL")]
    Credentials,
    #[error("the URL must not have a query or a fragment")]
    QueryOrFragment,
    #[error("the URL path must be owner/name with letters, digits, '.', '-', '_' or '~'")]
    Path,
}

impl UrlError {
    /// Message key of the user interface catalog.
    pub fn message_key(self) -> &'static str {
        match self {
            Self::Invalid => "url.invalid",
            Self::Scheme => "url.scheme",
            Self::Credentials => "url.credentials",
            Self::QueryOrFragment => "url.query",
            Self::Path => "url.path",
        }
    }
}

pub fn parse(input: &str) -> Result<RepoLocation, UrlError> {
    let url = Url::parse(input.trim()).map_err(|_| UrlError::Invalid)?;
    check_parts(&url)?;
    let host = host_with_port(&url).ok_or(UrlError::Invalid)?;
    let segments = path_segments(&url)?;
    let (last, owners) = segments.split_last().ok_or(UrlError::Path)?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    if owners.is_empty() || !is_safe_segment(name) {
        return Err(UrlError::Path);
    }
    Ok(RepoLocation {
        url: format!("{}://{host}/{}", url.scheme(), segments.join("/")),
        host,
        owner: owners.join("/"),
        name: name.to_owned(),
    })
}

/// The lowercase host name, with `:port` when the URL names a port that is
/// not the default of its scheme.
fn host_with_port(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

/// Validates a host name as [`RepoLocation::host`] stores it: a name with an
/// optional port, without scheme, credentials or path. Returns the
/// normalised (lowercase) form.
pub fn parse_host(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() || input.contains(['/', '\\', '@', '?', '#', ' ']) {
        return None;
    }
    let url = Url::parse(&format!("https://{input}/")).ok()?;
    let host = host_with_port(&url)?;
    // The parser rewrites what it does not keep as given (a default port,
    // an internationalised name), so only an exact match is accepted.
    host.eq_ignore_ascii_case(input).then_some(host)
}

/// Validates an optional API base URL. A blank field gives `None`; a
/// trailing slash is removed.
pub fn parse_api_url(input: &str) -> Result<Option<String>, UrlError> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    let url = Url::parse(input).map_err(|_| UrlError::Invalid)?;
    check_parts(&url)?;
    Ok(Some(url.as_str().trim_end_matches('/').to_owned()))
}

/// Rejects every URL part other than scheme, host, port and path.
fn check_parts(url: &Url) -> Result<(), UrlError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(UrlError::Scheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(UrlError::Credentials);
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(UrlError::QueryOrFragment);
    }
    Ok(())
}

fn path_segments(url: &Url) -> Result<Vec<&str>, UrlError> {
    let segments: Vec<&str> = url
        .path_segments()
        .ok_or(UrlError::Path)?
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.iter().all(|segment| is_safe_segment(segment)) {
        Ok(segments)
    } else {
        Err(UrlError::Path)
    }
}

/// A segment becomes a directory name, so it must not climb out of the
/// archive or carry encoded characters.
fn is_safe_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
}

/// `<data dir>/repos/<host>/<owner>/<name>.git`. The parts come from
/// [`parse`], so they hold only safe characters; the `:` of a port and the
/// brackets of an IPv6 host become `_`.
pub fn archive_dir(data_dir: &Path, host: &str, owner: &str, name: &str) -> PathBuf {
    let host_dir: String = host
        .chars()
        .map(|c| if matches!(c, ':' | '[' | ']') { '_' } else { c })
        .collect();
    let mut path = data_dir.join("repos").join(host_dir);
    for part in owner.split('/') {
        path.push(part);
    }
    path.push(format!("{name}.git"));
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(url: &str, host: &str, owner: &str, name: &str) -> RepoLocation {
        RepoLocation {
            url: url.to_owned(),
            host: host.to_owned(),
            owner: owner.to_owned(),
            name: name.to_owned(),
        }
    }

    #[test]
    fn parses_common_hosting_urls() {
        assert_eq!(
            parse(" https://github.com/rust-lang/rust.git/ ").unwrap(),
            location(
                "https://github.com/rust-lang/rust.git",
                "github.com",
                "rust-lang",
                "rust"
            )
        );
        assert_eq!(
            parse("https://GitLab.com/group/sub.group/project").unwrap(),
            location(
                "https://gitlab.com/group/sub.group/project",
                "gitlab.com",
                "group/sub.group",
                "project"
            )
        );
        assert_eq!(
            parse("http://gitea.local:3000/team/.github").unwrap(),
            location(
                "http://gitea.local:3000/team/.github",
                "gitea.local:3000",
                "team",
                ".github"
            )
        );
    }

    #[test]
    fn rejects_unsafe_or_unsupported_urls() {
        let cases = [
            ("not a url", UrlError::Invalid),
            ("git@github.com:a/b.git", UrlError::Invalid),
            ("ssh://github.com/a/b", UrlError::Scheme),
            ("file:///etc/passwd", UrlError::Scheme),
            ("https://user:token@github.com/a/b", UrlError::Credentials),
            ("https://github.com/a/b?x=1", UrlError::QueryOrFragment),
            ("https://github.com/a/b#readme", UrlError::QueryOrFragment),
            ("https://github.com/onlyname", UrlError::Path),
            ("https://github.com/a/%2e%2e", UrlError::Path),
            ("https://github.com/a/b%20c", UrlError::Path),
            ("https://github.com/a/.git", UrlError::Path),
        ];
        for (input, expected) in cases {
            assert_eq!(parse(input), Err(expected), "{input}");
        }
    }

    #[test]
    fn dot_segments_are_resolved_before_validation() {
        // The URL parser removes "..", so the result stays inside the host.
        let parsed = parse("https://github.com/a/../b/c").unwrap();
        assert_eq!((parsed.owner.as_str(), parsed.name.as_str()), ("b", "c"));
    }

    #[test]
    fn archive_dir_nests_owner_groups_and_escapes_ports() {
        let base = Path::new("/data");
        assert_eq!(
            archive_dir(base, "gitea.local:3000", "group/sub", "proj"),
            PathBuf::from("/data/repos/gitea.local_3000/group/sub/proj.git")
        );
        assert_eq!(
            archive_dir(base, "[::1]:8080", "o", "n"),
            PathBuf::from("/data/repos/___1__8080/o/n.git")
        );
    }

    #[test]
    fn host_names_are_validated_and_lowercased() {
        assert_eq!(parse_host(" GitHub.com ").as_deref(), Some("github.com"));
        assert_eq!(
            parse_host("gitea.local:3000").as_deref(),
            Some("gitea.local:3000")
        );
        assert_eq!(parse_host("[::1]:8080").as_deref(), Some("[::1]:8080"));
        for bad in [
            "",
            "https://github.com",
            "github.com/owner",
            "user@github.com",
            "github.com:443",
            "git hub.com",
            "github.com:99999",
        ] {
            assert_eq!(parse_host(bad), None, "{bad}");
        }
    }

    #[test]
    fn api_urls_are_optional_and_lose_the_trailing_slash() {
        assert_eq!(parse_api_url("  "), Ok(None));
        assert_eq!(
            parse_api_url("https://gitlab.example.com/api/v4/").unwrap(),
            Some("https://gitlab.example.com/api/v4".to_owned())
        );
        assert_eq!(parse_api_url("ftp://x/api"), Err(UrlError::Scheme));
        assert_eq!(
            parse_api_url("https://t:k@x/api"),
            Err(UrlError::Credentials)
        );
    }

    #[test]
    fn every_error_has_a_message_key() {
        for err in [
            UrlError::Invalid,
            UrlError::Scheme,
            UrlError::Credentials,
            UrlError::QueryOrFragment,
            UrlError::Path,
        ] {
            assert!(err.message_key().starts_with("url."));
        }
    }
}
