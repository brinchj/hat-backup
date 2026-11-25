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

//! External API for staging entries and checking the flat local stat cache.

use crate::backend::StoreBackend;
use crate::blob;
use crate::crypto;
use crate::db;
use crate::errors::{DieselError, RetryError};
use crate::hash;
use crate::hash::tree::{LeafIterator, SimpleHashTreeWriter};
use std::borrow::Cow;
use std::cell::RefCell;
use std::io::{self, Read};
use std::sync::{Arc, Mutex};

mod hash_store_backend;
mod index;

#[cfg(all(test, feature = "benchmarks"))]
mod benchmarks;
#[cfg(test)]
mod tests;

pub use self::hash_store_backend::HashStoreBackend;
pub use self::index::{Data, Entry, Info, StagedTree};

#[derive(Debug, thiserror::Error)]
pub enum MsgError {
    #[error("{0}")]
    Message(Cow<'static, str>),
    #[error(transparent)]
    RetryError(#[from] RetryError),
    #[error(transparent)]
    DieselError(#[from] DieselError),
    #[error(transparent)]
    Blob(#[from] blob::BlobError),
}

impl From<&'static str> for MsgError {
    fn from(s: &'static str) -> Self {
        MsgError::Message(Cow::Borrowed(s))
    }
}

impl From<String> for MsgError {
    fn from(s: String) -> Self {
        MsgError::Message(Cow::Owned(s))
    }
}

pub type DirElem<B> = (
    Entry,
    Option<hash::tree::HashRef>,
    Option<HashTreeReaderInitializer<B>>,
);

pub struct HashTreeReaderInitializer<B> {
    hash_ref: hash::tree::HashRef,
    hash_index: Arc<hash::HashIndex>,
    blob_store: Arc<blob::BlobStore<B>>,
    keys: Arc<crypto::keys::Keeper>,
}

impl<B: StoreBackend> HashTreeReaderInitializer<B> {
    pub fn init(self) -> Result<Option<LeafIterator<HashStoreBackend<B>>>, MsgError> {
        let backend = HashStoreBackend::new(self.hash_index, self.blob_store, self.keys.clone());
        LeafIterator::new(backend, self.hash_ref.clone())
    }
}

pub struct Store<B> {
    family_id: i64,
    db: Arc<db::Index>,
    staged: Arc<Mutex<StagedTree>>,
    hash_index: Arc<hash::HashIndex>,
    blob_store: Arc<blob::BlobStore<B>>,
    keys: Arc<crypto::keys::Keeper>,
}
impl<B> Clone for Store<B> {
    fn clone(&self) -> Store<B> {
        Store {
            family_id: self.family_id,
            db: self.db.clone(),
            staged: self.staged.clone(),
            hash_index: self.hash_index.clone(),
            blob_store: self.blob_store.clone(),
            keys: self.keys.clone(),
        }
    }
}

impl<B: StoreBackend> Store<B> {
    pub fn new(
        family_id: i64,
        db: Arc<db::Index>,
        staged: Arc<Mutex<StagedTree>>,
        hash_index: Arc<hash::HashIndex>,
        blob_store: Arc<blob::BlobStore<B>>,
        keys: Arc<crypto::keys::Keeper>,
    ) -> Store<B> {
        Store {
            family_id,
            db,
            staged,
            hash_index,
            blob_store,
            keys,
        }
    }

    #[cfg(test)]
    pub fn new_for_testing(backend: Arc<B>, max_blob_size: usize) -> Result<Store<B>, DieselError> {
        let keys = Arc::new(crypto::keys::Keeper::new_for_testing());
        let db_p = Arc::new(db::Index::new_for_testing());
        let family_id = db_p.lock().get_or_create_family_id("test");
        let hi_p = Arc::new(hash::HashIndex::new(db_p.clone())?);
        let blob_index = Arc::new(blob::BlobIndex::new(keys.clone(), db_p.clone())?);
        let bs_p = Arc::new(blob::BlobStore::new(
            keys.clone(),
            blob_index,
            backend,
            max_blob_size,
        ));
        Ok(Store {
            family_id,
            db: db_p,
            staged: Arc::new(Mutex::new(StagedTree::default())),
            hash_index: hi_p,
            blob_store: bs_p,
            keys,
        })
    }

    pub fn family_id(&self) -> i64 {
        self.family_id
    }

    pub fn clear_staged(&self) {
        self.staged.lock().unwrap().clear();
    }

    pub fn begin_stat_walk(&self) {
        self.staged.lock().unwrap().begin_stat_walk();
    }

    pub fn clear_staged_entries(&self) {
        self.staged.lock().unwrap().clear_entries();
    }

    pub fn take_seen_stat_paths(&self) -> Option<std::collections::HashSet<Vec<u8>>> {
        self.staged.lock().unwrap().take_seen_stat_paths()
    }

    pub fn flush(&self) -> Result<(), MsgError> {
        self.blob_store.flush();
        self.hash_index.flush();
        Ok(())
    }

    pub fn hash_tree_writer(
        &self,
        leaf: blob::LeafType,
    ) -> SimpleHashTreeWriter<HashStoreBackend<B>> {
        let backend = HashStoreBackend::new(
            self.hash_index.clone(),
            self.blob_store.clone(),
            self.keys.clone(),
        );
        SimpleHashTreeWriter::new(leaf, 8, backend)
    }

    pub fn db(&self) -> &Arc<db::Index> {
        &self.db
    }

    pub fn hash_index(&self) -> &Arc<hash::HashIndex> {
        &self.hash_index
    }

    /// List staged children of `parent`.
    pub fn list_dir(&self, parent: Option<u64>) -> Result<Vec<DirElem<B>>, MsgError> {
        let entries = self.staged.lock().unwrap().list_dir(parent);
        let mut my_entries: Vec<DirElem<B>> = Vec::with_capacity(entries.len());
        for (entry, hash_ref) in entries {
            let open_fn = hash_ref.as_ref().map(|r| HashTreeReaderInitializer {
                hash_ref: r.clone(),
                hash_index: self.hash_index.clone(),
                blob_store: self.blob_store.clone(),
                keys: self.keys.clone(),
            });
            my_entries.push((entry, hash_ref, open_fn));
        }
        Ok(my_entries)
    }

    /// Stage an entry with no associated file payload (directory or symlink).
    pub fn insert_entry(&self, entry: Entry) -> Result<u64, MsgError> {
        self.insert_path::<io::Empty, fn() -> Option<io::Empty>>(None, entry, None)
    }

    /// Stage an entry directly (without a flat stat-cache path).
    pub fn insert<IT, F>(
        &self,
        insert_entry: Entry,
        chunk_it_opt: Option<F>,
    ) -> Result<u64, MsgError>
    where
        IT: io::Read,
        F: FnOnce() -> Option<IT>,
    {
        self.insert_path(None, insert_entry, chunk_it_opt)
    }

    /// Stage an entry under `rel_path`, consulting the flat `stat_cache` for unchanged files.
    /// If `chunk_it_opt` is `Some` and opening/reading fails, returns `Err` without staging.
    pub fn insert_path<IT, F>(
        &self,
        rel_path: Option<&[u8]>,
        insert_entry: Entry,
        chunk_it_opt: Option<F>,
    ) -> Result<u64, MsgError>
    where
        IT: io::Read,
        F: FnOnce() -> Option<IT>,
    {
        // Fast path: check flat local stat cache for unchanged regular files.
        if let (Some(rp), Some(_)) = (rel_path, &chunk_it_opt) {
            let cached_row = self.db.lock().stat_cache_lookup(self.family_id, rp);
            if let Some(cached_row) = cached_row {
                let (cached_entry, cached_href) = Entry::from_stat_cache_entry(
                    insert_entry.parent_id,
                    insert_entry.info.name.clone(),
                    cached_row,
                );
                if insert_entry.data_looks_unchanged(&cached_entry) {
                    if let Some(href) = cached_href {
                        if self.hash_index.hash_exists(&href.hash) {
                            debug!("Stat-cache hit: {:?}", insert_entry.info.name);
                            let mut staged = self.staged.lock().unwrap();
                            staged.record_stat_path(rp);
                            let id = staged.insert(insert_entry, Some(href));
                            return Ok(id);
                        }
                    }
                }
            }
        }

        // Check if we have a data source:
        let mut reader = match chunk_it_opt {
            None => {
                // Directory or symlink: no payload chunks to read.
                debug!("Stage entry: {:?}", insert_entry.info.name);
                let id = self.staged.lock().unwrap().insert(insert_entry, None);
                return Ok(id);
            }
            Some(open) => match open() {
                Some(reader) => reader,
                None => {
                    // Opening a file failed (e.g. EACCES / vanished). Do not stage a
                    // NULL-hash entry that would masquerade as an empty directory.
                    return Err(MsgError::from(format!(
                        "could not open file {:?}",
                        insert_entry.info.name.utf8()
                    )));
                }
            },
        };

        // Stream file chunks into a content-addressed FileChunk hash tree using
        // keyed FastCDC content-defined chunking.
        let mut tree = self.hash_tree_writer(blob::LeafType::FileChunk);
        let mut file_len = 0u64;
        let max_blob_size = self.blob_store.max_blob_size();
        if let Some((min_size, avg_size, max_size)) = fastcdc_params(max_blob_size) {
            let mut reader = RetryOnInterrupt(reader);
            // Fast path for files <= min_size (FastCDC never cuts before min_size):
            // probe up to min_size + 1 bytes into a reused per-thread buffer to skip
            // allocating StreamCDC's 256 KiB window for small files.
            let overflow_prefix: Option<Vec<u8>> = SMALL_FILE_BUF.with(|cell| {
                let mut buf = cell.borrow_mut();
                // Keep >= 4 KiB headroom past `min_size` so every read() passes a
                // comfortably sized buffer and detects >min_size in a single syscall.
                let probe_len = min_size + 4096;
                if buf.len() < probe_len {
                    buf.resize(probe_len, 0);
                }
                let mut filled = 0usize;
                while filled <= min_size {
                    match reader.read(&mut buf[filled..probe_len]) {
                        Ok(0) => break,
                        Ok(n) => filled += n,
                        Err(e) => {
                            return Err(MsgError::from(format!(
                                "failed reading file {:?}: {}",
                                insert_entry.info.name.utf8(),
                                e
                            )));
                        }
                    }
                }
                if filled == 0 {
                    Ok(None)
                } else if filled <= min_size {
                    file_len += filled as u64;
                    tree.append(&buf[..filled])?;
                    Ok(None)
                } else {
                    Ok(Some(buf[..filled].to_vec()))
                }
            })?;

            if let Some(prefix) = overflow_prefix {
                let chained = io::Cursor::new(prefix).chain(reader);
                let chunker = fastcdc::v2020::StreamCDC::with_level_and_seed(
                    chained,
                    min_size,
                    avg_size,
                    max_size,
                    fastcdc::v2020::Normalization::Level1,
                    self.keys.chunker_seed(),
                );
                for chunk_res in chunker {
                    let chunk = chunk_res.map_err(|e| {
                        MsgError::from(format!(
                            "failed reading file {:?}: {}",
                            insert_entry.info.name.utf8(),
                            e
                        ))
                    })?;
                    if chunk.data.is_empty() {
                        continue;
                    }
                    file_len += chunk.data.len() as u64;
                    tree.append(&chunk.data)?;
                }
            }
        } else {
            let max_chunk_len = (max_blob_size / 4).max(1);
            let mut chunk = vec![0; max_chunk_len];
            loop {
                let mut chunk_len = 0;
                while chunk_len < max_chunk_len {
                    chunk_len += match reader.read(&mut chunk[chunk_len..]) {
                        Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            return Err(MsgError::from(format!(
                                "failed reading file {:?}: {}",
                                insert_entry.info.name.utf8(),
                                e
                            )));
                        }
                        Ok(0) => break,
                        Ok(size) => size,
                    }
                }
                if chunk_len == 0 {
                    break;
                }
                file_len += chunk_len as u64;
                tree.append(&chunk[..chunk_len])?;
            }
        }

        if let Some(s) = insert_entry.info.byte_length {
            file_size_warning(insert_entry.info.name.utf8(), s, file_len);
        }

        let mut entry = insert_entry;
        entry.info.byte_length = Some(file_len);

        let hash_ref = tree.hash(Some(&entry.info))?;

        if let Some(rp) = rel_path {
            let cache_entry = entry.to_stat_cache_entry(Some(&hash_ref));
            self.db
                .lock()
                .stat_cache_upsert(self.family_id, rp, &cache_entry);
        }

        debug!("Stage file: {:?}", entry.info.name);
        let mut staged = self.staged.lock().unwrap();
        if let Some(rp) = rel_path {
            staged.record_stat_path(rp);
        }
        let id = staged.insert(entry, Some(hash_ref));
        Ok(id)
    }
}

fn file_size_warning(name: &str, wanted: u64, got: u64) {
    if wanted < got {
        warn!(
            "File grew while reading it: {} (wanted {}, got {})",
            name, wanted, got
        );
    } else if wanted > got {
        warn!(
            "Could not read whole file (or it shrank): {} (wanted {}, got {})",
            name, wanted, got
        );
    }
}

pub const FASTCDC_MIN_CHUNK_SIZE: usize = 16 * 1024;
pub const FASTCDC_AVG_CHUNK_SIZE: usize = 64 * 1024;
pub const FASTCDC_MAX_CHUNK_SIZE: usize = 256 * 1024;

thread_local! {
    /// Reused per-thread read buffer for files up to `FASTCDC_MIN_CHUNK_SIZE`
    /// so small files skip allocating FastCDC's 256 KiB streaming window.
    static SMALL_FILE_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Choose FastCDC `(min, avg, max)` chunk sizes for a given `max_blob_size`.
///
/// Normal repositories (`max_blob_size >= 512 KiB`) always use the canonical
/// 16 KiB / 64 KiB / 256 KiB parameters. Smaller test blob sizes scale down so
/// a chunk never exceeds blob capacity; returns `None` if `max_blob_size` is
/// below FastCDC's minimum supported max chunk size.
fn fastcdc_params(max_blob_size: usize) -> Option<(usize, usize, usize)> {
    if max_blob_size >= 512 * 1024 {
        return Some((
            FASTCDC_MIN_CHUNK_SIZE,
            FASTCDC_AVG_CHUNK_SIZE,
            FASTCDC_MAX_CHUNK_SIZE,
        ));
    }
    let cap = (max_blob_size / 2) & !1;
    if cap < fastcdc::v2020::MAXIMUM_MIN {
        return None;
    }
    let max_size = cap.min(FASTCDC_MAX_CHUNK_SIZE);
    let avg_size = ((max_size / 4).max(fastcdc::v2020::AVERAGE_MIN)) & !1;
    let min_size = ((avg_size / 4).max(fastcdc::v2020::MINIMUM_MIN)) & !1;
    Some((min_size, avg_size, max_size))
}

struct RetryOnInterrupt<R>(R);

impl<R: io::Read> io::Read for RetryOnInterrupt<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.0.read(buf) {
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => return other,
            }
        }
    }
}
