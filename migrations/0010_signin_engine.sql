CREATE TABLE oauth_credentials_new (
    provider TEXT NOT NULL REFERENCES providers(name) ON DELETE CASCADE ON UPDATE CASCADE,
    flow TEXT NOT NULL,
    secret TEXT NOT NULL,
    PRIMARY KEY(provider, flow)
);
INSERT INTO oauth_credentials_new SELECT * FROM oauth_credentials;
DROP TABLE oauth_credentials;
ALTER TABLE oauth_credentials_new RENAME TO oauth_credentials;
