CREATE TABLE guardrail_sets (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    rules TEXT NOT NULL DEFAULT '[]',
    hook_url TEXT NOT NULL DEFAULT '',
    fail_closed INTEGER NOT NULL DEFAULT 0,
    max_text_bytes INTEGER NOT NULL DEFAULT 1048576
);
CREATE TABLE guardrail_attachments (
    id INTEGER PRIMARY KEY,
    set_id INTEGER NOT NULL REFERENCES guardrail_sets(id) ON DELETE CASCADE,
    route TEXT REFERENCES routes(name) ON DELETE CASCADE ON UPDATE CASCADE,
    team_id INTEGER REFERENCES teams(id) ON DELETE CASCADE,
    key_id INTEGER REFERENCES virtual_keys(id) ON DELETE CASCADE,
    CHECK ((route IS NOT NULL) + (team_id IS NOT NULL) + (key_id IS NOT NULL) = 1)
);
CREATE INDEX guardrail_attach_route ON guardrail_attachments(route);
CREATE INDEX guardrail_attach_team ON guardrail_attachments(team_id);
CREATE INDEX guardrail_attach_key ON guardrail_attachments(key_id);
CREATE TABLE guardrail_events (
    ledger_id INTEGER PRIMARY KEY REFERENCES ledger(id) ON DELETE CASCADE,
    rule_names TEXT NOT NULL
);
