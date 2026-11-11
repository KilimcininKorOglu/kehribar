-- Repository name globs of a source, one per line, and whether archived
-- repositories are left out. They decide only what a scan adds; archived
-- copies stay.
ALTER TABLE sources ADD COLUMN include_patterns TEXT;
ALTER TABLE sources ADD COLUMN exclude_patterns TEXT;
ALTER TABLE sources ADD COLUMN skip_archived INTEGER NOT NULL DEFAULT 0;
