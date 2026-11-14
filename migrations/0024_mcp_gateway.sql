CREATE TABLE mcp_servers (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    url TEXT NOT NULL,
    auth TEXT NOT NULL DEFAULT 'none' CHECK(auth IN ('none','bearer','header')),
    header_name TEXT NOT NULL DEFAULT '',
    secret TEXT,
    allow_private INTEGER NOT NULL DEFAULT 0,
    enabled INTEGER NOT NULL DEFAULT 1,
    tools_json TEXT NOT NULL DEFAULT '[]'
);
ALTER TABLE virtual_keys ADD COLUMN mcp_all INTEGER NOT NULL DEFAULT 0;
ALTER TABLE virtual_keys ADD COLUMN mcp_allow TEXT NOT NULL DEFAULT '[]';
ALTER TABLE teams ADD COLUMN mcp_all INTEGER NOT NULL DEFAULT 0;
ALTER TABLE teams ADD COLUMN mcp_allow TEXT NOT NULL DEFAULT '[]';
CREATE TABLE mcp_calls (
    id INTEGER PRIMARY KEY,
    ts_ms INTEGER NOT NULL,
    key_id INTEGER NOT NULL,
    user_id INTEGER NOT NULL,
    server TEXT NOT NULL,
    tool TEXT NOT NULL,
    status TEXT NOT NULL,
    latency_ms INTEGER NOT NULL,
    error_class TEXT NOT NULL DEFAULT ''
);
CREATE INDEX mcp_calls_time ON mcp_calls(id DESC);
