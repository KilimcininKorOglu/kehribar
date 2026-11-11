-- Scans of a source with use_ssh add the SSH clone URL of each repository
-- instead of the HTTPS one; the archive path stays the same.
ALTER TABLE sources ADD COLUMN use_ssh INTEGER NOT NULL DEFAULT 0 CHECK (use_ssh IN (0, 1));
