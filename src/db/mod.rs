// Copyright 2014 Google Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Communication with SQLite.

use crate::blob;

use crate::models;
use crate::cbor_util;

use diesel::connection::TransactionManager;
use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;
use diesel_migrations::MigrationHarness;
use crate::errors::DieselError;

use crate::hash;
use std::sync::{Mutex, MutexGuard};
use crate::tags;
use std::time::Duration;
use crate::util::{Counter, PeriodicTimer};

mod schema;
mod sqlite;

pub(crate) use sqlite::open_connection;

pub struct Index(Mutex<InternalIndex>);
pub type IndexGuard<'a> = MutexGuard<'a, InternalIndex>;

impl Index {
    pub fn new(path: &str) -> Result<Index, DieselError> {
        Ok(Index(Mutex::new(InternalIndex::new(path)?)))
    }
    pub fn lock(&self) -> MutexGuard<'_, InternalIndex> {
        self.0.lock().expect("Database mutex is poisoned")
    }
    #[cfg(test)]
    pub fn new_for_testing() -> Index {
        Index(Mutex::new(InternalIndex::new(":memory:").unwrap()))
    }
}

fn encode_childs(childs: &[u64]) -> Vec<u8> {
    let model = models::HashIds {
        ids: childs.to_vec(),
    };
    cbor_util::to_vec(&model).unwrap()
}

fn decode_childs(bytes: &[u8]) -> Result<Vec<u64>, cbor_util::DeError> {
    let model: models::HashIds = cbor_util::from_slice(bytes)?;
    Ok(model.ids)
}

fn decode_chunk_ref(
    cref: Option<&Vec<u8>>,
    blob: Option<self::schema::Blob>,
) -> Option<blob::ChunkRef> {
    cref.map(|c| {
        let mut r = blob::ChunkRef::from_bytes(&c[..]).expect("Failed to decode chunk");
        if r.length > 0 {
            r.blob_name = blob.expect("Non-empty chunk without blob name").name;
        } else {
            r.blob_name = vec![0];
        }
        r
    })
}

#[derive(Clone, Debug)]
pub struct StatCacheEntry {
    pub kind: i32,
    pub created: Option<i64>,
    pub modified: Option<i64>,
    pub accessed: Option<i64>,
    pub permissions: Option<i64>,
    pub user_id: Option<i64>,
    pub group_id: Option<i64>,
    pub file_size: Option<i64>,
    pub dev: Option<i64>,
    pub ino: Option<i64>,
    pub ctime: Option<i64>,
    pub ctime_nsec: Option<i64>,
    pub mtime_nsec: Option<i64>,
    pub snapshot_ts_utc: i64,
    pub hash: Option<Vec<u8>>,
    pub hash_ref: Option<Vec<u8>>,
    pub symlink_target: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct SnapshotInfo {
    pub unique_id: u64,
    pub family_id: u64,
    pub snapshot_id: u64,
}

#[derive(Debug)]
pub enum SnapshotWorkStatus {
    CommitInProgress,
    CommitComplete,
    DeleteInProgress,
    DeleteComplete,
    RecoverInProgress,
}

#[derive(Debug)]
pub struct SnapshotStatus {
    pub family_name: String,
    pub info: SnapshotInfo,
    pub hash: Option<hash::Hash>,
    pub hash_ref: Option<Vec<u8>>,
    pub created: chrono::DateTime<chrono::Utc>,
    pub msg: Option<String>,
    pub status: SnapshotWorkStatus,
}

fn tag_to_work_status(tag: tags::Tag) -> SnapshotWorkStatus {
    match tag {
        tags::Tag::Reserved | tags::Tag::InProgress => SnapshotWorkStatus::CommitInProgress,
        tags::Tag::Complete | tags::Tag::Done => SnapshotWorkStatus::CommitComplete,
        tags::Tag::WillDelete => SnapshotWorkStatus::DeleteInProgress,
        tags::Tag::ReadyDelete | tags::Tag::DeleteComplete => SnapshotWorkStatus::DeleteComplete,
        tags::Tag::RecoverInProgress => SnapshotWorkStatus::RecoverInProgress,
    }
}

fn work_status_to_tag(status: SnapshotWorkStatus) -> tags::Tag {
    match status {
        SnapshotWorkStatus::CommitInProgress => tags::Tag::InProgress,
        SnapshotWorkStatus::CommitComplete => tags::Tag::Done,
        SnapshotWorkStatus::DeleteInProgress => tags::Tag::WillDelete,
        SnapshotWorkStatus::DeleteComplete => tags::Tag::DeleteComplete,
        SnapshotWorkStatus::RecoverInProgress => tags::Tag::RecoverInProgress,
    }
}

/// An entry that can be inserted into the hash index.
#[derive(Clone)]
pub struct Entry {
    /// The hash of this entry (unique among all entries in the index).
    pub hash: hash::Hash,

    /// Metadata on what sort of content we are referencing.
    pub node: blob::NodeType,
    pub leaf: blob::LeafType,

    /// An optional list of child hash ids.
    pub childs: Option<Vec<u64>>,

    /// A reference to a location in the external persistent storage (a chunk reference) that
    /// contains the data for this entry (e.g. an object-name and a byte range).
    pub persistent_ref: Option<blob::ChunkRef>,

    pub ready: bool,
}

#[derive(Clone)]
pub struct QueueEntry {
    pub id: u64,
    pub node: blob::NodeType,
    pub leaf: blob::LeafType,
    pub childs: Option<Vec<u64>>,
    pub persistent_ref: Option<blob::ChunkRef>,
    pub tag: Option<tags::Tag>,
}

pub struct InternalIndex {
    conn: SqliteConnection,
    hash_id_counter: Counter,
    blob_id_counter: Counter,
    flush_timer: PeriodicTimer,
    flush_periodically: bool,
}

pub const MIGRATIONS: diesel_migrations::EmbeddedMigrations = embed_migrations!();

impl InternalIndex {
    fn new(path: &str) -> Result<InternalIndex, DieselError> {
        let conn = open_connection(path)?;

        let mut idx = InternalIndex {
            conn,
            hash_id_counter: Counter::new(0),
            blob_id_counter: Counter::new(0),
            flush_timer: PeriodicTimer::new(Duration::from_secs(10)),
            flush_periodically: true,
        };

        idx.conn
            .run_pending_migrations(MIGRATIONS)
            .map_err(DieselError::Migration)?;

        <SqliteConnection as Connection>::TransactionManager::begin_transaction(&mut idx.conn)?;

        idx.hash_refresh_id_counter();
        idx.blob_refresh_id_counter();
        Ok(idx)
    }

    #[cfg_attr(feature = "flame_it", flame)]
    pub fn hash_locate(&mut self, hash_: &hash::Hash) -> Option<QueueEntry> {
        assert!(!hash_.bytes.is_empty());
        use self::schema::blobs::dsl::blobs;
        use self::schema::hashes::dsl::*;

        let result_opt = hashes
            .left_outer_join(blobs)
            .filter(hash.eq(&hash_.bytes))
            .first::<(self::schema::Hash, Option<self::schema::Blob>)>(&mut self.conn)
            .optional()
            .expect("Error querying hashes");

        result_opt.map(|(hash_, blob_)| {
            let childs_ = hash_.childs.and_then(|b| {
                if b.is_empty() {
                    None
                } else {
                    Some(decode_childs(&b).unwrap())
                }
            });
            let persistent_ref = decode_chunk_ref(hash_.blob_ref.as_ref(), blob_);
            QueueEntry {
                id: hash_.id as u64,
                node: From::from(hash_.height as u64),
                leaf: From::from(hash_.leaf_type as u64),
                tag: tags::tag_from_num(hash_.tag),
                childs: childs_,
                persistent_ref,
            }
        })
    }

    pub fn hash_locate_by_id(&mut self, id_: u64) -> Option<Entry> {
        use self::schema::blobs::dsl::blobs;
        use self::schema::hashes::dsl::*;

        let result_opt = hashes
            .left_outer_join(blobs)
            .filter(id.eq(id_ as i64))
            .first::<(self::schema::Hash, Option<self::schema::Blob>)>(&mut self.conn)
            .optional()
            .expect("Error querying hashes");

        result_opt.map(|(hash_, blob_)| Entry {
            hash: self::hash::Hash { bytes: hash_.hash },
            node: From::from(hash_.height as u64),
            leaf: From::from(hash_.leaf_type as u64),
            childs: hash_.childs.and_then(|p| {
                if p.is_empty() {
                    None
                } else {
                    Some(decode_childs(&p).unwrap())
                }
            }),
            persistent_ref: decode_chunk_ref(hash_.blob_ref.as_ref(), blob_),
            ready: hash_.ready,
        })
    }

    pub fn hash_refresh_id_counter(&mut self) {
        use self::schema::hashes::dsl::*;
        use diesel::dsl::max;

        let id_opt = hashes
            .select(max(id))
            .first::<Option<i64>>(&mut self.conn)
            .expect("Error selecting max hash id");

        self.hash_id_counter = Counter::new(id_opt.unwrap_or(0));
    }

    pub fn hash_next_id(&mut self) -> u64 {
        self.hash_id_counter.next_id() as u64
    }

    pub fn hash_insert_new(&mut self, id_: u64, hash_bytes: Vec<u8>, entry: QueueEntry) {
        use self::schema::hashes::dsl::*;

        let blob_ref_ = entry.persistent_ref.as_ref().map(|c| c.as_bytes_no_name());
        let childs_ = entry.childs.as_ref().map(|v| encode_childs(&v[..]));

        let height_: u64 = From::from(entry.node);
        let leaf_type_: u64 = From::from(entry.leaf);

        let new = schema::NewHash {
            id: id_ as i64,
            hash: &hash_bytes,
            tag: entry.tag.unwrap_or(tags::Tag::Done) as i64,
            height: height_ as i64,
            leaf_type: leaf_type_ as i64,
            childs: childs_.as_ref().map(|v| &v[..]),
            blob_id: entry.persistent_ref.and_then(|r| r.blob_id).unwrap_or(0),
            blob_ref: blob_ref_.as_ref().map(|v| &v[..]),
            ready: false,
        };

        diesel::insert_into(hashes)
            .values(&new)
            .execute(&mut self.conn)
            .expect("Error inserting new hash");
    }

    pub fn hash_delete_not_ready(&mut self) {
        {
            use self::schema::hashes::dsl::*;
            diesel::delete(hashes.filter(ready.eq(false)))
                .execute(&mut self.conn)
                .expect("Failed to delete non-ready hashes");
        }
        // Purge any orphaned tree_edges whose TreeList root no longer exists in hashes
        // (e.g., after a crash left unready hashes behind) so reused hash IDs cannot
        // inherit stale edges.
        diesel::sql_query(
            "DELETE FROM tree_edges WHERE tree_hash_id NOT IN (SELECT id FROM hashes)",
        )
        .execute(&mut self.conn)
        .expect("Failed to purge orphaned tree_edges");
    }

    pub fn hash_set_ready(&mut self, id_: u64, entry: &QueueEntry) {
        use self::schema::hashes::dsl::*;
        let blob_ref_ = entry
            .persistent_ref
            .as_ref()
            .expect("ready")
            .as_bytes_no_name();
        let blob_id_ = entry
            .persistent_ref
            .as_ref()
            .expect("ready")
            .blob_id
            .expect("ready");
        let childs_ = entry.childs.as_ref().map(|v| encode_childs(&v[..]));

        let height_: u64 = From::from(entry.node);
        let leaf_type_: u64 = From::from(entry.leaf);

        diesel::update(hashes.find(id_ as i64))
            .set((
                blob_id.eq(blob_id_),
                blob_ref.eq(&blob_ref_[..]),
                ready.eq(true),
                height.eq(height_ as i64),
                leaf_type.eq(leaf_type_ as i64),
                childs.eq(childs_.as_ref().map(|v| &v[..])),
            ))
            .execute(&mut self.conn)
            .expect("Failed to set hash ready");
    }

    pub fn hash_all_ids(&mut self) -> Vec<u64> {
        use self::schema::hashes::dsl::*;

        hashes
            .select(id)
            .load::<i64>(&mut self.conn)
            .expect("Error listing hash ids")
            .into_iter()
            .map(|i| i as u64)
            .collect()
    }

    pub fn hash_list(&mut self) -> Vec<Entry> {
        use self::schema::blobs::dsl::blobs;
        use self::schema::hashes::dsl::*;

        hashes
            .left_outer_join(blobs)
            .load::<(self::schema::Hash, Option<self::schema::Blob>)>(&mut self.conn)
            .expect("Error listing hashes")
            .into_iter()
            .map(|(hash_, blob_)| Entry {
                hash: self::hash::Hash { bytes: hash_.hash },
                node: From::from(hash_.height as u64),
                leaf: From::from(hash_.leaf_type as u64),
                childs: hash_.childs.as_ref().map(|p| decode_childs(p).unwrap()),
                persistent_ref: decode_chunk_ref(hash_.blob_ref.as_ref(), blob_),
                ready: hash_.ready,
            })
            .collect()
    }

    pub fn hash_delete(&mut self, id_: u64) {
        {
            use self::schema::hashes::dsl::*;
            let hash_count = diesel::delete(hashes.find(id_ as i64))
                .execute(&mut self.conn)
                .expect("Error deleting hash");
            assert!(hash_count <= 1);
        }

        self.tree_edges_delete(id_);
    }

    /// Look up cached directory edges (`TreeList` root hash_id -> child file/subdir hash_ids).
    /// Returns `None` if no edge cache record exists for `tree_hash_id_`, or `Some(child_ids)`
    /// (empty Vec for an empty directory, stored via a `-1` sentinel row).
    pub fn tree_edges_get(&mut self, tree_hash_id_: u64) -> Option<Vec<u64>> {
        use self::schema::tree_edges::dsl::*;

        let rows = tree_edges
            .filter(tree_hash_id.eq(tree_hash_id_ as i64))
            .select(child_hash_id)
            .load::<i64>(&mut self.conn)
            .expect("Error querying tree_edges");

        if rows.is_empty() {
            return None;
        }
        Some(rows.into_iter().filter(|&x| x >= 0).map(|x| x as u64).collect())
    }

    /// Record the complete child `hash_id` set for a `TreeList` root in the local edge cache.
    pub fn tree_edges_set(&mut self, tree_hash_id_: u64, child_ids: &[u64]) {
        use self::schema::tree_edges::dsl::*;
        use std::collections::BTreeSet;

        diesel::delete(tree_edges.filter(tree_hash_id.eq(tree_hash_id_ as i64)))
            .execute(&mut self.conn)
            .expect("Error clearing tree_edges");

        if child_ids.is_empty() {
            let sentinel = schema::NewTreeEdge {
                tree_hash_id: tree_hash_id_ as i64,
                child_hash_id: -1,
            };
            diesel::insert_into(tree_edges)
                .values(&sentinel)
                .execute(&mut self.conn)
                .expect("Error inserting empty tree_edges sentinel");
            return;
        }

        let unique: BTreeSet<u64> = child_ids.iter().copied().collect();
        let rows: Vec<schema::NewTreeEdge> = unique
            .into_iter()
            .map(|cid| schema::NewTreeEdge {
                tree_hash_id: tree_hash_id_ as i64,
                child_hash_id: cid as i64,
            })
            .collect();
        diesel::insert_into(tree_edges)
            .values(&rows)
            .execute(&mut self.conn)
            .expect("Error inserting tree_edges");
    }

    pub fn tree_edges_delete(&mut self, tree_hash_id_: u64) {
        use self::schema::tree_edges::dsl::*;

        diesel::delete(tree_edges.filter(tree_hash_id.eq(tree_hash_id_ as i64)))
            .execute(&mut self.conn)
            .expect("Error deleting tree_edges");
    }

    pub fn stat_cache_lookup(
        &mut self,
        family_id_: i64,
        rel_path_: &[u8],
    ) -> Option<StatCacheEntry> {
        use self::schema::stat_cache::dsl::*;

        stat_cache
            .filter(family_id.eq(family_id_))
            .filter(rel_path.eq(rel_path_))
            .first::<schema::StatCacheRow>(&mut self.conn)
            .optional()
            .expect("Error querying stat_cache")
            .map(|r| StatCacheEntry {
                kind: r.kind,
                created: r.created,
                modified: r.modified,
                accessed: r.accessed,
                permissions: r.permissions,
                user_id: r.user_id,
                group_id: r.group_id,
                file_size: r.file_size,
                dev: r.dev,
                ino: r.ino,
                ctime: r.ctime,
                ctime_nsec: r.ctime_nsec,
                mtime_nsec: r.mtime_nsec,
                snapshot_ts_utc: r.snapshot_ts_utc,
                hash: r.hash,
                hash_ref: r.hash_ref,
                symlink_target: r.symlink_target,
            })
    }

    pub fn stat_cache_upsert(
        &mut self,
        family_id_: i64,
        rel_path_: &[u8],
        entry: &StatCacheEntry,
    ) {
        use self::schema::stat_cache::dsl::*;

        let row = schema::NewStatCacheRow {
            family_id: family_id_,
            rel_path: rel_path_,
            kind: entry.kind,
            created: entry.created,
            modified: entry.modified,
            accessed: entry.accessed,
            permissions: entry.permissions,
            user_id: entry.user_id,
            group_id: entry.group_id,
            file_size: entry.file_size,
            dev: entry.dev,
            ino: entry.ino,
            ctime: entry.ctime,
            ctime_nsec: entry.ctime_nsec,
            mtime_nsec: entry.mtime_nsec,
            snapshot_ts_utc: entry.snapshot_ts_utc,
            hash: entry.hash.as_deref(),
            hash_ref: entry.hash_ref.as_deref(),
            symlink_target: entry.symlink_target.as_deref(),
        };

        diesel::replace_into(stat_cache)
            .values(&row)
            .execute(&mut self.conn)
            .expect("Error upserting stat_cache");
        self.maybe_flush();
    }

    /// Prune `stat_cache` rows for `family_id_` whose `rel_path` was not seen in the
    /// just-committed snapshot walk. Performs zero deletes if nothing was removed/renamed.
    pub fn stat_cache_prune_family(
        &mut self,
        family_id_: i64,
        live_rel_paths: &std::collections::HashSet<Vec<u8>>,
    ) -> usize {
        use self::schema::stat_cache::dsl::*;

        let existing: Vec<Vec<u8>> = stat_cache
            .filter(family_id.eq(family_id_))
            .select(rel_path)
            .load::<Vec<u8>>(&mut self.conn)
            .expect("Error listing stat_cache paths for prune");

        let mut deleted = 0usize;
        for path in existing {
            if !live_rel_paths.contains(&path) {
                deleted += diesel::delete(
                    stat_cache
                        .filter(family_id.eq(family_id_))
                        .filter(rel_path.eq(&path)),
                )
                .execute(&mut self.conn)
                .expect("Error deleting stale stat_cache row");
            }
        }
        deleted
    }

    /// Delete `stat_cache` rows belonging to families with no remaining snapshots, or
    /// referencing content hashes that were already swept from `hashes`.
    pub fn stat_cache_prune_orphans(&mut self) -> usize {
        diesel::sql_query(
            "DELETE FROM stat_cache \
             WHERE family_id NOT IN (SELECT DISTINCT family_id FROM snapshots) \
                OR (hash IS NOT NULL AND hash NOT IN (SELECT hash FROM hashes))",
        )
        .execute(&mut self.conn)
        .expect("Error pruning orphan stat_cache rows")
    }

    #[cfg(test)]
    pub fn stat_cache_count_for_family(&mut self, family_id_: i64) -> usize {
        use self::schema::stat_cache::dsl::*;

        stat_cache
            .filter(family_id.eq(family_id_))
            .select(rel_path)
            .load::<Vec<u8>>(&mut self.conn)
            .expect("Error counting stat_cache rows")
            .len()
    }

    pub fn maybe_flush(&mut self) {
        if self.flush_periodically && self.flush_timer.did_fire() {
            debug!("SQL: hash db maybe_flush commit");
            self.flush();
        }
    }

    #[cfg_attr(feature = "flame_it", flame)]
    pub fn flush(&mut self) {
        debug!("SQL: hash db commit");

        <SqliteConnection as Connection>::TransactionManager::commit_transaction(&mut self.conn)
            .unwrap();
        <SqliteConnection as Connection>::TransactionManager::begin_transaction(&mut self.conn)
            .unwrap();
    }

    pub fn blob_refresh_id_counter(&mut self) {
        use self::schema::blobs::dsl::*;
        use diesel::dsl::max;

        let id_opt = blobs
            .select(max(id))
            .first::<Option<i64>>(&mut self.conn)
            .optional()
            .expect("Error querying blobs")
            .and_then(|x| x)
            .unwrap_or(0);

        self.blob_id_counter = Counter::new(id_opt);
    }

    pub fn blob_next_id(&mut self) -> i64 {
        self.blob_id_counter.next_id()
    }

    pub fn blob_in_air(&mut self, blob: &blob::BlobDesc) {
        use self::schema::blobs::dsl::*;

        let new = schema::NewBlob {
            id: blob.id,
            name: &blob.name,
            tag: tags::Tag::InProgress as i32,
        };
        diesel::insert_into(blobs)
            .values(&new)
            .execute(&mut self.conn)
            .expect("Error inserting blob");

        self.flush();
    }

    pub fn blob_commit(&mut self, blob: &blob::BlobDesc) {
        use self::schema::blobs::dsl::*;

        diesel::update(blobs.find(blob.id))
            .set(tag.eq(tags::Tag::Done as i32))
            .execute(&mut self.conn)
            .expect("Error updating blob");
        self.flush();
    }

    pub fn blob_id_from_name(&mut self, name_: &[u8]) -> Option<i64> {
        use self::schema::blobs::dsl::*;
        blobs
            .filter(name.eq(name_))
            .select(id)
            .first::<i64>(&mut self.conn)
            .optional()
            .expect("Error reading blob")
    }

    pub fn blob_set_tag(&mut self, tag_: tags::Tag, target: Option<&blob::BlobDesc>) {
        use self::schema::blobs::dsl::*;
        match target {
            None => diesel::update(blobs)
                .set(tag.eq(tag_ as i32))
                .execute(&mut self.conn)
                .expect("Error updating blob tags"),
            Some(t) if t.id > 0 => diesel::update(blobs.find(t.id))
                .set(tag.eq(tag_ as i32))
                .execute(&mut self.conn)
                .expect("Error updating blob tags"),
            Some(t) if !t.name.is_empty() => diesel::update(blobs.filter(name.eq(&t.name)))
                .set(tag.eq(tag_ as i32))
                .execute(&mut self.conn)
                .expect("Error updating blob tags"),
            Some(t) => unreachable!(
                "blob with neither id nor name: id={}, name={}",
                t.id,
                t.name.len()
            ),
        };
    }

    pub fn blob_delete_by_tag(&mut self, tag_: tags::Tag) {
        use self::schema::blobs::dsl::*;
        diesel::delete(blobs.filter(tag.eq(tag_ as i32)))
            .execute(&mut self.conn)
            .expect("Error deleting blobs");
    }

    pub fn blob_list_by_tag(&mut self, tag_: tags::Tag) -> Vec<blob::BlobDesc> {
        use self::schema::blobs::dsl::*;
        blobs
            .filter(tag.eq(tag_ as i32))
            .order(id.desc())
            .load::<schema::Blob>(&mut self.conn)
            .expect("Error listing blobs")
            .into_iter()
            .map(|blob_| blob::BlobDesc {
                id: blob_.id,
                name: blob_.name,
            })
            .collect()
    }

    pub fn last_insert_rowid(&mut self) -> i64 {
        let rows: Vec<self::schema::RowId> = diesel::sql_query(
            "SELECT last_insert_rowid() AS row_id",
        ).load(&mut self.conn)
            .unwrap();

        assert_eq!(1, rows.len());

        rows[0].row_id
    }

    pub fn family_id_from_name(&mut self, name_: &str) -> Option<i64> {
        use self::schema::family::dsl::*;

        family
            .filter(name.eq(name_))
            .select(id)
            .first::<i64>(&mut self.conn)
            .optional()
            .expect("Error reading family")
    }

    /// Delete snapshot.
    pub fn snapshot_delete(&mut self, info: SnapshotInfo) {
        use self::schema::snapshots::dsl::*;

        let count = diesel::delete(
            snapshots
                .find(info.unique_id as i64)
                .filter(family_id.eq(info.family_id as i64))
                .filter(snapshot_id.eq(info.snapshot_id as i64)),
        ).execute(&mut self.conn)
            .expect("Error deleting snapshots");
        assert!(count <= 1);
    }

    pub fn get_or_create_family_id(&mut self, name_: &str) -> i64 {
        let id_opt = self.family_id_from_name(name_);
        match id_opt {
            Some(id) => id,
            None => {
                use self::schema::family::dsl::*;

                let new = self::schema::NewFamily { name: name_ };

                diesel::insert_into(family)
                    .values(&new)
                    .execute(&mut self.conn)
                    .expect("Error inserting family");
                self.last_insert_rowid()
            }
        }
    }

    pub fn snapshot_latest_id(&mut self, family_id_: i64) -> Option<i64> {
        use self::schema::snapshots::dsl::*;
        use diesel::dsl::max;
        snapshots
            .filter(family_id.eq(family_id_))
            .select(max(snapshot_id))
            .first::<Option<i64>>(&mut self.conn)
            .optional()
            .expect("Error reading latest snapshot id")
            .and_then(|x| x)
    }

    /// Lookup exact snapshot info from family and snapshot id.
    pub fn snapshot_lookup(
        &mut self,
        family_name_: &str,
        snapshot_id_: u64,
    ) -> Option<(SnapshotInfo, hash::Hash, Option<hash::tree::HashRef>)> {
        use self::schema::family::dsl::{family, name};
        use self::schema::snapshots::dsl::*;

        let row_opt = snapshots
            .inner_join(family)
            .filter(name.eq(family_name_))
            .filter(snapshot_id.eq(snapshot_id_ as i64))
            .select((
                id,
                tag,
                family_id,
                snapshot_id,
                utc_datetime,
                msg,
                hash,
                hash_ref,
            ))
            .first::<self::schema::Snapshot>(&mut self.conn)
            .optional()
            .expect("Error reading snapshot info");

        row_opt.map(|snap| {
            (
                SnapshotInfo {
                    unique_id: snap.id as u64,
                    family_id: snap.family_id as u64,
                    snapshot_id: snap.snapshot_id as u64,
                },
                crate::hash::Hash {
                    bytes: snap.hash.unwrap().to_vec(),
                },
                snap.hash_ref
                    .and_then(|r| crate::hash::tree::HashRef::from_bytes(&r[..]).ok()),
            )
        })
    }

    /// Atomically insert a completed (`Tag::Done`) snapshot row.
    pub fn snapshot_insert_done(
        &mut self,
        family_: String,
        msg_: &str,
        hash_: &hash::Hash,
        hash_ref_: &hash::tree::HashRef,
    ) -> SnapshotInfo {
        use self::schema::snapshots::dsl::*;

        let family_id_ = self.get_or_create_family_id(&family_);
        let snapshot_id_ = 1 + self.snapshot_latest_id(family_id_).unwrap_or(0);
        let hash_ref_bytes = hash_ref_.as_bytes();

        let new = self::schema::NewSnapshot {
            family_id: family_id_,
            snapshot_id: snapshot_id_,
            tag: tags::Tag::Done as i32,
            utc_datetime: chrono::Utc::now().naive_utc(),
            msg: Some(msg_),
            hash: Some(&hash_.bytes[..]),
            hash_ref: Some(&hash_ref_bytes[..]),
        };

        diesel::insert_into(snapshots)
            .values(&new)
            .execute(&mut self.conn)
            .expect("Error inserting snapshot");

        let unique_id_ = self.last_insert_rowid();

        SnapshotInfo {
            unique_id: unique_id_ as u64,
            family_id: family_id_ as u64,
            snapshot_id: snapshot_id_ as u64,
        }
    }

    /// Delete any legacy incomplete (`tag != Done`) snapshot rows.
    pub fn snapshot_delete_incomplete(&mut self) -> usize {
        use self::schema::snapshots::dsl::*;

        diesel::delete(snapshots.filter(tag.ne(tags::Tag::Done as i32)))
            .execute(&mut self.conn)
            .expect("Error deleting incomplete snapshots")
    }

    /// Extract latest snapshot data for family.
    pub fn snapshot_latest(
        &mut self,
        family: &str,
    ) -> Option<(SnapshotInfo, hash::Hash, Option<hash::tree::HashRef>)> {
        let family_id_opt = self.family_id_from_name(family);
        family_id_opt.and_then(|family_id_| {
            use self::schema::snapshots::dsl::*;

            let row_opt = snapshots
                .filter(family_id.eq(family_id_))
                .order(snapshot_id.desc())
                .first::<self::schema::Snapshot>(&mut self.conn)
                .optional()
                .expect("Error reading latest snapshot");

            row_opt.map(|snap| {
                (
                    SnapshotInfo {
                        unique_id: snap.id as u64,
                        family_id: snap.family_id as u64,
                        snapshot_id: snap.snapshot_id as u64,
                    },
                    crate::hash::Hash {
                        bytes: snap.hash.expect("Snapshot without top hash"),
                    },
                    snap.hash_ref
                        .and_then(|r| crate::hash::tree::HashRef::from_bytes(&r[..]).ok()),
                )
            })
        })
    }

    pub fn snapshot_list(&mut self, skip_tag: Option<tags::Tag>) -> Vec<SnapshotStatus> {
        use self::schema::family::dsl::family;
        use self::schema::snapshots::dsl::*;
        use diesel::*;
        let rows = match skip_tag {
            None => snapshots
                .inner_join(family)
                .load::<(self::schema::Snapshot, self::schema::Family)>(&mut self.conn),
            Some(skip) => snapshots
                .inner_join(family)
                .filter(tag.ne(skip as i32))
                .load::<(self::schema::Snapshot, self::schema::Family)>(&mut self.conn),
        }.unwrap();

        rows.into_iter()
            .map(|(snap, fam)| {
                let status = tags::tag_from_num(snap.tag as i64)
                    .map_or(SnapshotWorkStatus::CommitComplete, tag_to_work_status);
                let hash_ = snap.hash.and_then(|bytes| {
                    if bytes.is_empty() {
                        None
                    } else {
                        Some(crate::hash::Hash { bytes })
                    }
                });

                SnapshotStatus {
                    family_name: fam.name,
                    created: chrono::DateTime::from_naive_utc_and_offset(snap.utc_datetime, chrono::Utc),
                    msg: snap.msg,
                    hash: hash_,
                    hash_ref: snap.hash_ref,
                    status,
                    info: SnapshotInfo {
                        unique_id: snap.id as u64,
                        snapshot_id: snap.snapshot_id as u64,
                        family_id: fam.id as u64,
                    },
                }
            })
            .collect()
    }

    /// Recover snapshot information.
    pub fn snapshot_recover(
        &mut self,
        snapshot_id_: u64,
        family: &str,
        created: chrono::DateTime<chrono::Utc>,
        msg_: &str,
        hash_ref_: &hash::tree::HashRef,
        work_opt_: Option<SnapshotWorkStatus>,
    ) {
        let family_id_ = self.get_or_create_family_id(family);
        let insert = match self.snapshot_lookup(family, snapshot_id_) {
            Some((_info, h, _r)) => {
                if h.bytes != hash_ref_.hash.bytes {
                    panic!("Snapshot already exists, but with different hash");
                }
                false
            }
            None => true,
        };
        if insert {
            use self::schema::snapshots::dsl::*;

            let hash_ref_bytes = hash_ref_.as_bytes();
            let new = self::schema::NewSnapshot {
                family_id: family_id_,
                snapshot_id: snapshot_id_ as i64,
                utc_datetime: created.naive_utc(),
                msg: Some(msg_),
                hash: Some(&hash_ref_.hash.bytes[..]),
                hash_ref: Some(&hash_ref_bytes[..]),
                tag: work_opt_.map_or(tags::Tag::Done, work_status_to_tag) as i32,
            };

            diesel::insert_into(snapshots)
                .values(&new)
                .execute(&mut self.conn)
                .expect("Error inserting new snapshot");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diesel::sql_types::Text;

    #[derive(QueryableByName)]
    struct TableName {
        #[diesel(sql_type = Text)]
        name: String,
    }

    #[test]
    fn migrations_drop_legacy_tables_and_create_stat_cache_and_tree_edges() {
        let idx = Index::new_for_testing();
        let mut guard = idx.lock();
        let tables: Vec<String> = diesel::sql_query(
            "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
        )
        .load::<TableName>(&mut guard.conn)
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .collect();

        for legacy in ["gc_metadata", "key_data", "key_tree"] {
            assert!(
                !tables.iter().any(|t| t == legacy),
                "legacy table {} should have been dropped by migration (tables: {:?})",
                legacy,
                tables
            );
        }
        for current in ["blobs", "family", "hashes", "snapshots", "stat_cache", "tree_edges"] {
            assert!(
                tables.iter().any(|t| t == current),
                "expected table {} missing (tables: {:?})",
                current,
                tables
            );
        }
    }

    #[test]
    fn hash_delete_not_ready_purges_orphaned_tree_edges() {
        let idx = Index::new_for_testing();
        let mut guard = idx.lock();

        // Insert a non-ready hash and attach tree_edges to it, plus an edge whose
        // tree_hash_id has no hash row at all.
        let unready_id = guard.hash_next_id();
        guard.hash_insert_new(
            unready_id,
            vec![1, 2, 3, 4],
            QueueEntry {
                id: unready_id,
                node: blob::NodeType::Leaf,
                leaf: blob::LeafType::TreeList,
                tag: None,
                childs: None,
                persistent_ref: None,
            },
        );
        guard.tree_edges_set(unready_id, &[42, 43]);
        guard.tree_edges_set(999_999, &[77]);

        assert_eq!(guard.tree_edges_get(unready_id), Some(vec![42, 43]));
        assert_eq!(guard.tree_edges_get(999_999), Some(vec![77]));

        guard.hash_delete_not_ready();

        assert_eq!(guard.tree_edges_get(unready_id), None);
        assert_eq!(guard.tree_edges_get(999_999), None);
    }
}
