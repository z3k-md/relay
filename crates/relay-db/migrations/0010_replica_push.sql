CREATE TABLE replica_push (
    space_id BLOB PRIMARY KEY REFERENCES spaces(id) ON DELETE CASCADE,
    pushed_seq INTEGER NOT NULL
);
