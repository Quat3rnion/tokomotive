-- A spool replay uses the same key even after an ambiguous commit or restart.
ALTER TABLE ledger ADD COLUMN write_key TEXT;
CREATE UNIQUE INDEX ledger_write_key ON ledger(write_key) WHERE write_key IS NOT NULL;
