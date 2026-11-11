-- Until this time no API request goes to the host, because it answered
-- with a rate limit.
ALTER TABLE hosts ADD COLUMN api_blocked_until INTEGER;

-- A sync that a rate limit postponed is recorded as 'deferred'. SQLite
-- cannot change a CHECK constraint, so the table is rebuilt; nothing
-- references it.
CREATE TABLE sync_runs_new (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repos (id) ON DELETE CASCADE,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    outcome TEXT CHECK (outcome IN ('ok', 'error', 'unreachable', 'deferred')),
    refs_changed INTEGER NOT NULL DEFAULT 0,
    refs_rescued INTEGER NOT NULL DEFAULT 0,
    message TEXT
);

INSERT INTO sync_runs_new (
    id, repo_id, started_at, finished_at, outcome, refs_changed, refs_rescued, message
)
SELECT id, repo_id, started_at, finished_at, outcome, refs_changed, refs_rescued, message
FROM sync_runs;

DROP TABLE sync_runs;
ALTER TABLE sync_runs_new RENAME TO sync_runs;
CREATE INDEX sync_runs_repo ON sync_runs (repo_id, started_at DESC);
