use super::fs;
use crate::backend;
use crate::errors::HatError;
use crate::hash;
use crate::hat::{self, walker};
use fuser::{
    self, FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, Request,
};
use libc::c_int;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

#[derive(Clone)]
enum NodeKind {
    Parent,
    ParentTop(hash::tree::HashRef),
    FileTop(hash::tree::HashRef),
    SymbolicLink(PathBuf),
}

type INode = u64;

#[derive(Clone)]
struct File {
    name: OsString,
    file_type: NodeKind,
    attr: FileAttr,
    parent: Option<INode>,
}

pub struct Fuse<B: backend::StoreBackend> {
    hat: Arc<Mutex<hat::HatRc<B>>>,
    inodes: HashMap<INode, File>,
    parent: HashMap<INode, Vec<INode>>,
    open_files: HashMap<usize, fs::FileReader>,
}

impl<B: backend::StoreBackend> Fuse<B> {
    pub fn new(hat: hat::HatRc<B>) -> Fuse<B> {
        let mut fs = Fuse {
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
        fuser::mount2(self, mountpoint, &[])
    }

    fn add_file(&mut self, mut file: File) -> u64 {
        file.attr.ino = self.inodes.len() as u64 + 1u64;
        let ino = file.attr.ino;

        if let Some(parent_ino) = file.parent.as_ref() {
            if !self.parent.contains_key(parent_ino) {
                self.parent.insert(*parent_ino, vec![]);
            }
            self.parent.get_mut(parent_ino).unwrap().push(ino);
        }

        self.inodes.insert(ino, file);
        ino
    }

    fn default_attr(file_type: FileType) -> FileAttr {
        FileAttr {
            kind: file_type,
            perm: 0o755,
            ino: 0,
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            nlink: 0,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    fn populate_from_snapshot_list(&mut self) {
        let root_ino = self.add_file(File {
            name: "root".into(),
            file_type: NodeKind::Parent,
            attr: Self::default_attr(FileType::Directory),
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
                file_type: NodeKind::Parent,
                attr: Self::default_attr(FileType::Directory),
                parent: Some(root_ino),
            });
            for s in snapshots {
                if let Some(Ok(hash_ref)) = s
                    .hash_ref
                    .as_ref()
                    .map(|b| hash::tree::HashRef::from_bytes(&b[..]))
                {
                    let mut attr = Self::default_attr(FileType::Directory);
                    let created = UNIX_EPOCH + Duration::from_secs(s.created.timestamp() as u64);
                    attr.ctime = created;
                    attr.mtime = created;

                    self.add_file(File {
                        name: format!("{}", s.info.snapshot_id).into(),
                        file_type: NodeKind::ParentTop(hash_ref),
                        attr,
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
    ) -> Result<(), HatError> {
        let backend = self.hat.lock().unwrap().hash_backend();
        let entries = hat::Family::<B>::fetch_dir_data(hash_ref, backend)?;

        for (entry, hash_ref) in entries {
            let mut file = File {
                name: entry.info.name.into(),
                file_type: NodeKind::Parent,
                attr: Self::default_attr(FileType::Directory),
                parent: Some(parent),
            };

            match hash_ref {
                walker::Content::Data(hash_ref) => {
                    file.file_type = NodeKind::FileTop(hash_ref);
                    file.attr.kind = FileType::RegularFile;
                    file.attr.size = entry.info.byte_length.unwrap_or(0);
                }
                walker::Content::Dir(hash_ref) => {
                    file.file_type = NodeKind::ParentTop(hash_ref);
                    file.attr.kind = FileType::Directory;
                }
                walker::Content::Link(link_path) => {
                    file.file_type = NodeKind::SymbolicLink(link_path);
                    file.attr.kind = FileType::Symlink;
                }
            }

            if let Some(perms) = entry.info.permissions {
                use std::os::unix::fs::PermissionsExt;
                file.attr.perm = perms.mode() as u16;
            }

            if let (Some(m), Some(a)) = (entry.info.modified_ts_secs, entry.info.accessed_ts_secs) {
                file.attr.atime = UNIX_EPOCH + Duration::from_secs(a as u64);
                file.attr.mtime = UNIX_EPOCH + Duration::from_secs(m as u64);
            }

            self.add_file(file);
        }

        Ok(())
    }

    pub fn childs(&mut self, parent: INode) -> Vec<INode> {
        if let Some(file) = self.inodes.get(&parent).cloned() {
            if let NodeKind::ParentTop(hash_ref) = file.file_type {
                if !self.parent.contains_key(&parent) {
                    self.fetch_dir(parent, hash_ref).unwrap();
                }
            }
        }

        self.parent.get(&parent).cloned().unwrap_or_default()
    }
}

impl<B: backend::StoreBackend> Filesystem for Fuse<B> {
    fn init(&mut self, _req: &Request<'_>, _config: &mut fuser::KernelConfig) -> Result<(), c_int> {
        Ok(())
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        for child_ino in self.childs(parent) {
            let child = self.inodes.get(&child_ino).unwrap();
            if child.name.as_os_str() == name {
                reply.entry(&Duration::from_secs(60), &child.attr, 1);
                return;
            }
        }
        reply.error(libc::ENOENT);
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.inodes.get(&ino) {
            None => reply.error(libc::ENOENT),
            Some(file) => {
                reply.attr(&Duration::from_secs(60), &file.attr);
            }
        }
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        if let Some(file) = self.inodes.get(&ino) {
            use std::os::unix::ffi::OsStrExt;
            if let NodeKind::SymbolicLink(ref path) = file.file_type {
                reply.data(path.as_os_str().as_bytes());
                return;
            }
        }
        reply.error(libc::ENOENT);
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let backend = self.hat.lock().unwrap().hash_backend();

        if let Some(file) = self.inodes.get(&ino).cloned() {
            match file.file_type {
                NodeKind::FileTop(hash_ref) => {
                    let fh = self.open_files.len() + 1;
                    self.open_files
                        .insert(fh, fs::FileReader::new(backend, hash_ref).unwrap());
                    reply.opened(fh as u64, 0);
                    return;
                }
                _ => (),
            }
        }
        reply.error(libc::ENOENT);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        if let Some(ref mut file) = self.open_files.get_mut(&(fh as usize)) {
            match file.read(offset as u64, size as usize) {
                None => reply.data(&[]),
                Some(data) => reply.data(&data),
            }
        } else {
            reply.error(libc::EBADF);
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.open_files.remove(&(fh as usize));
        reply.ok();
    }

    fn opendir(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let file = match self.inodes.get(&ino).cloned() {
            Some(f) => f,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        let mut files: Vec<(INode, FileType, OsString)> = vec![];

        files.push((ino, FileType::Directory, ".".into()));
        if let Some(parent) = file.parent {
            files.push((parent, FileType::Directory, "..".into()));
        }

        match file.file_type {
            NodeKind::Parent | NodeKind::ParentTop(..) => {
                for f_ino in self.childs(ino) {
                    if let Some(f) = self.inodes.get(&f_ino) {
                        match f.file_type {
                            NodeKind::Parent | NodeKind::ParentTop(..) => {
                                files.push((f_ino, FileType::Directory, f.name.clone()));
                            }
                            NodeKind::SymbolicLink(..) => {
                                files.push((f_ino, FileType::Symlink, f.name.clone()));
                            }
                            NodeKind::FileTop(..) => {
                                files.push((f_ino, FileType::RegularFile, f.name.clone()));
                            }
                        };
                    }
                }
            }
            NodeKind::FileTop(..) | NodeKind::SymbolicLink(..) => (),
        }

        for (i, (ino, ft, name)) in files.into_iter().enumerate().skip(offset as usize) {
            if reply.add(ino, (i as i64) + 1, ft, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }
}
