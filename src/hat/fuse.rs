use hash;
use hat::walker;
use errors;
use libc;

use std::path::{Path, PathBuf};
use std::mem;
use std::io;
use std::borrow::Cow;
use fuse;
use backend;
use hat;
use std::ffi::{OsStr, OsString};
use libc::c_int;
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use time::Timespec;

#[derive(Clone)]
enum FileType {
    Parent,
    ParentTop(hash::tree::HashRef),
    FileTop(hash::tree::HashRef),
    SymbolicLink(PathBuf),
}

type INode = u64;

#[derive(Clone)]
struct File {
    name: OsString,
    file_type: FileType,
    attr: fuse::FileAttr,
    parent: Option<INode>,
}

struct FileReader {
    rest: Option<Box<Iterator<Item = Vec<u8>>>>,
    offset: u64,
    buf: Vec<u8>,
    eof: bool,
}

impl FileReader {
    fn new(rest: Option<Box<Iterator<Item = Vec<u8>>>>) -> FileReader {
        FileReader {
            rest,
            offset: 0,
            buf: Vec::with_capacity(128 * 1024),
            eof: false,
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
        while self.offset + (self.buf.len() as u64) < offset || self.buf.is_empty() {
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

    fn read(&mut self, offset: u64, size: usize) -> Option<Cow<[u8]>> {
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

pub struct Fs<B: backend::StoreBackend> {
    hat: Arc<Mutex<hat::HatRc<B>>>,
    inodes: HashMap<INode, File>,
    parent: HashMap<INode, Vec<INode>>,
    open_files: HashMap<usize, FileReader>,
}

impl<B: backend::StoreBackend> Fs<B> {
    pub fn new(hat: hat::HatRc<B>) -> Fs<B> {
        let mut fs = Fs {
            hat: Arc::new(Mutex::new(hat)),
            inodes: HashMap::new(),
            parent: HashMap::new(),
            open_files: HashMap::new(),
        };

        fs.populate_from_snapshot_list();

        fs
    }

    pub fn mount<P>(self, mountpoint: &P) -> Result<(), io::Error>
    where
        P: AsRef<Path>,
    {
        fuse::mount(self, mountpoint, &[])
    }

    fn add_file(&mut self, mut file: File) -> u64 {
        file.attr.ino = self.inodes.len() as u64 + 1u64;
        let ino = file.attr.ino;

        if let Some(parent_ino) = file.parent.as_ref() {
            if !self.parent.contains_key(&parent_ino) {
                self.parent.insert(*parent_ino, vec![]);
            }
            self.parent.get_mut(&parent_ino).unwrap().push(ino);
        }

        self.inodes.insert(ino, file);
        ino
    }

    fn default_attr(file_type: fuse::FileType) -> fuse::FileAttr {
        fuse::FileAttr {
            kind: file_type,
            perm: 0o755,
            ino: 0,
            size: 0,
            blocks: 0,
            atime: Timespec::new(0, 0),
            ctime: Timespec::new(0, 0),
            mtime: Timespec::new(0, 0),
            crtime: Timespec::new(0, 0),
            nlink: 0,
            uid: 0,
            gid: 0,
            rdev: 0,
            flags: 0,
        }
    }

    fn populate_from_snapshot_list(&mut self) {
        let root_ino = self.add_file(File {
            name: "root".into(),
            file_type: FileType::Parent,
            attr: Self::default_attr(fuse::FileType::Directory),
            parent: None,
        });

        let mut snapshots = HashMap::new();
        for si in self.hat.lock().unwrap().list_snapshots() {
            if !snapshots.contains_key(&si.family_name) {
                snapshots.insert(si.family_name.clone(), vec![]);
            }
            snapshots.get_mut(&si.family_name).unwrap().push(si);
        }

        for (family_name, snapshots) in snapshots {
            if family_name == "__hat__roots__" {
                continue;
            }

            let family_ino = self.add_file(File {
                name: family_name.into(),
                file_type: FileType::Parent,
                attr: Self::default_attr(fuse::FileType::Directory),
                parent: Some(root_ino),
            });
            for s in snapshots {
                if let Some(Ok(hash_ref)) = s.hash_ref
                    .as_ref()
                    .map(|b| hash::tree::HashRef::from_bytes(&b[..]))
                {
                    let mut attr = Self::default_attr(fuse::FileType::Directory);
                    attr.ctime.sec = s.created.timestamp();
                    attr.mtime.sec = s.created.timestamp();

                    self.add_file(File {
                        name: format!("{}", s.info.snapshot_id).into(),
                        file_type: FileType::ParentTop(hash_ref),
                        attr: attr,
                        parent: Some(family_ino),
                    });
                }
            }
        }
    }

    pub fn fetch_dir(
        &mut self,
        parent: INode,
        hash_ref: hash::tree::HashRef,
    ) -> Result<(), errors::HatError> {
        let backend = self.hat.lock().unwrap().hash_backend();
        let entries = hat::family::Family::<B>::fetch_dir_data(hash_ref, backend)?;

        for (entry, hash_ref) in entries {
            let mut file = File {
                name: entry.info.name.clone().into(),
                file_type: FileType::Parent,
                attr: Self::default_attr(fuse::FileType::Directory),
                parent: Some(parent),
            };

            match hash_ref {
                walker::Content::Data(hash_ref) => {
                    file.file_type = FileType::FileTop(hash_ref);
                    file.attr.kind = fuse::FileType::RegularFile;
                    file.attr.size = entry.info.byte_length.unwrap_or(0);
                }
                walker::Content::Dir(hash_ref) => {
                    file.file_type = FileType::ParentTop(hash_ref);
                    file.attr.kind = fuse::FileType::Directory;
                }
                walker::Content::Link(link_path) => {
                    file.file_type = FileType::SymbolicLink(link_path);
                    file.attr.kind = fuse::FileType::Symlink;
                }
            }

            if let Some(perms) = entry.info.permissions {
                use std::os::unix::fs::PermissionsExt;
                file.attr.perm = perms.mode() as u16;
            }

            if let (Some(m), Some(a)) = (entry.info.modified_ts_secs, entry.info.accessed_ts_secs) {
                file.attr.atime.sec = a as i64;
                file.attr.mtime.sec = m as i64;
            }

            self.add_file(file);
        }

        Ok(())
    }

    pub fn childs(&mut self, parent: INode) -> Vec<INode> {
        if let Some(file) = self.inodes.get(&parent).cloned() {
            if let FileType::ParentTop(hash_ref) = file.file_type {
                if !self.parent.contains_key(&parent) {
                    self.fetch_dir(parent, hash_ref).unwrap();
                }
            }
        }

        self.parent.get(&parent).unwrap().clone()
    }
}

impl<B: backend::StoreBackend> fuse::Filesystem for Fs<B> {
    fn init(&mut self, req: &fuse::Request) -> Result<(), c_int> {
        Ok(())
    }
    fn lookup(&mut self, req: &fuse::Request, parent: u64, name: &OsStr, reply: fuse::ReplyEntry) {
        for child_ino in self.childs(parent) {
            let child = self.inodes.get(&child_ino).unwrap();
            if child.name.as_os_str() == name {
                reply.entry(&Timespec { sec: 60, nsec: 0 }, &child.attr, 1);
                return;
            }
        }
    }
    fn getattr(&mut self, req: &fuse::Request, ino: u64, reply: fuse::ReplyAttr) {
        match self.inodes.get(&ino) {
            None => (),
            Some(file) => {
                reply.attr(&Timespec { sec: 60, nsec: 0 }, &file.attr);
            }
        }
    }
    fn readlink(&mut self, req: &fuse::Request, ino: u64, reply: fuse::ReplyData) {
        if let Some(file) = self.inodes.get(&ino) {
            use std::os::unix::ffi::OsStrExt;
            if let FileType::SymbolicLink(ref path) = file.file_type {
                reply.data(path.as_os_str().as_bytes());
            }
        }
    }
    fn open(&mut self, req: &fuse::Request, ino: u64, flags: u32, reply: fuse::ReplyOpen) {
        let backend = self.hat.lock().unwrap().hash_backend();

        if let Some(file) = self.inodes.get(&ino).cloned() {
            match file.file_type {
                FileType::FileTop(hash_ref) => {
                    let tree = hash::tree::LeafIterator::new(backend, hash_ref)
                        .unwrap()
                        .map(|t| Box::new(t) as Box<Iterator<Item = Vec<u8>>>);

                    let fh = self.open_files.len() + 1;
                    self.open_files.insert(fh, FileReader::new(tree));
                    reply.opened(fh as u64, flags);
                }
                _ => (),
            }
        }
    }
    fn read(
        &mut self,
        req: &fuse::Request,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        reply: fuse::ReplyData,
    ) {
        if let Some(ref mut file) = self.open_files.get_mut(&(fh as usize)) {
            match file.read(offset as u64, size as usize) {
                None => reply.error(libc::EOF),
                Some(data) => reply.data(&data),
            }
        }
    }
    fn release(
        &mut self,
        req: &fuse::Request,
        ino: u64,
        fh: u64,
        flags: u32,
        lock_owner: u64,
        flush: bool,
        reply: fuse::ReplyEmpty,
    ) {
        self.open_files.remove(&(fh as usize));
        reply.ok();
    }
    fn opendir(&mut self, req: &fuse::Request, ino: u64, flags: u32, reply: fuse::ReplyOpen) {
        reply.opened(0, flags);
    }
    fn readdir(
        &mut self,
        req: &fuse::Request,
        ino: u64,
        fh: u64,
        offset: i64,
        mut reply: fuse::ReplyDirectory,
    ) {
        let file = self.inodes.get(&ino).unwrap().clone();
        let mut files: Vec<(INode, fuse::FileType, OsString)> = vec![];

        files.push((ino, fuse::FileType::Directory, ".".into()));
        if let Some(parent) = file.parent {
            files.push((parent, fuse::FileType::Directory, "..".into()));
        }

        match file.file_type {
            FileType::Parent | FileType::ParentTop(..) => for f_ino in self.childs(ino) {
                if let Some(f) = self.inodes.get(&f_ino) {
                    match f.file_type {
                        FileType::Parent | FileType::ParentTop(..) => {
                            files.push((f_ino, fuse::FileType::Directory, f.name.clone()));
                        }
                        FileType::SymbolicLink(..) => {
                            files.push((f_ino, fuse::FileType::Symlink, f.name.clone()));
                        }
                        FileType::FileTop(..) => {
                            files.push((f_ino, fuse::FileType::RegularFile, f.name.clone()));
                        }
                    };
                }
            },
            FileType::FileTop(..) | FileType::SymbolicLink(..) => (),
        }

        files
            .into_iter()
            .enumerate()
            .skip(offset as usize)
            .for_each(|(offset, (ino, ft, name))| {
                reply.add(ino, (offset as i64) + 1, ft, name);
            });
        reply.ok();
    }

    fn releasedir(
        &mut self,
        req: &fuse::Request,
        ino: u64,
        fh: u64,
        flags: u32,
        reply: fuse::ReplyEmpty,
    ) {
        reply.ok();
    }
}
