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

//! Local state for known snapshots.

use crate::db;
use crate::hash;
use std::sync::Arc;
use crate::tags;

pub struct SnapshotIndex {
    index: Arc<db::Index>,
}

impl SnapshotIndex {
    pub fn new(idx: Arc<db::Index>) -> SnapshotIndex {
        SnapshotIndex { index: idx }
    }

    /// Delete snapshot.
    pub fn delete(&self, info: db::SnapshotInfo) {
        self.index.lock().snapshot_delete(info);
    }

    /// Lookup exact snapshot info from family and snapshot id.
    pub fn lookup(
        &mut self,
        family_name: &str,
        snapshot_id: u64,
    ) -> Option<(db::SnapshotInfo, hash::Hash, Option<hash::tree::HashRef>)> {
        self.index.lock().snapshot_lookup(family_name, snapshot_id)
    }

    /// Atomically insert a completed snapshot row.
    pub fn insert_done(
        &mut self,
        family: String,
        hash: &hash::Hash,
        hash_ref: &hash::tree::HashRef,
    ) -> db::SnapshotInfo {
        self.index
            .lock()
            .snapshot_insert_done(family, "anonymous", hash, hash_ref)
    }

    /// Delete any stale incomplete snapshot rows left by older versions.
    pub fn delete_incomplete(&mut self) -> usize {
        self.index.lock().snapshot_delete_incomplete()
    }

    /// Extract latest snapshot data for family.
    pub fn latest(
        &mut self,
        family: &str,
    ) -> Option<(db::SnapshotInfo, hash::Hash, Option<hash::tree::HashRef>)> {
        self.index.lock().snapshot_latest(family)
    }

    fn list(&mut self, skip_tag: Option<tags::Tag>) -> Vec<db::SnapshotStatus> {
        self.index.lock().snapshot_list(skip_tag)
    }

    /// List incomplete snapshots (either committing or deleting).
    pub fn list_not_done(&mut self) -> Vec<db::SnapshotStatus> {
        self.list(Some(tags::Tag::Done) /* not_tag */)
    }

    /// List all snapshots.
    pub fn list_all(&mut self) -> Vec<db::SnapshotStatus> {
        self.list(None)
    }

    /// Recover snapshot information.
    pub fn recover(
        &mut self,
        snapshot_id: u64,
        family: &str,
        created: chrono::DateTime<chrono::Utc>,
        msg: &str,
        hash_ref: &hash::tree::HashRef,
        work_opt: Option<db::SnapshotWorkStatus>,
    ) {
        self.index
            .lock()
            .snapshot_recover(snapshot_id, family, created, msg, hash_ref, work_opt)
    }

    /// Flush the hash index to clear internal buffers and commit the underlying database.
    pub fn flush(&mut self) {
        self.index.lock().flush()
    }
}
