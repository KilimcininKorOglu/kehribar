-- A repository with lfs_enabled fetches its Git LFS objects after every
-- fetch; lfs_size_bytes is the space they take in the archive.
ALTER TABLE repos ADD COLUMN lfs_enabled INTEGER NOT NULL DEFAULT 0 CHECK (lfs_enabled IN (0, 1));
ALTER TABLE repos ADD COLUMN lfs_size_bytes INTEGER NOT NULL DEFAULT 0;
