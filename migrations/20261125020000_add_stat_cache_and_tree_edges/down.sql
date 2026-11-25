DROP INDEX IF EXISTS tree_edges_tree_idx;
DROP TABLE IF EXISTS tree_edges;
DROP TABLE IF EXISTS stat_cache;

CREATE TABLE IF NOT EXISTS gc_metadata (
    id        INTEGER PRIMARY KEY,
    hash_id   INTEGER,
    family_id INTEGER,
    gc_int    INTEGER,
    gc_vec    BLOB
);

CREATE UNIQUE INDEX IF NOT EXISTS GcMetadata_UniqueHashFamily ON gc_metadata(hash_id, family_id);

CREATE TABLE IF NOT EXISTS key_tree (
    node_id   INTEGER PRIMARY KEY,
    parent_id INTEGER,
    name      BLOB,
    FOREIGN KEY(parent_id) REFERENCES key_tree(node_id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS key_tree_unique_parent_id_name ON key_tree(parent_id, name);

CREATE TABLE IF NOT EXISTS key_data (
    node_id            INTEGER,
    committed          BOOLEAN,
    tag                INTEGER,
    created            INTEGER,
    modified           INTEGER,
    accessed           INTEGER,
    permissions        INTEGER,
    user_id            INTEGER,
    group_id           INTEGER,
    symbolic_link_path BLOB,
    file_size          INTEGER,
    hash               BLOB,
    hash_ref           BLOB,
    snapshot_ts_utc    INTEGER,
    dev                INTEGER,
    ino                INTEGER,
    ctime              INTEGER,
    ctime_nsec         INTEGER,
    mtime_nsec         INTEGER,
    PRIMARY KEY (node_id, committed) ON CONFLICT REPLACE,
    FOREIGN KEY(node_id) REFERENCES key_tree(node_id) ON DELETE CASCADE
);
