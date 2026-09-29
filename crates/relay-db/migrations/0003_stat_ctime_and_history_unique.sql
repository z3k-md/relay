ALTER TABLE entries ADD COLUMN stat_ctime_ns INTEGER;

-- Earlier versions could append the same (entry, sequence) twice; keep the
-- first so the unique index below can be created on existing databases.
DELETE FROM history
WHERE id NOT IN (SELECT MIN(id) FROM history GROUP BY entry_id, sequence);

DROP INDEX IF EXISTS idx_history_entry_sequence;
CREATE UNIQUE INDEX idx_history_entry_sequence ON history (entry_id, sequence);
