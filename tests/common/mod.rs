#![allow(dead_code)]
//! Shared helpers for hermetic on-disk confidence tests.
//!
//! Tree compare asserts path set, file bytes, symlink targets, and permission bits.
//! uid/gid/mtime are stored by hat but not asserted here (checkout restore of those
//! fields is incomplete relative to a full metadata promise).

use hat::backend::FileBackend;
use hat::crypto::keys::Keeper;
use hat::hat::HatRc;
use hat::Hat;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

pub fn init_sodium() {
    hat::crypto::keys::init_sodium();
}

pub const MAX_BLOB_SIZE: usize = 1024 * 1024;

pub struct Sandbox {
    pub root: TempDir,
    pub repo: PathBuf,
    pub backend: PathBuf,
    pub fixture: PathBuf,
    pub restore: PathBuf,
}

pub fn sandbox() -> Sandbox {
    init_sodium();
    let root = tempfile::tempdir().expect("tempdir");
    let repo = root.path().join("repo");
    let backend = root.path().join("backend");
    let fixture = root.path().join("fixture");
    let restore = root.path().join("restore");
    fs::create_dir_all(&backend).unwrap();
    fs::create_dir_all(&fixture).unwrap();
    fs::create_dir_all(&restore).unwrap();
    Sandbox {
        root,
        repo,
        backend,
        fixture,
        restore,
    }
}

#[derive(Clone, Debug)]
pub enum FileSpec {
    File { data: Vec<u8>, mode: u32 },
    Dir { mode: u32 },
    Symlink { target: PathBuf },
}

impl FileSpec {
    pub fn file(data: impl Into<Vec<u8>>) -> Self {
        FileSpec::File {
            data: data.into(),
            mode: 0o644,
        }
    }

    pub fn file_mode(data: impl Into<Vec<u8>>, mode: u32) -> Self {
        FileSpec::File {
            data: data.into(),
            mode,
        }
    }

    pub fn dir() -> Self {
        FileSpec::Dir { mode: 0o755 }
    }

    pub fn symlink(target: impl Into<PathBuf>) -> Self {
        FileSpec::Symlink {
            target: target.into(),
        }
    }
}

pub fn write_fixture(root: &Path, entries: &[(&str, FileSpec)]) {
    for (rel, spec) in entries {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        match spec {
            FileSpec::File { data, mode } => {
                fs::write(&path, data).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(*mode)).unwrap();
            }
            FileSpec::Dir { mode } => {
                fs::create_dir_all(&path).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(*mode)).unwrap();
            }
            FileSpec::Symlink { target } => {
                symlink(target, &path).unwrap();
            }
        }
    }
}

pub fn init_repo(repo_dir: &Path) {
    fs::create_dir_all(repo_dir).unwrap();
    fs::create_dir_all(repo_dir.join("cache")).unwrap();
    Keeper::write_new_universal_key(repo_dir).unwrap();
}

pub fn open_hat(repo_dir: &Path, backend_dir: &Path) -> HatRc<FileBackend> {
    let backend = Arc::new(FileBackend::new(backend_dir.to_path_buf()));
    Hat::open_repository(repo_dir.to_path_buf(), backend, MAX_BLOB_SIZE).unwrap()
}

pub fn commit_tree(hat: &mut HatRc<FileBackend>, family: &str, fixture_path: &Path) {
    let mut fam = hat.open_family(family.to_string()).unwrap();
    fam.snapshot_dir(fixture_path.to_path_buf()).unwrap();
    hat.commit(&mut fam, None).unwrap();
    hat.meta_commit().unwrap();
    hat.data_flush().unwrap();
}

#[derive(Debug, PartialEq, Eq)]
enum NodeKind {
    File(Vec<u8>, u32),
    Dir(u32),
    Link(PathBuf),
}

fn read_tree(root: &Path) -> BTreeMap<PathBuf, NodeKind> {
    let mut out = BTreeMap::new();
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, NodeKind>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path.strip_prefix(base).unwrap().to_path_buf();
            let meta = fs::symlink_metadata(&path).unwrap();
            let ft = meta.file_type();
            if ft.is_symlink() {
                out.insert(rel, NodeKind::Link(fs::read_link(&path).unwrap()));
            } else if ft.is_dir() {
                out.insert(rel, NodeKind::Dir(meta.permissions().mode() & 0o777));
                walk(base, &path, out);
            } else if ft.is_file() {
                out.insert(
                    rel,
                    NodeKind::File(fs::read(&path).unwrap(), meta.permissions().mode() & 0o777),
                );
            }
        }
    }
    if root.exists() {
        walk(root, root, &mut out);
    }
    out
}

/// After relative-root snapshot, checkout places fixture contents directly under
/// `restore_root` (no absolute-path prefix).
pub fn restored_tree_path(_fixture: &Path, restore_root: &Path) -> PathBuf {
    restore_root.to_path_buf()
}

pub fn checkout_and_leaf(
    hat: &mut HatRc<FileBackend>,
    family: &str,
    _fixture: &Path,
    restore_root: &Path,
) -> PathBuf {
    if restore_root.exists() {
        fs::remove_dir_all(restore_root).ok();
    }
    fs::create_dir_all(restore_root).unwrap();
    hat.checkout_in_dir(family.to_string(), restore_root.to_path_buf())
        .unwrap();
    restore_root.to_path_buf()
}

/// Compare two directory trees: paths, file bytes, symlink targets, permission bits.
pub fn assert_tree_eq(expected: &Path, actual: &Path) {
    let exp = read_tree(expected);
    let act = read_tree(actual);
    assert_eq!(
        exp.keys().collect::<Vec<_>>(),
        act.keys().collect::<Vec<_>>(),
        "path sets differ\nexpected: {:?}\nactual: {:?}",
        exp.keys().collect::<Vec<_>>(),
        act.keys().collect::<Vec<_>>()
    );
    for (path, exp_node) in &exp {
        let act_node = act.get(path).unwrap();
        match (exp_node, act_node) {
            (NodeKind::File(ed, em), NodeKind::File(ad, am)) => {
                assert_eq!(ed, ad, "file bytes differ at {}", path.display());
                assert_eq!(em, am, "file mode differs at {}", path.display());
            }
            (NodeKind::Dir(em), NodeKind::Dir(am)) => {
                assert_eq!(em, am, "dir mode differs at {}", path.display());
            }
            (NodeKind::Link(et), NodeKind::Link(at)) => {
                assert_eq!(et, at, "symlink target differs at {}", path.display());
            }
            _ => panic!(
                "node kind mismatch at {}: {:?} vs {:?}",
                path.display(),
                exp_node,
                act_node
            ),
        }
    }
}

pub fn basic_fixture() -> Vec<(&'static str, FileSpec)> {
    vec![
        ("readme.txt", FileSpec::file(b"hello hat\n".to_vec())),
        ("empty", FileSpec::file(Vec::new())),
        ("nested/a.txt", FileSpec::file(b"nested-a".to_vec())),
        ("nested/b.bin", FileSpec::file(vec![0, 1, 2, 255, 10])),
        (
            "exec.sh",
            FileSpec::file_mode(b"#!/bin/sh\necho hi\n".to_vec(), 0o755),
        ),
    ]
}
