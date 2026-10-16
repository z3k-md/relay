-- Add the `store` materialization mode (D47): bytes in the object store, no
-- working-tree file. SQLite cannot change a CHECK in place, so the rule
-- tables are rebuilt. Selectors are set aside first: with foreign keys on,
-- dropping the rules table would cascade-delete them.
CREATE TABLE materialization_selectors_copy AS
    SELECT rule_id, position, pattern FROM materialization_selectors;
DROP TABLE materialization_selectors;

CREATE TABLE materialization_rules_new (
    id BLOB PRIMARY KEY CHECK (length(id) = 16),
    space_id BLOB NOT NULL REFERENCES spaces(id),
    name TEXT NOT NULL,
    mode TEXT NOT NULL CHECK (mode IN ('full', 'metadata', 'demand', 'exclude', 'store')),
    position INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    UNIQUE (space_id, name)
);
INSERT INTO materialization_rules_new (id, space_id, name, mode, position, created_at_ms)
    SELECT id, space_id, name, mode, position, created_at_ms FROM materialization_rules;
DROP TABLE materialization_rules;
ALTER TABLE materialization_rules_new RENAME TO materialization_rules;

CREATE TABLE materialization_selectors (
    rule_id BLOB NOT NULL REFERENCES materialization_rules(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    pattern TEXT NOT NULL,
    PRIMARY KEY (rule_id, position)
);
INSERT INTO materialization_selectors (rule_id, position, pattern)
    SELECT rule_id, position, pattern FROM materialization_selectors_copy;
DROP TABLE materialization_selectors_copy;
