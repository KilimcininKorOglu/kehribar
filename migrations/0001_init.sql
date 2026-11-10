-- Hosts hold the default credentials and the API settings for one Git server.
CREATE TABLE hosts (
    id INTEGER PRIMARY KEY,
    host TEXT NOT NULL UNIQUE COLLATE NOCASE,
    provider TEXT NOT NULL
        CHECK (provider IN ('github', 'gitlab', 'forgejo', 'bitbucket', 'generic')),
    api_base_url TEXT,
    username TEXT,
    token_enc BLOB,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Sources are watched users or organizations whose repositories are imported.
CREATE TABLE sources (
    id INTEGER PRIMARY KEY,
    host_id INTEGER NOT NULL REFERENCES hosts (id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('user', 'org')),
    name TEXT NOT NULL COLLATE NOCASE,
    auto_add INTEGER NOT NULL DEFAULT 1,
    last_scan_at INTEGER,
    last_error TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE (host_id, kind, name)
);

CREATE TABLE repos (
    id INTEGER PRIMARY KEY,
    url TEXT NOT NULL UNIQUE,
    host TEXT NOT NULL COLLATE NOCASE,
    owner TEXT NOT NULL COLLATE NOCASE,
    name TEXT NOT NULL COLLATE NOCASE,
    source_id INTEGER REFERENCES sources (id) ON DELETE SET NULL,
    username TEXT,
    token_enc BLOB,
    interval_minutes INTEGER CHECK (interval_minutes IS NULL OR interval_minutes >= 1),
    wiki_enabled INTEGER NOT NULL DEFAULT 1,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'syncing', 'ok', 'error', 'unreachable')),
    last_sync_at INTEGER,
    last_success_at INTEGER,
    next_sync_at INTEGER NOT NULL,
    last_error TEXT,
    size_bytes INTEGER NOT NULL DEFAULT 0,
    default_branch TEXT,
    has_wiki INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    -- The archive path is derived from these three, so they must be unique.
    UNIQUE (host, owner, name)
);

CREATE INDEX repos_next_sync ON repos (next_sync_at);

CREATE TABLE sync_runs (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repos (id) ON DELETE CASCADE,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    outcome TEXT CHECK (outcome IN ('ok', 'error', 'unreachable')),
    refs_changed INTEGER NOT NULL DEFAULT 0,
    refs_rescued INTEGER NOT NULL DEFAULT 0,
    message TEXT
);

CREATE INDEX sync_runs_repo ON sync_runs (repo_id, started_at DESC);

-- Settings changed in the web UI. A row here overrides the environment value.
CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE sessions (
    token_hash BLOB PRIMARY KEY,
    csrf_token TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
