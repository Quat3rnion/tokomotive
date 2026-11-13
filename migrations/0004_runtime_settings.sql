-- JSON keeps route and key-pool updates atomic. Secrets in keys_json are
-- AES-256-GCM values or explicit env: references, never plaintext keys.
CREATE TABLE providers (
    name TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    base_url TEXT NOT NULL,
    keys_json TEXT NOT NULL DEFAULT '[]'
);
CREATE TABLE routes (
    name TEXT PRIMARY KEY,
    config_json TEXT NOT NULL
);
