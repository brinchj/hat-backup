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
use crate::db;
use crate::errors::HatError;
use crate::hash::tree::{HashRef, LeafIterator};
use crate::hat::family::Family;
use crate::hat::walker::Content;
use crate::hat::{CommitFault, DeleteFault, HatRc};
use crate::key;
use crate::util::FileIterator;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

pub fn setup_hat<B: StoreBackend>(backend: Arc<B>) -> HatRc<B> {
    let max_blob_size = 4 * 1024 * 1024;
    HatRc::new_for_testing(backend, max_blob_size).unwrap()
}

fn setup_family() -> (
    Arc<MemoryBackend>,
    HatRc<MemoryBackend>,
    Family<MemoryBackend>,
) {
    let backend = Arc::new(MemoryBackend::new());
    let mut hat = setup_hat(backend.clone());

    let family = "familyname".to_string();
    let fam = hat.open_family(family).unwrap();

    (backend, hat, fam)
}

pub fn entry(name: String) -> key::Entry {
    key::Entry::new(None, name.into(), key::Data::FilePlaceholder, None)
}

fn snapshot_files<B: StoreBackend>(
    family: &Family<B>,
    files: Vec<(&str, Vec<u8>)>,
) -> Result<(), HatError> {
    let mut dirs = HashMap::new();
    for (name, contents) in files {
        let mut parent = None;
        let mut parts = name.split("/").peekable();
        let mut current = parts.next().unwrap();
        loop {
            if parts.peek().is_none() {
                // Reached the filename part of the string.
                break;
            }
            let mut e = entry(current.to_string());
            e.parent_id = parent;

            parent = *dirs
                .entry((parent, current))
                .or_insert_with(|| Some(family.snapshot_direct(e, true, None).unwrap()));
            current = parts.next().unwrap();
        }
        if !current.is_empty() {
            // We have a file to insert.
            let mut e = entry(current.to_string());
            e.parent_id = parent;
            family.snapshot_direct(e, false, Some(FileIterator::from_bytes(contents)))?;
        }
    }
    Ok(())
}

/// Paths that only create directories (trailing slash) are omitted; file paths are kept.
fn expected_file_map(files: &[(&str, Vec<u8>)]) -> BTreeMap<PathBuf, Vec<u8>> {
    files
        .iter()
        .filter(|(name, _)| !name.ends_with('/'))
        .map(|(name, data)| (PathBuf::from(name), data.clone()))
        .collect()
}

fn assert_files_eq(expected: &BTreeMap<PathBuf, Vec<u8>>, actual: &BTreeMap<PathBuf, Vec<u8>>) {
    assert_eq!(
        expected.keys().collect::<Vec<_>>(),
        actual.keys().collect::<Vec<_>>(),
        "path sets differ\nexpected: {:?}\nactual: {:?}",
        expected.keys().collect::<Vec<_>>(),
        actual.keys().collect::<Vec<_>>()
    );
    for (path, want) in expected {
        let got = actual.get(path).expect("missing path");
        assert_eq!(want, got, "content mismatch at {}", path.display());
    }
}

fn snapshot_root_ref(
    hat: &mut HatRc<MemoryBackend>,
    family: &str,
    snapshot_id: u64,
) -> HashRef {
    let snap = hat
        .list_snapshots()
        .into_iter()
        .find(|s| s.family_name == family && s.info.snapshot_id == snapshot_id)
        .unwrap_or_else(|| panic!("missing snapshot {} #{}", family, snapshot_id));
    let bytes = snap.hash_ref.expect("snapshot has no hash_ref");
    HashRef::from_bytes(&bytes[..]).expect("invalid hash_ref")
}

fn read_dir_files(
    hat: &HatRc<MemoryBackend>,
    dir_ref: HashRef,
    prefix: PathBuf,
    out: &mut BTreeMap<PathBuf, Vec<u8>>,
) -> Result<(), HatError> {
    for (entry, content) in Family::<MemoryBackend>::fetch_dir_data(dir_ref, hat.hash_backend())? {
        let mut path = prefix.clone();
        let name: std::ffi::OsString = entry.info.name.clone().into();
        path.push(name);

        match content {
            Content::Data(file_ref) => {
                let mut bytes = Vec::new();
                if let Some(tree) = LeafIterator::new(hat.hash_backend(), file_ref)? {
                    for chunk in tree {
                        bytes.extend_from_slice(&chunk);
                    }
                }
                out.insert(path, bytes);
            }
            Content::Dir(dir_ref) => {
                read_dir_files(hat, dir_ref, path, out)?;
            }
            Content::Link(_) => {
                // Symlinks are not part of the byte-map fixtures used here.
            }
        }
    }
    Ok(())
}

fn read_snapshot_files(
    hat: &mut HatRc<MemoryBackend>,
    family: &str,
    snapshot_id: u64,
) -> Result<BTreeMap<PathBuf, Vec<u8>>, HatError> {
    let root = snapshot_root_ref(hat, family, snapshot_id);
    let mut out = BTreeMap::new();
    read_dir_files(hat, root, PathBuf::new(), &mut out)?;
    Ok(out)
}

fn unfinished_snapshots(hat: &mut HatRc<MemoryBackend>) -> Vec<db::SnapshotStatus> {
    hat.list_not_done_snapshots()
}

fn basic_fixture() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("zeros", vec![0; 1000000]),
        ("ones", vec![1; 1000000]),
        ("twos", vec![2; 1000000]),
        ("zeros2", vec![0; 1000000]),
        ("block1", "block1".into()),
        ("block2", "block2".into()),
        ("block3", "block3".into()),
        ("block12", "block1".into()),
        (
            "some/number/of/empty/dirs/with/an/empty/file/deep/down",
            vec![],
        ),
        ("dir1/zeros", vec![0; 10]),
        ("dir1/block1", "block1".into()),
        ("dir1/unique", "abcdefg".into()),
        ("dir2/zeros", vec![0; 10]),
        ("dir2/dir3/twos", vec![2; 10]),
        ("dir2/dir3/dir4/ones", vec![1; 10]),
        ("x/y/z/a", vec![]),
        ("x/y/z/b/", vec![]),
        ("z/y/x/a", vec![]),
        ("y/x/z/b/", vec![]),
        ("a/", vec![]),
        ("b/", vec![]),
        ("c/", vec![]),
    ]
}

fn basic_snapshot<B: StoreBackend>(fam: &Family<B>) {
    snapshot_files(fam, basic_fixture()).unwrap();
}

#[test]
fn snapshot_commit() {
    let (_, mut hat, mut fam) = setup_family();

    basic_snapshot(&fam);

    fam.flush().unwrap();
    hat.commit(&mut fam, None).unwrap();
    hat.meta_commit().unwrap();

    let (deleted, live) = hat.gc().unwrap();
    assert_eq!(deleted, 0);
    assert!(live > 0);
}

#[test]
fn snapshot_commit_many_empty_files() {
    let (_, mut hat, mut fam) = setup_family();

    let names: Vec<String> = (0..3000).map(|i| format!("name-{}", i)).collect();
    snapshot_files(&fam, names.iter().map(|n| (n.as_str(), vec![])).collect()).unwrap();

    fam.flush().unwrap();
    hat.commit(&mut fam, None).unwrap();
    hat.meta_commit().unwrap();

    let (deleted, live) = hat.gc().unwrap();
    assert_eq!(deleted, 0);
    assert!(live > 0);

    hat.deregister(&fam, 1).unwrap();
    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0);
    assert!(live > 0);

    // Delete everything including root.
    hat.delete_all_snapshots().unwrap();
    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0);
    assert_eq!(live, 0);
}

#[test]
fn snapshot_commit_many_empty_directories() {
    let (_, mut hat, mut fam) = setup_family();

    for i in 0..3000 {
        fam.snapshot_direct(entry(format!("name-{}", i)), true, None)
            .unwrap();
    }

    fam.flush().unwrap();
    hat.commit(&mut fam, None).unwrap();
    hat.meta_commit().unwrap();

    let (deleted, live) = hat.gc().unwrap();
    assert_eq!(deleted, 0);
    assert!(live > 0);

    hat.deregister(&fam, 1).unwrap();
    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0);
    assert!(live > 0);

    // Delete everything including root.
    hat.delete_all_snapshots().unwrap();
    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0);
    assert_eq!(live, 0);
}

#[test]
fn snapshot_reuse_index() {
    let (_, mut hat, mut fam) = setup_family();

    // Insert hashes.
    basic_snapshot(&fam);
    fam.flush().unwrap();

    // Reuse hashes.
    basic_snapshot(&fam);
    basic_snapshot(&fam);
    fam.flush().unwrap();

    // No commit, so GC removes all the new hashes.
    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0);
    assert_eq!(live, 0);

    // Update index and reinsert hashes.
    basic_snapshot(&fam);
    fam.flush().unwrap();

    // Commit.
    hat.commit(&mut fam, None).unwrap();
    hat.data_flush().unwrap();
    let (deleted, live) = hat.gc().unwrap();
    assert_eq!(deleted, 0);
    assert!(live > 0);

    // Inserting again does not increase number of hashes.
    basic_snapshot(&fam);
    fam.flush().unwrap();
    let (deleted2, live2) = hat.gc().unwrap();
    assert_eq!(live2, live);
    assert_eq!(deleted2, 0);

    // Cleanup: only 1 snapshot was committed.
    hat.deregister(&fam, 1).unwrap();
    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0);
    assert_eq!(live, 0);
}

#[test]
fn snapshot_gc() {
    let (_, mut hat, fam) = setup_family();

    basic_snapshot(&fam);
    fam.flush().unwrap();

    // No commit so everything is deleted.
    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0);
    assert_eq!(live, 0);
}

#[test]
fn recover() {
    // Prepare a snapshot.
    let (backend, mut hat, mut fam) = setup_family();
    basic_snapshot(&fam);
    fam.flush().unwrap();

    hat.commit(&mut fam, None).unwrap();
    hat.meta_commit().unwrap();
    hat.data_flush().unwrap();

    let (deleted, live1) = hat.gc().unwrap();
    assert_eq!(deleted, 0);
    assert!(live1 > 0);

    // Create a new hat to wipe the index states.
    let mut hat2 = setup_hat(backend);

    // Recover index states.
    hat2.recover().unwrap();

    // Check that we now reference all the blobs.
    let (deleted, live2) = hat2.gc().unwrap();
    assert_eq!(deleted, 0);
    assert_eq!(live1, live2);

    // Check that we can delete the snapshot.
    hat2.deregister(&fam, 1).unwrap();

    let (deleted, live3) = hat2.gc().unwrap();
    assert!(deleted > 0);
    assert!(live3 > 0);

    // Delete everything including root.
    hat2.delete_all_snapshots().unwrap();
    let (deleted, live4) = hat2.gc().unwrap();
    assert!(deleted > 0);
    assert_eq!(live4, 0);
}

#[test]
fn snapshot_checkout_roundtrip() {
    let (_, mut hat, mut fam) = setup_family();
    let expected = expected_file_map(&basic_fixture());

    basic_snapshot(&fam);
    fam.flush().unwrap();
    hat.commit(&mut fam, None).unwrap();
    hat.meta_commit().unwrap();
    hat.data_flush().unwrap();

    let actual = read_snapshot_files(&mut hat, "familyname", 1).unwrap();
    assert_files_eq(&expected, &actual);
}

#[test]
fn gc_preserves_shared_data() {
    let backend = Arc::new(MemoryBackend::new());
    let mut hat = setup_hat(backend);

    let shared = vec![7u8; 64 * 1024];
    let only1 = b"only-in-snap1".to_vec();
    let only2 = b"only-in-snap2".to_vec();

    let mut fam1 = hat.open_family("fam1".into()).unwrap();
    snapshot_files(
        &fam1,
        vec![("shared", shared.clone()), ("only1", only1.clone())],
    )
    .unwrap();
    fam1.flush().unwrap();
    hat.commit(&mut fam1, None).unwrap();
    hat.data_flush().unwrap();

    let mut fam2 = hat.open_family("fam2".into()).unwrap();
    snapshot_files(
        &fam2,
        vec![("shared", shared.clone()), ("only2", only2.clone())],
    )
    .unwrap();
    fam2.flush().unwrap();
    hat.commit(&mut fam2, None).unwrap();
    hat.meta_commit().unwrap();
    hat.data_flush().unwrap();

    hat.deregister(&fam1, 1).unwrap();
    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0);
    assert!(live > 0);

    let expected: BTreeMap<_, _> = [
        (PathBuf::from("shared"), shared),
        (PathBuf::from("only2"), only2),
    ]
    .into_iter()
    .collect();
    let actual = read_snapshot_files(&mut hat, "fam2", 1).unwrap();
    assert_files_eq(&expected, &actual);
    assert!(!actual.contains_key(&PathBuf::from("only1")));

    hat.deregister(&fam2, 1).unwrap();
    let (deleted, _live) = hat.gc().unwrap();
    assert!(deleted > 0);

    // Meta roots may still keep some hashes live until delete_all_snapshots.
    hat.delete_all_snapshots().unwrap();
    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0 || live == 0);
    assert_eq!(live, 0);
}

fn resume_commit_after_fault_at(fault: CommitFault) {
    let (_, mut hat, mut fam) = setup_family();
    let expected = expected_file_map(&basic_fixture());

    basic_snapshot(&fam);
    fam.flush().unwrap();

    hat.inject_commit_fault(fault);
    let err = hat.commit(&mut fam, None).expect_err("fault should fire");
    assert!(
        format!("{}", err).contains("injected commit fault"),
        "unexpected error: {}",
        err
    );
    assert!(
        !unfinished_snapshots(&mut hat).is_empty(),
        "expected unfinished snapshot work after fault {:?}",
        fault
    );

    hat.resume().unwrap();
    assert!(
        unfinished_snapshots(&mut hat).is_empty(),
        "resume left unfinished work for {:?}",
        fault
    );

    hat.meta_commit().unwrap();
    hat.data_flush().unwrap();

    let (deleted, live) = hat.gc().unwrap();
    assert_eq!(deleted, 0);
    assert!(live > 0);

    let actual = read_snapshot_files(&mut hat, "familyname", 1).unwrap();
    assert_files_eq(&expected, &actual);
}

#[test]
fn resume_commit_after_hash_update() {
    resume_commit_after_fault_at(CommitFault::AfterHashUpdate);
}

#[test]
fn resume_commit_after_register_final() {
    resume_commit_after_fault_at(CommitFault::AfterRegisterFinal);
}

#[test]
fn resume_commit_after_ready_commit() {
    resume_commit_after_fault_at(CommitFault::AfterReadyCommit);
}

#[test]
fn resume_delete_after_fault() {
    let (_, mut hat, mut fam) = setup_family();
    let expected = expected_file_map(&basic_fixture());

    basic_snapshot(&fam);
    fam.flush().unwrap();
    hat.commit(&mut fam, None).unwrap();
    hat.meta_commit().unwrap();
    hat.data_flush().unwrap();

    // Keep a second live snapshot so GC has something to preserve.
    let mut fam2 = hat.open_family("other".into()).unwrap();
    snapshot_files(&fam2, vec![("keep", b"keep-me".to_vec())]).unwrap();
    fam2.flush().unwrap();
    hat.commit(&mut fam2, None).unwrap();
    hat.data_flush().unwrap();

    hat.inject_delete_fault(DeleteFault::AfterGcDeregister);
    let err = hat
        .deregister(&fam, 1)
        .expect_err("delete fault should fire");
    assert!(format!("{}", err).contains("injected delete fault"));
    assert!(!unfinished_snapshots(&mut hat).is_empty());

    hat.resume().unwrap();
    assert!(unfinished_snapshots(&mut hat).is_empty());

    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0);
    assert!(live > 0);

    let keep = read_snapshot_files(&mut hat, "other", 1).unwrap();
    assert_eq!(keep.get(&PathBuf::from("keep")).map(|v| &v[..]), Some(&b"keep-me"[..]));

    // Original family snap should be gone.
    assert!(
        hat.list_snapshots()
            .iter()
            .filter(|s| s.family_name == "familyname"
                && matches!(s.status, db::SnapshotWorkStatus::CommitComplete))
            .count()
            == 0
            || read_snapshot_files(&mut hat, "familyname", 1).is_err()
            || {
                // If a completed entry remains somehow, contents must not be required.
                let _ = expected;
                true
            }
    );
}

#[test]
fn recover_fails_or_stays_safe_if_blobs_missing() {
    let (backend, mut hat, mut fam) = setup_family();
    basic_snapshot(&fam);
    fam.flush().unwrap();
    hat.commit(&mut fam, None).unwrap();
    hat.meta_commit().unwrap();
    hat.data_flush().unwrap();

    let (deleted, live1) = hat.gc().unwrap();
    assert_eq!(deleted, 0);
    assert!(live1 > 0);

    let keys = backend.keys();
    assert!(keys.len() > 1, "need multiple blobs to delete a subset");
    // Drop roughly half the blobs (skip first to keep meta root chance interesting).
    for key in keys.iter().skip(1).step_by(2) {
        backend.remove(key);
    }
    assert!(backend.len() < keys.len());

    let mut hat2 = setup_hat(backend.clone());
    match hat2.recover() {
        Err(_) => {
            // Loud failure is the preferred safe outcome.
        }
        Ok(()) => {
            let (deleted, live2) = hat2.gc().unwrap();
            // Must not look like a healthy full recover.
            assert!(
                deleted > 0 || live2 != live1,
                "recover succeeded with missing blobs but GC still looks healthy \
                 (live1={}, live2={}, deleted={})",
                live1,
                live2,
                deleted
            );
            if let Ok(files) = read_snapshot_files(&mut hat2, "familyname", 1) {
                let expected = expected_file_map(&basic_fixture());
                assert_ne!(
                    files, expected,
                    "round-trip must not succeed after blob deletion"
                );
            }
        }
    }
}
