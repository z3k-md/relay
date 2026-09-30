CREATE TABLE delete_holds (
    device_ref INTEGER NOT NULL REFERENCES devices(ref),
    space_id BLOB NOT NULL REFERENCES spaces(id),
    mount_id BLOB NOT NULL REFERENCES mounts(id),
    deletions INTEGER NOT NULL,
    live INTEGER NOT NULL,
    held_at_ms INTEGER NOT NULL,
    decision TEXT,
    decided_at_ms INTEGER,
    PRIMARY KEY (device_ref, space_id, mount_id)
);

CREATE TABLE delete_hold_paths (
    device_ref INTEGER NOT NULL,
    space_id BLOB NOT NULL,
    mount_id BLOB NOT NULL,
    path TEXT NOT NULL,
    PRIMARY KEY (device_ref, space_id, mount_id, path),
    FOREIGN KEY (device_ref, space_id, mount_id)
        REFERENCES delete_holds(device_ref, space_id, mount_id)
        ON DELETE CASCADE
);
