use hash::tree::{self, HashRef, HashTreeBackend};
use hat;
use hat::walker::Content;
use backend::StoreBackend;
use errors::HatError;
use key::Entry;
use db;

use std::borrow::Cow;
use std::mem;
use std::path::{self, Path, PathBuf};

pub struct FileReader {
    rest: Option<Box<Iterator<Item = Vec<u8>>>>,
    offset: u64,
    buf: Vec<u8>,
    eof: bool,
}

impl FileReader {
    pub fn new<B>(backend: B, file: tree::HashRef) -> Result<FileReader, B::Err>
    where
        B: HashTreeBackend + 'static,
    {
        let tree = tree::LeafIterator::new(backend, file)?
            .map(|t| Box::new(t) as Box<Iterator<Item = Vec<u8>>>);

        Ok(FileReader::new_from_iter(tree))
    }

    pub fn new_from_iter(rest: Option<Box<Iterator<Item = Vec<u8>>>>) -> FileReader {
        FileReader {
            eof: rest.is_none(),
            rest,
            offset: 0,
            buf: Vec::with_capacity(16 * 1024),
        }
    }

    fn next(&mut self) -> Vec<u8> {
        if let Some(ref mut rest) = self.rest {
            self.offset += self.buf.len() as u64;
            if let Some(buf) = rest.next() {
                return mem::replace(&mut self.buf, buf);
            }
        }
        self.buf.clear();
        self.eof = true;
        vec![]
    }

    fn advance(&mut self, offset: u64) {
        while self.offset + (self.buf.len() as u64) <= offset || self.buf.is_empty() {
            self.next();
            if self.eof {
                break;
            }
        }
    }

    fn from(&mut self, offset: u64) -> &[u8] {
        assert!(self.offset <= offset);
        assert!(offset - self.offset <= (self.buf.len() as u64));
        &self.buf[(offset as usize) - (self.offset as usize)..]
    }

    fn take(&mut self, offset: u64, size: usize) -> &[u8] {
        &self.from(offset)[..size]
    }

    pub fn read(&mut self, offset: u64, size: usize) -> Option<Cow<[u8]>> {
        self.advance(offset);

        if self.eof || self.from(offset).is_empty() {
            return None;
        }

        let avail = self.from(offset).len();

        if size <= avail {
            Some(Cow::Borrowed(self.take(offset, size)))
        } else {
            let mut buf = Vec::with_capacity(size as usize);
            buf.extend_from_slice(self.take(offset, avail));
            if let Some(slice) = self.read(offset + (avail as u64), size - avail) {
                buf.extend_from_slice(&slice);
            }
            Some(Cow::Owned(buf))
        }
    }
}

#[derive(Debug)]
pub enum List {
    Root(Vec<db::SnapshotStatus>),
    Snapshots(Vec<db::SnapshotStatus>),
    Dir(Vec<(Entry, Content)>),
}

pub struct Filesystem<B: StoreBackend> {
    hat: hat::HatRc<B>,
}

impl<B: StoreBackend> Filesystem<B> {
    pub fn new(hat: hat::HatRc<B>) -> Filesystem<B> {
        Filesystem { hat }
    }

    pub fn ls(&mut self, path: &Path) -> Result<Option<List>, HatError> {
        let snapshots = self.hat.list_snapshots();

        let mut components = path.components();

        let snapshots: Vec<_> = match components.next() {
            None | Some(path::Component::RootDir) => return Ok(Some(List::Root(snapshots))),
            Some(f) => snapshots
                .into_iter()
                .filter(|s| s.family_name == f.as_os_str().to_string_lossy())
                .collect(),
        };

        let snapshot_opt = match components.next() {
            None => return Ok(Some(List::Snapshots(snapshots))),
            Some(n) => snapshots
                .iter()
                .find(|s| format!("{}", s.info.snapshot_id) == n.as_os_str().to_string_lossy()),
        };

        if let Some(href_bytes) = snapshot_opt.and_then(|s| s.hash_ref.as_ref()) {
            let href = HashRef::from_bytes(&href_bytes[..])?;
            let mut listing = self.ls_ref(href.clone())?;
            let mut href_opt = Some(href);
            loop {
                match (href_opt, components.next()) {
                    (_, None) => return Ok(Some(List::Dir(listing))),
                    (None, _) => return Ok(None),
                    (Some(href), Some(name)) => {
                        let name_str = name.as_os_str().to_string_lossy();
                        if let Some((entry, content)) = listing
                            .into_iter()
                            .find(|&(ref e, ref c)| e.info.name == name_str)
                        {
                            match content {
                                Content::Data(..) | Content::Link(..) => {
                                    href_opt = None;
                                    listing = vec![(entry, content)];
                                    continue;
                                }
                                Content::Dir(dir_href) => {
                                    listing = self.ls_ref(dir_href.clone())?;
                                    href_opt = Some(dir_href);
                                    continue;
                                }
                            }
                        } else {
                            return Ok(None);
                        }
                    }
                }
            }
        } else {
            Ok(None)
        }
    }

    pub fn ls_ref(&mut self, hash_ref: HashRef) -> Result<Vec<(Entry, Content)>, HatError> {
        let backend = self.hat.hash_backend();
        Ok(hat::Family::<B>::fetch_dir_data(hash_ref, backend)?)
    }
}
