CREATE TABLE admin_accounts (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 username TEXT NOT NULL UNIQUE COLLATE NOCASE,
 password_hash TEXT NOT NULL,
 role TEXT NOT NULL CHECK(role IN ('owner','admin','viewer')),
 session_version INTEGER NOT NULL DEFAULT 0,
 created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
INSERT INTO admin_accounts(username,password_hash,role)
 SELECT 'admin',value,'owner' FROM settings WHERE key='admin_password_hash';
ALTER TABLE sessions ADD COLUMN account_id INTEGER REFERENCES admin_accounts(id) ON DELETE CASCADE;
ALTER TABLE sessions ADD COLUMN account_version INTEGER NOT NULL DEFAULT 0;
UPDATE sessions SET account_id=(SELECT id FROM admin_accounts WHERE username='admin')
 WHERE password_hash IS (SELECT value FROM settings WHERE key='admin_password_hash');
DELETE FROM sessions WHERE account_id IS NULL;
CREATE INDEX sessions_account ON sessions(account_id);
CREATE TRIGGER protect_last_owner_delete BEFORE DELETE ON admin_accounts
 WHEN OLD.role='owner' AND (SELECT count(*) FROM admin_accounts WHERE role='owner')=1
 BEGIN SELECT RAISE(ABORT,'Keep at least one Owner account.'); END;
CREATE TRIGGER protect_last_owner_role BEFORE UPDATE OF role ON admin_accounts
 WHEN OLD.role='owner' AND NEW.role!='owner' AND (SELECT count(*) FROM admin_accounts WHERE role='owner')=1
 BEGIN SELECT RAISE(ABORT,'Keep at least one Owner account.'); END;
CREATE TRIGGER invalidate_admin_sessions AFTER UPDATE OF role,password_hash ON admin_accounts
 WHEN OLD.role!=NEW.role OR OLD.password_hash!=NEW.password_hash
 BEGIN
  DELETE FROM sessions WHERE account_id=NEW.id;
  UPDATE admin_accounts SET session_version=session_version+1 WHERE id=NEW.id;
 END;
-- Preserve the environment recovery override and old session helpers.
-- This setting is not a separate login identity.
CREATE TRIGGER legacy_admin_insert AFTER INSERT ON settings WHEN NEW.key='admin_password_hash'
 BEGIN
  INSERT INTO admin_accounts(username,password_hash,role) VALUES('admin',NEW.value,'owner')
   ON CONFLICT(username) DO UPDATE SET password_hash=excluded.password_hash,role='owner';
 END;
CREATE TRIGGER legacy_admin_update AFTER UPDATE OF value ON settings
 WHEN NEW.key='admin_password_hash' AND OLD.value!=NEW.value
 BEGIN
  INSERT INTO admin_accounts(username,password_hash,role) VALUES('admin',NEW.value,'owner')
   ON CONFLICT(username) DO UPDATE SET password_hash=excluded.password_hash,role='owner';
 END;
CREATE TABLE admin_audit (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 time INTEGER NOT NULL DEFAULT (unixepoch()),
 account_id INTEGER,
 account TEXT NOT NULL,
 action TEXT NOT NULL,
 target_type TEXT NOT NULL,
 target_id TEXT NOT NULL,
 summary TEXT NOT NULL,
 before_json TEXT NOT NULL,
 after_json TEXT NOT NULL
);
CREATE INDEX admin_audit_time ON admin_audit(time,id);
CREATE INDEX admin_audit_account ON admin_audit(account_id,id);
INSERT INTO settings(key,value) VALUES('audit_retention_days','365');
