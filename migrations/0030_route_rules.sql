ALTER TABLE routes ADD COLUMN rules TEXT NOT NULL DEFAULT '[]';
ALTER TABLE ledger ADD COLUMN route_rule TEXT;
ALTER TABLE ledger ADD COLUMN route_rule_name TEXT;
ALTER TABLE ledger ADD COLUMN canary_side TEXT;
CREATE INDEX ledger_route_rule_id ON ledger(route_rule,id);
CREATE INDEX ledger_route_rule_name ON ledger(route_rule_name,id);
ALTER TABLE request_attempts ADD COLUMN route_rule TEXT;
ALTER TABLE request_attempts ADD COLUMN route_rule_name TEXT;
ALTER TABLE request_attempts ADD COLUMN canary_side TEXT;
