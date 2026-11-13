-- Bind each session to the password hash that its login verified.
-- Existing sessions must log in again when a password is configured.
ALTER TABLE sessions ADD COLUMN password_hash TEXT;
