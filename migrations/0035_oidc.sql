ALTER TABLE admin_accounts ADD COLUMN local_login INTEGER NOT NULL DEFAULT 1;
CREATE TABLE admin_oidc_identities (
 issuer TEXT NOT NULL,
 subject TEXT NOT NULL,
 account_id INTEGER NOT NULL REFERENCES admin_accounts(id) ON DELETE CASCADE,
 PRIMARY KEY(issuer,subject)
);
CREATE INDEX admin_oidc_account ON admin_oidc_identities(account_id);
-- An OIDC-only Owner must not replace the final local recovery account.
CREATE TRIGGER protect_last_local_owner_delete BEFORE DELETE ON admin_accounts
 WHEN OLD.role='owner' AND OLD.local_login=1
 AND (SELECT count(*) FROM admin_accounts WHERE role='owner' AND local_login=1)=1
 BEGIN SELECT RAISE(ABORT,'Keep at least one Owner with a local password.'); END;
CREATE TRIGGER protect_last_local_owner_update BEFORE UPDATE OF role,local_login ON admin_accounts
 WHEN OLD.role='owner' AND OLD.local_login=1 AND (NEW.role!='owner' OR NEW.local_login!=1)
 AND (SELECT count(*) FROM admin_accounts WHERE role='owner' AND local_login=1)=1
 BEGIN SELECT RAISE(ABORT,'Keep at least one Owner with a local password.'); END;
