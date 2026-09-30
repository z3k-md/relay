ALTER TABLE spaces ADD COLUMN policy_epoch INTEGER NOT NULL DEFAULT 0;

CREATE TABLE device_groups (
    name TEXT PRIMARY KEY,
    created_at_ms INTEGER NOT NULL
);

CREATE TABLE device_group_members (
    group_name TEXT NOT NULL REFERENCES device_groups(name) ON DELETE CASCADE,
    device_id BLOB NOT NULL,
    PRIMARY KEY (group_name, device_id)
);

CREATE TABLE replication_policies (
    id BLOB PRIMARY KEY,
    space_id BLOB NOT NULL REFERENCES spaces(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    selectors_json TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    UNIQUE (space_id, name)
);

CREATE TABLE replication_policy_peers (
    policy_id BLOB NOT NULL REFERENCES replication_policies(id) ON DELETE CASCADE,
    device_id BLOB NOT NULL,
    PRIMARY KEY (policy_id, device_id)
);

CREATE TABLE replication_policy_groups (
    policy_id BLOB NOT NULL REFERENCES replication_policies(id) ON DELETE CASCADE,
    group_name TEXT NOT NULL,
    PRIMARY KEY (policy_id, group_name)
);

CREATE TABLE peer_policy_snapshots (
    device_ref INTEGER NOT NULL REFERENCES devices(ref) ON DELETE CASCADE,
    space_id BLOB NOT NULL,
    epoch INTEGER NOT NULL,
    policies_json TEXT NOT NULL,
    PRIMARY KEY (device_ref, space_id)
);
