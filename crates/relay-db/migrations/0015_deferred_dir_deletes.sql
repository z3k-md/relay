-- Remote folder tombstones that could not be applied because the folder still
-- had something in it. They are retried after each applied batch and each
-- scan, so a folder whose last file goes away later is still removed (E-M5).
CREATE TABLE deferred_dir_deletes (
    mount_id BLOB NOT NULL REFERENCES mounts(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    vector_json TEXT NOT NULL,
    modified_by BLOB NOT NULL CHECK (length(modified_by) = 32),
    modified_at_ms INTEGER NOT NULL,
    sequence INTEGER NOT NULL,
    PRIMARY KEY (mount_id, path)
);
