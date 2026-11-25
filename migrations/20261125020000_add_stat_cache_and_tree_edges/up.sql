DROP TABLE IF EXISTS gc_metadata;
DROP TABLE IF EXISTS key_data;
DROP TABLE IF EXISTS key_tree;

CREATE TABLE stat_cache (
    family_id       INTEGER NOT NULL,
    rel_path        BLOB NOT NULL,
    kind            INTEGER NOT NULL,
    created         INTEGER,
    modified        INTEGER,
    accessed        INTEGER,
    permissions     INTEGER,
    user_id         INTEGER,
    group_id        INTEGER,
    file_size       INTEGER,
    dev             INTEGER,
    ino             INTEGER,
    ctime           INTEGER,
    ctime_nsec      INTEGER,
    mtime_nsec      INTEGER,
    snapshot_ts_utc INTEGER NOT NULL,
    hash            BLOB,
    hash_ref        BLOB,
    symlink_target  BLOB,
    PRIMARY KEY (family_id, rel_path)
);

CREATE TABLE tree_edges (
    tree_hash_id  INTEGER NOT NULL,
    child_hash_id INTEGER NOT NULL,
    PRIMARY KEY (tree_hash_id, child_hash_id)
);

CREATE INDEX tree_edges_tree_idx ON tree_edges (tree_hash_id);
