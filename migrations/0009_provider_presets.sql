ALTER TABLE providers ADD COLUMN include_usage INTEGER NOT NULL DEFAULT 1;
ALTER TABLE providers ADD COLUMN key_header TEXT;
ALTER TABLE ledger ADD COLUMN reported_cost_micros INTEGER;
