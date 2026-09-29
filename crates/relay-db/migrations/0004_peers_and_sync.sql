CREATE TABLE peers (
    device_ref INTEGER PRIMARY KEY REFERENCES devices(ref),
    name TEXT NOT NULL UNIQUE,
    addresses TEXT NOT NULL,
    added_at_ms INTEGER NOT NULL
);

CREATE TABLE space_shares (
    space_id BLOB NOT NULL REFERENCES spaces(id),
    device_ref INTEGER NOT NULL REFERENCES devices(ref),
    PRIMARY KEY (space_id, device_ref)
);

CREATE INDEX idx_space_shares_device ON space_shares (device_ref);

CREATE TABLE peer_offers (
    device_ref INTEGER NOT NULL REFERENCES devices(ref),
    space_id BLOB NOT NULL,
    name TEXT NOT NULL,
    mounts_json TEXT NOT NULL,
    received_at_ms INTEGER NOT NULL,
    PRIMARY KEY (device_ref, space_id)
);

CREATE TABLE sync_progress (
    device_ref INTEGER NOT NULL REFERENCES devices(ref),
    space_id BLOB NOT NULL REFERENCES spaces(id),
    received_seq INTEGER NOT NULL DEFAULT 0,
    acked_seq INTEGER NOT NULL DEFAULT 0,
    last_sync_ms INTEGER,
    PRIMARY KEY (device_ref, space_id)
);

CREATE INDEX idx_entries_mount_sequence ON entries (mount_id, sequence);
