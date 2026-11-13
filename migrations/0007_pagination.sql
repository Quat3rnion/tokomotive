-- Name constraints already provide unique name indexes on configuration tables.
CREATE INDEX users_team_id ON users(team_id, id);
CREATE INDEX virtual_keys_user_revoked_id ON virtual_keys(user_id, revoked_at, id);
CREATE INDEX virtual_keys_revoked_id ON virtual_keys(revoked_at, id);
CREATE INDEX virtual_keys_revoked_page ON virtual_keys(id) WHERE revoked_at IS NOT NULL;
CREATE INDEX ledger_user_id ON ledger(user_id, id);
CREATE INDEX ledger_provider_id ON ledger(provider, id);
CREATE INDEX ledger_model_id ON ledger(model, id);
CREATE INDEX ledger_requested_model_id ON ledger(requested_model, id);
CREATE INDEX ledger_estimated_id ON ledger(id) WHERE estimated = 1;
CREATE INDEX ledger_errors_id ON ledger(id) WHERE status >= 400;
CREATE INDEX cards_model_id ON cards(model_id, id);
