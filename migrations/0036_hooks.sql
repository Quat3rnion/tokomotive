CREATE TABLE hooks (
 id INTEGER PRIMARY KEY,
 name TEXT NOT NULL UNIQUE CHECK(length(name) BETWEEN 1 AND 100),
 url TEXT NOT NULL,
 pre_request INTEGER NOT NULL DEFAULT 0,
 post_response INTEGER NOT NULL DEFAULT 0,
 event INTEGER NOT NULL DEFAULT 0,
 include_bodies INTEGER NOT NULL DEFAULT 0,
 timeout_ms INTEGER NOT NULL DEFAULT 2000 CHECK(timeout_ms BETWEEN 1 AND 30000),
 fail_closed INTEGER NOT NULL DEFAULT 0,
 enabled INTEGER NOT NULL DEFAULT 1,
 allow_private INTEGER NOT NULL DEFAULT 0,
 priority INTEGER NOT NULL DEFAULT 0,
 secret TEXT NOT NULL
);
CREATE TABLE hook_attachments (
 id INTEGER PRIMARY KEY,
 hook_id INTEGER NOT NULL REFERENCES hooks(id) ON DELETE CASCADE,
 route TEXT REFERENCES routes(name) ON DELETE CASCADE,
 team_id INTEGER REFERENCES teams(id) ON DELETE CASCADE,
 key_id INTEGER REFERENCES virtual_keys(id) ON DELETE CASCADE,
 global INTEGER NOT NULL DEFAULT 0,
 CHECK((route IS NOT NULL)+(team_id IS NOT NULL)+(key_id IS NOT NULL)+global=1)
);
CREATE INDEX hook_attachments_hook ON hook_attachments(hook_id);
CREATE TABLE hook_calls (
 id INTEGER PRIMARY KEY,
 ledger_id INTEGER REFERENCES ledger(id) ON DELETE CASCADE,
 request_id TEXT NOT NULL,
 hook TEXT NOT NULL,
 stage TEXT NOT NULL,
 outcome TEXT NOT NULL,
 ms INTEGER NOT NULL,
 status INTEGER,
 ts_ms INTEGER NOT NULL
);
CREATE INDEX hook_calls_request ON hook_calls(request_id);
CREATE INDEX hook_calls_ledger ON hook_calls(ledger_id);
CREATE TRIGGER hook_calls_bound AFTER INSERT ON hook_calls BEGIN
 DELETE FROM hook_calls WHERE id <= NEW.id-10000;
END;
