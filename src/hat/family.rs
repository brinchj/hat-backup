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
use crate::errors::HatError;
use crate::hash;
use crate::hat::insert_path_handler::InsertPathHandler;
use crate::hat::walker;
use crate::key;
use crate::models;
use crate::cbor_util;
use std::ffi;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use crate::util::{FileIterator, PathHandler};

pub mod recover {
    use super::parse_dir_data;
    use crate::blob;
    use crate::hash;
    use crate::hash::tree;
    use crate::hat::walker;
    use std::collections::VecDeque;
    use std::mem;

    #[derive(Clone)]
    pub struct Node {
        pub href: tree::HashRef,
        pub childs: Option<Vec<tree::HashRef>>,
    }

    pub struct FileVisitor {
        tops: Vec<hash::Hash>,
        nodes: VecDeque<Node>,
    }

    impl FileVisitor {
        pub fn new() -> FileVisitor {
            FileVisitor {
                tops: vec![],
                nodes: VecDeque::new(),
            }
        }
        pub fn tops(&mut self) -> Vec<hash::Hash> {
            mem::take(&mut self.tops)
        }
        pub fn nodes(&mut self) -> VecDeque<Node> {
            mem::take(&mut self.nodes)
        }
    }

    impl tree::Visitor for FileVisitor {
        fn branch_enter(&mut self, href: &tree::HashRef, childs: &Vec<tree::HashRef>) -> bool {
            self.nodes.push_back(Node {
                href: href.clone(),
                childs: Some(childs.clone()),
            });
            true
        }
        fn leaf_enter(&mut self, href: &tree::HashRef) -> bool {
            self.nodes.push_back(Node {
                href: href.clone(),
                childs: None,
            });

            // Do not proceed to read the actual file data.
            false
        }
        fn leaf_leave(&mut self, _chunk: Vec<u8>, _href: &tree::HashRef) -> bool {
            unreachable!();
        }
    }

    impl walker::LikesFiles for FileVisitor {
        fn include_file(&mut self, file: &walker::FileEntry) -> bool {
            if let walker::Content::Data(ref hash_ref) = file.hash_ref {
                self.tops.push(hash_ref.hash.clone());
                true
            } else {
                false
            }
        }

        fn include_dir(&mut self, file: &walker::FileEntry) -> bool {
            if let walker::Content::Dir(ref hash_ref) = file.hash_ref {
                self.tops.push(hash_ref.hash.clone());
                true
            } else {
                false
            }
        }
    }

    pub struct DirVisitor {
        nodes: VecDeque<Node>,
        files: Vec<walker::FileEntry>,
    }

    impl DirVisitor {
        pub fn new() -> DirVisitor {
            DirVisitor {
                nodes: VecDeque::new(),
                files: vec![],
            }
        }
        pub fn nodes(&mut self) -> VecDeque<Node> {
            mem::take(&mut self.nodes)
        }
    }

    impl walker::HasFiles for DirVisitor {
        fn files(&mut self) -> Vec<walker::FileEntry> {
            mem::take(&mut self.files)
        }
    }

    impl tree::Visitor for DirVisitor {
        fn branch_enter(&mut self, href: &tree::HashRef, childs: &Vec<tree::HashRef>) -> bool {
            self.nodes.push_back(Node {
                href: href.clone(),
                childs: Some(childs.clone()),
            });
            true
        }
        fn leaf_enter(&mut self, href: &tree::HashRef) -> bool {
            self.nodes.push_back(Node {
                href: href.clone(),
                childs: None,
            });
            // Only proceed to the leaf if it contains a tree list.
            match href.leaf {
                blob::LeafType::TreeList => true,
                blob::LeafType::SnapshotList => false,
                blob::LeafType::FileChunk => unreachable!("Opened a file with DirVisitor"),
            }
        }
        fn leaf_leave(&mut self, chunk: Vec<u8>, _href: &tree::HashRef) -> bool {
            parse_dir_data(&chunk[..], &mut self.files).unwrap();
            true
        }
    }
}

fn parse_dir_data(chunk: &[u8], out: &mut Vec<walker::FileEntry>) -> Result<(), HatError> {
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
    pub key_store_process: Vec<key::StoreProcess<FileIterator, B>>,
}
impl<B: StoreBackend> Clone for Family<B> {
    fn clone(&self) -> Family<B> {
        Family {
            name: self.name.clone(),
            key_store: self.key_store.clone(),
            key_store_process: self.key_store_process.clone(),
        }
    }
}

impl<B: StoreBackend> Family<B> {
    pub fn snapshot_dir(&self, dir: PathBuf) -> Result<(), HatError> {
        let handler = InsertPathHandler::new(self.key_store_process.clone());

        let dir = fs::canonicalize(dir)?;
        info!("Committing: {}", dir.display());
        assert!(dir.is_absolute());

        // Snapshot the directory *contents* at the family key-tree root.
        // Do not insert absolute path prefixes (/private/var/folders/...), which
        // inflated every commit/checkout with junk parent directories.
        match self.key_store_process[0].send_reply(key::Msg::<FileIterator>::ResetEntryTags)? {
            key::Reply::Ok => {}
            _ => return Err(From::from("Unexpected reply from keystore")),
        }

        if dir.is_dir() {
            handler.recurse(dir, None);

            match self.key_store_process[0].send_reply(
                key::Msg::<FileIterator>::CommitReservedNodes(Some(None)),
            )? {
                key::Reply::Ok => Ok(()),
                _ => Err(From::from("Unexpected reply from keystore")),
            }
        } else {
            // Single file: insert under the key-tree root (returns None = non-dir).
            let _ = handler.handle_path(&None, &dir);
            match self.key_store_process[0]
                .send_reply(key::Msg::<FileIterator>::CommitReservedNodes(Some(None)))?
            {
                key::Reply::Ok => Ok(()),
                _ => Err(From::from("Unexpected reply from keystore")),
            }
        }
    }

    pub fn snapshot_direct(
        &self,
        file: key::Entry,
        is_directory: bool,
        contents: Option<FileIterator>,
    ) -> Result<u64, HatError> {
        let id = self.snapshot_direct_no_commit(file, is_directory, contents)?;
        let ks = self.key_store_process.iter().last().unwrap();
        match ks.send_reply(key::Msg::<FileIterator>::CommitReservedNodes(None)) {
            Ok(key::Reply::Ok) => (),
            _ => return Err(From::from("Unexpected reply from keystore")),
        }
        Ok(id)
    }

    pub fn snapshot_direct_no_commit(
        &self,
        file: key::Entry,
        is_directory: bool,
        contents: Option<FileIterator>,
    ) -> Result<u64, HatError> {
        let f = if is_directory {
            None
        } else {
            Some(Box::new(move || contents) as Box<dyn FnOnce() -> Option<FileIterator> + Send>)
        };
        let ks = self.key_store_process.iter().last().unwrap();
        let id = match ks.send_reply(key::Msg::Insert(file, f))? {
            key::Reply::Id(id) => id,
            _ => return Err(From::from("Unexpected reply from key store")),
        };
        Ok(id)
    }

    pub fn flush(&self) -> Result<(), HatError> {
        for ks in &self.key_store_process {
            if let key::Reply::FlushOk = ks.send_reply(key::Msg::<FileIterator>::Flush)? {
                continue;
            }
            return Err(From::from("Unexpected reply from key store"));
        }
        Ok(())
    }

    pub fn write_file_chunks<HTB: hash::tree::HashTreeBackend<Err = key::MsgError>>(
        fd: &mut fs::File,
        tree: hash::tree::LeafIterator<HTB>,
    ) -> Result<(), HatError> {
        for chunk in tree {
            fd.write_all(&chunk[..])?;
        }
        fd.flush()?;
        Ok(())
    }

    // Checks out the family key-index. Hat::checkout_in_dir checks out a committed snapshot;
    // sharing more of the write path is a future cleanup.
    pub fn checkout_in_dir(
        &self,
        output_dir: PathBuf,
        dir_id: Option<u64>,
    ) -> Result<(), HatError> {
        let mut path = output_dir;
        for (entry, _ref, read_fn_opt) in self.list_from_key_store(dir_id)? {
            // Extend directory with filename:
            let name_os_string: ffi::OsString = entry.info.name.clone().into();
            path.push(&name_os_string);

            match entry.data {
                key::Data::DirPlaceholder => {
                    // This is a directory, recurse!
                    fs::create_dir_all(&path)?;
                    self.checkout_in_dir(path.clone(), entry.node_id)?;
                }
                key::Data::FilePlaceholder => {
                    // This is a file, write it
                    let mut fd = fs::File::create(&path)?;
                    if let Some(tree) = read_fn_opt.ok_or("File has data")?.init()? {
                        Self::write_file_chunks(&mut fd, tree)?;
                    }
                }
                key::Data::Symlink(link_path) => {
                    use std::os::unix::fs::symlink;
                    symlink(link_path, &path)?
                }
                _ => unreachable!("Unexpected data entry"),
            }

            if let Some(perms) = entry.info.permissions {
                let current = fs::symlink_metadata(&path)?.permissions();
                if current != perms {
                    fs::set_permissions(&path, perms)?;
                }
            }

            if let (Some(m), Some(a)) = (entry.info.modified_ts_secs, entry.info.accessed_ts_secs) {
                crate::util::set_times_nofollow(&path, a, m)?;
            }

            // Prepare for next filename:
            path.pop();
        }

        Ok(())
    }

    pub fn list_from_key_store(
        &self,
        dir_id: Option<u64>,
    ) -> Result<Vec<key::DirElem<B>>, HatError> {
        match self
            .key_store_process
            .iter()
            .last()
            .unwrap()
            .send_reply(key::Msg::<FileIterator>::ListDir(dir_id))?
        {
            key::Reply::ListResult(ls) => Ok(ls),
            _ => Err(From::from("Unexpected result from key store")),
        }
    }

    pub fn fetch_dir_data<HTB: hash::tree::HashTreeBackend<Err = key::MsgError>>(
        dir_hash: hash::tree::HashRef,
        backend: HTB,
    ) -> Result<Vec<(key::Entry, walker::Content)>, HatError> {
        let it = hash::tree::LeafIterator::new(backend, dir_hash)?.expect("unable to open dir");

        let mut out = Vec::new();
        for chunk in it {
            if !chunk.is_empty() {
                parse_dir_data(&chunk[..], &mut out)?;
            }
        }

        Ok(out.into_iter().map(|f| (f.meta, f.hash_ref)).collect())
    }

    pub fn commit<F>(&mut self, top_hash_fn: &F) -> Result<hash::tree::HashRef, HatError>
    where
        F: Fn(&hash::Hash) -> Result<(), HatError>,
    {
        let mut top_tree = self.key_store.hash_tree_writer(blob::LeafType::TreeList);
        self.commit_to_tree(&mut top_tree, None, top_hash_fn)?;

        let info = key::Info::new(self.name.clone().into(), None);
        Ok(top_tree.hash(Some(&info))?)
    }

    pub fn commit_to_tree<F>(
        &mut self,
        tree: &mut hash::tree::SimpleHashTreeWriter<key::HashStoreBackend<B>>,
        dir_id: Option<u64>,
        top_hash_fn: &F,
    ) -> Result<(), HatError>
    where
        F: Fn(&hash::Hash) -> Result<(), HatError>,
    {
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
                        // This is a file, store its data hash.
                        let href = data_ref.ok_or("File entry has no data reference")?;
                        top_hash_fn(&hash::Hash {
                            bytes: href.hash.bytes.clone(),
                        })?;
                        models::Content::Data(href.to_model())
                    }
                    key::Data::DirPlaceholder => {
                        // This is a directory, recurse!
                        let mut inner_tree =
                            self.key_store.hash_tree_writer(blob::LeafType::TreeList);
                        self.commit_to_tree(&mut inner_tree, entry.node_id, top_hash_fn)?;

                        // Store a reference for the sub-tree in our tree:
                        let dir_hash_ref = inner_tree.hash(Some(&entry.info))?;
                        top_hash_fn(&dir_hash_ref.hash)?;

                        models::Content::Directory(dir_hash_ref.to_model())
                    }
                    key::Data::Symlink(path) => {
                        // Set symbolic link content.
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

            // Flush to our own tree when we have a decent amount.
            // The tree prevents large directories from clogging ram.
            if files.is_empty() {
                break;
            } else {
                tree.append(&cbor_util::to_vec(&models::Files { files }).unwrap()[..])?;
            }
        }

        Ok(())
    }
}
