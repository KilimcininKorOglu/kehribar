-- The push webhook of a repository: /hooks/<token> starts a sync. The
-- token is found by its SHA-256 and kept encrypted, so the repository page
-- can show the URL again.
ALTER TABLE repos ADD COLUMN webhook_hash BLOB;
ALTER TABLE repos ADD COLUMN webhook_enc BLOB;

CREATE UNIQUE INDEX repos_webhook_hash ON repos (webhook_hash) WHERE webhook_hash IS NOT NULL;

-- A sync asked for while one runs: the repository syncs again when the
-- running sync finishes.
ALTER TABLE repos ADD COLUMN sync_requested INTEGER NOT NULL DEFAULT 0;
