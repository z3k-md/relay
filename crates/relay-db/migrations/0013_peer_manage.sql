-- Whether this peer may manage this device: browse its folders and set up
-- sync on it (D37). Set only on purpose, never by membership adoption.
ALTER TABLE peers ADD COLUMN may_manage INTEGER NOT NULL DEFAULT 0
    CHECK (may_manage IN (0, 1));
