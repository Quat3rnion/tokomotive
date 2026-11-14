CREATE TABLE request_attempts (
 ledger_id INTEGER NOT NULL REFERENCES ledger(id) ON DELETE CASCADE,
 attempt_index INTEGER NOT NULL,
 route TEXT NOT NULL,
 deployment TEXT NOT NULL,
 provider TEXT NOT NULL,
 system TEXT NOT NULL,
 provider_key INTEGER,
 key_last4 TEXT,
 status INTEGER,
 error_class TEXT NOT NULL DEFAULT '',
 skip_reason TEXT NOT NULL DEFAULT '',
 started_ms INTEGER NOT NULL,
 latency_ms INTEGER NOT NULL,
 ttft_ms INTEGER,
 PRIMARY KEY(ledger_id,attempt_index)
);
CREATE INDEX request_attempts_error ON request_attempts(error_class,ledger_id);
CREATE INDEX request_attempts_key ON request_attempts(provider_key,ledger_id);
CREATE INDEX ledger_status_id ON ledger(status,id);
CREATE INDEX ledger_attempts_id ON ledger(attempts,id);
CREATE TABLE captured_bodies (
 ledger_id INTEGER PRIMARY KEY REFERENCES ledger(id) ON DELETE CASCADE,
 created_ms INTEGER NOT NULL,
 request TEXT NOT NULL,
 response TEXT NOT NULL,
 request_truncated INTEGER NOT NULL,
 response_truncated INTEGER NOT NULL
);
CREATE INDEX captured_bodies_time ON captured_bodies(created_ms);
ALTER TABLE routes ADD COLUMN capture_bodies INTEGER NOT NULL DEFAULT 0;
ALTER TABLE virtual_keys ADD COLUMN capture_bodies INTEGER NOT NULL DEFAULT 0;
