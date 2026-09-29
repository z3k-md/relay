CREATE TABLE mount_state (
    mount_id BLOB PRIMARY KEY REFERENCES mounts(id),
    last_scan_ms INTEGER,
    last_full_scan_ms INTEGER,
    last_error TEXT,
    last_error_ms INTEGER
);
