ALTER TABLE response_cache ADD COLUMN semantic_bucket TEXT;
ALTER TABLE response_cache ADD COLUMN vector BLOB;
CREATE INDEX response_cache_semantic ON response_cache(semantic_bucket,created_ms DESC);
ALTER TABLE ledger ADD COLUMN semantic_cache_hit INTEGER NOT NULL DEFAULT 0;
ALTER TABLE ledger ADD COLUMN semantic_cache_embed INTEGER NOT NULL DEFAULT 0;
