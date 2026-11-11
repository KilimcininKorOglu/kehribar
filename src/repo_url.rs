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
    #[error("only http, https and ssh URLs are supported")]
    Scheme,
    #[error("only http and https API URLs are supported")]
    ApiScheme,
    #[error("credentials belong in the token fields, not in the URL")]
    Credentials,
    #[error("the URL must not have a query or a fragment")]
    QueryOrFragment,
    #[error("the URL path must be owner/name with letters, digits, '.', '-', '_' or '~'")]
    Path,
    #[error("a gist URL must name its owner: https://gist.github.com/<user>/<id>")]
    GistOwner,
}

impl UrlError {
    /// Message key of the user interface catalog.
    pub fn message_key(self) -> &'static str {
        match self {
            Self::Invalid => "url.invalid",
            Self::Scheme => "url.scheme",
            Self::ApiScheme => "url.api_scheme",
            Self::Credentials => "url.credentials",
            Self::QueryOrFragment => "url.query",
            Self::Path => "url.path",
            Self::GistOwner => "url.gist_owner",
        }
    }
}

/// The host of GitHub gists. A gist is fetched from
/// `https://gist.github.com/<id>.git` and archived as `<user>/<id>`.
pub const GIST_HOST: &str = "gist.github.com";

/// The path segment before the repository name in Azure DevOps URLs:
/// `https://dev.azure.com/<org>/<project>/_git/<name>`.
const AZURE_GIT_SEGMENT: &str = "_git";

/// The host whose settings and token a repository on `host` uses: gists
/// use the ones of github.com.
pub fn settings_host(host: &str) -> &str {
    if host == GIST_HOST {
        "github.com"
    } else {
        host
    }
}

/// Parses an `http`, `https` or `ssh` repository URL, or the SSH short
/// form `user@host:owner/name`. An SSH URL keeps its user and port for the
/// fetch, but its host is the bare name, so it uses the settings and the
/// archive path of that host. The short form is fetched as it was given,
/// because its path is relative to the home folder of the SSH user, which
/// an `ssh://` URL cannot say.
pub fn parse(input: &str) -> Result<RepoLocation, UrlError> {
    let input = input.trim();
    let url = Url::parse(&scp_to_ssh(input)).map_err(|_| UrlError::Invalid)?;
    check_parts(&url)?;
    let ssh = is_ssh_url(&url);
    let host = archive_host(&url, ssh).ok_or(UrlError::Invalid)?;
    let segments = path_segments(&url)?;
    if host == GIST_HOST && !ssh {
        return parse_gist(url.scheme(), &segments);
    }
    let (last, owners) = segments.split_last().ok_or(UrlError::Path)?;
    // Azure DevOps puts `_git` before the name; the archive path leaves it out.
    let owners = owners.strip_suffix(&[AZURE_GIT_SEGMENT]).unwrap_or(owners);
    let name = last.strip_suffix(".git").unwrap_or(last);
    if owners.is_empty() || !is_safe_segment(name) {
        return Err(UrlError::Path);
    }
    Ok(RepoLocation {
        url: fetch_url(input, &url, &host, &segments),
        host,
        owner: owners.join("/"),
        name: name.to_owned(),
    })
}

/// The host the repository is archived under: the bare name for SSH, the
/// name with a non-default port for HTTP(S).
fn archive_host(url: &Url, ssh: bool) -> Option<String> {
    if ssh {
        url.host_str().map(str::to_ascii_lowercase)
    } else {
        host_with_port(url)
    }
}

/// Whether a stored repository URL is fetched over SSH.
pub fn is_ssh(url: &str) -> bool {
    url.starts_with("ssh://") || is_scp(url)
}

fn is_ssh_url(url: &Url) -> bool {
    url.scheme() == "ssh"
}

/// The URL a fetch uses: the SSH short form as given, else the URL rebuilt
/// from its checked parts.
fn fetch_url(input: &str, url: &Url, host: &str, segments: &[&str]) -> String {
    if is_scp(input) {
        input.to_owned()
    } else {
        format!("{}/{}", fetch_origin(url, host), segments.join("/"))
    }
}

/// `scheme://host[:port]` for HTTP(S), `ssh://[user@]host[:port]` for SSH.
fn fetch_origin(url: &Url, host: &str) -> String {
    if !is_ssh_url(url) {
        return format!("{}://{host}", url.scheme());
    }
    let user = match url.username() {
        "" => String::new(),
        name => format!("{name}@"),
    };
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    format!("ssh://{user}{host}{port}")
}

/// Whether `input` has the scp-like form `user@host:path` Git accepts.
fn is_scp(input: &str) -> bool {
    !input.contains("://")
        && input
            .split_once(':')
            .is_some_and(|(user_host, _)| user_host.contains('@') && !user_host.contains('/'))
}

/// `user@host:path` as an `ssh://` URL, to check its parts. Anything else,
/// a URL with a scheme included, stays as it is.
fn scp_to_ssh(input: &str) -> String {
    match input.split_once(':') {
        Some((user_host, path)) if is_scp(input) => {
            format!("ssh://{user_host}/{}", path.trim_start_matches('/'))
        }
        _ => input.to_owned(),
    }
}

/// `https://gist.github.com/<user>/<id>`, the address of a gist page. The
/// bare clone URL `/<id>.git` names no owner, so it is refused.
fn parse_gist(scheme: &str, segments: &[&str]) -> Result<RepoLocation, UrlError> {
    let [user, id] = segments else {
        return Err(if segments.len() == 1 {
            UrlError::GistOwner
        } else {
            UrlError::Path
        });
    };
    let id = id.strip_suffix(".git").unwrap_or(id);
    if !is_safe_segment(id) {
        return Err(UrlError::Path);
    }
    Ok(RepoLocation {
        url: format!("{scheme}://{GIST_HOST}/{id}.git"),
        host: GIST_HOST.to_owned(),
        owner: (*user).to_owned(),
        name: id.to_owned(),
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
    if !matches!(url.scheme(), "http" | "https") {
        return Err(UrlError::ApiScheme);
    }
    check_parts(&url)?;
    Ok(Some(url.as_str().trim_end_matches('/').to_owned()))
}

/// Accepts any http(s) URL, query included, as webhook receivers need one
/// sometimes. Returns it without surrounding blanks and a trailing slash.
pub fn parse_http_url(input: &str) -> Option<String> {
    let input = input.trim();
    let url = Url::parse(input).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| input.trim_end_matches('/').to_owned())
}

/// Validates a user, organization or workspace name for a provider API.
/// `nested` allows a GitLab group path such as `group/sub`. Returns the
/// name without surrounding blanks and slashes.
pub fn parse_owner(input: &str, nested: bool) -> Option<String> {
    let name = input.trim().trim_matches('/');
    let valid = (nested || !name.contains('/')) && name.split('/').all(is_safe_segment);
    valid.then(|| name.to_owned())
}

/// Rejects every URL part other than scheme, host, port and path. An SSH
/// URL may name the user, which is not a secret.
fn check_parts(url: &Url) -> Result<(), UrlError> {
    if !matches!(url.scheme(), "http" | "https" | "ssh") {
        return Err(UrlError::Scheme);
    }
    let user_allowed = is_ssh_url(url) && url.username().bytes().all(is_name_byte);
    if (!url.username().is_empty() && !user_allowed) || url.password().is_some() {
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
    !segment.is_empty() && segment != "." && segment != ".." && segment.bytes().all(is_name_byte)
}

/// A byte that a path segment of a repository URL may hold.
pub fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
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
            ("github.com:a/b.git", UrlError::Scheme),
            ("git://github.com/a/b", UrlError::Scheme),
            ("file:///etc/passwd", UrlError::Scheme),
            ("ssh://git:secret@github.com/a/b", UrlError::Credentials),
            ("ssh://git@github.com/a/../../b", UrlError::Path),
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
    fn gist_pages_are_fetched_by_id_and_archived_under_the_owner() {
        let expected = location(
            "https://gist.github.com/aa5a315d61ae9438b18d.git",
            "gist.github.com",
            "octocat",
            "aa5a315d61ae9438b18d",
        );
        for input in [
            "https://gist.github.com/octocat/aa5a315d61ae9438b18d",
            "https://Gist.GitHub.com/octocat/aa5a315d61ae9438b18d.git/",
        ] {
            assert_eq!(parse(input).unwrap(), expected, "{input}");
        }
        assert_eq!(
            parse("https://gist.github.com/aa5a315d61ae9438b18d.git"),
            Err(UrlError::GistOwner)
        );
        assert_eq!(parse("https://gist.github.com/a/b/c"), Err(UrlError::Path));
        assert_eq!(settings_host("gist.github.com"), "github.com");
        assert_eq!(settings_host("gitlab.com"), "gitlab.com");
    }

    #[test]
    fn ssh_urls_keep_user_and_port_but_archive_under_the_bare_host() {
        assert_eq!(
            parse("ssh://git@GitHub.com/rust-lang/rust.git/").unwrap(),
            location(
                "ssh://git@github.com/rust-lang/rust.git",
                "github.com",
                "rust-lang",
                "rust",
            )
        );
        // The short form is fetched as given: its path starts in the home
        // folder of the SSH user, which an ssh:// URL cannot express.
        let short = parse(" git@GitHub.com:rust-lang/rust.git ").unwrap();
        assert_eq!(
            short,
            location(
                "git@GitHub.com:rust-lang/rust.git",
                "github.com",
                "rust-lang",
                "rust"
            )
        );
        assert!(is_ssh(&short.url));
        assert!(parse("git@host:/a b/c").is_err());
        assert_eq!(
            parse("ssh://git@git.example:2222/team/app").unwrap(),
            location(
                "ssh://git@git.example:2222/team/app",
                "git.example",
                "team",
                "app"
            )
        );
        // SourceHut names its users with a tilde.
        let srht = parse("git@git.sr.ht:~alice/tools").unwrap();
        assert_eq!(srht.url, "git@git.sr.ht:~alice/tools");
        assert_eq!(srht.owner, "~alice");
        assert!(is_ssh(&srht.url));
        assert!(!is_ssh("https://github.com/a/b"));
    }

    #[test]
    fn azure_devops_urls_are_archived_without_the_git_segment() {
        assert_eq!(
            parse("https://dev.azure.com/fabrikam/Fiber/_git/App").unwrap(),
            location(
                "https://dev.azure.com/fabrikam/Fiber/_git/App",
                "dev.azure.com",
                "fabrikam/Fiber",
                "App"
            )
        );
        // A repository named like its project has no project segment.
        let parsed = parse("https://dev.azure.com/fabrikam/_git/Fiber").unwrap();
        assert_eq!(parsed.url, "https://dev.azure.com/fabrikam/_git/Fiber");
        assert_eq!(
            (parsed.owner.as_str(), parsed.name.as_str()),
            ("fabrikam", "Fiber")
        );
        assert_eq!(parse("https://dev.azure.com/_git/App"), Err(UrlError::Path));
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
        assert_eq!(parse_api_url("ftp://x/api"), Err(UrlError::ApiScheme));
        assert_eq!(parse_api_url("ssh://x/api"), Err(UrlError::ApiScheme));
        assert_eq!(
            parse_api_url("https://t:k@x/api"),
            Err(UrlError::Credentials)
        );
    }

    #[test]
    fn owner_names_are_single_segments_unless_nested() {
        assert_eq!(
            parse_owner(" octo-org ", false).as_deref(),
            Some("octo-org")
        );
        assert_eq!(
            parse_owner("/group/sub/", true).as_deref(),
            Some("group/sub")
        );
        for bad in ["", "group/sub", "a b", "..", "a?b", "a%2Fb"] {
            assert_eq!(parse_owner(bad, false), None, "{bad}");
        }
        assert_eq!(parse_owner("group//sub", true), None);
        assert_eq!(parse_owner("group/../x", true), None);
    }

    #[test]
    fn every_error_has_a_message_key() {
        for err in [
            UrlError::Invalid,
            UrlError::Scheme,
            UrlError::Credentials,
            UrlError::QueryOrFragment,
            UrlError::Path,
            UrlError::GistOwner,
        ] {
            assert!(err.message_key().starts_with("url."));
        }
    }
}
