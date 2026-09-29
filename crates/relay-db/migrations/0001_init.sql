CREATE TABLE devices (
    ref INTEGER PRIMARY KEY,
    device_id BLOB NOT NULL UNIQUE CHECK (length(device_id) = 32),
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    created_at_ms INTEGER NOT NULL,
    last_seen_ms INTEGER
);

CREATE TABLE local_device (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    device_ref INTEGER NOT NULL REFERENCES devices(ref),
    next_sequence INTEGER NOT NULL
);

CREATE TABLE spaces (
    id BLOB PRIMARY KEY CHECK (length(id) = 16),
    name TEXT NOT NULL UNIQUE,
    created_at_ms INTEGER
);

CREATE TABLE mounts (
    id BLOB PRIMARY KEY CHECK (length(id) = 16),
    space_id BLOB NOT NULL REFERENCES spaces(id),
    name TEXT NOT NULL,
    created_at_ms INTEGER,
    UNIQUE (space_id, name)
);

CREATE TABLE device_mounts (
    device_ref INTEGER NOT NULL REFERENCES devices(ref),
    mount_id BLOB NOT NULL REFERENCES mounts(id),
    local_path TEXT NOT NULL,
    mode TEXT NOT NULL DEFAULT 'materialized',
    PRIMARY KEY (device_ref, mount_id)
);

CREATE TABLE mount_rules (
    mount_id BLOB NOT NULL REFERENCES mounts(id),
    position INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('include', 'exclude')),
    pattern TEXT NOT NULL,
    PRIMARY KEY (mount_id, position)
);

CREATE TABLE entries (
    id INTEGER PRIMARY KEY,
    mount_id BLOB NOT NULL REFERENCES mounts(id),
    path TEXT NOT NULL,
    kind TEXT CHECK (kind IS NULL OR kind IN ('file', 'directory', 'symlink')),
    deleted INTEGER NOT NULL CHECK (deleted IN (0, 1)),
    object_id BLOB CHECK (object_id IS NULL OR length(object_id) = 32),
    size INTEGER,
    executable INTEGER NOT NULL DEFAULT 0 CHECK (executable IN (0, 1)),
    symlink_target TEXT,
    parent_object BLOB CHECK (parent_object IS NULL OR length(parent_object) = 32),
    sequence INTEGER NOT NULL UNIQUE,
    modified_by INTEGER NOT NULL REFERENCES devices(ref),
    modified_at_ms INTEGER NOT NULL,
    stat_size INTEGER,
    stat_mtime_ns INTEGER,
    stat_file_id INTEGER,
    UNIQUE (mount_id, path)
);

CREATE INDEX idx_entries_sequence ON entries (sequence);

CREATE TABLE entry_versions (
    entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    device_ref INTEGER NOT NULL REFERENCES devices(ref),
    counter INTEGER NOT NULL CHECK (counter > 0),
    PRIMARY KEY (entry_id, device_ref)
);

CREATE TABLE objects (
    id BLOB PRIMARY KEY CHECK (length(id) = 32),
    size INTEGER NOT NULL,
    first_seen_ms INTEGER NOT NULL
);

CREATE TABLE history (
    id INTEGER PRIMARY KEY,
    entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL,
    kind TEXT,
    deleted INTEGER NOT NULL CHECK (deleted IN (0, 1)),
    object_id BLOB CHECK (object_id IS NULL OR length(object_id) = 32),
    size INTEGER,
    executable INTEGER NOT NULL CHECK (executable IN (0, 1)),
    symlink_target TEXT,
    parent_object BLOB CHECK (parent_object IS NULL OR length(parent_object) = 32),
    vector_json TEXT NOT NULL,
    modified_by INTEGER NOT NULL REFERENCES devices(ref),
    modified_at_ms INTEGER NOT NULL
);

CREATE INDEX idx_history_entry_sequence ON history (entry_id, sequence);
