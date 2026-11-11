-- Results of the scheduled integrity check (git fsck) and repack.
ALTER TABLE repos ADD COLUMN last_fsck_at INTEGER;
ALTER TABLE repos ADD COLUMN fsck_error TEXT;
ALTER TABLE repos ADD COLUMN last_repack_at INTEGER;
ALTER TABLE repos ADD COLUMN repack_saved_bytes INTEGER;
