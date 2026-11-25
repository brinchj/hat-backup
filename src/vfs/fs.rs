use crate::backend::StoreBackend;
use crate::db;
use crate::errors::HatError;
use crate::hash::tree::{self, HashRef, HashTreeBackend};
use crate::hat;
use crate::hat::walker::Content;
use crate::key::Entry;
use crate::models::FileName;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::mem;
use std::path::{self, Path, PathBuf};

pub struct FileReader {
    rest: Option<Box<dyn Iterator<Item = Result<Vec<u8>, String>>>>,
    offset: u64,
    buf: Vec<u8>,
    eof: bool,
}

impl FileReader {
    pub fn new<B>(backend: B, file: tree::HashRef) -> Result<FileReader, B::Err>
    where
        B: HashTreeBackend + 'static,
    {
        let tree = tree::LeafIterator::new(backend, file)?.map(|t| {
            Box::new(t.map(|res| res.map_err(|e| format!("{:?}", e))))
                as Box<dyn Iterator<Item = Result<Vec<u8>, String>>>
        });

        Ok(FileReader {
            eof: tree.is_none(),
            rest: tree,
            offset: 0,
            buf: Vec::with_capacity(16 * 1024),
        })
    }

    pub fn new_from_iter(rest: Option<Box<dyn Iterator<Item = Vec<u8>>>>) -> FileReader {
        let rest = rest.map(|iter| {
            Box::new(iter.map(Ok)) as Box<dyn Iterator<Item = Result<Vec<u8>, String>>>
        });
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
            match rest.next() {
                Some(Ok(buf)) => return mem::replace(&mut self.buf, buf),
                Some(Err(e)) => {
                    error!("failed reading file chunk: {}", e);
                }
                None => {}
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

    pub fn read(&mut self, offset: u64, size: usize) -> Option<Cow<'_, [u8]>> {
        self.advance(offset);

        if self.eof || self.from(offset).is_empty() {
            return None;
        }

        let avail = self.from(offset).len();

        if size <= avail {
            Some(Cow::Borrowed(self.take(offset, size)))
        } else {
            let mut buf = Vec::with_capacity(size);
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

#[derive(Debug)]
pub enum WalkItem {
    Family {
        name: String,
        created: chrono::DateTime<chrono::Utc>,
    },
    Snapshot {
        family: String,
        snapshot_id: u64,
        path: PathBuf,
        created: chrono::DateTime<chrono::Utc>,
    },
    Entry {
        path: PathBuf,
        entry: Entry,
        content: Content,
    },
}

pub struct Filesystem<B: StoreBackend> {
    hat: hat::HatRc<B>,
}

fn find_snapshot_by_selector<'a>(
    snapshots: &'a [db::SnapshotStatus],
    selector: &str,
) -> Option<&'a db::SnapshotStatus> {
    if selector == "latest" {
        snapshots
            .iter()
            .filter(|s| s.hash_ref.is_some())
            .max_by_key(|s| s.info.snapshot_id)
    } else {
        snapshots
            .iter()
            .find(|s| format!("{}", s.info.snapshot_id) == selector)
    }
}

impl<B: StoreBackend> Filesystem<B> {
    pub fn new(hat: hat::HatRc<B>) -> Filesystem<B> {
        Filesystem { hat }
    }

    pub fn hat_mut(&mut self) -> &mut hat::HatRc<B> {
        &mut self.hat
    }

    pub fn ls(&mut self, path: &Path) -> Result<Option<List>, HatError> {
        let snapshots = self.hat.list_user_snapshots();

        // Skip leading `/` or `.` so `/family/1/dir` and `family/1/dir` resolve identically,
        // while `/` and `.` list root families.
        let mut components = path
            .components()
            .skip_while(|c| matches!(c, path::Component::RootDir | path::Component::CurDir));

        let snapshots: Vec<_> = match components.next() {
            None => return Ok(Some(List::Root(snapshots))),
            Some(f) => snapshots
                .into_iter()
                .filter(|s| s.family_name == f.as_os_str().to_string_lossy())
                .collect(),
        };

        if snapshots.is_empty() {
            return Ok(None);
        }

        let snapshot_opt = match components.next() {
            None => return Ok(Some(List::Snapshots(snapshots))),
            Some(n) => find_snapshot_by_selector(&snapshots, &n.as_os_str().to_string_lossy()),
        };

        if let Some(href_bytes) = snapshot_opt.and_then(|s| s.hash_ref.as_ref()) {
            let href = HashRef::from_bytes(&href_bytes[..])?;
            let mut listing = self.ls_ref(href.clone())?;
            let mut href_opt = Some(href);
            loop {
                match (href_opt, components.next()) {
                    (_, None) => return Ok(Some(List::Dir(listing))),
                    (None, _) => return Ok(None),
                    (Some(_href), Some(name)) => {
                        let name: FileName = name.as_os_str().to_owned().into();
                        if let Some((entry, content)) = listing
                            .into_iter()
                            .find(|(e, _c)| e.info.name == name)
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

    pub fn walk_with<F>(&mut self, path: &Path, visitor: &mut F) -> Result<Option<()>, HatError>
    where
        F: FnMut(WalkItem),
    {
        let snapshots = self.hat.list_user_snapshots();

        let mut components = path
            .components()
            .skip_while(|c| matches!(c, path::Component::RootDir | path::Component::CurDir));

        let Some(family_comp) = components.next() else {
            // Walk all families -> snapshots -> directory trees in deterministic order.
            let mut by_family: BTreeMap<String, Vec<db::SnapshotStatus>> = BTreeMap::new();
            for s in snapshots {
                by_family.entry(s.family_name.clone()).or_default().push(s);
            }
            for (family_name, mut fam_snaps) in by_family {
                fam_snaps.sort_by_key(|s| s.info.snapshot_id);
                if let Some(latest_created) = fam_snaps.iter().map(|s| s.created).max() {
                    visitor(WalkItem::Family {
                        name: family_name.clone(),
                        created: latest_created,
                    });
                }
                for si in fam_snaps {
                    let snap_path =
                        PathBuf::from(&si.family_name).join(si.info.snapshot_id.to_string());
                    visitor(WalkItem::Snapshot {
                        family: si.family_name.clone(),
                        snapshot_id: si.info.snapshot_id,
                        path: snap_path.clone(),
                        created: si.created,
                    });
                    if let Some(href_bytes) = si.hash_ref.as_ref() {
                        let href = HashRef::from_bytes(&href_bytes[..])?;
                        self.walk_dir_ref(&snap_path, href, visitor)?;
                    }
                }
            }
            return Ok(Some(()));
        };

        let family_name = family_comp.as_os_str().to_string_lossy().into_owned();
        let mut fam_snaps: Vec<_> = snapshots
            .into_iter()
            .filter(|s| s.family_name == family_name)
            .collect();
        if fam_snaps.is_empty() {
            return Ok(None);
        }
        fam_snaps.sort_by_key(|s| s.info.snapshot_id);

        let Some(snap_comp) = components.next() else {
            for si in fam_snaps {
                let snap_path =
                    PathBuf::from(&si.family_name).join(si.info.snapshot_id.to_string());
                visitor(WalkItem::Snapshot {
                    family: si.family_name.clone(),
                    snapshot_id: si.info.snapshot_id,
                    path: snap_path.clone(),
                    created: si.created,
                });
                if let Some(href_bytes) = si.hash_ref.as_ref() {
                    let href = HashRef::from_bytes(&href_bytes[..])?;
                    self.walk_dir_ref(&snap_path, href, visitor)?;
                }
            }
            return Ok(Some(()));
        };

        let selector = snap_comp.as_os_str().to_string_lossy().into_owned();
        let Some(href_bytes) = find_snapshot_by_selector(&fam_snaps, &selector)
            .and_then(|s| s.hash_ref.as_ref())
            .cloned()
        else {
            return Ok(None);
        };

        let mut current_dir = HashRef::from_bytes(&href_bytes[..])?;
        let remaining: Vec<OsString> = components.map(|c| c.as_os_str().to_owned()).collect();
        if remaining.is_empty() {
            self.walk_dir_ref(path, current_dir, visitor)?;
            return Ok(Some(()));
        }

        for (idx, name_os) in remaining.iter().enumerate() {
            let is_last = idx + 1 == remaining.len();
            let target_name: FileName = name_os.clone().into();
            let entries = self.ls_ref(current_dir.clone())?;
            let Some((entry, content)) =
                entries.into_iter().find(|(e, _)| e.info.name == target_name)
            else {
                return Ok(None);
            };
            match content {
                Content::Dir(dir_href) => {
                    if is_last {
                        self.walk_dir_ref(path, dir_href, visitor)?;
                        return Ok(Some(()));
                    }
                    current_dir = dir_href;
                }
                Content::Data(..) | Content::Link(..) => {
                    if !is_last {
                        return Ok(None);
                    }
                    visitor(WalkItem::Entry {
                        path: path.to_path_buf(),
                        entry,
                        content,
                    });
                    return Ok(Some(()));
                }
            }
        }

        Ok(Some(()))
    }

    fn walk_dir_ref<F>(
        &mut self,
        base_path: &Path,
        dir_href: HashRef,
        visitor: &mut F,
    ) -> Result<(), HatError>
    where
        F: FnMut(WalkItem),
    {
        let mut entries = self.ls_ref(dir_href)?;
        entries.sort_by(|(a, _), (b, _)| a.info.name.as_bytes().cmp(b.info.name.as_bytes()));
        for (entry, content) in entries {
            let name_os: OsString = entry.info.name.clone().into();
            let child_path = base_path.join(name_os);
            let subdir = match &content {
                Content::Dir(h) => Some(h.clone()),
                _ => None,
            };
            visitor(WalkItem::Entry {
                path: child_path.clone(),
                entry,
                content,
            });
            if let Some(sub_href) = subdir {
                self.walk_dir_ref(&child_path, sub_href, visitor)?;
            }
        }
        Ok(())
    }

    pub fn walk(&mut self, path: &Path) -> Result<Option<Vec<WalkItem>>, HatError> {
        let mut items = Vec::new();
        let found = self.walk_with(path, &mut |item| items.push(item))?;
        Ok(found.map(|()| items))
    }

    /// Recursively search filenames (or full paths when `query` contains `/`) under `path`.
    /// Matching is case-insensitive substring matching.
    pub fn find(&mut self, path: &Path, query: &str) -> Result<Option<Vec<WalkItem>>, HatError> {
        let needle = query.to_lowercase();
        let match_full_path = needle.contains('/');
        let mut matches = Vec::new();
        let found = self.walk_with(path, &mut |item| {
            let matched = if needle.is_empty() {
                true
            } else {
                match &item {
                    WalkItem::Family { name, .. } => name.to_lowercase().contains(&needle),
                    WalkItem::Snapshot {
                        family,
                        snapshot_id,
                        path: p,
                        ..
                    } => {
                        if match_full_path {
                            p.to_string_lossy().to_lowercase().contains(&needle)
                        } else {
                            snapshot_id.to_string().contains(&needle)
                                || family.to_lowercase().contains(&needle)
                        }
                    }
                    WalkItem::Entry {
                        path: p, entry, ..
                    } => {
                        if match_full_path {
                            p.to_string_lossy().to_lowercase().contains(&needle)
                        } else {
                            let name_os: OsString = entry.info.name.clone().into();
                            name_os.to_string_lossy().to_lowercase().contains(&needle)
                        }
                    }
                }
            };
            if matched {
                matches.push(item);
            }
        })?;
        Ok(found.map(|()| matches))
    }

    pub fn ls_ref(&mut self, hash_ref: HashRef) -> Result<Vec<(Entry, Content)>, HatError> {
        let backend = self.hat.hash_backend();
        hat::Family::<B>::fetch_dir_data(hash_ref, backend)
    }
}
