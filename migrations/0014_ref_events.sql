-- The history of the branches and tags of each repository. `baseline`
-- records the state when the recording began; `rewritten` rows written
-- then from the kept commits know only the old commit (new_oid is NULL).
-- `deleted_at_source` and `back_at_source` keep the commit of the archive.
CREATE TABLE ref_events (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repos (id) ON DELETE CASCADE,
    run_id INTEGER REFERENCES sync_runs (id) ON DELETE SET NULL,
    ref TEXT NOT NULL,
    old_oid TEXT,
    new_oid TEXT,
    kind TEXT NOT NULL CHECK (kind IN ('baseline', 'created', 'moved', 'rewritten',
        'deleted_at_source', 'back_at_source')),
    at INTEGER NOT NULL
);

CREATE INDEX ref_events_repo_at ON ref_events (repo_id, at);
CREATE INDEX ref_events_run ON ref_events (run_id);

-- Branches and tags the archive keeps but the source no longer has.
CREATE TABLE source_missing_refs (
    repo_id INTEGER NOT NULL REFERENCES repos (id) ON DELETE CASCADE,
    ref TEXT NOT NULL,
    since INTEGER NOT NULL,
    PRIMARY KEY (repo_id, ref)
);
