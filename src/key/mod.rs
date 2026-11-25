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

//! External API for creating and manipulating snapshots.

use crate::backend::StoreBackend;
use crate::blob;
use crate::crypto;
use crate::errors::{DieselError, RetryError};
use crate::hash;
use crate::hash::tree::{LeafIterator, SimpleHashTreeWriter};
use std::borrow::Cow;
use std::io;
use std::sync::Arc;

mod hash_store_backend;
mod index;
mod schema;

#[cfg(all(test, feature = "benchmarks"))]
mod benchmarks;
#[cfg(test)]
mod tests;

pub use self::hash_store_backend::HashStoreBackend;
pub use self::index::{Data, Entry, Info, KeyIndex};

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
    index: Arc<index::KeyIndex>,
    hash_index: Arc<hash::HashIndex>,
    blob_store: Arc<blob::BlobStore<B>>,
    keys: Arc<crypto::keys::Keeper>,
}
impl<B> Clone for Store<B> {
    fn clone(&self) -> Store<B> {
        Store {
            index: self.index.clone(),
            hash_index: self.hash_index.clone(),
            blob_store: self.blob_store.clone(),
            keys: self.keys.clone(),
        }
    }
}

// Implementations
impl<B: StoreBackend> Store<B> {
    pub fn new(
        index: Arc<index::KeyIndex>,
        hash_index: Arc<hash::HashIndex>,
        blob_store: Arc<blob::BlobStore<B>>,
        keys: Arc<crypto::keys::Keeper>,
    ) -> Store<B> {
        Store {
            index,
            hash_index,
            blob_store,
            keys,
        }
    }

    #[cfg(test)]
    pub fn new_for_testing(backend: Arc<B>, max_blob_size: usize) -> Result<Store<B>, DieselError> {
        use crate::crypto;
        use crate::db;

        let keys = Arc::new(crypto::keys::Keeper::new_for_testing());
        let db_p = Arc::new(db::Index::new_for_testing());
        let ki_p = Arc::new(index::KeyIndex::new_for_testing()?);
        let hi_p = Arc::new(hash::HashIndex::new(db_p.clone())?);
        let blob_index = Arc::new(blob::BlobIndex::new(keys.clone(), db_p)?);
        let bs_p = Arc::new(blob::BlobStore::new(
            keys,
            blob_index,
            backend,
            max_blob_size,
        ));
        Ok(Store {
            index: ki_p,
            hash_index: hi_p,
            blob_store: bs_p,
            keys: Arc::new(crypto::keys::Keeper::new_for_testing()),
        })
    }

    pub fn flush(&self) -> Result<(), MsgError> {
        self.blob_store.flush();
        self.hash_index.flush();
        self.index.flush()?;

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

    /// List a "directory" (aka. a `level`) in the index.
    pub fn list_dir(&self, parent: Option<u64>) -> Result<Vec<DirElem<B>>, MsgError> {
        let entries = self.index.list_dir(parent)?;
        let mut my_entries: Vec<DirElem<B>> = Vec::with_capacity(entries.len());
        for (entry, hash_ref_opt) in entries {
            let hash_ref = hash_ref_opt.or_else(|| match entry.data {
                Data::FileHash(ref hash_bytes) => {
                    let h = hash::Hash {
                        bytes: hash_bytes.clone(),
                    };
                    self.hash_index.fetch_hash_ref(&h).expect("Unknown hash")
                }
                _ => None,
            });
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

    /// Commit all reserved nodes and optionally execute recursive cleanup of part of the tree.
    pub fn commit_reserved_nodes(
        &self,
        clean_parent_opt: Option<Option<u64>>,
    ) -> Result<(), MsgError> {
        self.index.commit_reserved_nodes()?;
        if let Some(parent) = clean_parent_opt {
            self.index.cleanup_unused(parent)?;
        }
        Ok(())
    }

    /// Clear entry reservation tags before a new directory snapshot walk.
    pub fn reset_entry_tags(&self) -> Result<(), MsgError> {
        self.index.reset_entry_tags()?;
        Ok(())
    }

    /// Insert an entry with no associated file payload (directory or symlink).
    pub fn insert_entry(&self, entry: Entry) -> Result<u64, MsgError> {
        self.insert::<io::Empty, fn() -> Option<io::Empty>>(entry, None)
    }

    /// Insert a key into the index. If this key has associated data, `chunk_it_opt` opens a
    /// reader for it; returning `None` signals that opening failed and returns `Err`.
    pub fn insert<IT, F>(
        &self,
        insert_entry: Entry,
        chunk_it_opt: Option<F>,
    ) -> Result<u64, MsgError>
    where
        IT: io::Read,
        F: FnOnce() -> Option<IT>,
    {
        let entry = match self
            .index
            .lookup(insert_entry.parent_id, insert_entry.info.name.clone())?
        {
            Some(ref stored_entry) if insert_entry.data_looks_unchanged(stored_entry) => {
                match (&stored_entry.data, &insert_entry.data) {
                    (Data::FileHash(ref hash_bytes), Data::FilePlaceholder)
                        if chunk_it_opt.is_some() =>
                    {
                        let hash = hash::Hash {
                            bytes: hash_bytes.to_vec(),
                        };
                        if self.hash_index.hash_exists(&hash) {
                            // Short-circuit: We have the data.
                            debug!("Skip entry: {:?}", stored_entry.info.name);
                            self.index.mark_reserved(stored_entry)?;
                            return Ok(stored_entry.node_id.unwrap());
                        }
                    }
                    (Data::DirPlaceholder, Data::DirPlaceholder) if chunk_it_opt.is_none() => {
                        // Short-circuit: No data needed.
                        debug!("Skip unchanged dir: {:?}", stored_entry.info.name);
                        self.index.mark_reserved(stored_entry)?;
                        return Ok(stored_entry.node_id.unwrap());
                    }
                    (Data::Symlink(a), Data::Symlink(b))
                        if a == b && chunk_it_opt.is_none() =>
                    {
                        // Short-circuit: Unchanged symlink.
                        debug!("Skip unchanged symlink: {:?}", stored_entry.info.name);
                        self.index.mark_reserved(stored_entry)?;
                        return Ok(stored_entry.node_id.unwrap());
                    }
                    _ => (),
                }
                // Our stored entry is incomplete.
                Entry {
                    node_id: stored_entry.node_id,
                    ..insert_entry
                }
            }
            Some(entry) => Entry {
                node_id: entry.node_id,
                ..insert_entry
            },
            None => insert_entry,
        };

        // Check if we have a data source:
        let mut reader = match chunk_it_opt {
            None => {
                // No data is associated with this entry (directory or symlink).
                debug!("Insert entry: {:?}", entry.info.name);
                let entry = self.index.insert(entry, None)?;

                // Bail out before storing data that does not exist:
                return Ok(entry.node_id.unwrap());
            }
            Some(open) => match open() {
                Some(reader) => reader,
                None => {
                    // Opening a file failed (e.g. EACCES / vanished). Do not insert a
                    // NULL-hash row that would masquerade as an empty directory.
                    return Err(MsgError::from(format!(
                        "could not open file {:?}",
                        entry.info.name.utf8()
                    )));
                }
            },
        };

        // Setup hash tree structure
        let mut tree = self.hash_tree_writer(blob::LeafType::FileChunk);

        // Read and insert all file chunks:
        // (see HashStoreBackend::insert_chunk above)
        let max_chunk_len = 128 * 1024;
        let mut chunk = vec![0; max_chunk_len];
        let mut file_len = 0u64;
        loop {
            let mut chunk_len = 0;
            while chunk_len < max_chunk_len {
                chunk_len += match reader.read(&mut chunk[chunk_len..]) {
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        return Err(MsgError::from(format!(
                            "failed reading file {:?}: {}",
                            entry.info.name.utf8(),
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
            tree.append(&chunk[..chunk_len])?
        }

        // Warn the user if we did not read the expected size:
        if let Some(s) = entry.info.byte_length {
            file_size_warning(entry.info.name.utf8(), s, file_len);
        }

        let mut entry = entry;
        entry.info.byte_length = Some(file_len);

        // Get top tree hash:
        let hash_ref = tree.hash(Some(&entry.info))?;

        // It is OK that this hash is not yet valid, as we check hashes at snapshot time.
        debug!("Insert entry: {:?}", entry.info.name);
        let entry = self.index.insert(entry, Some(&hash_ref))?;

        Ok(entry.node_id.unwrap())
    }
}

fn file_size_warning(name: &str, wanted: u64, got: u64) {
    if wanted < got {
        println!(
            "Warning: File grew while reading it: {} (wanted {}, got {})",
            name, wanted, got
        )
    } else if wanted > got {
        println!(
            "Warning: Could not read whole file (or it shrank): {} (wanted {}, got {})",
            name, wanted, got
        )
    }
}
