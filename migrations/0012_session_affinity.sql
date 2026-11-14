ALTER TABLE ledger ADD COLUMN session_affinity TEXT
    CHECK (session_affinity IS NULL OR session_affinity IN ('new', 'pinned', 're-pinned'));
