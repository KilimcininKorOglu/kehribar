-- SQLite cannot change a CHECK constraint, so hosts and sources are rebuilt
-- with the source kinds (starred, gists) and providers (gitee, azure,
-- sourcehut) that the application knows. Rows and ids stay the same.

CREATE TABLE hosts_new (
    id INTEGER PRIMARY KEY,
    host TEXT NOT NULL UNIQUE COLLATE NOCASE,
    provider TEXT NOT NULL CHECK (provider IN (
        'github', 'gitlab', 'forgejo', 'bitbucket', 'gitee', 'azure', 'sourcehut', 'generic'
    )),
    api_base_url TEXT,
    username TEXT,
    token_enc BLOB,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    ref_exclude TEXT
);

INSERT INTO hosts_new (
    id, host, provider, api_base_url, username, token_enc, created_at, updated_at, ref_exclude
)
SELECT id, host, provider, api_base_url, username, token_enc, created_at, updated_at, ref_exclude
FROM hosts;

-- References hosts_new, so dropping the old hosts below does not cascade
-- into it; the rename of hosts_new updates the reference.
CREATE TABLE sources_new (
    id INTEGER PRIMARY KEY,
    host_id INTEGER NOT NULL REFERENCES hosts_new (id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('user', 'org', 'starred', 'gists')),
    name TEXT NOT NULL COLLATE NOCASE,
    auto_add INTEGER NOT NULL DEFAULT 1,
    last_scan_at INTEGER,
    last_error TEXT,
    created_at INTEGER NOT NULL,
    include_forks INTEGER NOT NULL DEFAULT 0,
    include_patterns TEXT,
    exclude_patterns TEXT,
    skip_archived INTEGER NOT NULL DEFAULT 0,
    UNIQUE (host_id, kind, name)
);

INSERT INTO sources_new (
    id, host_id, kind, name, auto_add, last_scan_at, last_error, created_at,
    include_forks, include_patterns, exclude_patterns, skip_archived
)
SELECT id, host_id, kind, name, auto_add, last_scan_at, last_error, created_at,
    include_forks, include_patterns, exclude_patterns, skip_archived
FROM sources;

-- Dropping sources sets repos.source_id to NULL; keep the links to restore.
CREATE TEMP TABLE kept_source_links AS
SELECT id, source_id FROM repos WHERE source_id IS NOT NULL;

DROP TABLE sources;
DROP TABLE hosts;
ALTER TABLE hosts_new RENAME TO hosts;
ALTER TABLE sources_new RENAME TO sources;

UPDATE repos
SET source_id = (SELECT k.source_id FROM kept_source_links k WHERE k.id = repos.id)
WHERE id IN (SELECT id FROM kept_source_links);

DROP TABLE kept_source_links;
