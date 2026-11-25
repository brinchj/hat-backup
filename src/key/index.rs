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

//! Entry metadata, flat stat-cache conversion, and in-memory staged directory tree.

use std::collections::{BTreeMap, HashSet};
use std::ffi;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use crate::db::StatCacheEntry;
use crate::hash::tree::HashRef;
use crate::models;

const KIND_FILE: i32 = 0;
const KIND_DIR: i32 = 1;
const KIND_SYMLINK: i32 = 2;

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum Data {
    FilePlaceholder,
    FileHash(Vec<u8>),
    DirPlaceholder,
    Symlink(PathBuf),
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub node_id: Option<u64>,
    pub parent_id: Option<u64>,

    pub data: Data,
    pub info: Info,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Info {
    pub name: models::FileName,

    pub created_ts_secs: Option<i64>,
    pub modified_ts_secs: Option<i64>,
    pub accessed_ts_secs: Option<i64>,

    pub permissions: Option<fs::Permissions>,
    pub user_id: Option<u64>,
    pub group_id: Option<u64>,

    pub byte_length: Option<u64>,
    pub snapshot_ts_utc: i64,

    // Local stat-cache identity (not serialized to remote snapshots): detects file
    // replacement (different dev/ino), backdated utimes / metadata changes (ctime),
    // and sub-second modifications (mtime_nsec/ctime_nsec).
    pub dev: Option<u64>,
    pub ino: Option<u64>,
    pub ctime_secs: Option<i64>,
    pub ctime_nsec: Option<i64>,
    pub mtime_nsec: Option<i64>,
}

impl Entry {
    pub fn new(
        parent: Option<u64>,
        name: models::FileName,
        data: Data,
        meta: Option<&fs::Metadata>,
    ) -> Entry {
        Entry {
            node_id: None,
            parent_id: parent,
            data,
            info: Info::new(name, meta),
        }
    }

    pub fn data_looks_unchanged(&self, them: &Entry) -> bool {
        let same_kind = match (&self.data, &them.data) {
            (Data::FilePlaceholder, Data::FileHash(_))
            | (Data::FilePlaceholder, Data::FilePlaceholder)
            | (Data::DirPlaceholder, Data::DirPlaceholder) => true,
            (Data::Symlink(a), Data::Symlink(b)) => a == b,
            (Data::FileHash(a), Data::FileHash(b)) => a == b,
            _ => false,
        };
        if !same_kind {
            return false;
        }
        let Some(mtime) = self.info.modified_ts_secs else {
            return false;
        };
        // Racily-clean guard: mtime/ctime seconds are checked against the 1-second
        // snapshot_ts_utc clock, so never short-circuit if either timestamp is in the
        // same second as (or after) either scan.
        if mtime >= them.info.snapshot_ts_utc || mtime >= self.info.snapshot_ts_utc {
            return false;
        }
        if let Some(ctime) = self.info.ctime_secs {
            if ctime >= them.info.snapshot_ts_utc || ctime >= self.info.snapshot_ts_utc {
                return false;
            }
        }
        self.parent_id == them.parent_id
            && self.info.name == them.info.name
            && self.info.modified_ts_secs == them.info.modified_ts_secs
            && self.info.mtime_nsec == them.info.mtime_nsec
            && self.info.ctime_secs == them.info.ctime_secs
            && self.info.ctime_nsec == them.info.ctime_nsec
            && self.info.dev == them.info.dev
            && self.info.ino == them.info.ino
            && self.info.byte_length == them.info.byte_length
            && self.info.permissions == them.info.permissions
            && self.info.user_id == them.info.user_id
            && self.info.group_id == them.info.group_id
            && self.info.created_ts_secs == them.info.created_ts_secs
    }

    pub fn to_stat_cache_entry(&self, hash_ref: Option<&HashRef>) -> StatCacheEntry {
        let (kind, symlink_target) = match &self.data {
            Data::FilePlaceholder | Data::FileHash(_) => (KIND_FILE, None),
            Data::DirPlaceholder => (KIND_DIR, None),
            Data::Symlink(p) => (KIND_SYMLINK, Some(p.as_os_str().as_bytes().to_vec())),
        };
        StatCacheEntry {
            kind,
            created: self.info.created_ts_secs,
            modified: self.info.modified_ts_secs,
            accessed: self.info.accessed_ts_secs,
            permissions: self.info.permissions.as_ref().map(|p| p.mode() as i64),
            user_id: self.info.user_id.map(|x| x as i64),
            group_id: self.info.group_id.map(|x| x as i64),
            file_size: self.info.byte_length.map(|x| x as i64),
            dev: self.info.dev.map(|x| x as i64),
            ino: self.info.ino.map(|x| x as i64),
            ctime: self.info.ctime_secs,
            ctime_nsec: self.info.ctime_nsec,
            mtime_nsec: self.info.mtime_nsec,
            snapshot_ts_utc: self.info.snapshot_ts_utc,
            hash: hash_ref.map(|r| r.hash.bytes.clone()),
            hash_ref: hash_ref.map(|r| r.as_bytes()),
            symlink_target,
        }
    }

    pub fn from_stat_cache_entry(
        parent_id: Option<u64>,
        name: models::FileName,
        row: StatCacheEntry,
    ) -> (Entry, Option<HashRef>) {
        let hash_ref = row
            .hash_ref
            .as_deref()
            .and_then(|b| HashRef::from_bytes(b).ok());
        let data = match row.kind {
            KIND_FILE => match row.hash {
                Some(h) => Data::FileHash(h),
                None => Data::FilePlaceholder,
            },
            KIND_DIR => Data::DirPlaceholder,
            KIND_SYMLINK => Data::Symlink(PathBuf::from(ffi::OsStr::from_bytes(
                row.symlink_target.as_deref().unwrap_or(b""),
            ))),
            _ => Data::FilePlaceholder,
        };
        let entry = Entry {
            node_id: None,
            parent_id,
            data,
            info: Info {
                name,
                created_ts_secs: row.created,
                modified_ts_secs: row.modified,
                accessed_ts_secs: row.accessed,
                permissions: row
                    .permissions
                    .map(|m| fs::Permissions::from_mode(m as u32)),
                user_id: row.user_id.map(|x| x as u64),
                group_id: row.group_id.map(|x| x as u64),
                byte_length: row.file_size.map(|x| x as u64),
                snapshot_ts_utc: row.snapshot_ts_utc,
                dev: row.dev.map(|x| x as u64),
                ino: row.ino.map(|x| x as u64),
                ctime_secs: row.ctime,
                ctime_nsec: row.ctime_nsec,
                mtime_nsec: row.mtime_nsec,
            },
        };
        (entry, hash_ref)
    }
}

impl From<models::FileInfo> for Info {
    fn from(info: models::FileInfo) -> Info {
        fn none_if_zero_i64(x: i64) -> Option<i64> {
            if x == 0 {
                None
            } else {
                Some(x)
            }
        }
        fn none_if_zero_u64(x: u64) -> Option<u64> {
            if x == 0 {
                None
            } else {
                Some(x)
            }
        }

        Info {
            name: info.name,
            created_ts_secs: none_if_zero_i64(info.created_ts),
            modified_ts_secs: none_if_zero_i64(info.modified_ts),
            accessed_ts_secs: none_if_zero_i64(info.accessed_ts),
            permissions: match info.permissions {
                models::Permissions::None => None,
                models::Permissions::Mode(mode) => Some(fs::Permissions::from_mode(mode)),
            },
            byte_length: none_if_zero_u64(info.byte_length as u64),
            user_id: match info.owner {
                models::Owner::None => None,
                models::Owner::UserGroup(ref ug) => Some(ug.user_id as u64),
            },
            group_id: match info.owner {
                models::Owner::None => None,
                models::Owner::UserGroup(ref ug) => Some(ug.group_id as u64),
            },
            snapshot_ts_utc: info.snapshot_ts_utc,
            dev: None,
            ino: None,
            ctime_secs: None,
            ctime_nsec: None,
            mtime_nsec: None,
        }
    }
}

impl Info {
    pub fn new(name: models::FileName, meta: Option<&fs::Metadata>) -> Info {
        use std::os::unix::fs::MetadataExt;

        use crate::util::system_time_secs;

        let created = meta.and_then(|m| m.created().ok()).map(system_time_secs);
        let modified = meta.and_then(|m| m.modified().ok()).map(system_time_secs);
        let accessed = meta.and_then(|m| m.accessed().ok()).map(system_time_secs);

        Info {
            name,

            created_ts_secs: created,
            modified_ts_secs: modified,
            accessed_ts_secs: accessed,

            permissions: meta.map(|m| m.permissions()),

            user_id: meta.map(|m| m.uid() as u64),
            group_id: meta.map(|m| m.gid() as u64),

            byte_length: meta.map(|m| m.len()),
            snapshot_ts_utc: chrono::Utc::now().timestamp(),

            dev: meta.map(|m| m.dev()),
            ino: meta.map(|m| m.ino()),
            ctime_secs: meta.map(|m| m.ctime()),
            ctime_nsec: meta.map(|m| m.ctime_nsec()),
            mtime_nsec: meta.map(|m| m.mtime_nsec()),
        }
    }

    pub fn to_model(&self) -> models::FileInfo {
        let owner = match (self.user_id, self.group_id) {
            (Some(user_id), Some(group_id)) => models::Owner::UserGroup(models::UserGroup {
                user_id: user_id as i64,
                group_id: group_id as i64,
            }),
            _ => models::Owner::None,
        };

        models::FileInfo {
            name: self.name.clone(),
            created_ts: self.created_ts_secs.unwrap_or(0),
            modified_ts: self.modified_ts_secs.unwrap_or(0),
            accessed_ts: self.accessed_ts_secs.unwrap_or(0),
            permissions: match self.permissions {
                None => models::Permissions::None,
                Some(ref perm) => models::Permissions::Mode(perm.mode()),
            },
            byte_length: self.byte_length.unwrap_or(0) as i64,
            owner,
            snapshot_ts_utc: self.snapshot_ts_utc,
        }
    }
}

/// Ephemeral in-memory staged directory tree built during a single snapshot scan and
/// serialized into immutable `TreeList` chunks on commit. Never persisted directly.
#[derive(Default)]
pub struct StagedTree {
    next_id: u64,
    /// Per-parent children keyed by raw filename bytes so lookup/dedup is O(log N)
    /// and iteration is already in deterministic byte order.
    children: BTreeMap<Option<u64>, BTreeMap<Vec<u8>, (Entry, Option<HashRef>)>>,
    /// True when the current staging pass originated from a filesystem walk (`snapshot_dir`),
    /// enabling safe pruning of stale `stat_cache` rows once the snapshot publishes.
    stat_walk_active: bool,
    seen_stat_paths: HashSet<Vec<u8>>,
}

impl StagedTree {
    pub fn clear(&mut self) {
        self.next_id = 0;
        self.children.clear();
        self.stat_walk_active = false;
        self.seen_stat_paths.clear();
    }

    pub fn begin_stat_walk(&mut self) {
        self.clear();
        self.stat_walk_active = true;
    }

    /// Release staged tree nodes while retaining `seen_stat_paths` until publish finishes.
    pub fn clear_entries(&mut self) {
        self.next_id = 0;
        self.children.clear();
    }

    pub fn record_stat_path(&mut self, rel_path: &[u8]) {
        if self.stat_walk_active {
            self.seen_stat_paths.insert(rel_path.to_vec());
        }
    }

    pub fn take_seen_stat_paths(&mut self) -> Option<HashSet<Vec<u8>>> {
        if !self.stat_walk_active {
            return None;
        }
        self.stat_walk_active = false;
        Some(std::mem::take(&mut self.seen_stat_paths))
    }

    pub fn insert(&mut self, mut entry: Entry, hash_ref: Option<HashRef>) -> u64 {
        let name_key = entry.info.name.as_bytes().to_vec();
        let siblings = self.children.entry(entry.parent_id).or_default();
        if let Some(existing) = siblings.get_mut(&name_key) {
            let id = existing.0.node_id.expect("staged entry has node_id");
            entry.node_id = Some(id);
            *existing = (entry, hash_ref);
            return id;
        }
        self.next_id += 1;
        let id = self.next_id;
        entry.node_id = Some(id);
        siblings.insert(name_key, (entry, hash_ref));
        id
    }

    pub fn list_dir(&self, parent: Option<u64>) -> Vec<(Entry, Option<HashRef>)> {
        self.children
            .get(&parent)
            .map(|siblings| siblings.values().cloned().collect())
            .unwrap_or_default()
    }
}
