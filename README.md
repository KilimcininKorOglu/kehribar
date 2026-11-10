# git-archive

git-archive keeps complete mirrors of Git repositories from GitHub, GitLab, Forgejo, Gitea, Bitbucket Cloud or any Git server that speaks HTTP(S). It fetches every branch, tag and commit, and the wiki when there is one, on a schedule. A web interface shows the archived code, and the archives can be cloned or downloaded back at any time.

The archive never loses data that it fetched once:

- A fetch never prunes. A branch or tag deleted at the source stays in the archive.
- When the source rewrites a ref with a force-push, the old commit is kept under `refs/git-archive/rewritten/<unix-time>/<ref>`, for example `refs/git-archive/rewritten/1794336958/heads/main`. The web interface lists these refs as rewritten history.
- Garbage collection never expires unreachable objects (`gc.pruneExpire=never`).
- A repository that the source deletes or makes unreachable is marked "source unreachable". Its archive stays as it is.
- A repository that disappears from a watched user or organization stays in the archive.

Pull requests, issues and Git LFS objects are not archived.

## Features

- Add a repository by URL, or import every repository of a user, an organization, a GitLab group (with subgroups) or a Bitbucket workspace.
- Watched sources: scheduled scans add the repositories that appear later. Forks are optional per source.
- A global sync interval, a per-repository interval and a "sync now" button.
- Default tokens per host, and a per-repository token that overrides the host token. Tokens are stored encrypted (XChaCha20-Poly1305).
- Browsing: branches, tags, rewritten history, paged commit history with message search, commit detail with diff, file tree, files with syntax highlighting, rendered README, blame, raw files.
- Restore: read-only `git clone` over smart HTTP, and ZIP or tar.gz download of any revision.
- One admin account, light and dark themes, Turkish and English.

## Quick start

Requirements: Docker with Docker Compose.

```sh
cp .env.example .env
# Set GIT_ARCHIVE_ADMIN_PASSWORD and a secret key:
openssl rand -hex 32   # paste the output into GIT_ARCHIVE_SECRET_KEY
docker compose up -d --build
```

Open http://localhost:8600 and sign in with the admin username and password.

The data lives in the named volume `git-archive-data`, mounted at `/data`:

```
/data/git-archive.db                         SQLite database
/data/repos/<host>/<owner>/<name>.git        bare mirror
/data/repos/<host>/<owner>/<name>.wiki.git   wiki mirror, when there is one
```

Keep the secret key. Without it the stored tokens cannot be decrypted. The mirrors themselves are plain bare Git repositories and do not need the key.

## Configuration

Every setting is an environment variable. Docker Compose reads them from `.env`.

| Variable | Default | Description |
|---|---|---|
| `GIT_ARCHIVE_ADMIN_USERNAME` | (required) | Admin username for the web interface and for `git clone`. |
| `GIT_ARCHIVE_ADMIN_PASSWORD` | (required) | Admin password. |
| `GIT_ARCHIVE_SECRET_KEY` | (required) | 64 hex characters. Encrypts the stored tokens. |
| `GIT_ARCHIVE_LISTEN_ADDR` | empty | Listen address. Empty listens on `[::]:8600`, or on `0.0.0.0:8600` without IPv6. |
| `GIT_ARCHIVE_DATA_DIR` | `/data` | Database and mirrors. |
| `GIT_ARCHIVE_SYNC_INTERVAL_MINUTES` | `360` | Global sync interval. Also the scan interval of watched sources. |
| `GIT_ARCHIVE_SYNC_CONCURRENCY` | `2` | Syncs that run at the same time. |
| `GIT_ARCHIVE_GIT_TIMEOUT_SECONDS` | `3600` | Time limit of one git command. |
| `GIT_ARCHIVE_SESSION_TTL_HOURS` | `168` | Lifetime of a web session. |
| `GIT_ARCHIVE_COOKIE_SECURE` | `false` | Set to `true` when the site is served over HTTPS. The clone URL on the repository page then uses `https`. |
| `GIT_ARCHIVE_LOG_LEVEL` | `info` | Log filter, for example `debug` or `git_archive=debug`. |

The settings page can override the sync interval, the concurrency and the git timeout. A value saved there wins over the environment value; an empty field returns to the environment value.

## Hosts and tokens

Public repositories need no token. For private repositories and for higher API rate limits, add a host on the Hosts page with a token. Every repository on that host uses the host token unless it has its own token. Git sends a token only when the server asks for authentication.

The API URL of a host is optional. When it is empty, it is derived from the host name:

| Provider | Derived API URL |
|---|---|
| GitHub | `https://api.github.com` for github.com, else `https://<host>/api/v3` |
| GitLab | `https://<host>/api/v4` |
| Forgejo / Gitea | `https://<host>/api/v1` |
| Bitbucket | `https://api.bitbucket.org/2.0` for bitbucket.org |

Tokens with read access are enough:

- GitHub: a fine-grained personal access token with read access to "Contents" and "Metadata" of the repositories, or a classic token with the `repo` scope for private repositories.
- GitLab: a personal, group or project access token with `read_api` and `read_repository`.
- Forgejo / Gitea: an access token with read permission for repositories, and for organizations and users when you import them.
- Bitbucket Cloud: an app password with repository read permission. Put the Bitbucket username in the username field; it is used for both Git and the API.

The username field may stay empty for GitHub, GitLab, Forgejo and Gitea.

## Importing users and organizations

On the Sources page, choose a host, the type (user or organization), and the name as it appears in URLs. The first scan runs at once and adds every repository it finds. With "add new repositories" on, the source is scanned again on the global interval. "Also import forks" includes forked repositories.

For GitHub, a user source lists the private repositories too when the host token belongs to that user. A scan reports repositories whose clone URL is on another host than the source's host instead of adding them; check the API URL of the host when this happens.

## Getting an archive back

Clone from the archive with the admin credentials. The repository page shows the URL:

```sh
git clone http://localhost:8600/git/github.com/owner/name.git
# Every ref, including the rewritten history:
git clone --mirror http://localhost:8600/git/github.com/owner/name.git
```

The server accepts only fetches; a push is refused. The file view has ZIP and tar.gz downloads of the selected revision.

## Development

Requirements: Rust 1.85 or later, Git, and [lizard](https://github.com/terryyin/lizard) for the complexity check.

```sh
make run     # cargo run; reads the environment, see .env.example
make test    # all tests
make lint    # rustfmt check, clippy with warnings as errors, cyclomatic complexity limit 10
make up      # docker compose up -d --build
```

The tests run git against local repositories and mock provider APIs; they need no network access.

## License

MIT
