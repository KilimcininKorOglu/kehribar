-- The archive size of each day (UTC), written by the first scheduler tick
-- of the day. Rows older than 400 days are deleted.
CREATE TABLE disk_usage_daily (
    day TEXT PRIMARY KEY,
    total_bytes INTEGER NOT NULL,
    lfs_bytes INTEGER NOT NULL,
    repo_count INTEGER NOT NULL
);
