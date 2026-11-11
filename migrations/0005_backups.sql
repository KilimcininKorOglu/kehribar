-- The last bundle upload of each repository. refs_hash is the SHA-256 of
-- the ref list of the repository and its wiki at that upload; a backup
-- with the same hash is skipped.
CREATE TABLE repo_backups (
    repo_id INTEGER PRIMARY KEY REFERENCES repos (id) ON DELETE CASCADE,
    backed_up_at INTEGER NOT NULL,
    refs_hash TEXT NOT NULL
);
