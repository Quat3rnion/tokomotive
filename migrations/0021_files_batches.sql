CREATE TABLE proxy_objects (
    kind TEXT NOT NULL CHECK (kind IN ('file','batch')),
    id TEXT NOT NULL,
    key_id INTEGER NOT NULL REFERENCES virtual_keys(id),
    provider TEXT NOT NULL,
    provider_key INTEGER NOT NULL,
    credential_hash TEXT NOT NULL,
    base_url TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    status TEXT NOT NULL,
    object_json TEXT NOT NULL,
    manifest_json TEXT,
    PRIMARY KEY(kind,id)
);
CREATE INDEX proxy_objects_owner ON proxy_objects(key_id,kind,created_at,id);

-- A durable Reservation is written before the upstream accepts a batch.
CREATE TABLE batch_jobs (
    id TEXT PRIMARY KEY,
    key_id INTEGER NOT NULL REFERENCES virtual_keys(id),
    user_id INTEGER NOT NULL REFERENCES users(id),
    team_id INTEGER REFERENCES teams(id),
    held_micros INTEGER NOT NULL CHECK (held_micros >= 0),
    snapshot_json TEXT NOT NULL,
    simple INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    last_poll INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL,
    accounted INTEGER NOT NULL DEFAULT 0 CHECK (accounted IN (0,1))
);
CREATE INDEX batch_jobs_poll ON batch_jobs(accounted,last_poll);
CREATE INDEX batch_jobs_key ON batch_jobs(key_id);
CREATE INDEX batch_jobs_user ON batch_jobs(user_id);
CREATE INDEX batch_jobs_team ON batch_jobs(team_id);

CREATE TABLE batch_entries (
    batch_id TEXT NOT NULL REFERENCES batch_jobs(id),
    custom_id TEXT NOT NULL,
    ledger_id INTEGER NOT NULL UNIQUE REFERENCES ledger(id),
    PRIMARY KEY(batch_id,custom_id)
);
