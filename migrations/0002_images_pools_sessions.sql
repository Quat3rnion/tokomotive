ALTER TABLE ledger ADD COLUMN image_input INTEGER NOT NULL DEFAULT 0;
ALTER TABLE ledger ADD COLUMN image_output INTEGER NOT NULL DEFAULT 0;
-- Index of the Provider Key in the pool that served the request.
ALTER TABLE ledger ADD COLUMN provider_key INTEGER;
-- Number of upstream attempts (more than 1 after a retry with the next key).
ALTER TABLE ledger ADD COLUMN attempts INTEGER NOT NULL DEFAULT 1;

ALTER TABLE rollup_hourly ADD COLUMN image_input INTEGER NOT NULL DEFAULT 0;
ALTER TABLE rollup_hourly ADD COLUMN image_output INTEGER NOT NULL DEFAULT 0;

CREATE TABLE sessions (
    token_hash TEXT PRIMARY KEY,
    expires_at INTEGER NOT NULL
);
