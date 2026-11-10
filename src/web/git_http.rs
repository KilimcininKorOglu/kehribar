//! Read-only Git over HTTP, so an archive can be cloned back:
//! `git clone http://<site>/git/<host>/<owner>/<name>.git`.
//!
//! Only the two upload-pack requests of the smart protocol are served.
//! Pushing is refused, and every request needs the admin credentials
//! through HTTP Basic authentication.

use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::prelude::{BASE64_STANDARD, Engine as _};
use tokio_util::io::ReaderStream;

use super::AppState;
use super::auth::secrets_equal;
use crate::db::repos;
use crate::git::http_backend::{self, CgiRequest};
use crate::repo_url;

const REALM: &str = "Basic realm=\"git-archive\", charset=\"UTF-8\"";

/// The two requests of a smart HTTP fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Service {
    /// `GET .../info/refs?service=git-upload-pack`
    Advertise,
    /// `POST .../git-upload-pack`
    UploadPack,
}

impl Service {
    fn suffix(self) -> &'static str {
        match self {
            Self::Advertise => "/info/refs",
            Self::UploadPack => "/git-upload-pack",
        }
    }
}

/// Splits a request into the repository path and the service, or gives the
/// status to answer with.
fn route(method: &Method, path: &str, query: &str) -> Result<(String, Service), StatusCode> {
    let rest = path.strip_prefix("/git/").ok_or(StatusCode::NOT_FOUND)?;
    if rest.ends_with("/git-receive-pack") || query.contains("git-receive-pack") {
        return Err(StatusCode::FORBIDDEN);
    }
    let (repo, service) = if let Some(repo) = rest.strip_suffix(Service::Advertise.suffix()) {
        let ok = *method == Method::GET && query == "service=git-upload-pack";
        (repo, ok.then_some(Service::Advertise))
    } else if let Some(repo) = rest.strip_suffix(Service::UploadPack.suffix()) {
        (
            repo,
            (*method == Method::POST).then_some(Service::UploadPack),
        )
    } else {
        (rest, None)
    };
    service
        .map(|service| (repo.to_owned(), service))
        .ok_or(StatusCode::NOT_FOUND)
}

/// `<host>/<owner...>/<name>[.git]` as the archive stores it.
fn split_repo_path(path: &str) -> Option<(&str, String, &str)> {
    let parts: Vec<&str> = path.split('/').collect();
    let (name, rest) = parts.split_last()?;
    let (host, owner) = rest.split_first()?;
    let name = name.strip_suffix(".git").unwrap_or(name);
    let valid = |p: &&str| !p.is_empty() && *p != "." && *p != "..";
    let ok = !owner.is_empty() && valid(host) && valid(&name) && owner.iter().all(valid);
    ok.then(|| (*host, owner.join("/"), name))
}

/// Why a request was not let through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Denied {
    /// The client failed too often and has to wait.
    Locked,
    /// No or wrong credentials: ask for them.
    Challenge,
}

impl IntoResponse for Denied {
    fn into_response(self) -> Response {
        match self {
            Self::Locked => StatusCode::TOO_MANY_REQUESTS.into_response(),
            Self::Challenge => challenge(),
        }
    }
}

/// Checks the Basic credentials.
fn authenticate(state: &AppState, ip: IpAddr, headers: &HeaderMap) -> Result<(), Denied> {
    let now = Instant::now();
    if state.limiter.is_locked(ip, now) {
        tracing::warn!(%ip, "git request refused, too many failures");
        return Err(Denied::Locked);
    }
    // A git client first asks without credentials; only wrong ones count.
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Err(Denied::Challenge);
    };
    if credentials_match(state, value) {
        state.limiter.reset(ip);
        return Ok(());
    }
    state.limiter.record_failure(ip, now);
    tracing::warn!(%ip, "git request with wrong credentials");
    Err(Denied::Challenge)
}

fn credentials_match(state: &AppState, value: &HeaderValue) -> bool {
    let decoded = value
        .to_str()
        .ok()
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|encoded| BASE64_STANDARD.decode(encoded.trim()).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok());
    let Some((user, password)) = decoded.as_deref().and_then(|d| d.split_once(':')) else {
        return false;
    };
    secrets_equal(user, &state.config.admin_username)
        & secrets_equal(password, &state.config.admin_password)
}

fn challenge() -> Response {
    let mut response = StatusCode::UNAUTHORIZED.into_response();
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static(REALM));
    response
}

pub async fn serve(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    let uri = request.uri().clone();
    let (repo_path, service) = match route(request.method(), uri.path(), uri.query().unwrap_or(""))
    {
        Ok(found) => found,
        Err(status) => return status.into_response(),
    };
    if let Err(denied) = authenticate(&state, peer.ip(), request.headers()) {
        return denied.into_response();
    }
    match backend(&state, &repo_path, service, request).await {
        Ok(response) => response,
        Err(status) => status.into_response(),
    }
}

async fn backend(
    state: &AppState,
    repo_path: &str,
    service: Service,
    request: Request,
) -> Result<Response, StatusCode> {
    let (host, owner, name) = split_repo_path(repo_path).ok_or(StatusCode::NOT_FOUND)?;
    let repo = repos::find_by_path(&state.pool, host, &owner, name)
        .await
        .map_err(|err| internal(&err))?
        .ok_or(StatusCode::NOT_FOUND)?;
    let dir = repo_url::archive_dir(&state.config.data_dir, &repo.host, &repo.owner, &repo.name);
    let (Some(root), Some(dir_name)) = (dir.parent(), dir.file_name()) else {
        return Err(StatusCode::NOT_FOUND);
    };
    if !dir.join("HEAD").is_file() {
        return Err(StatusCode::NOT_FOUND);
    }
    let headers = request.headers();
    let text = |name: HeaderName| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    let cgi = CgiRequest {
        method: request.method().as_str().to_owned(),
        path_info: format!("/{}{}", dir_name.to_string_lossy(), service.suffix()),
        query: request.uri().query().unwrap_or_default().to_owned(),
        content_type: text(header::CONTENT_TYPE),
        content_encoding: text(header::CONTENT_ENCODING),
        git_protocol: text(HeaderName::from_static("git-protocol")),
    };
    let body = request.into_body().into_data_stream();
    let answer = http_backend::run(root, &cgi, body)
        .await
        .map_err(|err| internal(&err))?;
    let mut response = Body::from_stream(ReaderStream::new(answer.body)).into_response();
    *response.status_mut() = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::BAD_GATEWAY);
    for (name, value) in answer.headers {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
            response.headers_mut().insert(name, value);
        }
    }
    Ok(response)
}

fn internal(err: &dyn std::fmt::Display) -> StatusCode {
    tracing::error!(%err, "git request failed");
    StatusCode::INTERNAL_SERVER_ERROR
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_upload_pack_requests_are_routed() {
        let get = Method::GET;
        let post = Method::POST;
        assert_eq!(
            route(
                &get,
                "/git/github.com/a/b.git/info/refs",
                "service=git-upload-pack"
            ),
            Ok(("github.com/a/b.git".to_owned(), Service::Advertise))
        );
        assert_eq!(
            route(&post, "/git/gitlab.com/g/sub/p.git/git-upload-pack", ""),
            Ok(("gitlab.com/g/sub/p.git".to_owned(), Service::UploadPack))
        );
        assert_eq!(
            route(&get, "/git/h/a/b.git/info/refs", "service=git-receive-pack"),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            route(&post, "/git/h/a/b.git/git-receive-pack", ""),
            Err(StatusCode::FORBIDDEN)
        );
        // The dumb protocol and other files are not served.
        for (method, path, query) in [
            (&get, "/git/h/a/b.git/HEAD", ""),
            (&get, "/git/h/a/b.git/info/refs", ""),
            (&get, "/git/h/a/b.git/git-upload-pack", ""),
            (&get, "/other", ""),
        ] {
            assert_eq!(
                route(method, path, query),
                Err(StatusCode::NOT_FOUND),
                "{path}"
            );
        }
    }

    #[test]
    fn repository_paths_split_into_host_owner_and_name() {
        assert_eq!(
            split_repo_path("gitlab.com/group/sub/project.git"),
            Some(("gitlab.com", "group/sub".to_owned(), "project"))
        );
        assert_eq!(
            split_repo_path("gitea.local:3000/team/repo"),
            Some(("gitea.local:3000", "team".to_owned(), "repo"))
        );
        for bad in [
            "github.com/repo.git",
            "github.com/../x.git",
            "github.com//x.git",
            "",
        ] {
            assert_eq!(split_repo_path(bad), None, "{bad}");
        }
    }
}
