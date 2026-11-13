ALTER TABLE virtual_keys ADD COLUMN rpm_limit INTEGER CHECK (rpm_limit >= 0);
ALTER TABLE virtual_keys ADD COLUMN tpm_limit INTEGER CHECK (tpm_limit >= 0);
ALTER TABLE users ADD COLUMN rpm_limit INTEGER CHECK (rpm_limit >= 0);
ALTER TABLE users ADD COLUMN tpm_limit INTEGER CHECK (tpm_limit >= 0);
