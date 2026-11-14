CREATE TABLE provider_health (
    provider TEXT NOT NULL REFERENCES providers(name) ON DELETE CASCADE ON UPDATE CASCADE,
    account TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('ok','rate_limited','credentials_invalid')),
    until_at INTEGER,
    observed_at INTEGER NOT NULL,
    last_failure TEXT,
    failure_at INTEGER,
    PRIMARY KEY(provider, account)
);
CREATE TABLE provider_quota (
    provider TEXT NOT NULL,
    account TEXT NOT NULL,
    window TEXT NOT NULL,
    used_percent REAL,
    remaining REAL,
    quota_limit REAL,
    reset_at INTEGER,
    observed_at INTEGER NOT NULL,
    PRIMARY KEY(provider, account, window),
    FOREIGN KEY(provider, account) REFERENCES provider_health(provider, account) ON DELETE CASCADE ON UPDATE CASCADE
);
