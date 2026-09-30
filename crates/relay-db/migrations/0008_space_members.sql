ALTER TABLE peer_offers ADD COLUMN members_json TEXT NOT NULL DEFAULT '[]';

CREATE TABLE dismissed_peers (
    device_id BLOB PRIMARY KEY
);
