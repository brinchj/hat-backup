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

use crate::backend::StoreBackend;
use crate::blob;
use crate::cbor_util;
use crate::errors::HatError;
use crate::hash;
use crate::hat::insert_path_handler::InsertPathHandler;
use crate::hat::walker;
use crate::key;
use crate::models;
use crate::util::{FileIterator, PathHandler};
use std::collections::HashSet;
use std::ffi;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

pub(crate) fn parse_dir_data(chunk: &[u8], out: &mut Vec<walker::FileEntry>) -> Result<(), HatError> {
    if chunk.is_empty() {
        return Ok(());
    }

    let file_list: models::Files = cbor_util::from_slice(chunk)?;

    for f in file_list.files {
        if f.info.name.is_empty() {
            // Legacy end-of-list sentinel from older writers; no longer emitted.
            break;
        }

        let (data, hash_ref) = match f.content {
            models::Content::Data(r) => (
                key::Data::FilePlaceholder,
                walker::Content::Data(From::from(r)),
            ),
            models::Content::Directory(d) => (
                key::Data::DirPlaceholder,
                walker::Content::Dir(From::from(d)),
            ),
            models::Content::SymbolicLink(path) => {
                let link = PathBuf::from(ffi::OsStr::from_bytes(&path[..]));
                (
                    key::Data::Symlink(link.clone()),
                    walker::Content::Link(link),
                )
            }
        };

        let entry = key::Entry {
            info: From::from(f.info),
            data,
            parent_id: None,
            node_id: Some(f.id),
        };

        out.push(walker::FileEntry {
            hash_ref,
            meta: entry,
        });
    }
    Ok(())
}

pub struct Family<B> {
    pub name: String,
    pub key_store: key::Store<B>,
    pub key_stores: Vec<key::Store<B>>,
    /// `(dev, ino)` identities of directories automatically skipped during `snapshot_dir`
    /// (Hat's own state/cache directories and any local backend storage directory).
    pub excluded_dirs: HashSet<(u64, u64)>,
}
impl<B: StoreBackend> Clone for Family<B> {
    fn clone(&self) -> Family<B> {
        Family {
            name: self.name.clone(),
            key_store: self.key_store.clone(),
            key_stores: self.key_stores.clone(),
            excluded_dirs: self.excluded_dirs.clone(),
        }
    }
}

impl<B: StoreBackend> Family<B> {
    pub fn snapshot_dir(&self, dir: PathBuf) -> Result<(), HatError> {
        let dir = fs::canonicalize(dir)?;
        info!("Committing: {}", dir.display());
        assert!(dir.is_absolute());

        // Start a fresh in-memory staged tree for this scan; unchanged files reuse
        // cached HashRefs from the flat SQLite stat_cache.
        self.key_store.begin_stat_walk();

        let root_meta = fs::symlink_metadata(&dir)?;
        if root_meta.is_dir() && self.excluded_dirs.contains(&(root_meta.dev(), root_meta.ino())) {
            warn!(
                "Skipping excluded root directory '{}' (Hat state/cache or backend storage)",
                dir.display()
            );
            return Ok(());
        }

        let handler = InsertPathHandler::new(self.key_stores.clone(), self.excluded_dirs.clone());
        let root_ctx = (None, Vec::new());
        if dir.is_dir() {
            handler.recurse(dir, root_ctx);
        } else {
            let _ = handler.handle_path(&root_ctx, &dir);
        }

        Ok(())
    }

    pub fn snapshot_direct(
        &self,
        file: key::Entry,
        is_directory: bool,
        contents: Option<FileIterator>,
    ) -> Result<u64, HatError> {
        self.snapshot_direct_no_commit(file, is_directory, contents)
    }

    pub fn snapshot_direct_no_commit(
        &self,
        mut file: key::Entry,
        is_directory: bool,
        contents: Option<FileIterator>,
    ) -> Result<u64, HatError> {
        let id = if is_directory {
            file.data = key::Data::DirPlaceholder;
            self.key_store.insert_entry(file)?
        } else {
            self.key_store
                .insert(file, contents.map(|c| move || Some(c)))?
        };
        Ok(id)
    }

    pub fn flush(&self) -> Result<(), HatError> {
        for ks in &self.key_stores {
            ks.flush()?;
        }
        Ok(())
    }

    pub fn write_file_chunks<HTB: hash::tree::HashTreeBackend<Err = key::MsgError>>(
        fd: &mut fs::File,
        tree: hash::tree::LeafIterator<HTB>,
    ) -> Result<(), HatError> {
        for chunk in tree {
            let chunk = chunk?;
            fd.write_all(&chunk[..])?;
        }
        fd.flush()?;
        Ok(())
    }

    pub fn list_from_key_store(
        &self,
        dir_id: Option<u64>,
    ) -> Result<Vec<key::DirElem<B>>, HatError> {
        Ok(self.key_store.list_dir(dir_id)?)
    }

    pub fn fetch_dir_data<HTB: hash::tree::HashTreeBackend<Err = key::MsgError>>(
        dir_hash: hash::tree::HashRef,
        backend: HTB,
    ) -> Result<Vec<(key::Entry, walker::Content)>, HatError> {
        let it = hash::tree::LeafIterator::new(backend, dir_hash)?.expect("unable to open dir");

        let mut out = Vec::new();
        for chunk in it {
            let chunk = chunk?;
            if !chunk.is_empty() {
                parse_dir_data(&chunk[..], &mut out)?;
            }
        }

        Ok(out.into_iter().map(|f| (f.meta, f.hash_ref)).collect())
    }

    /// Serialize the staged directory tree bottom-up into immutable `TreeList` chunks and
    /// populate the local `tree_edges` cache for every written directory node.
    pub fn commit(&mut self) -> Result<hash::tree::HashRef, HatError> {
        let info = key::Info::new(self.name.clone().into(), None);
        let root = self.commit_dir(None, &info)?;
        self.key_store.clear_staged_entries();
        Ok(root)
    }

    fn commit_dir(
        &mut self,
        dir_id: Option<u64>,
        dir_info: &key::Info,
    ) -> Result<hash::tree::HashRef, HatError> {
        let mut tree = self.key_store.hash_tree_writer(blob::LeafType::TreeList);
        let mut child_hash_ids = Vec::new();
        let files_at_a_time = 1024;
        let mut it = self.list_from_key_store(dir_id)?.into_iter();

        loop {
            let mut files = vec![];

            for (idx, (entry, data_ref, _data_res_open)) in
                it.by_ref().take(files_at_a_time).enumerate()
            {
                assert!(idx < files_at_a_time);

                let content = match entry.data {
                    key::Data::FilePlaceholder => {
                        let href = data_ref.ok_or("File entry has no data reference")?;
                        let child_id = self
                            .key_store
                            .hash_index()
                            .get_id(&href.hash)
                            .ok_or_else(|| {
                                format!("File hash not found in hash index: {:?}", href.hash.bytes)
                            })?;
                        child_hash_ids.push(child_id);
                        models::Content::Data(href.to_model())
                    }
                    key::Data::DirPlaceholder => {
                        let dir_hash_ref = self.commit_dir(entry.node_id, &entry.info)?;
                        let child_id = self
                            .key_store
                            .hash_index()
                            .get_id(&dir_hash_ref.hash)
                            .ok_or_else(|| {
                                format!(
                                    "Directory hash not found in hash index: {:?}",
                                    dir_hash_ref.hash.bytes
                                )
                            })?;
                        child_hash_ids.push(child_id);
                        models::Content::Directory(dir_hash_ref.to_model())
                    }
                    key::Data::Symlink(path) => {
                        models::Content::SymbolicLink(path.as_os_str().as_bytes().to_vec())
                    }
                    _ => unreachable!("Unexpected key::Data"),
                };

                files.push(models::File {
                    id: entry.node_id.unwrap_or(0),
                    info: entry.info.to_model(),
                    content,
                });
            }

            if files.is_empty() {
                break;
            } else {
                tree.append(&cbor_util::to_vec(&models::Files { files })?[..])?;
            }
        }

        let dir_hash_ref = tree.hash(Some(dir_info))?;
        let dir_hash_id = self
            .key_store
            .hash_index()
            .get_id(&dir_hash_ref.hash)
            .ok_or("Committed directory hash missing from hash index")?;
        self.key_store
            .db()
            .lock()
            .tree_edges_set(dir_hash_id, &child_hash_ids);

        Ok(dir_hash_ref)
    }
}
