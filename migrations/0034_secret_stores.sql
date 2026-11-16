CREATE TABLE secret_stores (
    name TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK(kind IN ('vault','aws-sm','gcp-sm','bws','bw')),
    address TEXT NOT NULL,
    credentials TEXT NOT NULL,
    options TEXT NOT NULL DEFAULT '{}',
    allow_private INTEGER NOT NULL DEFAULT 0,
    is_default INTEGER NOT NULL DEFAULT 0,
    refresh_secs INTEGER NOT NULL DEFAULT 900 CHECK(refresh_secs BETWEEN 1 AND 86400)
);
CREATE UNIQUE INDEX secret_store_default ON secret_stores(kind) WHERE is_default=1;
