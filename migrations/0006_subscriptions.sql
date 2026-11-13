ALTER TABLE providers ADD COLUMN auth TEXT NOT NULL DEFAULT 'api_key';
ALTER TABLE virtual_keys ADD COLUMN allowed_providers TEXT;
ALTER TABLE ledger ADD COLUMN subscription INTEGER NOT NULL DEFAULT 0;
CREATE TABLE oauth_credentials (
    provider TEXT NOT NULL REFERENCES providers(name) ON DELETE CASCADE ON UPDATE CASCADE,
    flow TEXT NOT NULL CHECK(flow IN ('chatgpt','claude','codex')),
    secret TEXT NOT NULL,
    PRIMARY KEY(provider, flow)
);
