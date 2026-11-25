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

use crate::backend::{MemoryBackend, StoreBackend};
use crate::key::*;


use rand::distributions::{Alphanumeric, DistString};
use rand::thread_rng;
use rand::Rng;
use std::io;
use std::sync::Arc;

fn random_string() -> String {
    Alphanumeric.sample_string(&mut thread_rng(), 32)
}

fn random_ascii_bytes() -> Vec<u8> {
    random_string().into_bytes()
}

#[derive(Clone, Debug)]
pub struct EntryStub {
    pub key_entry: Entry,
    pub data: Option<Vec<Vec<u8>>>,
}

impl io::Read for EntryStub {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.data.as_mut() {
            Some(x) => {
                if !x.is_empty() {
                    let c = x.remove(0);
                    buf[..c.len()].copy_from_slice(&c[..]);
                    Ok(c.len())
                } else {
                    Ok(0)
                }
            }
            None => Ok(0),
        }
    }
}

#[derive(Clone, Debug)]
struct FileSystem {
    file: EntryStub,
    filelist: Vec<FileSystem>,
}

fn rng_filesystem(size: usize) -> FileSystem {
    fn create_files(size: usize) -> Vec<FileSystem> {
        let children = size as f32 / 10.0;

        // dist_factor * i for i in range(children) + children == size
        let dist_factor: f32 = (size as f32 - children) / ((children * (children - 1.0)) / 2.0);

        let mut child_size = 0.0_f32;

        let mut files = Vec::new();
        for _ in 0..children as usize {
            let data_opt = if thread_rng().gen() {
                None
            } else {
                let mut v = vec![];
                for _ in 0..8 {
                    v.push(random_ascii_bytes())
                }
                Some(v)
            };

            let data = if data_opt.is_some() {
                Data::FilePlaceholder
            } else {
                Data::DirPlaceholder
            };
            let new_root = EntryStub {
                data: data_opt,
                key_entry: Entry {
                    node_id: None,
                    parent_id: None, // updated by insert_and_update_fs()
                    data,

                    info: Info {
                        name: random_string().into(),
                        byte_length: None,

                        created_ts_secs: thread_rng().gen(),
                        modified_ts_secs: thread_rng().gen(),
                        accessed_ts_secs: thread_rng().gen(),

                        permissions: None,
                        user_id: None,
                        group_id: None,

                        snapshot_ts_utc: 0,
                        dev: None,
                        ino: None,
                        ctime_secs: None,
                        ctime_nsec: None,
                        mtime_nsec: None,
                    },
                },
            };

            files.push(FileSystem {
                file: new_root,
                filelist: create_files(child_size as usize),
            });
            child_size += dist_factor;
        }
        files
    }

    let root = EntryStub {
        data: None,
        key_entry: Entry {
            parent_id: None,
            node_id: None, // updated by insert_and_update_fs()
            data: Data::DirPlaceholder,
            info: Info {
                name: "root".to_string().into(),
                created_ts_secs: thread_rng().gen(),
                modified_ts_secs: thread_rng().gen(),
                accessed_ts_secs: thread_rng().gen(),
                permissions: None,
                user_id: None,
                group_id: None,
                byte_length: None,
                snapshot_ts_utc: 0,
                dev: None,
                ino: None,
                ctime_secs: None,
                ctime_nsec: None,
                mtime_nsec: None,
            },
        },
    };

    FileSystem {
        file: root,
        filelist: create_files(size),
    }
}

fn insert_and_update_fs<B: StoreBackend>(fs: &mut FileSystem, ks: &Store<B>) {
    let local_file = fs.file.clone();
    fs.file.key_entry.node_id = Some(
        if fs.file.data.is_some() {
            ks.insert(fs.file.key_entry.clone(), Some(move || Some(local_file)))
        } else {
            ks.insert_entry(fs.file.key_entry.clone())
        }
        .unwrap(),
    );

    for f in fs.filelist.iter_mut() {
        f.file.key_entry.parent_id = fs.file.key_entry.node_id;
        insert_and_update_fs(f, ks);
    }
}

fn verify_filesystem<B: StoreBackend>(fs: &FileSystem, ks: &Store<B>) -> usize {
    let listing = ks.list_dir(fs.file.key_entry.node_id).unwrap();

    assert_eq!(fs.filelist.len(), listing.len());

    for (entry, persistent_ref, tree_data) in listing {
        let mut found = false;

        for dir in fs.filelist.iter() {
            if dir.file.key_entry.info.name == entry.info.name {
                found = true;

                assert_eq!(dir.file.key_entry.node_id, entry.node_id);
                assert_eq!(
                    dir.file.key_entry.info.created_ts_secs,
                    entry.info.created_ts_secs
                );
                assert_eq!(
                    dir.file.key_entry.info.accessed_ts_secs,
                    entry.info.accessed_ts_secs
                );
                assert_eq!(
                    dir.file.key_entry.info.modified_ts_secs,
                    entry.info.modified_ts_secs
                );

                match dir.file.data {
                    Some(ref original) => {
                        let it = match tree_data.expect("has data").init().unwrap() {
                            None => panic!("No data."),
                            Some(it) => it,
                        };
                        let mut original_all = vec![];
                        for chunk in original {
                            original_all.extend_from_slice(&chunk[..]);
                        }
                        let mut recovered_all = vec![];
                        for chunk in it {
                            recovered_all.extend_from_slice(&chunk.unwrap()[..]);
                        }
                        assert_eq!(original_all.len(), recovered_all.len());
                        assert_eq!(original_all, recovered_all);
                    }
                    None => {
                        assert_eq!(Data::DirPlaceholder, entry.data);
                        assert!(persistent_ref.is_none());
                    }
                }

                break; // Proceed to check next file
            }
        }
        assert!(found);
    }

    let mut count = fs.filelist.len();
    for dir in fs.filelist.iter() {
        count += verify_filesystem(dir, ks);
    }

    count
}

#[test]
fn identity() {
    fn prop(size: u8) -> bool {
        let backend = Arc::new(MemoryBackend::new());
        let ks = Store::new_for_testing(backend, 4096).unwrap();

        let mut fs = rng_filesystem(size as usize);
        insert_and_update_fs(&mut fs, &ks);
        let fs = fs;
        ks.flush().unwrap();

        verify_filesystem(&fs, &ks);
        true
    }
    quickcheck::quickcheck(prop as fn(u8) -> bool);
}

#[test]
fn data_looks_unchanged_checks_kind_metadata_and_racily_clean_window() {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    let make = |data: Data, mtime: i64, snapshot_ts: i64, len: u64, mode: u32| Entry {
        node_id: Some(1),
        parent_id: None,
        data,
        info: Info {
            name: "item".to_string().into(),
            created_ts_secs: Some(900),
            modified_ts_secs: Some(mtime),
            accessed_ts_secs: Some(950),
            permissions: Some(Permissions::from_mode(mode)),
            user_id: Some(1000),
            group_id: Some(1000),
            byte_length: Some(len),
            snapshot_ts_utc: snapshot_ts,
            dev: Some(42),
            ino: Some(100),
            ctime_secs: Some(mtime),
            ctime_nsec: Some(123_456),
            mtime_nsec: Some(789_012),
        },
    };

    // Clean old file: mtime strictly before both scans -> fast-path skip allowed.
    let stored = make(Data::FileHash(vec![1, 2, 3]), 1000, 1005, 8, 0o644);
    let fresh = make(Data::FilePlaceholder, 1000, 1006, 8, 0o644);
    assert!(fresh.data_looks_unchanged(&stored));

    // Delayed-v2 racily-clean: stored scan happened in the same second as mtime (1000 == 1000),
    // even though v2 runs later (1005 > 1000) -> must NOT skip.
    let stored_racy = make(Data::FileHash(vec![1, 2, 3]), 1000, 1000, 8, 0o644);
    assert!(!fresh.data_looks_unchanged(&stored_racy));

    // Current-second racily-clean: mtime == current scan second -> must NOT skip.
    let fresh_same_sec = make(Data::FilePlaceholder, 1006, 1006, 8, 0o644);
    let stored_same_sec = make(Data::FileHash(vec![1, 2, 3]), 1006, 1005, 8, 0o644);
    assert!(!fresh_same_sec.data_looks_unchanged(&stored_same_sec));

    // Metadata and kind mutations must NOT skip even with clean old mtime.
    assert!(!make(Data::FilePlaceholder, 1000, 1006, 9, 0o644).data_looks_unchanged(&stored));
    assert!(!make(Data::FilePlaceholder, 1000, 1006, 8, 0o755).data_looks_unchanged(&stored));
    assert!(!make(Data::DirPlaceholder, 1000, 1006, 8, 0o644).data_looks_unchanged(&stored));

    // Stat-identity changes (inode, device, ctime, sub-second mtime/ctime) must NOT skip
    // even when 1-second mtime and byte_length match and are older than snapshot_ts_utc.
    let mut diff_ino = fresh.clone();
    diff_ino.info.ino = Some(101);
    assert!(!diff_ino.data_looks_unchanged(&stored));

    let mut diff_dev = fresh.clone();
    diff_dev.info.dev = Some(43);
    assert!(!diff_dev.data_looks_unchanged(&stored));

    let mut diff_mtime_nsec = fresh.clone();
    diff_mtime_nsec.info.mtime_nsec = Some(789_013);
    assert!(!diff_mtime_nsec.data_looks_unchanged(&stored));

    let mut diff_ctime = fresh.clone();
    diff_ctime.info.ctime_secs = Some(1001);
    assert!(!diff_ctime.data_looks_unchanged(&stored));

    let mut racy_ctime = fresh.clone();
    racy_ctime.info.ctime_secs = Some(1006);
    let mut stored_racy_ctime = stored.clone();
    stored_racy_ctime.info.ctime_secs = Some(1006);
    assert!(!racy_ctime.data_looks_unchanged(&stored_racy_ctime));

    // Symlinks: identical target with clean old mtime skips; changed target does not.
    let stored_link = make(Data::Symlink(PathBuf::from("a")), 1000, 1005, 1, 0o777);
    assert!(make(Data::Symlink(PathBuf::from("a")), 1000, 1006, 1, 0o777)
        .data_looks_unchanged(&stored_link));
    assert!(!make(Data::Symlink(PathBuf::from("b")), 1000, 1006, 1, 0o777)
        .data_looks_unchanged(&stored_link));

    // Non-UTF-8 symlink targets compare raw bytes accurately.
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let raw_a = PathBuf::from(OsStr::from_bytes(b"target_\xff\xfe"));
        let raw_b = PathBuf::from(OsStr::from_bytes(b"target_\xff\xfd"));
        let stored_raw = make(Data::Symlink(raw_a.clone()), 1000, 1005, 9, 0o777);
        assert!(make(Data::Symlink(raw_a), 1000, 1006, 9, 0o777)
            .data_looks_unchanged(&stored_raw));
        assert!(!make(Data::Symlink(raw_b), 1000, 1006, 9, 0o777)
            .data_looks_unchanged(&stored_raw));
    }
}

#[test]
fn mid_read_io_error_and_failed_open_do_not_commit_corrupt_entries() {
    struct FailingReader {
        yielded_once: bool,
    }
    impl io::Read for FailingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.yielded_once {
                self.yielded_once = true;
                let data = b"partial-prefix-before-eio";
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                Ok(n)
            } else {
                Err(io::Error::new(io::ErrorKind::Other, "simulated EIO"))
            }
        }
    }

    let backend = Arc::new(MemoryBackend::new());
    let ks = Store::new_for_testing(backend, 4096).unwrap();

    // 1. Mid-read I/O error must return Err and NOT commit a truncated file.
    let eio_entry = Entry::new(None, "eio.bin".to_string().into(), Data::FilePlaceholder, None);
    let res = ks.insert(
        eio_entry,
        Some(|| Some(FailingReader { yielded_once: false })),
    );
    assert!(res.is_err(), "mid-read I/O error must fail insert");

    // 2. Failed file open (Some closure returning None) must return Err, not forge a directory.
    let open_fail_entry = Entry::new(
        None,
        "unreadable.bin".to_string().into(),
        Data::FilePlaceholder,
        None,
    );
    let res = ks.insert::<FailingReader, _>(open_fail_entry, Some(|| None));
    assert!(res.is_err(), "failed file open must fail insert");

    ks.flush().unwrap();

    let entries = ks.list_dir(None).unwrap();
    assert!(
        entries.is_empty(),
        "failed inserts must not leave truncated files or fake dirs in index: {:?}",
        entries.iter().map(|(e, _, _)| &e.info.name).collect::<Vec<_>>()
    );
}

#[test]
fn small_file_fast_path_and_fastcdc_boundary_roundtrip() {
    let backend = Arc::new(MemoryBackend::new());
    let ks = Store::new_for_testing(backend, 1024 * 1024).unwrap();

    let sizes = [
        0usize,
        1,
        FASTCDC_MIN_CHUNK_SIZE - 1,
        FASTCDC_MIN_CHUNK_SIZE,
        FASTCDC_MIN_CHUNK_SIZE + 1,
        100_000,
    ];
    let mut expected = std::collections::HashMap::new();
    for &sz in &sizes {
        let name = format!("f_{}.bin", sz);
        let data: Vec<u8> = (0..sz)
            .map(|i| ((i.wrapping_mul(31) ^ (sz >> 3)) & 0xff) as u8)
            .collect();
        let entry = Entry::new(None, name.clone().into(), Data::FilePlaceholder, None);
        let cloned = data.clone();
        ks.insert(entry, Some(move || Some(io::Cursor::new(cloned))))
            .unwrap();
        expected.insert(name, data);
    }
    ks.flush().unwrap();

    let listed = ks.list_dir(None).unwrap();
    assert_eq!(listed.len(), sizes.len());
    for (entry, _href, reader_init) in listed {
        let name = entry.info.name.utf8().to_string();
        let mut got = Vec::new();
        if let Some(it) = reader_init.unwrap().init().unwrap() {
            for chunk in it {
                got.extend_from_slice(&chunk.unwrap());
            }
        }
        assert_eq!(got, expected[&name], "roundtrip mismatch for {}", name);
        assert_eq!(entry.info.byte_length, Some(expected[&name].len() as u64));
    }
}
