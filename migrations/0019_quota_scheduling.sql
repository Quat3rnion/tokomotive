ALTER TABLE providers ADD COLUMN key_strategy TEXT NOT NULL DEFAULT 'round_robin'
    CHECK (key_strategy IN ('round_robin','quota'));
CREATE TABLE provider_health_alerts (
    provider TEXT NOT NULL,
    account TEXT NOT NULL,
    state TEXT NOT NULL,
    emitted_at INTEGER NOT NULL,
    pending INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY(provider, account, state),
    FOREIGN KEY(provider,account) REFERENCES provider_health(provider,account) ON DELETE CASCADE ON UPDATE CASCADE
);
