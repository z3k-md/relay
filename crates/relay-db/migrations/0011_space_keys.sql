-- Per-space keys wrapped for a device or for the recovery secret (D30).
-- `recipient` is the device id, or 32 zero bytes for a recovery wrap.
CREATE TABLE space_key_wraps (
    space_id BLOB NOT NULL,
    generation INTEGER NOT NULL,
    purpose TEXT NOT NULL CHECK (purpose IN ('device', 'recovery')),
    recipient BLOB NOT NULL CHECK (length(recipient) = 32),
    wrapped BLOB NOT NULL,
    PRIMARY KEY (space_id, generation, purpose, recipient)
);

-- Peers' X25519 box public keys, verified against their device id.
CREATE TABLE peer_box_keys (
    device_id BLOB PRIMARY KEY CHECK (length(device_id) = 32),
    public_key BLOB NOT NULL CHECK (length(public_key) = 32)
);
