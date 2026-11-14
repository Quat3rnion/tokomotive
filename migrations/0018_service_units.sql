-- Non-token usage is endpoint-defined. Keep it separate from TPM and tokens.
ALTER TABLE ledger ADD COLUMN service_units INTEGER NOT NULL DEFAULT 0;
ALTER TABLE rollup_hourly ADD COLUMN service_units INTEGER NOT NULL DEFAULT 0;
