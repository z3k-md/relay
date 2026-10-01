CREATE TABLE materialization_rules (
    id BLOB PRIMARY KEY CHECK (length(id) = 16),
    space_id BLOB NOT NULL REFERENCES spaces(id),
    name TEXT NOT NULL,
    mode TEXT NOT NULL CHECK (mode IN ('full', 'metadata', 'demand', 'exclude')),
    position INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    UNIQUE (space_id, name)
);

CREATE TABLE materialization_selectors (
    rule_id BLOB NOT NULL REFERENCES materialization_rules(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    pattern TEXT NOT NULL,
    PRIMARY KEY (rule_id, position)
);

ALTER TABLE entries ADD COLUMN materialized INTEGER NOT NULL DEFAULT 1
    CHECK (materialized IN (0, 1));

-- Hydration only looks at index-only rows. A metadata-heavy mount must not
-- make that lookup scan every materialized file.
CREATE INDEX idx_entries_index_only ON entries (mount_id)
    WHERE materialized = 0 AND deleted = 0;
