-- Tokens for access without the admin password: `clone` tokens fetch over
-- /git/, `api` tokens call /api/v1. Only the SHA-256 of a token is stored.
CREATE TABLE access_tokens (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('clone', 'api')),
    name TEXT NOT NULL,
    token_hash BLOB NOT NULL UNIQUE,
    -- Clone tokens: every repository, else only the ones in access_token_repos.
    all_repos INTEGER NOT NULL DEFAULT 0,
    -- API tokens: changes allowed, else reading only.
    can_write INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    last_used_at INTEGER
);

CREATE TABLE access_token_repos (
    token_id INTEGER NOT NULL REFERENCES access_tokens (id) ON DELETE CASCADE,
    repo_id INTEGER NOT NULL REFERENCES repos (id) ON DELETE CASCADE,
    PRIMARY KEY (token_id, repo_id)
);

CREATE INDEX access_token_repos_repo ON access_token_repos (repo_id);
