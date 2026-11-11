-- External images that the Markdown files of a repository show, kept in
-- the archive. The file is $DATA_DIR/assets/<sha256>, shared by every row
-- with the same content.
CREATE TABLE repo_assets (
    repo_id INTEGER NOT NULL REFERENCES repos (id) ON DELETE CASCADE,
    url TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    content_type TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    fetched_at INTEGER NOT NULL,
    PRIMARY KEY (repo_id, url)
);

CREATE INDEX repo_assets_sha256 ON repo_assets (sha256);

-- The last image check of a repository: how many images it found and how
-- many it left out because of the per-repository limit.
CREATE TABLE repo_asset_checks (
    repo_id INTEGER PRIMARY KEY REFERENCES repos (id) ON DELETE CASCADE,
    found INTEGER NOT NULL,
    skipped INTEGER NOT NULL,
    checked_at INTEGER NOT NULL
);
