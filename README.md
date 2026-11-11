# git-archive

git-archive keeps complete mirrors of Git repositories from GitHub, GitLab, Forgejo, Gitea, Bitbucket Cloud, Gitee, Azure DevOps, SourceHut or any Git server that speaks HTTP(S). It fetches every branch, tag and commit, and the wiki when there is one, on a schedule. A web interface shows the archived code, and the archives can be cloned or downloaded back at any time.

The archive never loses data that it fetched once:

- A fetch never prunes. A branch or tag deleted at the source stays in the archive.
- When the source rewrites a ref with a force-push, the old commit is kept under `refs/git-archive/rewritten/<unix-time>/<ref>`, for example `refs/git-archive/rewritten/1794336958/heads/main`. The web interface lists these refs as rewritten history.
- Garbage collection never expires unreachable objects (`gc.pruneExpire=never`).
- A repository that the source deletes or makes unreachable is marked "source unreachable". Its archive stays as it is.
- A repository that disappears from a watched user or organization stays in the archive.

Pull requests, issues and Git LFS objects are not archived.

## Features

- Add a repository by URL, or import every repository of a user, an organization, a GitLab group (with subgroups), a Bitbucket workspace or an Azure DevOps organization or project.
- Watched sources: scheduled scans add the repositories that appear later. Forks are optional per source.
- A global sync interval, a per-repository interval and a "sync now" button.
- Default tokens per host, and a per-repository token that overrides the host token. Tokens are stored encrypted (XChaCha20-Poly1305).
- Browsing: branches, tags, rewritten history, paged commit history with message search, commit detail with diff, file tree, files with syntax highlighting, rendered README, blame, raw files.
- Restore: read-only `git clone` over smart HTTP, and ZIP or tar.gz download of any revision.
- Ref exclude patterns per host and per repository, for example `refs/pull/*`.
- Scheduled integrity checks (`git fsck`) and disk maintenance (`git repack`) of every archive.
- Webhook and Telegram notifications when a repository, a source scan or the backup fails and when it works again.
- Daily `git bundle` backups to S3-compatible storage (AWS S3, Cloudflare R2, Backblaze B2, MinIO and others).
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
| `GIT_ARCHIVE_FSCK_INTERVAL_DAYS` | `7` | Days between two `git fsck` runs per repository. `0` turns the check off. |
| `GIT_ARCHIVE_REPACK_INTERVAL_DAYS` | `7` | Days between two `git repack` runs per repository. `0` turns the repack off. |
| `GIT_ARCHIVE_PUBLIC_URL` | empty | Address of the web interface, for example `https://archive.example.com`. Notifications then link to the repository page. |
| `GIT_ARCHIVE_NOTIFY_WEBHOOK_URL` | empty | Every notification is POSTed to this URL as JSON. |
| `GIT_ARCHIVE_NOTIFY_TELEGRAM_BOT_TOKEN` | empty | Telegram bot token from BotFather. |
| `GIT_ARCHIVE_NOTIFY_TELEGRAM_CHAT_ID` | empty | Chat that receives the Telegram notifications. |
| `GIT_ARCHIVE_NOTIFY_TELEGRAM_API_URL` | `https://api.telegram.org` | Telegram Bot API server. Change it only for a self-hosted Bot API server. |
| `GIT_ARCHIVE_NOTIFY_LANG` | `tr` | Language of the notification text: `tr` or `en`. |
| `GIT_ARCHIVE_BACKUP_S3_ENDPOINT` | empty | S3 endpoint, for example `https://s3.eu-central-1.amazonaws.com`. |
| `GIT_ARCHIVE_BACKUP_S3_REGION` | `us-east-1` | Region used to sign the requests. |
| `GIT_ARCHIVE_BACKUP_S3_BUCKET` | empty | Bucket that receives the bundles. |
| `GIT_ARCHIVE_BACKUP_S3_PREFIX` | empty | Key prefix of the bundles. |
| `GIT_ARCHIVE_BACKUP_S3_ACCESS_KEY_ID` | empty | Access key ID. |
| `GIT_ARCHIVE_BACKUP_S3_SECRET_ACCESS_KEY` | empty | Secret access key. |
| `GIT_ARCHIVE_BACKUP_S3_PATH_STYLE` | `false` | `true` puts the bucket name into the path instead of the host name. MinIO and most self-hosted storage need it. |
| `GIT_ARCHIVE_BACKUP_INTERVAL_HOURS` | `24` | Hours between two backups. |
| `GIT_ARCHIVE_BACKUP_KEEP` | `7` | Bundles kept per repository. |

The settings page can override the sync, maintenance, notification and backup settings. A value saved there wins over the environment value; an empty field returns to the environment value. The Telegram bot token and the S3 secret access key are stored encrypted with the secret key.

## Excluding refs

A host and a repository can each have a list of ref patterns, one per line, for example `refs/pull/*` or `refs/merge-requests/*`. The patterns of the host and of the repository together are left out of every later fetch. Refs that the archive already holds stay. A pattern starts with `refs/`, holds at most one `*`, and cannot name the `refs/git-archive/` refs.

## Integrity checks and maintenance

After a successful sync, git-archive runs `git fsck` when the check interval has passed, and `git pack-refs --all` followed by `git repack -a -d --keep-unreachable` when the repack interval has passed. The repack keeps every object, including the rewritten history. A failed check marks the repository "Error", and the check runs again after every sync until it passes. The repository page shows the time of the last check and of the last repack, and the space the repack saved.

## Notifications

A notification is sent when a repository changes to "Error" or "source unreachable", when a source scan fails, and when the backup fails, and once more when it works again. While an error lasts, it is not sent again. Set a webhook URL, a Telegram bot and chat, or both. The settings page has a button that sends a test notification.

The webhook body is JSON:

```json
{
  "event": "repo_failed",
  "text": "git-archive: github.com/owner/name could not be synced. ...",
  "content": "git-archive: github.com/owner/name could not be synced. ...",
  "repo": { "id": 1, "name": "github.com/owner/name", "url": "https://github.com/owner/name" },
  "source": null,
  "status": "unreachable",
  "previous_status": "ok",
  "message": "...",
  "at": 1794336958,
  "link": "https://archive.example.com/repos/1"
}
```

`event` is one of `repo_failed`, `repo_recovered`, `source_failed`, `source_recovered`, `backup_failed`, `backup_recovered` and `test`. `content` repeats `text` so that Discord webhooks work without changes; Slack incoming webhooks read `text`. Fields that do not apply to an event are `null`; `source` holds the source name of a source scan event.

## Backups

When the endpoint, the bucket, the access key ID and the secret access key are set, git-archive uploads a full `git bundle` of every repository whose refs changed since its last upload, once per backup interval. The wiki gets its own bundle. The keys are:

```
<prefix>/<host>/<owner>/<name>/<unix-time>.bundle
<prefix>/<host>/<owner>/<name>.wiki/<unix-time>.bundle
```

After an upload, the oldest bundles beyond the number to keep are deleted. Bundles are not encrypted; use a private bucket or the server-side encryption of the storage. The settings page shows the last backup, tests the connection and starts a backup on demand.

To restore a repository from a bundle:

```sh
git clone --mirror name-1794336958.bundle name.git
```

## Hosts and tokens

Public repositories need no token. For private repositories and for higher API rate limits, add a host on the Hosts page with a token. Every repository on that host uses the host token unless it has its own token. Git sends a token only when the server asks for authentication.

The API URL of a host is optional. When it is empty, it is derived from the host name:

| Provider | Derived API URL |
|---|---|
| GitHub | `https://api.github.com` for github.com, else `https://<host>/api/v3` |
| GitLab | `https://<host>/api/v4` |
| Forgejo / Gitea | `https://<host>/api/v1` |
| Bitbucket | `https://api.bitbucket.org/2.0` for bitbucket.org |
| Gitee | `https://<host>/api/v5` |
| Azure DevOps | `https://<host>`; for Azure DevOps Server, enter the collection URL, for example `https://tfs.example/tfs/DefaultCollection`, and the project as the source name |
| SourceHut | `https://<host>/query`, so the host is `git.sr.ht` |

Tokens with read access are enough:

- GitHub: a fine-grained personal access token with read access to "Contents" and "Metadata" of the repositories, or a classic token with the `repo` scope for private repositories.
- GitLab: a personal, group or project access token with `read_api` and `read_repository`.
- Forgejo / Gitea: an access token with read permission for repositories, and for organizations and users when you import them.
- Bitbucket Cloud: an API token with scopes (app passwords no longer work), created for the Bitbucket app with the `read:repository:bitbucket` scope. Put the email address of the Atlassian account in the username field. The REST API authenticates with that email; for Git, git-archive sends the fixed username `x-bitbucket-api-token-auth` that Bitbucket requires with API tokens.

- Gitee: a personal access token with read access to repositories. The API receives it in the `access_token` query parameter.
- Azure DevOps: a personal access token with the "Code (Read)" scope (`vso.code`). Azure DevOps ignores the username.
- SourceHut: a personal access token for git.sr.ht with read access to profile and repositories. The SourceHut API does not answer without a token.

The username field may stay empty for GitHub, GitLab, Forgejo, Gitea, Gitee, Azure DevOps and SourceHut.

An Azure DevOps source is an organization (every project) or `organization/project`. Repository URLs keep their `_git` segment for fetching, but the archive path leaves it out: `dev.azure.com/<org>/<project>/<name>`. Disabled Azure DevOps repositories are skipped. A SourceHut source is a user, with or without its `~`. Gitee, Azure DevOps and SourceHut report no archived state, so "skip archived repositories" has no effect on them.

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

Requirements: Rust 1.89 or later, Git, and [lizard](https://github.com/terryyin/lizard) for the complexity check.

```sh
make run     # cargo run; reads the environment, see .env.example
make test    # all tests
make lint    # rustfmt check, clippy with warnings as errors, cyclomatic complexity limit 10
make up      # docker compose up -d --build
```

The tests run git against local repositories and mock provider APIs; they need no network access.

## License

MIT
