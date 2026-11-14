CREATE TABLE models_dev_providers (
    id TEXT PRIMARY KEY, name TEXT NOT NULL, api TEXT NOT NULL,
    npm TEXT NOT NULL, doc TEXT NOT NULL
);
CREATE TABLE models_dev_models (
    provider TEXT NOT NULL REFERENCES models_dev_providers(id),
    id TEXT NOT NULL, name TEXT NOT NULL, canonical_model_id TEXT NOT NULL,
    context INTEGER, input_limit INTEGER, output_limit INTEGER,
    details TEXT NOT NULL,
    PRIMARY KEY(provider,id)
);
CREATE INDEX models_dev_name ON models_dev_models(name COLLATE NOCASE);
CREATE INDEX models_dev_canonical ON models_dev_models(provider,canonical_model_id);
CREATE TABLE models_dev_costs (
    provider TEXT NOT NULL, model TEXT NOT NULL, spec TEXT NOT NULL,
    PRIMARY KEY(provider,model),
    FOREIGN KEY(provider,model) REFERENCES models_dev_models(provider,id)
);
ALTER TABLE models ADD COLUMN models_dev_provider TEXT;
ALTER TABLE models ADD COLUMN models_dev_model TEXT;
ALTER TABLE models ADD COLUMN cost_pinned INTEGER NOT NULL DEFAULT 0;
ALTER TABLE cards ADD COLUMN source TEXT NOT NULL DEFAULT 'manual';
ALTER TABLE providers ADD COLUMN preset TEXT;
CREATE TABLE models_dev_alerts (
    id INTEGER PRIMARY KEY,
    model_id INTEGER NOT NULL REFERENCES models(id),
    old_spec TEXT NOT NULL, new_spec TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
INSERT INTO settings(key,value) VALUES('models_dev_url','https://models.dev/api.json');
INSERT INTO settings(key,value) VALUES('models_dev_enabled','true');
