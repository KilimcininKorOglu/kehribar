# git-archive

git-archive keeps complete mirrors of Git repositories from GitHub, GitLab, Forgejo, Gitea, Bitbucket Cloud, Gitee, Azure DevOps, SourceHut or any Git server that speaks HTTP(S). It fetches every branch, tag and commit, and the wiki when there is one, on a schedule. A web interface shows the archived code, and the archives can be cloned or downloaded back at any time.

The archive never loses data that it fetched once:

- A fetch never prunes. A branch or tag deleted at the source stays in the archive.
- When the source rewrites a ref with a force-push, the old commit is kept under `refs/git-archive/rewritten/<unix-time>/<ref>`, for example `refs/git-archive/rewritten/1794336958/heads/main`. The web interface lists these refs as rewritten history.
- Garbage collection never expires unreachable objects (`gc.pruneExpire=never`).
- A repository that the source deletes or makes unreachable is marked "source unreachable". Its archive stays as it is.
- A repository that disappears from a watched user or organization stays in the archive.

Pull requests and issues are not archived. Git LFS objects are archived only for the repositories that turn LFS on.

## Features

- Add a repository by URL, many at once with one URL per line, or import every repository of a user, an organization, a GitLab group (with subgroups), a Bitbucket workspace or an Azure DevOps organization or project.
- Watched sources: scheduled scans add the repositories that appear later. Forks are optional per source.
- A global sync interval, a per-repository interval and a "sync now" button.
- Default tokens per host, and a per-repository token that overrides the host token. Tokens are stored encrypted (XChaCha20-Poly1305).
- Fetching over SSH with a generated or pasted key per host, and a stored host key that a fetch checks every time.
- Git LFS per repository: the objects are archived, shown in the file view and in downloads, and backed up.
- External images of Markdown files are archived and shown from the archive, with SSRF protection.
- Browsing: branches, tags, rewritten history, paged commit history with message search, commit detail with diff, file tree, files with syntax highlighting, rendered README and Markdown files, blame, raw files.
- Ref history: every branch and tag change of each sync (created, moved, rewritten, deleted at the source, back at the source), the details of each sync run, a "deleted at source" mark on the branches and tags page, and a timeline that shows where each ref pointed on a chosen date.
- Compare two branches, tags or commits: the commits of the second that the first lacks (at most 250) and their combined diff.
- Code search in one repository with `git grep`: fixed text or a regular expression, with or without case, in any revision; at most 500 matching lines.
- Wiki browsing: Markdown, AsciiDoc and reStructuredText pages are rendered, other formats are shown as text.
- The repository list filters by status, sorts by name, size, last sync or status, and pages by 50.
- A disk page: the archive size, the LFS share, the 20 largest repositories and the growth of the last 90 days.
- Restore: read-only `git clone` over smart HTTP with the admin credentials or a revocable clone token, and ZIP or tar.gz download of any revision.
- A push webhook URL per repository that syncs it right after a push, on any provider.
- A REST API with read-only or read-write API tokens for repositories, hosts and sources.
- Ref exclude patterns per host and per repository, for example `refs/pull/*`.
- Scheduled integrity checks (`git fsck`) and disk maintenance (`git repack`) of every archive.
- Webhook and Telegram notifications when a repository, a source scan or the backup fails and when it works again.
- Daily `git bundle` backups to S3-compatible storage (AWS S3, Cloudflare R2, Backblaze B2, MinIO and others).
- Export of the hosts, sources and repositories to a JSON file, and import of that file into another installation.
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
/data/ssh/known_hosts                        SSH host keys of the servers
```

Keep the secret key. Without it the stored tokens and SSH keys cannot be decrypted. The mirrors themselves are plain bare Git repositories and do not need the key.

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
| `GIT_ARCHIVE_PUBLIC_URL` | empty | Address of the web interface, for example `https://archive.example.com`. Notifications then link to the repository page, and push webhook URLs start with it. |
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

## SSH

A repository URL can also be `ssh://git@host[:port]/owner/name.git` or the short form `git@host:owner/name.git`. The short form is fetched as given, so its path stays relative to the home folder of the SSH user. The archive path is the same as for HTTPS: `<host>/<owner>/<name>`, without the port.

Each host can hold one SSH key. On the host page, generate a new ed25519 key or paste a private key without a passphrase, then add the shown public key to the account on the server or as a read-only deploy key. The private key is stored encrypted with the secret key. During a fetch it is written to a file under `/data/tmp` that only the app user can read, and the file is removed when the fetch ends. Without a key, an SSH fetch uses no identity.

The first connection to a server stores its host key in `/data/ssh/known_hosts`. When the server later shows another key, the fetch stops and the repository changes to "Error" with a message that names the host key. The host page lists the stored host keys of the host on every port. When a change is expected, for example after a server move, "Forget host keys" removes them and the next fetch stores the new key.

A source with the "SSH URL" option adds its repositories with the SSH clone URL the provider names, under the same archive path as the HTTPS URL. The SSH URL must be on the server of the host or on its `ssh.` name, as Azure DevOps uses (`ssh.dev.azure.com`), because the SSH key of the host goes to that server. A repository without such a URL, for example a gist, is skipped and named in the scan result.

## Git LFS

"Fetch Git LFS files" in the settings of a repository makes every sync run `git lfs fetch --all origin` after the fetch, which downloads every LFS object that a branch or tag points to. The objects stay in `<name>.git/lfs/objects`. A failed LFS download marks the repository "Error"; the commits of that sync are archived all the same. The repository page shows how much of the archive size the LFS objects take.

With LFS on, the file view, the raw file and the ZIP or tar.gz download give the real content of an LFS file. A file whose object the archive does not hold is shown as its pointer file; a download never fetches a missing object from the source. Without LFS, every LFS file shows as its pointer file. `git clone` from the archive serves the pointer files only.

The backup uploads every LFS object once to `<prefix>/<host>/<owner>/<name>/lfs/<oid>`, next to the bundles. Objects are named by their SHA-256 and are never pruned. To restore a repository with its LFS files, clone the bundle and copy each object to `lfs/objects/<first two characters>/<next two characters>/<oid>` in the restored repository:

```sh
git clone --mirror name-1794336958.bundle name.git
# for each downloaded object <oid>:
mkdir -p name.git/lfs/objects/${oid:0:2}/${oid:2:2}
cp <oid> name.git/lfs/objects/${oid:0:2}/${oid:2:2}/<oid>
```

## Markdown images

After every successful sync, git-archive reads the Markdown files (`.md`, `.markdown`) of the default branch, and the Markdown pages of the archived wiki, and downloads the external images they show: Markdown images and HTML `<img>` elements with an `http` or `https` URL. A rendered README, the rendered view of a Markdown file and a Markdown wiki page then show the archived copy from `/repos/<id>/asset/<sha256>` instead of the external address; the "Source" switch of a Markdown file shows its text. Images of AsciiDoc, reStructuredText and other wiki pages are not downloaded and stay external.

- At most 200 images per repository and its wiki together, in the order the files name them, repository files first. The repository page says when the files name more.
- At most 5 MB per image, and only answers with an `image/*` content type. A request may take 20 seconds and follow 3 redirects.
- An archived image is downloaded again after 24 hours. A failed download keeps the copy the archive has and does not fail the sync; it is written to the log.
- Images go only to public addresses. A host name is resolved first, and the request is refused when any of its addresses is loopback, private, link-local, shared (100.64.0.0/10), multicast, documentation or another reserved range; the connection then goes to the checked address. Every redirect is checked the same way, so a redirect to an internal address is refused too. Proxy settings of the environment are not used for these requests.
- The files are stored in `/data/assets/<sha256>`, once for every repository that shows the same image. The copies are served in a sandbox (`Content-Security-Policy: sandbox; default-src 'none'`), so an SVG image cannot run a script.
- Deleting a repository deletes its image records and every stored file that no repository uses any more. A file younger than an hour is kept until a later sync or deletion, because a sync running at the same time may still record it.

The backup uploads every archived image once to `<prefix>/<host>/<owner>/<name>/assets/<sha256>`. The database (`/data/git-archive.db`) holds which image URL belongs to which file, so restore the images together with it: copy the downloaded objects to `/data/assets/` with the same names. Without the database, a sync downloads the images again from their sources, and an image whose source is gone stays external.

## Export and import

"Download as JSON" on the settings page saves every host, source and repository with their settings to one file (`format: 1`). Tokens and SSH private keys are in the file as the base64 of their encrypted bytes, never in plain text. The Git data of the repositories is not in the file; use the backup for that.

"Import" adds the records of such a file that the archive does not have. A host matches by its name, a source by host, type and name, and a repository by its URL or its archive path. An existing record stays as it is. Every record goes through the checks of its form, and the result page names each record as added, already there or invalid, with the reason.

A secret works only in an installation with the same `GIT_ARCHIVE_SECRET_KEY`. A secret that does not decrypt with the key of the importing installation is left out: the record is added without it, the result page marks it, and the token or key must be entered again. The import runs in one database transaction, so a database error leaves no part of the file behind. The file may be up to 32 MB.

## Importing users and organizations

On the Sources page, choose a host, the type (user or organization), and the name as it appears in URLs. The first scan runs at once and adds every repository it finds. With "add new repositories" on, the source is scanned again on the global interval. "Also import forks" includes forked repositories.

For GitHub, a user source lists the private repositories too when the host token belongs to that user. A scan reports repositories whose clone URL is on another host than the source's host instead of adding them; check the API URL of the host when this happens.

## Getting an archive back

Clone from the archive with the admin credentials or a clone token. The repository page shows the URL:

```sh
git clone http://localhost:8600/git/github.com/owner/name.git
# Every ref, including the rewritten history:
git clone --mirror http://localhost:8600/git/github.com/owner/name.git
```

A clone token is created on Settings, "Access tokens" (`/settings/tokens`). It opens every repository or only the selected ones, may expire after 30, 90 or 365 days, and can be revoked at any time. Git sends it as the password; the username is not checked. The page shows a new token once; the database keeps only its SHA-256 hash. A repository outside the token's selection answers 404, as if it did not exist.

```sh
git clone http://token:gac_...@localhost:8600/git/github.com/owner/name.git
```

The server accepts only fetches; a push is refused. Wrong credentials count against the same per-address lockout as the login form. The file view has ZIP and tar.gz downloads of the selected revision.

## Push webhooks

A repository syncs at its interval. To sync it right after a push, create a webhook URL on its page ("Push webhook") and add it as a push webhook on the source server:

```text
http://localhost:8600/hooks/gaw_...
```

- The URL is the secret: the token in it is random, stored as a SHA-256 hash and, encrypted with `GIT_ARCHIVE_SECRET_KEY`, for showing it again on the page. No webhook secret or signature is needed, and the payload is not read, so every provider works the same way.
- A `POST` answers `202` and makes the repository due at once. A call while the repository syncs makes it sync once more when that sync finishes; several calls during one sync add up to that one more sync.
- "Renew URL" replaces the token and the old URL answers `404`; "Remove" deletes it. Wrong tokens count against the per-address lockout of the login form.
- The URL uses `GIT_ARCHIVE_PUBLIC_URL` when it is set, else the address the browser used.

## REST API

The API lives under `/api/v1` and speaks JSON. Create an API token on Settings, "Access tokens" (`/settings/tokens`), with read-only or read-write permission and an optional expiry, and send it as a Bearer token:

```sh
curl -H "Authorization: Bearer gaa_..." http://localhost:8600/api/v1/repos
```

- A read-only token may only send `GET`; any other method answers `403` with the code `read_only`.
- A missing, wrong, revoked or expired token answers `401`. Wrong tokens count against the per-address lockout of the login form; a locked address gets `429`.
- Answers never contain a secret. A stored token or SSH private key shows as `has_token` or `has_ssh_key`; a host shows its SSH public key.
- Errors have the shape `{"error": {"code": "...", "message": "..."}}`. A value the web form would refuse answers `422` (or `409` when the record exists already) with the form's message key as the code, for example `form.interval_invalid`, `refs.invalid` or `repo_new.exists`. A body that is not valid JSON or has an unknown field answers with the code `invalid_body`.
- Times are Unix seconds. Ref and name patterns are arrays of strings.

| Method and path | Effect |
|---|---|
| `GET /repos` | Repositories, filtered and paged: `q` (text in the URL or `owner/name`), `status` (`pending`, `syncing`, `ok`, `error`, `unreachable`), `sort` (`name`, `size`, `synced`, `status`), `dir` (`asc`, `desc`), `page` (from 1), `per_page` (1 to 100, default 50). The answer is `{"repos": [...], "total", "page", "per_page"}`; `total` counts every match. |
| `GET /repos/{id}` | One repository. |
| `GET /repos/{id}/runs` | Its sync runs, newest first; `limit` is 1 to 200, default 50. |
| `POST /repos` | Adds a repository: `url` and optional `interval_minutes`, `wiki_enabled` (default `true`), `lfs_enabled`, `ref_exclude`, `username`, `token`. Answers `201` and syncs it at once. |
| `PATCH /repos/{id}` | Changes `interval_minutes` (`null` for the global interval), `wiki_enabled`, `lfs_enabled`, `ref_exclude`. Fields that are absent keep their value. |
| `PUT /repos/{id}/credentials` | Sets `username` and `token`. Without a `token` field the stored token stays; `null` removes it. |
| `DELETE /repos/{id}` | Deletes the repository with its archive. A repository that syncs now answers `409` (`repo.delete_busy`). |
| `POST /repos/{id}/sync` | Syncs now, or once more after the running sync, like a push webhook. Answers `202`. |
| `GET /hosts`, `GET /hosts/{id}` | Hosts. |
| `POST /hosts` | Adds a host: `host`, `provider` (`github`, `gitlab`, `forgejo`, `bitbucket`, `gitee`, `azure`, `sourcehut`, `generic`) and optional `api_base_url`, `username`, `token`, `ref_exclude`, `max_concurrency`. |
| `PATCH /hosts/{id}` | Changes any of those fields; `token: null` removes the token. |
| `DELETE /hosts/{id}` | Deletes the host and its sources; the repositories stay. |
| `PUT /hosts/{id}/ssh-key` | Stores `private_key`, a private key that `ssh-keygen` can read, and answers the host with its public key line. |
| `DELETE /hosts/{id}/ssh-key` | Removes the SSH key. |
| `GET /sources`, `GET /sources/{id}` | Sources. |
| `POST /sources` | Adds a source and scans it: `host_id`, `kind` (`user`, `org`, `starred`, `gists`), `name` and optional `auto_add` (default `true`), `include_forks`, `skip_archived`, `use_ssh`, `include_patterns`, `exclude_patterns`. |
| `PATCH /sources/{id}` | Changes the options. |
| `DELETE /sources/{id}` | Deletes the source; its repositories stay. |
| `POST /sources/{id}/scan` | Scans in the background. Answers `202`. |

```sh
TOKEN=gaa_...
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"url": "https://github.com/octocat/Hello-World", "ref_exclude": ["refs/pull/*"]}' \
  http://localhost:8600/api/v1/repos
curl -X PATCH -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"interval_minutes": 60}' http://localhost:8600/api/v1/repos/1
curl -X POST -H "Authorization: Bearer $TOKEN" http://localhost:8600/api/v1/repos/1/sync
```

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
