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

//! High level Hat API

use crate::backend::StoreBackend;
use crate::blob;
use crate::cbor_util;
use crate::crypto;
use crate::db;
use crate::errors::HatError;
use crate::hash;
use crate::key;
use crate::models;
use crate::snapshot;
use crate::tags;
use hex;
use std::cmp;
use std::collections::{HashSet, VecDeque};
use std::ffi;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub mod family;
mod insert_path_handler;
pub mod walker;
pub use self::family::Family;

#[cfg(all(test, feature = "benchmarks"))]
mod benchmarks;
#[cfg(test)]
mod tests;

pub struct Hat<B: StoreBackend> {
    keys: Arc<crypto::keys::Keeper>,
    _repo_lock: Option<fs::File>,
    /// `(dev, ino)` identities of directories excluded from `snapshot_dir` walks
    /// (state dir, cache dir, and local backend storage dir if any).
    excluded_dirs: HashSet<(u64, u64)>,
    families: Vec<Family<B>>,
    db: Arc<db::Index>,
    snapshot_index: snapshot::SnapshotIndex,
    hash_index: Arc<hash::HashIndex>,
    backend: Arc<B>,
    blob_index: Arc<blob::BlobIndex>,
    blob_store: Arc<blob::BlobStore<B>>,
    blob_max_size: usize,
    commit_fault: Option<CommitFault>,
}

/// Type alias preserved for API compatibility.
pub type HatRc<B> = Hat<B>;

/// Injection points for simulating crash mid-commit (tests / debug env).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitFault {
    /// Fires after all file and directory blobs are flushed to the backend,
    /// immediately before SQLite publishes the completed snapshot row.
    BeforePublish,
}

impl CommitFault {
    pub fn parse(s: &str) -> Option<CommitFault> {
        match s {
            "BeforePublish" => Some(CommitFault::BeforePublish),
            _ => None,
        }
    }
}

fn concat_filename(mut a: PathBuf, b: &str) -> String {
    a.push(b);
    a.into_os_string().into_string().unwrap()
}

fn dir_dev_ino(path: &std::path::Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path)
            .ok()
            .filter(|m| m.is_dir())
            .map(|m| (m.dev(), m.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn ensure_cachedir_tag(cache_dir: &std::path::Path) -> Result<(), HatError> {
    const TAG_BODY: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n\
# This file is a cache directory tag created by Hat Backup.\n\
# For information about cache directory tags, see:\n\
#   https://bford.info/cachedir/\n";
    let tag_path = cache_dir.join("CACHEDIR.TAG");
    if !tag_path.exists() {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&tag_path) {
            Ok(mut f) => {
                use std::io::Write;
                f.write_all(TAG_BODY)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn index_db_path(root: PathBuf) -> String {
    concat_filename(root, "index.sqlite3")
}

fn synthetic_roots_family() -> String {
    "__hat__roots__".to_string()
}

/// Visitor that collects every branch/leaf node of a hash tree without reading leaf payloads.
struct TreeNodeCollector {
    nodes: Vec<(hash::tree::HashRef, Option<Vec<hash::tree::HashRef>>)>,
}

impl TreeNodeCollector {
    fn new() -> Self {
        TreeNodeCollector { nodes: Vec::new() }
    }
}

impl hash::tree::Visitor for TreeNodeCollector {
    fn branch_enter(
        &mut self,
        href: &hash::tree::HashRef,
        childs: &Vec<hash::tree::HashRef>,
    ) -> bool {
        self.nodes.push((href.clone(), Some(childs.clone())));
        true
    }

    fn leaf_enter(&mut self, href: &hash::tree::HashRef) -> bool {
        self.nodes.push((href.clone(), None));
        false
    }

    fn leaf_leave(&mut self, _chunk: Vec<u8>, _href: &hash::tree::HashRef) -> bool {
        unreachable!("TreeNodeCollector never enters leaf payloads")
    }
}

/// Visitor for a `TreeList` directory hash tree: collects all `TreeList` nodes AND
/// decodes the directory entries from its leaves.
struct DirTreeCollector {
    nodes: Vec<(hash::tree::HashRef, Option<Vec<hash::tree::HashRef>>)>,
    entries: Vec<walker::FileEntry>,
    parse_err: Option<HatError>,
}

impl DirTreeCollector {
    fn new() -> Self {
        DirTreeCollector {
            nodes: Vec::new(),
            entries: Vec::new(),
            parse_err: None,
        }
    }
}

impl hash::tree::Visitor for DirTreeCollector {
    fn branch_enter(
        &mut self,
        href: &hash::tree::HashRef,
        childs: &Vec<hash::tree::HashRef>,
    ) -> bool {
        self.nodes.push((href.clone(), Some(childs.clone())));
        true
    }

    fn leaf_enter(&mut self, href: &hash::tree::HashRef) -> bool {
        self.nodes.push((href.clone(), None));
        matches!(href.leaf, blob::LeafType::TreeList)
    }

    fn leaf_leave(&mut self, chunk: Vec<u8>, _href: &hash::tree::HashRef) -> bool {
        if self.parse_err.is_none() {
            if let Err(e) = family::parse_dir_data(&chunk[..], &mut self.entries) {
                self.parse_err = Some(e);
            }
        }
        true
    }
}

impl<B: StoreBackend> Hat<B> {
    pub fn open_repository(
        mut repository_root: PathBuf,
        backend: Arc<B>,
        max_blob_size: usize,
    ) -> Result<Hat<B>, HatError> {
        let keys = Arc::new(crypto::keys::Keeper::load_from_universal_key(
            &repository_root,
        )?);

        let state_dir = repository_root.clone();
        repository_root = repository_root.join("cache");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            if !repository_root.exists() {
                fs::DirBuilder::new().mode(0o700).create(&repository_root)?;
            } else {
                let mode = fs::metadata(&repository_root)?.permissions().mode();
                if mode & 0o077 != 0 {
                    fs::set_permissions(&repository_root, fs::Permissions::from_mode(0o700))?;
                }
            }
        }
        #[cfg(not(unix))]
        if !repository_root.exists() {
            fs::create_dir(&repository_root)?;
        }

        ensure_cachedir_tag(&repository_root)?;

        // Acquire an exclusive, fail-fast cross-process lock for this repository state dir.
        // Held on `Hat` for its lifetime and automatically released on drop or process exit.
        let lock_path = repository_root.join(".hat.lock");
        let lock_file = {
            let mut opts = fs::OpenOptions::new();
            opts.read(true).write(true).create(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let file = opts.open(&lock_path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = file.metadata()?.permissions().mode();
                if mode & 0o077 != 0 {
                    fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600))?;
                }
            }
            match file.try_lock() {
                Ok(()) => file,
                Err(fs::TryLockError::WouldBlock) => {
                    return Err(format!(
                        "Repository is locked by another process ({})",
                        lock_path.display()
                    )
                    .into());
                }
                Err(fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        };

        let db_path = index_db_path(repository_root.clone());
        let db_p = Arc::new(db::Index::new(&db_path)?);

        let si_p = snapshot::SnapshotIndex::new(db_p.clone());
        let hi_p = Arc::new(hash::HashIndex::new(db_p.clone())?);
        let bi_p = Arc::new(blob::BlobIndex::new(keys.clone(), db_p.clone())?);
        let bs_p = Arc::new(blob::BlobStore::new(
            keys.clone(),
            bi_p.clone(),
            backend.clone(),
            max_blob_size,
        ));

        let mut excluded_dirs = HashSet::new();
        for dir in [
            Some(state_dir.as_path()),
            Some(repository_root.as_path()),
            backend.local_storage_dir(),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(id) = dir_dev_ino(dir) {
                excluded_dirs.insert(id);
            }
        }

        let mut hat = Hat {
            keys,
            _repo_lock: Some(lock_file),
            excluded_dirs,
            families: vec![],
            db: db_p,
            snapshot_index: si_p,
            hash_index: hi_p,
            backend,
            blob_index: bi_p,
            blob_store: bs_p,
            blob_max_size: max_blob_size,
            commit_fault: None,
        };

        hat.resume()?;

        Ok(hat)
    }

    #[cfg(test)]
    pub fn new_for_testing(backend: Arc<B>, max_blob_size: usize) -> Result<Hat<B>, HatError> {
        let keys = Arc::new(crypto::keys::Keeper::new_for_testing());

        let db_p = Arc::new(db::Index::new_for_testing());
        let si_p = snapshot::SnapshotIndex::new(db_p.clone());
        let bi_p = Arc::new(blob::BlobIndex::new(keys.clone(), db_p.clone()).unwrap());
        let hi_p = Arc::new(hash::HashIndex::new(db_p.clone()).unwrap());
        let bs_p = Arc::new(blob::BlobStore::new(
            keys.clone(),
            bi_p.clone(),
            backend.clone(),
            max_blob_size,
        ));

        let mut hat = Hat {
            keys,
            _repo_lock: None,
            excluded_dirs: HashSet::new(),
            families: vec![],
            db: db_p,
            snapshot_index: si_p,
            hash_index: hi_p,
            blob_index: bi_p,
            blob_store: bs_p,
            blob_max_size: max_blob_size,
            backend,
            commit_fault: None,
        };

        hat.resume()?;

        Ok(hat)
    }

    pub fn inject_commit_fault(&mut self, fault: CommitFault) {
        self.commit_fault = Some(fault);
    }

    /// Number of unfinished snapshots currently in the local index (always 0 under atomic commits).
    pub fn unfinished_snapshot_count(&mut self) -> usize {
        self.snapshot_index.list_not_done().len()
    }

    /// Latest committed snapshot ID for `family_name`, if any.
    pub fn latest_snapshot_id(&mut self, family_name: &str) -> Option<u64> {
        self.snapshot_index
            .latest(family_name)
            .map(|(info, _, _)| info.snapshot_id)
    }

    /// Latest committed `SnapshotList` root hash (published by `meta_commit`), if any.
    /// Callers can pin this out-of-band and pass it to `recover_with_expected_root`.
    pub fn latest_meta_root_hash(&mut self) -> Option<Vec<u8>> {
        self.snapshot_index
            .latest(&synthetic_roots_family())
            .map(|(_, hash, _)| hash.bytes)
    }

    fn env_commit_fault() -> Option<CommitFault> {
        std::env::var("HAT_TEST_FAULT")
            .ok()
            .and_then(|s| CommitFault::parse(&s))
    }

    fn maybe_commit_fault(&mut self, stage: CommitFault) -> Result<(), HatError> {
        let hit = self.commit_fault == Some(stage) || Self::env_commit_fault() == Some(stage);
        if hit {
            self.commit_fault = None;
            Self::wait_fault_barrier_if_set();
            return Err(From::from(format!("injected commit fault: {:?}", stage)));
        }
        Ok(())
    }

    fn wait_fault_barrier_if_set() {
        if let Ok(path) = std::env::var("HAT_TEST_BARRIER") {
            let _ = fs::write(&path, b"ready");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
    }

    pub fn hash_tree_writer(
        &mut self,
        leaf: blob::LeafType,
    ) -> hash::tree::SimpleHashTreeWriter<key::HashStoreBackend<B>> {
        hash::tree::SimpleHashTreeWriter::new(leaf, 8, self.hash_backend())
    }

    pub fn open_family(&mut self, name: String) -> Result<Family<B>, HatError> {
        for family in &self.families {
            if family.name == name {
                return Ok(family.clone());
            }
        }

        let family_id = self.db.lock().get_or_create_family_id(&name);
        let staged = Arc::new(Mutex::new(key::StagedTree::default()));

        let mut kss = vec![];
        for _ in 0..2 {
            // To avoid mixing chunks from different files, each worker store gets its own
            // dedicated BlobStore while sharing the single local SQLite cache DB.
            let bs = Arc::new(blob::BlobStore::new(
                self.keys.clone(),
                self.blob_index.clone(),
                self.backend.clone(),
                self.blob_max_size,
            ));
            kss.push(key::Store::new(
                family_id,
                self.db.clone(),
                staged.clone(),
                self.hash_index.clone(),
                bs,
                self.keys.clone(),
            ));
        }

        let ks = key::Store::new(
            family_id,
            self.db.clone(),
            staged,
            self.hash_index.clone(),
            self.blob_store.clone(),
            self.keys.clone(),
        );
        kss.push(ks.clone());

        let family = Family {
            name,
            key_store: ks,
            key_stores: kss,
            excluded_dirs: self.excluded_dirs.clone(),
        };
        self.families.push(family.clone());

        Ok(family)
    }

    pub fn delete_all_snapshots(&mut self) -> Result<(), HatError> {
        // Deletes ALL snapshots from ALL families, including meta snapshots used for
        // recovery. After calling this and running `gc()`, all blobs should be gone.
        for snapshot in self.snapshot_index.list_all() {
            let id = snapshot.info.snapshot_id;
            self.deregister_by_name(snapshot.family_name, id)?;
        }
        Ok(())
    }

    pub fn meta_commit(&mut self) -> Result<(), HatError> {
        let all_snapshots = self.snapshot_index.list_all();

        let mut snapshots = models::Snapshots { snapshots: vec![] };
        let mut all_root_ids = vec![];

        for snapshot in all_snapshots {
            let model = models::Snapshot {
                id: snapshot.info.snapshot_id,
                family_name: snapshot.family_name,
                msg: snapshot.msg.unwrap_or("".into()),
                hash_ref: hash::tree::HashRef::from_bytes(&snapshot.hash_ref.unwrap()[..])?
                    .to_model(),
                created_ts_utc: snapshot.created.timestamp(),
            };

            if model.family_name == synthetic_roots_family() {
                all_root_ids.push(snapshot.info.snapshot_id);
            }

            snapshots.snapshots.push(model);
        }

        // Chunk snapshot listings across multiple SnapshotList leaves so large histories
        // never exceed `blob_max_size`. Each leaf is a self-contained CBOR `models::Snapshots`.
        let mut tree = self.hash_tree_writer(blob::LeafType::SnapshotList);
        let max_leaf_bytes = cmp::min(64 * 1024, cmp::max(512, self.blob_max_size / 4));
        const MAX_SNAPSHOTS_PER_LEAF: usize = 256;

        if snapshots.snapshots.is_empty() {
            let listing = cbor_util::to_vec(&snapshots)?;
            tree.append(&listing[..])?;
        } else {
            let mut batch: Vec<models::Snapshot> = Vec::new();
            let mut batch_bytes: usize = 0;

            for snap in snapshots.snapshots {
                let snap_bytes = cbor_util::to_vec(&snap)?.len();
                if !batch.is_empty()
                    && (batch_bytes + snap_bytes > max_leaf_bytes
                        || batch.len() >= MAX_SNAPSHOTS_PER_LEAF)
                {
                    let listing = cbor_util::to_vec(&models::Snapshots {
                        snapshots: std::mem::take(&mut batch),
                    })?;
                    tree.append(&listing[..])?;
                    batch_bytes = 0;
                }
                batch_bytes += snap_bytes;
                batch.push(snap);
            }

            if !batch.is_empty() {
                let listing = cbor_util::to_vec(&models::Snapshots { snapshots: batch })?;
                tree.append(&listing[..])?;
            }
        }

        let top_ref = tree.hash(None)?;
        // Flush root blob(s) to backend before publishing the synthetic root row locally.
        self.flush_blob_store();
        self.snapshot_index
            .insert_done(synthetic_roots_family(), &top_ref.hash, &top_ref);

        // Keep a fixed window of meta roots.
        const META_ROOTS_TO_KEEP: usize = 10;
        all_root_ids.sort();
        for id in all_root_ids.iter().rev().skip(META_ROOTS_TO_KEEP) {
            self.deregister_by_name(synthetic_roots_family(), *id)?;
        }

        self.meta_flush();
        Ok(())
    }

    fn recover_root(&mut self) -> Result<Option<hash::tree::HashRef>, HatError> {
        let blobs = self.blob_store.list_by_tag(tags::Tag::Done);
        info!("{} blobs to investigate", blobs.len());
        for b in blobs.into_iter() {
            info!("Inspecting blob: {}", hex::encode(&b.name));
            // Iterate in reverse so the latest SnapshotList within the highest-ID blob wins.
            for r in self
                .blob_store
                .retrieve_refs(b)?
                .unwrap_or_default()
                .into_iter()
                .rev()
            {
                match r.leaf {
                    blob::LeafType::SnapshotList => {
                        return Ok(Some(r));
                    }
                    blob::LeafType::TreeList => {
                        warn!("Skipping directory listing: {}", hex::encode(&r.hash.bytes))
                    }
                    blob::LeafType::FileChunk => {
                        warn!("Skipping file contents: {}", hex::encode(&r.hash.bytes))
                    }
                }
            }
        }
        Ok(None)
    }

    pub fn recover(&mut self) -> Result<(), HatError> {
        self.recover_with_expected_root(None)
    }

    /// Recover local state from backend storage. When `expected_root_hash` is provided
    /// (out-of-band pinned root digest), fail closed if the discovered root does not match,
    /// preventing silent rollback/replay by an untrusted backend.
    pub fn recover_with_expected_root(
        &mut self,
        expected_root_hash: Option<&[u8]>,
    ) -> Result<(), HatError> {
        self.blob_store.recover()?;
        // Drop any cached Family handles whose worker BlobStores may hold pre-recovery blob IDs.
        self.families.clear();
        let root_href = self
            .recover_root()?
            .ok_or("Failed to find a commit-ed root")?;
        if let Some(expected) = expected_root_hash {
            if !crypto::keys::ct_eq(&root_href.hash.bytes, expected) {
                return Err(format!(
                    "recovered root hash mismatch (possible backend rollback): expected {}, got {}",
                    hex::encode(expected),
                    hex::encode(&root_href.hash.bytes)
                )
                .into());
            }
        }

        info!(
            "Recovering using root: {}",
            hex::encode(&root_href.hash.bytes)
        );
        info!(
            ".. from blob: {}",
            hex::encode(&root_href.persistent_ref.blob_name)
        );

        // Recover the SnapshotList tree's own hash nodes first.
        self.recover_hash_tree_nodes(&root_href)?;

        use chrono::TimeZone;
        let mut max_created = chrono::Utc.timestamp_opt(0, 0).single().unwrap();

        let mut recovered_snaps = Vec::new();
        for msg in hash::tree::LeafIterator::new(self.hash_backend(), root_href.clone())?
            .ok_or("Failed to open root snapshot list")?
        {
            let msg = msg?;
            let snapshot_list: models::Snapshots = cbor_util::from_slice(&msg[..])?;
            for s in snapshot_list.snapshots {
                let created = chrono::Utc
                    .timestamp_opt(s.created_ts_utc, 0)
                    .single()
                    .unwrap();
                max_created = cmp::max(max_created, created);
                let hash_ref: hash::tree::HashRef = From::from(s.hash_ref);
                recovered_snaps.push((s.id, s.family_name, created, s.msg, hash_ref));
            }
        }

        // Recover each snapshot's hash tree + local tree_edges cache, then record it as Done.
        for (id, family_name, created, msg, hash_ref) in recovered_snaps {
            self.recover_snapshot_tree(&hash_ref)?;
            self.snapshot_index
                .recover(id, &family_name, created, &msg, &hash_ref, None);
        }

        // Register the newly found root itself (since a root cannot contain itself).
        let latest = self.snapshot_index.latest(&synthetic_roots_family());
        let next_id = match latest {
            Some((_, _, Some(ref href))) if href.hash == root_href.hash => {
                self.flush_blob_store();
                self.meta_flush();
                return Ok(());
            }
            Some((info, _, _)) => info.snapshot_id + 1,
            None => 1,
        };

        self.snapshot_index.recover(
            next_id,
            &synthetic_roots_family(),
            max_created,
            "",
            &root_href,
            None,
        );
        self.flush_blob_store();
        self.meta_flush();

        Ok(())
    }

    fn recover_node_entry(
        &self,
        href: hash::tree::HashRef,
        childs: Option<Vec<hash::tree::HashRef>>,
    ) -> Result<u64, HatError> {
        let mut pref = href.persistent_ref.clone();
        pref.blob_id = Some(
            self.blob_store
                .find(&pref.blob_name)
                .map(|b| b.id)
                .ok_or_else(|| format!("unknown blob: {:?}", pref.blob_name))?,
        );

        let child_ids = childs.map(|hs| {
            hs.into_iter()
                .map(|h| {
                    let e = hash::Entry {
                        hash: h.hash,
                        persistent_ref: Some(h.persistent_ref),
                        node: h.node,
                        leaf: h.leaf,
                        childs: None,
                    };
                    match self.hash_index.reserve(&e) {
                        hash::ReserveResult::HashKnown(id)
                        | hash::ReserveResult::ReserveOk(id) => id,
                    }
                })
                .collect()
        });

        let entry = hash::Entry {
            hash: href.hash,
            persistent_ref: Some(pref),
            node: href.node,
            leaf: href.leaf,
            childs: child_ids,
        };

        let id = match self.hash_index.reserve(&entry) {
            hash::ReserveResult::HashKnown(id) => {
                if self.hash_index.reserved_id(&entry.hash).is_none() {
                    return Ok(id);
                }
                id
            }
            hash::ReserveResult::ReserveOk(id) => id,
        };
        self.hash_index.commit(id, Some(entry));
        Ok(id)
    }

    /// Recover all hash-tree branch and leaf index entries for `root_href` without reading
    /// leaf payload bytes. Returns the root `hash_id`.
    fn recover_hash_tree_nodes(&self, root_href: &hash::tree::HashRef) -> Result<u64, HatError> {
        let mut walker = hash::tree::Walker::new(self.hash_backend(), root_href.clone())?
            .ok_or("Failed to open hash tree during recover")?;
        let mut collector = TreeNodeCollector::new();
        while walker.resume(&mut collector)? {}

        // Commit leaves before their parent branches so child IDs are already ready.
        for (href, childs) in collector.nodes.into_iter().rev() {
            self.recover_node_entry(href, childs)?;
        }

        self.hash_index
            .get_id(&root_href.hash)
            .ok_or_else(|| "recovered root hash missing from hash index".into())
    }

    /// Recover a snapshot's hash tree (`TreeList` or `SnapshotList`) and populate
    /// the local `tree_edges` cache for every recovered directory.
    fn recover_snapshot_tree(&self, root_href: &hash::tree::HashRef) -> Result<(), HatError> {
        if root_href.leaf != blob::LeafType::TreeList {
            self.recover_hash_tree_nodes(root_href)?;
            return Ok(());
        }

        let mut dir_queue: VecDeque<hash::tree::HashRef> = VecDeque::new();
        let mut visited_dirs: HashSet<Vec<u8>> = HashSet::new();
        let mut recovered_dirs: Vec<(u64, Vec<walker::FileEntry>)> = Vec::new();
        dir_queue.push_back(root_href.clone());

        // Phase 1: recover every directory's full TreeList hash-tree nodes (leaves + branches).
        while let Some(dir_href) = dir_queue.pop_front() {
            if !visited_dirs.insert(dir_href.hash.bytes.clone()) {
                continue;
            }

            let mut walker = hash::tree::Walker::new(self.hash_backend(), dir_href.clone())?
                .ok_or("Failed to open TreeList during recover")?;
            let mut collector = DirTreeCollector::new();
            while walker.resume(&mut collector)? {}
            if let Some(err) = collector.parse_err {
                return Err(err);
            }

            // Commit leaves before their parent branches so every branch gets its real childs.
            for (href, childs) in collector.nodes.into_iter().rev() {
                self.recover_node_entry(href, childs)?;
            }

            let dir_hash_id = self
                .hash_index
                .get_id(&dir_href.hash)
                .ok_or("recovered dir hash missing from hash index")?;

            for entry in &collector.entries {
                if let walker::Content::Dir(ref sub_dir_href) = entry.hash_ref {
                    dir_queue.push_back(sub_dir_href.clone());
                }
            }

            recovered_dirs.push((dir_hash_id, collector.entries));
        }

        // Phase 2: recover file hash-trees and record local tree_edges for every directory.
        for (dir_hash_id, entries) in recovered_dirs {
            let mut child_ids = Vec::new();
            for entry in entries {
                match entry.hash_ref {
                    walker::Content::Data(file_href) => {
                        let file_id = self.recover_hash_tree_nodes(&file_href)?;
                        child_ids.push(file_id);
                    }
                    walker::Content::Dir(sub_dir_href) => {
                        let sub_id = self
                            .hash_index
                            .get_id(&sub_dir_href.hash)
                            .ok_or("recovered subdir hash missing from hash index")?;
                        child_ids.push(sub_id);
                    }
                    walker::Content::Link(_) => {}
                }
            }

            self.db.lock().tree_edges_set(dir_hash_id, &child_ids);
        }

        Ok(())
    }

    /// Clean up any legacy incomplete snapshot rows. Under atomic snapshot commits,
    /// new commits/deletes never leave incomplete rows behind.
    pub fn resume(&mut self) -> Result<(), HatError> {
        let removed = self.snapshot_index.delete_incomplete();
        if removed > 0 {
            self.meta_flush();
        }
        Ok(())
    }

    pub fn commit_by_name(&mut self, family_name: String) -> Result<(), HatError> {
        let mut family = self.open_family(family_name)?;
        self.commit(&mut family)?;
        Ok(())
    }

    /// Commit `family`'s staged tree atomically:
    /// 1. Serialize immutable `TreeList` chunks and record local `tree_edges`.
    /// 2. Flush all pending data/tree blobs to the backend.
    /// 3. Publish one completed `Tag::Done` snapshot row in SQLite.
    ///
    /// Crashing before step 3 leaves the repository consistent at the previous snapshot;
    /// any orphaned blobs are cleanly reclaimed by `gc()`.
    pub fn commit(&mut self, family: &mut Family<B>) -> Result<(), HatError> {
        let top_ref = family.commit()?;

        // All file and directory blobs must hit the backend before SQLite publishes the snapshot.
        family.flush()?;
        self.flush_blob_store();
        self.maybe_commit_fault(CommitFault::BeforePublish)?;

        self.snapshot_index
            .insert_done(family.name.clone(), &top_ref.hash, &top_ref);
        if let Some(seen_paths) = family.key_store.take_seen_stat_paths() {
            self.db
                .lock()
                .stat_cache_prune_family(family.key_store.family_id(), &seen_paths);
        }
        self.meta_flush();

        Ok(())
    }

    pub fn meta_flush(&self) {
        self.db.lock().flush();
    }

    pub fn data_flush(&self) -> Result<(), HatError> {
        for family in &self.families {
            family.flush()?
        }
        self.blob_store.flush();
        self.meta_flush();
        Ok(())
    }

    pub fn flush_snapshot_index(&mut self) {
        self.snapshot_index.flush();
    }

    pub fn flush_blob_store(&self) {
        self.blob_store.flush();
    }

    pub fn list_snapshots(&mut self) -> Vec<db::SnapshotStatus> {
        self.snapshot_index.list_all()
    }

    pub fn checkout_in_dir(
        &mut self,
        family_name: String,
        output_dir: PathBuf,
    ) -> Result<(), HatError> {
        let (_info, _dir_hash, dir_ref) = match self.snapshot_index.latest(&family_name) {
            Some((i, h, Some(r))) => (i, h, r),
            _ => {
                return Err(format!(
                    "Tried to checkout family '{}' before first completed commit",
                    family_name
                )
                .into())
            }
        };

        let mut output_dir = output_dir;
        self.checkout_dir_ref(&mut output_dir, dir_ref)
    }

    pub fn checkout_snapshot_in_dir(
        &mut self,
        family_name: String,
        snapshot_id: u64,
        output_dir: PathBuf,
    ) -> Result<(), HatError> {
        let (_info, _dir_hash, dir_ref) =
            match self.snapshot_index.lookup(&family_name, snapshot_id) {
                Some((i, h, Some(r))) => (i, h, r),
                _ => {
                    return Err(format!(
                        "No complete snapshot found for family '{}' with id {}",
                        family_name, snapshot_id
                    )
                    .into())
                }
            };

        let mut output_dir = output_dir;
        self.checkout_dir_ref(&mut output_dir, dir_ref)
    }

    /// Checkout a snapshot path resembling `ls` paths into `output_dir`:
    /// - `<family>` or `/<family>` (latest snapshot, or `snapshot_id_override` if given)
    /// - `<family>/<id|latest>` (full snapshot)
    /// - `<family>/<id|latest>/<subpath>` (directory contents or single file/symlink)
    pub fn checkout_path_in_dir(
        &mut self,
        snapshot_path: &std::path::Path,
        snapshot_id_override: Option<u64>,
        output_dir: PathBuf,
    ) -> Result<(), HatError> {
        use std::path::Component;

        let mut components = Vec::new();
        let mut started = false;
        for comp in snapshot_path.components() {
            match comp {
                Component::RootDir | Component::CurDir if !started => continue,
                Component::CurDir => continue,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                    return Err(format!(
                        "Invalid snapshot path '{}'",
                        snapshot_path.display()
                    )
                    .into());
                }
                Component::Normal(s) => {
                    started = true;
                    components.push(s.to_owned());
                }
            }
        }

        let family_name = match components.first() {
            Some(f) => f.to_string_lossy().into_owned(),
            None => {
                return Err(
                    "Snapshot path must specify a family (e.g. 'family', 'family/1', or 'family/1/subdir')"
                        .into(),
                )
            }
        };

        let (resolved_label, root_ref) = match components.get(1) {
            None => match snapshot_id_override {
                Some(id) => {
                    let (_info, _h, dir_ref) = match self.snapshot_index.lookup(&family_name, id) {
                        Some((i, h, Some(r))) => (i, h, r),
                        _ => {
                            return Err(format!(
                                "No complete snapshot found for family '{}' with id {}",
                                family_name, id
                            )
                            .into())
                        }
                    };
                    (id.to_string(), dir_ref)
                }
                None => {
                    let (info, _h, dir_ref) = match self.snapshot_index.latest(&family_name) {
                        Some((i, h, Some(r))) => (i, h, r),
                        _ => {
                            return Err(format!(
                                "Tried to checkout family '{}' before first completed commit",
                                family_name
                            )
                            .into())
                        }
                    };
                    (info.snapshot_id.to_string(), dir_ref)
                }
            },
            Some(id_os) => {
                if snapshot_id_override.is_some() {
                    return Err(format!(
                        "Cannot specify snapshot ID both in '{}' and via --id",
                        snapshot_path.display()
                    )
                    .into());
                }
                let id_str = id_os.to_string_lossy();
                if id_str == "latest" {
                    let (info, _h, dir_ref) = match self.snapshot_index.latest(&family_name) {
                        Some((i, h, Some(r))) => (i, h, r),
                        _ => {
                            return Err(format!(
                                "Tried to checkout family '{}' before first completed commit",
                                family_name
                            )
                            .into())
                        }
                    };
                    (info.snapshot_id.to_string(), dir_ref)
                } else {
                    let id: u64 = id_str.parse().map_err(|_| {
                        format!(
                            "Invalid snapshot ID '{}' in '{}' (expected a numeric ID or 'latest')",
                            id_str,
                            snapshot_path.display()
                        )
                    })?;
                    let (_info, _h, dir_ref) = match self.snapshot_index.lookup(&family_name, id) {
                        Some((i, h, Some(r))) => (i, h, r),
                        _ => {
                            return Err(format!(
                                "No complete snapshot found for family '{}' with id {}",
                                family_name, id
                            )
                            .into())
                        }
                    };
                    (id.to_string(), dir_ref)
                }
            }
        };

        let sub_components = if components.len() > 2 {
            &components[2..]
        } else {
            &[]
        };

        let mut output = output_dir;
        if sub_components.is_empty() {
            return self.checkout_dir_ref(&mut output, root_ref);
        }

        // Walk down the TreeList to locate the requested subpath.
        let mut current_dir_ref = root_ref;
        for (idx, comp) in sub_components.iter().enumerate() {
            let is_last = idx + 1 == sub_components.len();
            let target_name: models::FileName = comp.clone().into();
            let entries =
                family::Family::<B>::fetch_dir_data(current_dir_ref, self.hash_backend())?;
            let found = entries
                .into_iter()
                .find(|(e, _)| e.info.name == target_name);
            let Some((entry, content)) = found else {
                return Err(format!(
                    "Path '{}' not found in snapshot '{}/{}'",
                    snapshot_path.display(),
                    family_name,
                    resolved_label
                )
                .into());
            };

            if is_last {
                match content {
                    walker::Content::Dir(dir_ref) => {
                        return self.checkout_dir_ref(&mut output, dir_ref);
                    }
                    other => {
                        fs::create_dir_all(&output)?;
                        return self.checkout_entry(&mut output, entry, other);
                    }
                }
            } else {
                match content {
                    walker::Content::Dir(dir_ref) => current_dir_ref = dir_ref,
                    _ => {
                        return Err(format!(
                            "Path component '{}' is not a directory in snapshot '{}/{}'",
                            comp.to_string_lossy(),
                            family_name,
                            resolved_label
                        )
                        .into());
                    }
                }
            }
        }

        Ok(())
    }

    fn checkout_entry(
        &self,
        output: &mut PathBuf,
        entry: key::Entry,
        content: walker::Content,
    ) -> Result<(), HatError> {
        let name_os_string: ffi::OsString = entry.info.name.into();
        crate::util::validate_entry_name(&name_os_string)?;
        output.push(&name_os_string);

        let is_symlink = matches!(content, walker::Content::Link(_));
        match content {
            walker::Content::Data(hash_ref) => {
                let mut fd = fs::File::create(&output)?;
                let tree_opt = hash::tree::LeafIterator::new(self.hash_backend(), hash_ref)?;
                if let Some(tree) = tree_opt {
                    family::Family::<B>::write_file_chunks(&mut fd, tree)?;
                }
            }
            walker::Content::Dir(hash_ref) => {
                self.checkout_dir_ref(output, hash_ref)?;
            }
            walker::Content::Link(link_path) => {
                use std::os::unix::fs::symlink;
                symlink(link_path, &output)?
            }
        }

        if !is_symlink {
            if let Some(perms) = entry.info.permissions {
                let current = fs::symlink_metadata(&output)?.permissions();
                if current != perms {
                    fs::set_permissions(&output, perms)?;
                }
            }
        }

        if let (Some(m), Some(a)) = (entry.info.modified_ts_secs, entry.info.accessed_ts_secs) {
            crate::util::set_times_nofollow(output, a, m)?;
        }

        output.pop();
        Ok(())
    }

    fn checkout_dir_ref(
        &self,
        output: &mut PathBuf,
        dir_hash: hash::tree::HashRef,
    ) -> Result<(), HatError> {
        fs::create_dir_all(&output)?;
        for (entry, hash_ref) in
            family::Family::<B>::fetch_dir_data(dir_hash, self.hash_backend())?
        {
            self.checkout_entry(output, entry, hash_ref)?;
        }
        Ok(())
    }

    /// Delete a snapshot record locally in O(1) with zero remote downloads.
    /// Run `meta_commit()` to publish the updated snapshot listing and `gc()` to reclaim blobs.
    pub fn deregister_by_name(
        &mut self,
        family_name: String,
        snapshot_id: u64,
    ) -> Result<(), HatError> {
        let (info, _top_hash, _top_ref) = match self.snapshot_index.lookup(&family_name, snapshot_id)
        {
            Some((i, h, Some(r))) => (i, h, r),
            _ => {
                return Err(From::from(format!(
                    "No complete snapshot found for family {} with id {:?}",
                    family_name, snapshot_id
                )));
            }
        };

        self.snapshot_index.delete(info);
        self.meta_flush();
        Ok(())
    }

    pub fn deregister(&mut self, family: &Family<B>, snapshot_id: u64) -> Result<(), HatError> {
        self.deregister_by_name(family.name.clone(), snapshot_id)
    }

    /// Mark pass of mark-and-sweep GC: walk all live snapshots locally using `hashes.childs`
    /// and the local `tree_edges` cache (0 remote downloads in the steady state; lazily
    /// backfills `tree_edges` from the backend if a `TreeList` root is uncached).
    fn mark_live_hashes(&mut self) -> Result<HashSet<u64>, HatError> {
        let mut live_ids: HashSet<u64> = HashSet::new();
        let mut expanded_trees: HashSet<u64> = HashSet::new();
        let mut worklist: Vec<(u64, bool)> = Vec::new();

        for snap in self.snapshot_index.list_all() {
            let Some(hash) = snap.hash else {
                continue;
            };
            let id = self
                .hash_index
                .get_id(&hash)
                .ok_or("Live snapshot root hash missing from hash index")?;
            let is_tree_root = self
                .hash_index
                .locate_by_id(id)
                .map(|e| e.leaf == blob::LeafType::TreeList)
                .unwrap_or(false);
            worklist.push((id, is_tree_root));
        }

        while let Some((id, is_tree_root)) = worklist.pop() {
            let newly_marked = live_ids.insert(id);
            if !newly_marked && (!is_tree_root || expanded_trees.contains(&id)) {
                continue;
            }

            let entry = self.hash_index.locate_by_id(id).ok_or_else(|| {
                format!(
                    "Live hash id {} missing from hash index; run recover to rebuild cache",
                    id
                )
            })?;

            if newly_marked {
                if let Some(childs) = entry.childs {
                    for cid in childs {
                        worklist.push((cid, false));
                    }
                }
            }

            if is_tree_root && expanded_trees.insert(id) {
                let cached_edges = self.db.lock().tree_edges_get(id);
                let edges = match cached_edges {
                    Some(cached) => cached,
                    None => {
                        // Self-healing fallback: fetch this TreeList once from backend and cache edges.
                        let pref = entry
                            .persistent_ref
                            .ok_or("TreeList root missing persistent_ref during GC")?;
                        let href = hash::tree::HashRef {
                            hash: entry.hash,
                            node: entry.node,
                            leaf: entry.leaf,
                            info: None,
                            persistent_ref: pref,
                        };
                        let mut child_ids = Vec::new();
                        for (_e, content) in
                            family::Family::<B>::fetch_dir_data(href, self.hash_backend())?
                        {
                            let child_href = match content {
                                walker::Content::Data(h) | walker::Content::Dir(h) => h,
                                walker::Content::Link(_) => continue,
                            };
                            let cid = self.hash_index.get_id(&child_href.hash).ok_or_else(|| {
                                format!(
                                    "TreeList child hash {} missing from hash index; run recover to rebuild cache",
                                    hex::encode(&child_href.hash.bytes)
                                )
                            })?;
                            child_ids.push(cid);
                        }
                        self.db.lock().tree_edges_set(id, &child_ids);
                        child_ids
                    }
                };

                for cid in edges {
                    let child_is_tree = self
                        .hash_index
                        .locate_by_id(cid)
                        .map(|e| e.leaf == blob::LeafType::TreeList)
                        .unwrap_or(false);
                    worklist.push((cid, child_is_tree));
                }
            }
        }

        Ok(live_ids)
    }

    /// Dry-run garbage collection: return `(unused_hashes, live_hashes)` without deleting
    /// any hashes or backend blobs.
    pub fn gc_pretend(&mut self) -> Result<(u64, u64), HatError> {
        let live = self.mark_live_hashes()?;
        let all_ids = self.hash_index.all_ids();
        let dead_count = all_ids.iter().filter(|id| !live.contains(id)).count() as u64;

        let mut live_blobs = 0u64;
        for entry in self.hash_index.list() {
            if entry.persistent_ref.is_some() {
                if let Some(id) = self.hash_index.get_id(&entry.hash) {
                    if live.contains(&id) {
                        live_blobs += 1;
                    }
                }
            }
        }

        Ok((dead_count, live_blobs))
    }

    /// Mark-and-sweep garbage collection:
    /// 1. Mark all hashes reachable from live snapshots via `hashes.childs` + `tree_edges`.
    /// 2. Sweep unreachable hashes (and their cached `tree_edges`) from SQLite.
    /// 3. Delete backend blobs that are no longer referenced by any surviving hash.
    pub fn gc(&mut self) -> Result<(u64, u64), HatError> {
        let live = self.mark_live_hashes()?;
        let all_ids = self.hash_index.all_ids();

        let mut deleted_hashes = 0u64;
        for id in all_ids {
            if !live.contains(&id) {
                deleted_hashes += 1;
                self.hash_index.delete(id);
            }
        }
        self.db.lock().stat_cache_prune_orphans();
        self.hash_index.flush();

        // Mark used blobs.
        let entries = self.hash_index.list();
        self.blob_store.tag_all(tags::Tag::InProgress);

        let mut live_blobs = 0u64;
        for entry in entries {
            if let Some(pref) = entry.persistent_ref {
                live_blobs += 1;
                self.blob_store.tag(pref, tags::Tag::Reserved);
            }
        }
        // Anything still marked "in progress" is not referenced by any surviving hash.
        self.blob_store.delete_by_tag(tags::Tag::InProgress)?;
        self.blob_store.tag_all(tags::Tag::Done);
        self.blob_store.flush();

        Ok((deleted_hashes, live_blobs))
    }

    pub fn hash_backend(&self) -> key::HashStoreBackend<B> {
        key::HashStoreBackend::new(
            self.hash_index.clone(),
            self.blob_store.clone(),
            self.keys.clone(),
        )
    }

    /// Verify every indexed hash chunk is present in the backend and matches its content hash.
    pub fn check(&self) -> Result<(), HatError> {
        let mut problems = Vec::new();
        for entry in self.hash_index.list() {
            let Some(pref) = entry.persistent_ref else {
                continue;
            };
            let href = hash::tree::HashRef {
                hash: entry.hash.clone(),
                node: entry.node,
                leaf: entry.leaf,
                info: None,
                persistent_ref: pref,
            };
            match self.blob_store.retrieve(&href) {
                Ok(Some(chunk)) => {
                    let computed =
                        hash::Hash::new(&self.keys, entry.node, entry.leaf, &chunk[..]);
                    if computed != entry.hash {
                        problems.push(format!(
                            "hash mismatch for {} in blob {}",
                            hex::encode(&entry.hash.bytes),
                            hex::encode(&href.persistent_ref.blob_name)
                        ));
                    }
                }
                Ok(None) => problems.push(format!(
                    "missing chunk {} in blob {}",
                    hex::encode(&entry.hash.bytes),
                    hex::encode(&href.persistent_ref.blob_name)
                )),
                Err(e) => problems.push(format!(
                    "error reading {} from blob {}: {}",
                    hex::encode(&entry.hash.bytes),
                    hex::encode(&href.persistent_ref.blob_name),
                    e
                )),
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(From::from(format!(
                "check found {} error(s):\n{}",
                problems.len(),
                problems.join("\n")
            )))
        }
    }
}
