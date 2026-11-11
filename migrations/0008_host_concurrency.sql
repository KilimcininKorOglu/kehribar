-- Syncs that may run at once against one host. NULL leaves only the global
-- concurrency setting.
ALTER TABLE hosts ADD COLUMN max_concurrency INTEGER
    CHECK (max_concurrency IS NULL OR max_concurrency >= 1);
