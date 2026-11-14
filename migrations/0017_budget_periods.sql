ALTER TABLE users ADD COLUMN budget_period TEXT NOT NULL DEFAULT 'none' CHECK(budget_period IN ('none','daily','weekly','monthly'));
UPDATE users SET budget_period='monthly' WHERE budget_monthly=1;
ALTER TABLE teams ADD COLUMN budget_micros INTEGER CHECK(budget_micros >= 0);
ALTER TABLE teams ADD COLUMN budget_period TEXT NOT NULL DEFAULT 'none' CHECK(budget_period IN ('none','daily','weekly','monthly'));
ALTER TABLE virtual_keys ADD COLUMN budget_micros INTEGER CHECK(budget_micros >= 0);
ALTER TABLE virtual_keys ADD COLUMN budget_period TEXT NOT NULL DEFAULT 'none' CHECK(budget_period IN ('none','daily','weekly','monthly'));
ALTER TABLE virtual_keys ADD COLUMN expires_at INTEGER;
CREATE INDEX ledger_key_ts ON ledger(key_id, ts_ms);
CREATE INDEX ledger_team_ts ON ledger(team_id, ts_ms);
CREATE TABLE alert_webhooks (
 id INTEGER PRIMARY KEY,
 url TEXT NOT NULL,
 host TEXT NOT NULL,
 format TEXT NOT NULL CHECK(format IN ('generic','discord','slack')),
 enabled INTEGER NOT NULL DEFAULT 1,
 last_status TEXT NOT NULL DEFAULT 'Not sent',
 last_delivery_at INTEGER
);
CREATE TABLE alert_dedupe (
 subject TEXT NOT NULL,
 threshold INTEGER NOT NULL,
 period_start INTEGER NOT NULL,
 PRIMARY KEY(subject, threshold, period_start)
);
