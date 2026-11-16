CREATE TABLE exporters (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL CHECK(kind IN ('datadog','maxim')),
    site TEXT NOT NULL DEFAULT 'US1',
    endpoint TEXT NOT NULL DEFAULT '',
    repo_id TEXT NOT NULL DEFAULT '',
    spans INTEGER NOT NULL DEFAULT 1,
    metrics INTEGER NOT NULL DEFAULT 0,
    enabled INTEGER NOT NULL DEFAULT 0,
    sample_rate REAL NOT NULL DEFAULT 1 CHECK(sample_rate BETWEEN 0 AND 1),
    include_bodies INTEGER NOT NULL DEFAULT 0,
    tag_allowlist TEXT NOT NULL DEFAULT '',
    batch_size INTEGER NOT NULL DEFAULT 16 CHECK(batch_size BETWEEN 1 AND 32),
    interval_ms INTEGER NOT NULL DEFAULT 1000 CHECK(interval_ms BETWEEN 10 AND 30000),
    allow_private INTEGER NOT NULL DEFAULT 0,
    api_key TEXT NOT NULL
);
