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
use crate::key;
use crate::util::{FileIterator, PathHandler, SyncPool};
use std::collections::HashSet;
use std::error::Error;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{atomic, Mutex};
use std::time::{Duration, Instant};

struct FileEntry {
    key_entry: key::Entry,
    metadata: fs::Metadata,
    full_path: PathBuf,
    raw_name: Vec<u8>,
}

impl FileEntry {
    fn new(full_path: PathBuf, parent: Option<u64>) -> Result<FileEntry, Box<dyn Error>> {
        debug!("FileEntry::new({:?})", full_path);

        if let Some(filename) = full_path.file_name().map(|n| n.to_owned()) {
            let meta = fs::symlink_metadata(&full_path)?;
            let data = if meta.is_file() {
                key::Data::FilePlaceholder
            } else if meta.is_dir() {
                key::Data::DirPlaceholder
            } else if meta.file_type().is_symlink() {
                let path = fs::read_link(&full_path)?;
                key::Data::Symlink(path)
            } else {
                // Unsupported file type. Skipping.
                return Err(From::from("unknown file kind".to_string()));
            };
            let raw_name = filename.as_bytes().to_vec();
            Ok(FileEntry {
                key_entry: key::Entry::new(parent, filename.into(), data, Some(&meta)),
                metadata: meta,
                full_path,
                raw_name,
            })
        } else {
            Err(From::from("Could not parse filename.".to_owned()))
        }
    }

    fn is_file(&self) -> bool {
        self.metadata.is_file()
    }
    fn is_directory(&self) -> bool {
        self.metadata.is_dir()
    }
}

pub struct InsertPathHandler<B: StoreBackend> {
    count: atomic::AtomicIsize,
    last_print: Mutex<Instant>,
    key_store: SyncPool<key::Store<B>>,
    excluded_dirs: HashSet<(u64, u64)>,
}

impl<B: StoreBackend> InsertPathHandler<B> {
    pub fn new(
        key_stores: Vec<key::Store<B>>,
        excluded_dirs: HashSet<(u64, u64)>,
    ) -> InsertPathHandler<B> {
        InsertPathHandler {
            count: atomic::AtomicIsize::new(0),
            last_print: Mutex::new(Instant::now()),
            key_store: SyncPool::new(key_stores),
            excluded_dirs,
        }
    }

    fn is_excluded_dir(&self, meta: &fs::Metadata) -> bool {
        meta.is_dir() && self.excluded_dirs.contains(&(meta.dev(), meta.ino()))
    }
}

/// Parent payload carried down `listdir` traversal: `(staged_parent_id, parent_rel_path_bytes)`.
pub type ParentCtx = (Option<u64>, Vec<u8>);

impl<B: StoreBackend> PathHandler<ParentCtx> for InsertPathHandler<B> {
    type DirItem = fs::DirEntry;
    type DirIter = fs::ReadDir;

    fn read_dir(&self, path: &Path) -> io::Result<Self::DirIter> {
        fs::read_dir(path)
    }

    fn handle_path(&self, parent: &ParentCtx, path: &Path) -> Option<ParentCtx> {
        let count = self.count.fetch_add(1, atomic::Ordering::SeqCst) + 1;

        if count % 16 == 0 {
            // don't hammer the mutex
            let mut guarded_last_print = self.last_print.lock().unwrap();
            let now = Instant::now();
            if now.duration_since(*guarded_last_print) >= Duration::from_secs(1) {
                println!("#{}: {}", count, path.display());
                *guarded_last_print = now;
            }
        }

        let (parent_id, parent_rel) = parent;

        match FileEntry::new(path.to_path_buf(), *parent_id) {
            Err(e) => {
                println!("Skipping '{}': {}", path.display(), e);
            }
            Ok(file_entry) => {
                if self.is_excluded_dir(&file_entry.metadata) {
                    info!("Skipping excluded directory '{}'", path.display());
                    return None;
                }
                let is_file = file_entry.is_file();
                let is_directory = file_entry.is_directory();
                let local_root = path.to_path_buf();
                let full_path = file_entry.full_path.clone();

                let mut rel_path = Vec::with_capacity(parent_rel.len() + 1 + file_entry.raw_name.len());
                if !parent_rel.is_empty() {
                    rel_path.extend_from_slice(parent_rel);
                    rel_path.push(b'/');
                }
                rel_path.extend_from_slice(&file_entry.raw_name);

                let ks = self.key_store.lock().unwrap();
                let res = if is_file {
                    ks.insert_path(
                        Some(&rel_path),
                        file_entry.key_entry,
                        Some(move || match FileIterator::new(&full_path) {
                            Err(e) => {
                                println!("Skipping '{}': {}", local_root.display(), e);
                                None
                            }
                            Ok(it) => Some(it),
                        }),
                    )
                } else {
                    ks.insert_entry(file_entry.key_entry)
                };
                match res {
                    Ok(id) => {
                        if is_directory {
                            return Some((Some(id), rel_path));
                        }
                    }
                    Err(e) => {
                        warn!("Skipping '{}': error from key store: {:?}", path.display(), e);
                    }
                }
            }
        }

        None
    }
}
