CREATE TABLE response_cache (
    key TEXT PRIMARY KEY,
    body TEXT NOT NULL,
    body_bytes INTEGER NOT NULL,
    created_ms INTEGER NOT NULL,
    expires_ms INTEGER NOT NULL
);
CREATE INDEX response_cache_expiry ON response_cache(expires_ms);
ALTER TABLE ledger ADD COLUMN cache_hit INTEGER NOT NULL DEFAULT 0;
