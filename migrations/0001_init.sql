CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE teams (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);

CREATE TABLE users (
    id             INTEGER PRIMARY KEY,
    name           TEXT NOT NULL UNIQUE,
    team_id        INTEGER REFERENCES teams(id),
    budget_micros  INTEGER,
    budget_monthly INTEGER NOT NULL DEFAULT 0,
    disabled       INTEGER NOT NULL DEFAULT 0,
    created_at     INTEGER NOT NULL
);

CREATE TABLE virtual_keys (
    id             INTEGER PRIMARY KEY,
    user_id        INTEGER NOT NULL REFERENCES users(id),
    key_hash       TEXT NOT NULL UNIQUE,
    prefix         TEXT NOT NULL,
    label          TEXT NOT NULL DEFAULT '',
    allowed_models TEXT,              -- JSON array of Model names, NULL = all
    revoked_at     INTEGER,
    last_used_at   INTEGER,
    created_at     INTEGER NOT NULL
);

CREATE TABLE models (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    provider   TEXT NOT NULL,
    aliases    TEXT NOT NULL DEFAULT '[]', -- JSON array
    created_at INTEGER NOT NULL
);

-- One row per Card Version. The current card for (model, team, kind) is the
-- row with the highest id. Rows are never updated.
CREATE TABLE cards (
    id         INTEGER PRIMARY KEY,
    model_id   INTEGER NOT NULL REFERENCES models(id),
    team_id    INTEGER REFERENCES teams(id),
    kind       TEXT NOT NULL CHECK (kind IN ('cost', 'price')),
    spec       TEXT NOT NULL,         -- JSON: CardRates for cost, PriceSpec for price
    created_at INTEGER NOT NULL
);
CREATE INDEX cards_lookup ON cards(model_id, team_id, kind, id);

-- One row per request. Amounts are frozen at write time (ADR 0002).
CREATE TABLE ledger (
    id              INTEGER PRIMARY KEY,
    ts_ms           INTEGER NOT NULL,
    user_id         INTEGER NOT NULL REFERENCES users(id),
    key_id          INTEGER NOT NULL REFERENCES virtual_keys(id),
    team_id         INTEGER REFERENCES teams(id),
    provider        TEXT NOT NULL,
    endpoint        TEXT NOT NULL,
    requested_model TEXT,
    model           TEXT,
    model_id        INTEGER REFERENCES models(id),
    status          INTEGER NOT NULL,
    streamed        INTEGER NOT NULL,
    input           INTEGER NOT NULL,
    output          INTEGER NOT NULL,
    cache_read      INTEGER NOT NULL,
    cache_write_5m  INTEGER NOT NULL,
    cache_write_1h  INTEGER NOT NULL,
    cost_micros     INTEGER NOT NULL,
    price_micros    INTEGER NOT NULL,
    cost_card_id    INTEGER REFERENCES cards(id),
    price_card_id   INTEGER REFERENCES cards(id),
    unpriced        INTEGER NOT NULL,
    estimated       INTEGER NOT NULL,
    latency_ms      INTEGER NOT NULL,
    ttft_ms         INTEGER,
    client_aborted  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX ledger_ts ON ledger(ts_ms);
CREATE INDEX ledger_user_ts ON ledger(user_id, ts_ms);
CREATE INDEX ledger_unpriced ON ledger(unpriced) WHERE unpriced = 1;

-- Hourly aggregates of the ledger, for the dashboard and Budget checks.
CREATE TABLE rollup_hourly (
    hour           INTEGER NOT NULL,  -- unix seconds, start of the hour (UTC)
    user_id        INTEGER NOT NULL,
    model          TEXT NOT NULL,     -- response model, or '' when unknown
    requests       INTEGER NOT NULL,
    errors         INTEGER NOT NULL,
    input          INTEGER NOT NULL,
    output         INTEGER NOT NULL,
    cache_read     INTEGER NOT NULL,
    cache_write_5m INTEGER NOT NULL,
    cache_write_1h INTEGER NOT NULL,
    cost_micros    INTEGER NOT NULL,
    price_micros   INTEGER NOT NULL,
    latency_ms_sum INTEGER NOT NULL,
    ttft_ms_sum    INTEGER NOT NULL,
    ttft_count     INTEGER NOT NULL,
    PRIMARY KEY (hour, user_id, model)
);
CREATE INDEX rollup_user_hour ON rollup_hourly(user_id, hour);
