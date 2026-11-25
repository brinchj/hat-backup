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
use crate::errors::HatError;
use crate::hash::tree::{HashRef, LeafIterator};
use crate::hat::family::Family;
use crate::hat::walker::Content;
use crate::hat::{CommitFault, HatRc};
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

fn snapshot_root_ref<B: StoreBackend>(
    hat: &mut HatRc<B>,
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

fn read_dir_files<B: StoreBackend>(
    hat: &HatRc<B>,
    dir_ref: HashRef,
    prefix: PathBuf,
    out: &mut BTreeMap<PathBuf, Vec<u8>>,
) -> Result<(), HatError> {
    for (entry, content) in Family::<B>::fetch_dir_data(dir_ref, hat.hash_backend())? {
        let mut path = prefix.clone();
        let name: std::ffi::OsString = entry.info.name.clone().into();
        path.push(name);

        match content {
            Content::Data(file_ref) => {
                let mut bytes = Vec::new();
                if let Some(tree) = LeafIterator::new(hat.hash_backend(), file_ref)? {
                    for chunk in tree {
                        bytes.extend_from_slice(&chunk?);
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

fn read_snapshot_files<B: StoreBackend>(
    hat: &mut HatRc<B>,
    family: &str,
    snapshot_id: u64,
) -> Result<BTreeMap<PathBuf, Vec<u8>>, HatError> {
    let root = snapshot_root_ref(hat, family, snapshot_id);
    let mut out = BTreeMap::new();
    read_dir_files(hat, root, PathBuf::new(), &mut out)?;
    Ok(out)
}

fn unfinished_snapshots<B: StoreBackend>(hat: &mut HatRc<B>) -> usize {
    hat.unfinished_snapshot_count()
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
    hat.commit(&mut fam).unwrap();
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
    hat.commit(&mut fam).unwrap();
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
    hat.commit(&mut fam).unwrap();
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
    hat.commit(&mut fam).unwrap();
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

    hat.commit(&mut fam).unwrap();
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
    hat.commit(&mut fam).unwrap();
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
    hat.commit(&mut fam1).unwrap();
    hat.data_flush().unwrap();

    let mut fam2 = hat.open_family("fam2".into()).unwrap();
    snapshot_files(
        &fam2,
        vec![("shared", shared.clone()), ("only2", only2.clone())],
    )
    .unwrap();
    fam2.flush().unwrap();
    hat.commit(&mut fam2).unwrap();
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

#[test]
fn commit_fault_before_publish_is_atomic_and_gc_reclaims_orphans() {
    let (_, mut hat, mut fam) = setup_family();

    // Publish snapshot 1 cleanly.
    snapshot_files(&fam, vec![("keep.txt", b"keep-v1".to_vec())]).unwrap();
    fam.flush().unwrap();
    hat.commit(&mut fam).unwrap();
    hat.meta_commit().unwrap();
    hat.data_flush().unwrap();

    // Stage snapshot 2 and abort atomically right before publishing the snapshot row.
    snapshot_files(&fam, vec![("orphan.txt", b"orphan-v2".to_vec())]).unwrap();
    fam.flush().unwrap();
    hat.inject_commit_fault(CommitFault::BeforePublish);
    let err = hat.commit(&mut fam).expect_err("fault should fire");
    assert!(
        format!("{}", err).contains("injected commit fault"),
        "unexpected error: {}",
        err
    );
    assert_eq!(unfinished_snapshots(&mut hat), 0);

    // Snapshot 1 is untouched; snapshot 2 was never published.
    assert_eq!(hat.latest_snapshot_id("familyname"), Some(1));
    let actual = read_snapshot_files(&mut hat, "familyname", 1).unwrap();
    assert_eq!(
        actual.get(&PathBuf::from("keep.txt")).map(|v| &v[..]),
        Some(&b"keep-v1"[..])
    );

    // Mark-and-sweep GC reclaims the orphaned chunks from the aborted snapshot 2.
    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0, "expected gc to reclaim orphaned chunks");
    assert!(live > 0);
    hat.check().unwrap();
}

struct CountingBackend {
    inner: MemoryBackend,
    retrieves: std::sync::atomic::AtomicUsize,
}

impl CountingBackend {
    fn new() -> Self {
        CountingBackend {
            inner: MemoryBackend::new(),
            retrieves: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn reset_retrieves(&self) {
        self.retrieves.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    fn retrieve_count(&self) -> usize {
        self.retrieves.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl StoreBackend for CountingBackend {
    fn store(
        &self,
        name: &[u8],
        data: crate::crypto::CipherText,
        done_callback: Box<dyn FnOnce() + Send>,
    ) -> Result<(), String> {
        self.inner.store(name, data, done_callback)
    }

    fn retrieve(&self, name: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.retrieves
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.retrieve(name)
    }

    fn delete(&self, name: &[u8]) -> Result<(), String> {
        self.inner.delete(name)
    }

    fn list(&self) -> Result<Vec<Box<[u8]>>, String> {
        self.inner.list()
    }

    fn flush(&self) -> Result<(), String> {
        self.inner.flush()
    }
}

#[test]
fn deregister_and_gc_make_zero_remote_reads_including_after_recover() {
    let backend = Arc::new(CountingBackend::new());
    let mut hat = setup_hat(backend.clone());
    let mut fam = hat.open_family("familyname".into()).unwrap();

    // Commit two snapshots with nested directories and shared + unique files.
    snapshot_files(
        &fam,
        vec![
            ("shared.txt", b"shared-content".to_vec()),
            ("dir1/only1.txt", b"only-in-snap-1".to_vec()),
        ],
    )
    .unwrap();
    hat.commit(&mut fam).unwrap();

    snapshot_files(
        &fam,
        vec![
            ("shared.txt", b"shared-content".to_vec()),
            ("dir2/only2.txt", b"only-in-snap-2".to_vec()),
        ],
    )
    .unwrap();
    hat.commit(&mut fam).unwrap();
    hat.meta_commit().unwrap();
    hat.data_flush().unwrap();

    // Deregistering snapshot 1 + running mark-and-sweep GC must make ZERO remote retrieve() calls.
    backend.reset_retrieves();
    hat.deregister(&fam, 1).unwrap();
    assert_eq!(
        backend.retrieve_count(),
        0,
        "deregister must be O(1) local with 0 remote reads"
    );
    let (deleted, live) = hat.gc().unwrap();
    assert_eq!(
        backend.retrieve_count(),
        0,
        "mark-and-sweep gc with local tree_edges cache must make 0 remote reads"
    );
    assert!(deleted > 0);
    assert!(live > 0);

    // Publish updated snapshot list and add snapshot 3 so we can test zero-remote-read GC after recover().
    snapshot_files(&fam, vec![("dir3/only3.txt", b"only-in-snap-3".to_vec())]).unwrap();
    hat.commit(&mut fam).unwrap();
    hat.meta_commit().unwrap();
    hat.data_flush().unwrap();

    // Simulate a fresh cache rebuild via recover().
    let mut recovered = setup_hat(backend.clone());
    recovered.recover().unwrap();

    // After recover() populates tree_edges, deregister + gc must ALSO make ZERO remote reads.
    backend.reset_retrieves();
    recovered.deregister_by_name("familyname".into(), 2).unwrap();
    assert_eq!(backend.retrieve_count(), 0);
    let (deleted2, live2) = recovered.gc().unwrap();
    assert_eq!(
        backend.retrieve_count(),
        0,
        "gc after recover() must use cached tree_edges with 0 remote reads"
    );
    assert!(deleted2 > 0);
    assert!(live2 > 0);

    let files = read_snapshot_files(&mut recovered, "familyname", 3).unwrap();
    assert_eq!(
        files.get(&PathBuf::from("dir3/only3.txt")).map(|v| &v[..]),
        Some(&b"only-in-snap-3"[..])
    );
}

#[test]
fn flat_stat_cache_avoids_rehash_for_unchanged_files() {
    use std::fs;
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("src");
    fs::create_dir_all(root.join("sub")).unwrap();
    let file_path = root.join("sub/a.txt");
    fs::write(&file_path, b"hello-cache").unwrap();

    // Wait past the 1-second racily-clean window for both mtime and ctime so the
    // second pass genuinely exercises the stat_cache hit fast path.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let (_, mut hat, mut fam) = setup_family();
    fam.snapshot_dir(root.clone()).unwrap();
    hat.commit(&mut fam).unwrap();

    // Prove directly that an unchanged entry hits stat_cache without calling the reader closure
    // (and without deadlocking on the shared SQLite mutex).
    let meta = fs::symlink_metadata(&file_path).unwrap();
    let probe_entry = key::Entry::new(
        None,
        "a.txt".to_string().into(),
        key::Data::FilePlaceholder,
        Some(&meta),
    );
    fam.key_store.clear_staged();
    fam.key_store
        .insert_path(
            Some(b"sub/a.txt"),
            probe_entry,
            Some(|| -> Option<std::io::Empty> {
                panic!("stat_cache hit must not invoke reader closure");
            }),
        )
        .unwrap();

    // Second full snapshot_dir of identical on-disk tree must hit flat stat_cache and keep
    // identical file content (note: TreeList root hash may differ across seconds because
    // FileInfo records snapshot_ts_utc / accessed_ts).
    fam.snapshot_dir(root.clone()).unwrap();
    hat.commit(&mut fam).unwrap();

    let snaps = hat.list_snapshots();
    assert_eq!(snaps.len(), 2);
    let files1 = read_snapshot_files(&mut hat, "familyname", 1).unwrap();
    let files2 = read_snapshot_files(&mut hat, "familyname", 2).unwrap();
    assert_files_eq(&files1, &files2);
    assert_eq!(
        files2.get(&PathBuf::from("sub/a.txt")).map(|v| &v[..]),
        Some(&b"hello-cache"[..])
    );

    // Modifying the file must miss stat_cache and store the updated payload.
    fs::write(&file_path, b"hello-cache-updated!").unwrap();
    fam.snapshot_dir(root.clone()).unwrap();
    hat.commit(&mut fam).unwrap();

    let snaps = hat.list_snapshots();
    assert_eq!(snaps.len(), 3);
    let files3 = read_snapshot_files(&mut hat, "familyname", 3).unwrap();
    assert_eq!(
        files3.get(&PathBuf::from("sub/a.txt")).map(|v| &v[..]),
        Some(&b"hello-cache-updated!"[..])
    );
}

#[test]
fn recover_fails_or_stays_safe_if_blobs_missing() {
    let (backend, mut hat, mut fam) = setup_family();
    basic_snapshot(&fam);
    fam.flush().unwrap();
    hat.commit(&mut fam).unwrap();
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

#[cfg(unix)]
#[test]
fn non_utf8_filename_roundtrip_vfs_and_recover() {
    use crate::models::FileName;
    use crate::vfs::fs::{FileReader, List};
    use crate::vfs::Filesystem;
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStrExt;

    let (backend, mut hat, mut fam) = setup_family();

    let dir_raw: &[u8] = b"dir_\xff\xfe";
    let nested_raw: &[u8] = b"nested_\x80\x81.bin";
    let top_raw: &[u8] = b"top_\xc3\x28.dat";
    let link_raw: &[u8] = b"link_\xfe";
    let link_target_raw: &[u8] = b"target_\xff\xfe/sub_\x80";

    let dir_name = FileName::from(OsStr::from_bytes(dir_raw).to_owned());
    let nested_name = FileName::from(OsStr::from_bytes(nested_raw).to_owned());
    let top_name = FileName::from(OsStr::from_bytes(top_raw).to_owned());
    let link_name = FileName::from(OsStr::from_bytes(link_raw).to_owned());
    let link_target = PathBuf::from(OsStr::from_bytes(link_target_raw));

    assert!(matches!(dir_name, FileName::RawAndLossyUtf8(_, _)));
    assert!(matches!(nested_name, FileName::RawAndLossyUtf8(_, _)));
    assert!(matches!(top_name, FileName::RawAndLossyUtf8(_, _)));
    assert!(matches!(link_name, FileName::RawAndLossyUtf8(_, _)));

    let dir_entry = key::Entry::new(None, dir_name.clone(), key::Data::DirPlaceholder, None);
    let dir_id = fam.snapshot_direct(dir_entry, true, None).unwrap();

    let nested_entry = key::Entry::new(
        Some(dir_id),
        nested_name.clone(),
        key::Data::FilePlaceholder,
        None,
    );
    fam.snapshot_direct(
        nested_entry,
        false,
        Some(FileIterator::from_bytes(b"nested-raw-bytes".to_vec())),
    )
    .unwrap();

    let top_entry = key::Entry::new(None, top_name.clone(), key::Data::FilePlaceholder, None);
    fam.snapshot_direct(
        top_entry,
        false,
        Some(FileIterator::from_bytes(b"top-raw-bytes".to_vec())),
    )
    .unwrap();

    let link_entry = key::Entry::new(
        None,
        link_name.clone(),
        key::Data::Symlink(link_target.clone()),
        None,
    );
    fam.snapshot_direct(link_entry, false, None).unwrap();

    fam.flush().unwrap();
    hat.commit(&mut fam).unwrap();
    hat.meta_commit().unwrap();
    hat.data_flush().unwrap();
    hat.check().unwrap();

    let assert_snapshot_ok = |h: &mut HatRc<MemoryBackend>| {
        let files = read_snapshot_files(h, "familyname", 1).unwrap();
        let mut expected_nested_path = PathBuf::from(OsStr::from_bytes(dir_raw));
        expected_nested_path.push(OsStr::from_bytes(nested_raw));
        let expected_top_path = PathBuf::from(OsStr::from_bytes(top_raw));

        assert_eq!(files.len(), 2);
        assert_eq!(files.get(&expected_nested_path).unwrap(), b"nested-raw-bytes");
        assert_eq!(files.get(&expected_top_path).unwrap(), b"top-raw-bytes");

        // Verify symlink entry preserved exact non-UTF-8 name and non-UTF-8 target bytes.
        let root_ref = snapshot_root_ref(h, "familyname", 1);
        let root_entries = Family::<MemoryBackend>::fetch_dir_data(root_ref, h.hash_backend()).unwrap();
        let link = root_entries
            .iter()
            .find(|(e, _)| {
                let os: OsString = e.info.name.clone().into();
                os.as_bytes() == link_raw
            })
            .expect("non-UTF-8 symlink entry missing");
        match &link.1 {
            Content::Link(target) => {
                assert_eq!(target.as_os_str().as_bytes(), link_target_raw);
                assert_eq!(target, &link_target);
            }
            other => panic!("expected Content::Link, got {:?}", other),
        }
    };

    assert_snapshot_ok(&mut hat);

    // Verify recovery into a fresh index preserves non-UTF-8 names and VFS lookup works.
    let mut hat2 = setup_hat(backend);
    hat2.recover().unwrap();
    hat2.check().unwrap();
    assert_snapshot_ok(&mut hat2);

    let hash_backend = hat2.hash_backend();
    let mut vfs = Filesystem::new(hat2);
    let mut vfs_path = PathBuf::from("familyname/1");
    vfs_path.push(OsStr::from_bytes(dir_raw));
    vfs_path.push(OsStr::from_bytes(nested_raw));
    let listing = vfs
        .ls(&vfs_path)
        .unwrap()
        .expect("VFS ls must resolve non-UTF-8 path");
    let List::Dir(entries) = listing else {
        panic!("expected List::Dir for file entry");
    };
    assert_eq!(entries.len(), 1);
    let Content::Data(href) = entries[0].1.clone() else {
        panic!("expected Content::Data");
    };
    let mut reader = FileReader::new(hash_backend, href).unwrap();
    assert_eq!(&*reader.read(0, 1024).unwrap(), b"nested-raw-bytes");
}

#[cfg(unix)]
#[test]
fn checkout_rejects_path_traversal_entry_names() {
    use std::ffi::OsStr;

    assert!(crate::util::validate_entry_name(OsStr::new("")).is_err());

    let bad_names = [
        "..",
        "../escape.txt",
        "/tmp/hat_checkout_escape_test_never_create",
        ".",
        "nested/escape.txt",
    ];

    for bad in bad_names {
        let (_, mut hat, mut fam) = setup_family();
        let e = key::Entry::new(
            None,
            bad.to_string().into(),
            key::Data::FilePlaceholder,
            None,
        );
        fam.snapshot_direct(
            e,
            false,
            Some(FileIterator::from_bytes(b"pwned".to_vec())),
        )
        .unwrap();
        fam.flush().unwrap();
        hat.commit(&mut fam).unwrap();

        let td = tempfile::tempdir().unwrap();
        let restore = td.path().join("restore");
        std::fs::create_dir_all(&restore).unwrap();

        assert!(
            hat.checkout_in_dir("familyname".into(), restore.clone())
                .is_err(),
            "Hat::checkout_in_dir must reject unsafe entry name {:?}",
            bad
        );
        assert!(!td.path().join("escape.txt").exists());
        assert!(!std::path::Path::new("/tmp/hat_checkout_escape_test_never_create").exists());
    }
}

#[cfg(unix)]
#[test]
fn checkout_does_not_chmod_symlink_targets_or_fail_on_broken_symlink() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let td = tempfile::tempdir().unwrap();
    let outside = td.path().join("outside.txt");
    fs::write(&outside, b"do not chmod me").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();

    let (_, mut hat, mut fam) = setup_family();

    // Symlink pointing to outside 0o600 file, with stored symlink mode 0o777.
    let mut link_entry = key::Entry::new(
        None,
        "link_outside".to_string().into(),
        key::Data::Symlink(outside.clone()),
        None,
    );
    link_entry.info.permissions = Some(fs::Permissions::from_mode(0o777));
    fam.snapshot_direct(link_entry, false, None).unwrap();

    // Broken symlink with stored symlink mode 0o700 (differs from default symlink mode).
    let mut broken_entry = key::Entry::new(
        None,
        "broken_link".to_string().into(),
        key::Data::Symlink(PathBuf::from("nonexistent_target_xyz")),
        None,
    );
    broken_entry.info.permissions = Some(fs::Permissions::from_mode(0o700));
    fam.snapshot_direct(broken_entry, false, None).unwrap();

    fam.flush().unwrap();
    hat.commit(&mut fam).unwrap();

    let restore = td.path().join("restore_hat");
    fs::create_dir_all(&restore).unwrap();
    hat.checkout_in_dir("familyname".into(), restore.clone())
        .unwrap();

    let outside_mode = fs::metadata(&outside).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        outside_mode, 0o600,
        "checkout must not chmod symlink target (got {:o})",
        outside_mode
    );
    assert_eq!(
        fs::read_link(restore.join("link_outside")).unwrap(),
        outside
    );
    assert_eq!(
        fs::read_link(restore.join("broken_link")).unwrap(),
        PathBuf::from("nonexistent_target_xyz")
    );
}

#[test]
fn meta_commit_chunks_large_snapshot_list_and_recovers() {
    // Use a small max_blob_size (8 KiB) and 40 snapshots with a long family name so the
    // full SnapshotList CBOR (~10+ KiB) exceeds max_blob_size and spans multiple leaves/branches,
    // which previously panicked in Blob::try_append when serialized as a single chunk.
    let backend = Arc::new(MemoryBackend::new());
    let max_blob_size = 8192;
    let mut hat = HatRc::new_for_testing(backend.clone(), max_blob_size).unwrap();
    let family_name = format!("many_{}", "f".repeat(180));
    let mut fam = hat.open_family(family_name.clone()).unwrap();

    let count = 40u32;
    for i in 0..count {
        let entry = key::Entry::new(
            None,
            "file.txt".to_string().into(),
            key::Data::FilePlaceholder,
            None,
        );
        let payload = format!("payload-{}", i).into_bytes();
        fam.snapshot_direct(entry, false, Some(FileIterator::from_bytes(payload)))
            .unwrap();
        fam.flush().unwrap();
        hat.commit(&mut fam).unwrap();
    }

    hat.meta_commit().unwrap();
    hat.data_flush().unwrap();

    let expected_snaps: Vec<u64> = hat
        .list_snapshots()
        .into_iter()
        .filter(|s| s.family_name == family_name)
        .map(|s| s.info.snapshot_id)
        .collect();
    assert_eq!(expected_snaps.len(), count as usize);

    // Recover into a completely fresh Hat instance sharing only the backend.
    let mut hat2 = HatRc::new_for_testing(backend, max_blob_size).unwrap();
    hat2.recover().unwrap();

    let recovered_snaps: Vec<u64> = hat2
        .list_snapshots()
        .into_iter()
        .filter(|s| s.family_name == family_name)
        .map(|s| s.info.snapshot_id)
        .collect();
    assert_eq!(
        recovered_snaps, expected_snaps,
        "all chunked SnapshotList entries must survive recover()"
    );
}

#[test]
fn gc_fails_closed_on_corrupt_index_missing_child_hash() {
    let (_, mut hat, mut fam) = setup_family();
    snapshot_files(&fam, vec![("a.txt", b"hello-fail-closed".to_vec())]).unwrap();
    hat.commit(&mut fam).unwrap();
    hat.meta_commit().unwrap();
    hat.data_flush().unwrap();

    // Find the root TreeList hash id and its child file hash id.
    let snap = hat
        .list_snapshots()
        .into_iter()
        .find(|s| s.family_name == "familyname")
        .unwrap();
    let root_id = hat.hash_index.get_id(&snap.hash.unwrap()).unwrap();
    let edges = hat.db.lock().tree_edges_get(root_id).unwrap();
    assert_eq!(edges.len(), 1);
    let child_id = edges[0];

    // Case 1: tree_edges still present, but the referenced child hash row is missing -> fail closed.
    hat.hash_index.delete(child_id);
    let err1 = hat
        .gc()
        .expect_err("gc must fail closed when live tree_edges points to missing hash id");
    assert!(
        format!("{}", err1).contains("missing from hash index"),
        "unexpected error: {}",
        err1
    );

    // Case 2: tree_edges entry also cleared, forcing backend fallback -> must fail closed
    // and must NOT persist a truncated tree_edges row.
    hat.db.lock().tree_edges_delete(root_id);
    let err2 = hat
        .gc()
        .expect_err("gc fallback must fail closed when TreeList child hash is missing from index");
    assert!(
        format!("{}", err2).contains("missing from hash index"),
        "unexpected error: {}",
        err2
    );
    assert_eq!(
        hat.db.lock().tree_edges_get(root_id),
        None,
        "failed fallback must not persist truncated tree_edges"
    );
}

#[test]
fn stat_cache_prunes_deleted_files_on_commit_and_deleted_families_on_gc() {
    use std::fs;
    let td = tempfile::tempdir().unwrap();
    let dir1 = td.path().join("dir1");
    let dir2 = td.path().join("dir2");
    fs::create_dir_all(&dir1).unwrap();
    fs::create_dir_all(&dir2).unwrap();
    fs::write(dir1.join("keep.txt"), b"keep").unwrap();
    fs::write(dir1.join("removed.txt"), b"gone-soon").unwrap();
    fs::write(dir2.join("other.txt"), b"other-family").unwrap();

    let (_, mut hat, mut fam1) = setup_family();
    let mut fam2 = hat.open_family("other_family".into()).unwrap();
    let fam1_id = fam1.key_store.family_id();
    let fam2_id = fam2.key_store.family_id();

    fam1.snapshot_dir(dir1.clone()).unwrap();
    hat.commit(&mut fam1).unwrap();
    fam2.snapshot_dir(dir2.clone()).unwrap();
    hat.commit(&mut fam2).unwrap();

    assert_eq!(hat.db.lock().stat_cache_count_for_family(fam1_id), 2);
    assert_eq!(hat.db.lock().stat_cache_count_for_family(fam2_id), 1);

    // Remove a file in dir1, but abort commit before publish: stat_cache must NOT prune yet.
    fs::remove_file(dir1.join("removed.txt")).unwrap();
    fam1.snapshot_dir(dir1.clone()).unwrap();
    hat.inject_commit_fault(CommitFault::BeforePublish);
    assert!(hat.commit(&mut fam1).is_err());
    assert_eq!(hat.db.lock().stat_cache_count_for_family(fam1_id), 2);

    // Now complete commit on dir1: removed.txt is pruned from fam1, fam2 untouched.
    fam1.snapshot_dir(dir1.clone()).unwrap();
    hat.commit(&mut fam1).unwrap();
    assert_eq!(hat.db.lock().stat_cache_count_for_family(fam1_id), 1);
    assert_eq!(hat.db.lock().stat_cache_count_for_family(fam2_id), 1);

    // Delete fam2's only snapshot and run GC: fam2's stat_cache rows are reclaimed, fam1 untouched.
    hat.deregister(&fam2, 1).unwrap();
    hat.gc().unwrap();
    assert_eq!(hat.db.lock().stat_cache_count_for_family(fam2_id), 0);
    assert_eq!(hat.db.lock().stat_cache_count_for_family(fam1_id), 1);
}

#[test]
fn snapshot_selector_supports_composite_id_latest_and_date_prefix() {
    use super::{match_snapshot_selector, SelectorLookup};
    use crate::db;
    use chrono::TimeZone;

    let make = |id: u64, ts: i64| db::SnapshotStatus {
        family_name: "docs".to_string(),
        info: db::SnapshotInfo {
            unique_id: id,
            family_id: 1,
            snapshot_id: id,
        },
        hash: None,
        hash_ref: None,
        created: chrono::Utc.timestamp_opt(ts, 0).single().unwrap(),
        msg: None,
        status: db::SnapshotWorkStatus::CommitComplete,
    };

    // 2026-11-24T10:00:00Z, 2026-11-25T12:41:09Z, 2026-11-25T12:41:09Z (same-second tie!)
    let snaps = vec![
        make(1, 1_795_514_400),
        make(2, 1_795_610_469),
        make(3, 1_795_610_469),
    ];

    assert_eq!(snaps[0].display_name(), "2026-11-24T10-00-00Z-1");
    assert_eq!(snaps[1].display_name(), "2026-11-25T12-41-09Z-2");
    assert_eq!(snaps[2].display_name(), "2026-11-25T12-41-09Z-3");

    // `latest` picks highest snapshot_id.
    assert!(matches!(
        match_snapshot_selector(&snaps, "latest"),
        SelectorLookup::Found(s) if s.info.snapshot_id == 3
    ));
    // Exact composite name breaks same-second ties uniquely.
    assert!(matches!(
        match_snapshot_selector(&snaps, "2026-11-25T12-41-09Z-2"),
        SelectorLookup::Found(s) if s.info.snapshot_id == 2
    ));
    // Short numeric ID works directly.
    assert!(matches!(
        match_snapshot_selector(&snaps, "1"),
        SelectorLookup::Found(s) if s.info.snapshot_id == 1
    ));
    // Unique date prefix with colons or spaces normalizes cleanly.
    assert!(matches!(
        match_snapshot_selector(&snaps, "2026-11-24"),
        SelectorLookup::Found(s) if s.info.snapshot_id == 1
    ));
    assert!(matches!(
        match_snapshot_selector(&snaps, "2026-11-24 10:00"),
        SelectorLookup::Found(s) if s.info.snapshot_id == 1
    ));
    // Ambiguous date prefix lists all matching composite names in ID order.
    match match_snapshot_selector(&snaps, "2026-11-25T12:41") {
        SelectorLookup::Ambiguous(names) => {
            assert_eq!(
                names,
                vec!["2026-11-25T12-41-09Z-2", "2026-11-25T12-41-09Z-3"]
            );
        }
        _ => panic!("expected Ambiguous"),
    }
    // Non-matching selector returns NotFound.
    assert!(matches!(
        match_snapshot_selector(&snaps, "notanid"),
        SelectorLookup::NotFound
    ));
}
