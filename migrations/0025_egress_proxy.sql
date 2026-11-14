ALTER TABLE providers ADD COLUMN proxy_mode TEXT NOT NULL DEFAULT 'global'
    CHECK (proxy_mode IN ('global', 'none', 'custom'));
ALTER TABLE providers ADD COLUMN proxy_url TEXT;
-- Names can change or be reused. In-flight credentials belong to one row.
ALTER TABLE providers ADD COLUMN egress_identity TEXT NOT NULL DEFAULT '';
UPDATE providers SET egress_identity=lower(hex(randomblob(16)));
CREATE TRIGGER provider_egress_identity AFTER INSERT ON providers
BEGIN
    UPDATE providers SET egress_identity=lower(hex(randomblob(16))) WHERE name=NEW.name;
END;
