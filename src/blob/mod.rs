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

//! Combines data chunks into larger blobs to be stored externally.

use crate::backend::StoreBackend;
use crate::crypto;
use crate::errors;
use crate::hash::tree::HashRef;
use crate::hash::Hash;
use crate::key;
use lru::LruCache;
use crate::cbor_util;
use std::borrow::Cow;
use std::mem;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use crate::tags;

#[allow(clippy::module_inception)]
mod blob;
mod chunk;
mod index;
#[cfg(test)]
pub mod tests;

#[cfg(all(test, feature = "benchmarks"))]
mod benchmarks;

pub use self::blob::{Blob, BlobReader};
pub use self::chunk::{ChunkRef, Compression, Key, LeafType, NodeType, Packing};
pub use self::index::{BlobDesc, BlobIndex};

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("{0}")]
    Message(Cow<'static, str>),
    #[error(transparent)]
    CryptoError(#[from] errors::CryptoError),
    #[error(transparent)]
    Serde(#[from] cbor_util::DeError),
    #[error(transparent)]
    SerdeSer(#[from] cbor_util::SerError),
}

impl From<&'static str> for BlobError {
    fn from(s: &'static str) -> Self {
        BlobError::Message(Cow::Borrowed(s))
    }
}

impl From<String> for BlobError {
    fn from(s: String) -> Self {
        BlobError::Message(Cow::Owned(s))
    }
}

fn compression_to_raw(c: Compression) -> i32 {
    match c {
        Compression::None => 0,
        Compression::Zstd(level) => level,
    }
}

fn raw_to_compression(raw: i32) -> Compression {
    if raw <= 0 {
        Compression::None
    } else {
        Compression::Zstd(raw)
    }
}

struct FlushTracker {
    in_flight: Mutex<usize>,
    done: Condvar,
}

impl FlushTracker {
    fn new() -> Self {
        FlushTracker {
            in_flight: Mutex::new(0),
            done: Condvar::new(),
        }
    }

    fn begin(&self) -> InFlightGuard<'_> {
        let mut count = self.in_flight.lock().expect("FlushTracker poisoned");
        *count += 1;
        InFlightGuard { tracker: self }
    }

    fn wait_idle(&self) {
        let mut count = self.in_flight.lock().expect("FlushTracker poisoned");
        while *count > 0 {
            count = self.done.wait(count).expect("FlushTracker poisoned");
        }
    }
}

struct InFlightGuard<'a> {
    tracker: &'a FlushTracker,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        let mut count = self.tracker.in_flight.lock().expect("FlushTracker poisoned");
        *count -= 1;
        if *count == 0 {
            self.tracker.done.notify_all();
        }
    }
}

struct PreparedFlush {
    blob: Blob,
    desc: BlobDesc,
    callbacks: Vec<Box<dyn FnOnce() + Send>>,
}

pub struct BlobStore<B> {
    inner: Arc<Mutex<StoreInner<B>>>,
    backend: Arc<B>,
    blob_index: Arc<BlobIndex>,
    flushes: FlushTracker,
    compression: AtomicI32,
    max_blob_size: usize,
}

pub struct StoreInner<B> {
    keys: Arc<crypto::keys::Keeper>,
    backend: Arc<B>,
    blob_index: Arc<BlobIndex>,
    blob_desc: BlobDesc,
    blob_refs: Vec<Box<dyn FnOnce() + Send>>,
    blob: Blob,
    read_cache: LruCache<Vec<u8>, BlobReader>,
}

impl<B> Drop for StoreInner<B> {
    fn drop(&mut self) {
        // Sanity check that we flushed this blob store before dropping it.
        assert_eq!(0, self.blob.upperbound_len());
    }
}

impl<B: StoreBackend> StoreInner<B> {
    fn new(
        keys: Arc<crypto::keys::Keeper>,
        index: Arc<BlobIndex>,
        backend: Arc<B>,
        max_blob_size: usize,
    ) -> StoreInner<B> {
        let mut bs = StoreInner {
            keys: keys.clone(),
            backend,
            blob_index: index,
            blob_desc: Default::default(),
            blob_refs: Vec::new(),
            blob: Blob::new(keys, max_blob_size),
            read_cache: LruCache::new(NonZeroUsize::new(64).unwrap()),
        };
        bs.reserve_new_blob();
        bs
    }

    fn reserve_new_blob(&mut self) -> BlobDesc {
        mem::replace(&mut self.blob_desc, self.blob_index.reserve())
    }

    fn take_for_flush(&mut self, max_blob_size: usize) -> Option<PreparedFlush> {
        if self.blob.upperbound_len() == 0 {
            return None;
        }
        let blob = mem::replace(&mut self.blob, Blob::new(self.keys.clone(), max_blob_size));
        let desc = self.reserve_new_blob();
        let callbacks = mem::take(&mut self.blob_refs);
        Some(PreparedFlush {
            blob,
            desc,
            callbacks,
        })
    }

    fn retrieve(&mut self, href: &HashRef) -> Result<Option<Vec<u8>>, BlobError> {
        if href.persistent_ref.offset == 0 && href.persistent_ref.length == 0 {
            return Ok(Some(Vec::new()));
        }

        let name = &href.persistent_ref.blob_name[..];
        if self.read_cache.get_mut(name).is_some() {
            let reader = self.read_cache.get_mut(name).expect("is_some");
            Ok(Some(reader.read_chunk(href)?))
        } else {
            match self.backend.retrieve(name) {
                Ok(Some(blob)) => {
                    let text = crypto::CipherTextRef::new(&blob[..]);
                    let mut reader = BlobReader::new(self.keys.clone(), name, text)?;
                    let chunk = reader.read_chunk(href)?;
                    self.read_cache.put(name.to_vec(), reader);
                    Ok(Some(chunk))
                }
                Ok(None) => Ok(None),
                Err(e) => Err(e.into()),
            }
        }
    }

    fn retrieve_refs(&mut self, blob: BlobDesc) -> Result<Option<Vec<HashRef>>, BlobError> {
        match self.backend.retrieve(&blob.name[..])? {
            None => Ok(None),
            Some(ct) => {
                let hrefs = BlobReader::new(
                    self.keys.clone(),
                    &blob.name[..],
                    crypto::CipherTextRef::new(&ct[..]),
                )?
                .refs()?;
                if hrefs.is_empty() {
                    Ok(None)
                } else if blob.name[..] != hrefs[0].persistent_ref.blob_name[..] {
                    Err(BlobError::from("blob name mismatch in recovered footer"))
                } else {
                    Ok(Some(hrefs))
                }
            }
        }
    }

    fn recover(&mut self) -> Result<(), String> {
        // Blob names are sealed i64 ids (always longer than the old literal "root" marker).
        for b in self.backend.list()? {
            if let Err(e) = self.blob_index.recover(b.into_vec()) {
                warn!("Skipping unrecognized backend object during recover: {}", e);
            }
        }
        // Advance the blob ID counter past all recovered blobs and re-reserve the
        // pre-allocated blob_desc so subsequent commits never collide on blobs.id.
        self.blob_index.refresh_id_counter();
        if self.blob.upperbound_len() == 0 {
            self.reserve_new_blob();
        }
        Ok(())
    }

    fn tag(&mut self, chunk: ChunkRef, tag: tags::Tag) {
        self.blob_index.tag(
            &BlobDesc {
                id: chunk.blob_id.unwrap_or(0),
                name: chunk.blob_name,
            },
            tag,
        );
    }

    fn tag_all(&mut self, tag: tags::Tag) {
        self.blob_index.tag_all(tag);
    }

    fn delete_by_tag(&mut self, tag: tags::Tag) -> Result<(), String> {
        let blobs = self.blob_index.list_by_tag(tag);
        for b in &blobs {
            self.backend.delete(&b.name)?;
        }
        self.blob_index.delete_by_tag(tag);
        Ok(())
    }
}

impl<B: StoreBackend> BlobStore<B> {
    pub fn new(
        keys: Arc<crypto::keys::Keeper>,
        index: Arc<BlobIndex>,
        backend: Arc<B>,
        max_blob_size: usize,
    ) -> BlobStore<B> {
        BlobStore {
            inner: Arc::new(Mutex::new(StoreInner::new(
                keys,
                index.clone(),
                backend.clone(),
                max_blob_size,
            ))),
            backend,
            blob_index: index,
            flushes: FlushTracker::new(),
            compression: AtomicI32::new(compression_to_raw(Compression::default())),
            max_blob_size,
        }
    }

    fn lock(&self) -> MutexGuard<'_, StoreInner<B>> {
        self.inner.lock().expect("Blob store was poisoned")
    }

    /// Finalize (pad, seal footer, MAC) and durably store a detached blob outside
    /// the packing mutex so other worker threads can pack and flush concurrently.
    fn finish_and_store(&self, mut prepared: PreparedFlush, _guard: InFlightGuard<'_>) {
        let ct = match prepared.blob.finish(&prepared.desc.name[..]) {
            None => return,
            Some(ct) => ct,
        };
        let callbacks = prepared.callbacks;
        let done_callback = Box::new(move || {
            callbacks.into_iter().for_each(|c| c());
        });

        self.blob_index.in_air(&prepared.desc);
        self.backend
            .store(&prepared.desc.name[..], ct, done_callback)
            .expect("Store operation failed");
        self.blob_index.commit_done(&prepared.desc);
    }

    /// Store a new data chunk into the current blob. The callback is triggered after the blob
    /// containing the chunk has been committed to persistent storage (it is then safe to use the
    /// `ChunkRef` as persistent reference).
    pub fn store(
        &self,
        chunk: &[u8],
        hash: Hash,
        node: NodeType,
        leaf: LeafType,
        info: Option<&key::Info>,
        callback: Box<dyn FnOnce() + Send>,
    ) -> HashRef {
        if chunk.is_empty() {
            // We are not going to store an empty chunk, so commit it ASAP.
            thread::spawn(callback);
            return HashRef {
                hash,
                node,
                leaf,
                info: None,
                persistent_ref: ChunkRef {
                    blob_id: Some(0),
                    blob_name: vec![0],
                    packing: None,
                    offset: 0,
                    length: 0,
                    key: None,
                },
            };
        }

        // Compress on the calling worker thread before acquiring the blob-packing mutex
        // so parallel workers do not serialize on zstd compression.
        let (payload, packing) =
            blob::compress_chunk(chunk, self.compression()).expect("chunk compression failed");

        let mut href = HashRef {
            hash,
            node,
            leaf,
            info: info.cloned(),
            persistent_ref: ChunkRef {
                blob_id: Some(0),
                blob_name: vec![0],
                packing,
                // Updated by try_append.
                offset: 0,
                length: 0,
                key: None,
            },
        };

        loop {
            let pending_flush = {
                let mut guard = self.lock();
                href.persistent_ref.blob_id = Some(guard.blob_desc.id);
                href.persistent_ref.blob_name = guard.blob_desc.name.clone();
                match guard.blob.try_append(&payload, &mut href) {
                    Ok(()) => {
                        // Queue the callback; we will trigger it when the blob has been pushed.
                        guard.blob_refs.push(callback);
                        href.info = None;
                        return href;
                    }
                    Err(()) => {
                        // Current blob is full: detach it and register in-flight BEFORE
                        // dropping the lock so concurrent `flush()` waits for it.
                        let prepared = guard
                            .take_for_flush(self.max_blob_size)
                            .expect("full blob must be non-empty");
                        let in_flight = self.flushes.begin();
                        (prepared, in_flight)
                    }
                }
            };

            self.finish_and_store(pending_flush.0, pending_flush.1);
        }
    }

    /// Retrieve the data chunk identified by `ChunkRef`.
    pub fn retrieve(&self, href: &HashRef) -> Result<Option<Vec<u8>>, BlobError> {
        self.flushes.wait_idle();
        self.lock().retrieve(href)
    }

    /// Fetch a blob and recover the HashRefs for its contents.
    pub fn retrieve_refs(&self, blob: BlobDesc) -> Result<Option<Vec<HashRef>>, BlobError> {
        self.flushes.wait_idle();
        self.lock().retrieve_refs(blob)
    }

    /// Reinstall a blob recovered from external storage.
    pub fn recover(&self) -> Result<(), String> {
        self.flushes.wait_idle();
        self.lock().recover()
    }

    pub fn tag(&self, chunk: ChunkRef, tag: tags::Tag) {
        self.lock().tag(chunk, tag)
    }

    pub fn tag_all(&self, tag: tags::Tag) {
        self.lock().tag_all(tag)
    }

    pub fn delete_by_tag(&self, tag: tags::Tag) -> Result<(), String> {
        self.flushes.wait_idle();
        self.lock().delete_by_tag(tag)
    }

    pub fn list_by_tag(&self, tag: tags::Tag) -> Vec<BlobDesc> {
        self.lock().blob_index.list_by_tag(tag)
    }

    pub fn find(&self, name: &[u8]) -> Option<BlobDesc> {
        if name == [0] {
            Some(BlobDesc {
                name: vec![0],
                id: 0,
            })
        } else {
            self.lock().blob_index.find(name)
        }
    }

    /// Flush the current blob, independent of its size, and wait until all
    /// in-flight blob writes have durably completed.
    #[cfg_attr(feature = "flame_it", flame)]
    pub fn flush(&self) {
        let pending = {
            let mut guard = self.lock();
            guard
                .take_for_flush(self.max_blob_size)
                .map(|prepared| (prepared, self.flushes.begin()))
        };
        if let Some((prepared, in_flight)) = pending {
            self.finish_and_store(prepared, in_flight);
        }
        self.flushes.wait_idle();
        self.blob_index.flush();
    }

    pub fn max_blob_size(&self) -> usize {
        self.max_blob_size
    }

    pub fn compression(&self) -> Compression {
        raw_to_compression(self.compression.load(Ordering::Relaxed))
    }

    pub fn set_compression(&self, compression: Compression) {
        self.compression
            .store(compression_to_raw(compression), Ordering::Relaxed);
    }
}
