ALTER TABLE providers ADD COLUMN proxy_mode TEXT NOT NULL DEFAULT 'global'
    CHECK (proxy_mode IN ('global', 'none', 'custom'));
ALTER TABLE providers ADD COLUMN proxy_url TEXT;
