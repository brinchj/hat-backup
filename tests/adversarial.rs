mod common;

use common::*;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

fn wipe_cache(repo: &Path) {
    let cache = repo.join("cache");
    for entry in fs::read_dir(&cache).unwrap() {
        let path = entry.unwrap().path();
        let res = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        if let Err(e) = res {
            if e.kind() != io::ErrorKind::NotFound {
                panic!("failed to remove {:?}: {}", path, e);
            }
        }
    }
}

#[test]
fn checkout_fails_if_blob_truncated() {
    let sb = sandbox();
    write_fixture(
        &sb.fixture,
        &[
            ("big.bin", FileSpec::file(vec![7u8; 64 * 1024])),
            ("small.txt", FileSpec::file(b"tiny".to_vec())),
        ],
    );
    init_repo(&sb.repo);
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &sb.fixture);
        hat.check().expect("check should pass before corruption");
    }

    let blobs = backend_files(&sb.backend);
    assert!(!blobs.is_empty(), "expected backend blobs");
    // Every blob is padded to MAX_BLOB_SIZE, so truncating a single blob by size
    // can hit only the synthetic __hat__roots__ blob. Test every blob individually:
    // check() must fail for every blob, and checkout must fail for live data/tree blobs.
    let mut checkout_failures = 0usize;
    for victim in &blobs {
        let orig = fs::read(victim).unwrap();
        let mut f = fs::OpenOptions::new().write(true).open(victim).unwrap();
        f.set_len(16).unwrap();
        f.flush().unwrap();
        drop(f);

        let mut hat = open_hat(&sb.repo, &sb.backend);
        assert!(
            hat.check().is_err(),
            "check must reject truncated blob {}",
            victim.display()
        );
        if sb.restore.exists() {
            fs::remove_dir_all(&sb.restore).ok();
        }
        fs::create_dir_all(&sb.restore).unwrap();
        if hat.checkout_in_dir("family".into(), sb.restore.clone()).is_err() {
            checkout_failures += 1;
        }
        drop(hat);

        fs::write(victim, &orig).unwrap();
    }
    assert!(
        checkout_failures > 0,
        "truncating live data/tree blobs must fail checkout"
    );
}

#[test]
fn recover_ondisk_missing_blobs() {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());
    init_repo(&sb.repo);
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &sb.fixture);
        hat.gc().unwrap();
    }

    let mut blobs: Vec<_> = fs::read_dir(&sb.backend)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .collect();
    blobs.sort();
    assert!(blobs.len() > 1);
    for (i, path) in blobs.iter().enumerate() {
        if i % 2 == 1 {
            fs::remove_file(path).unwrap();
        }
    }

    // Wipe local indexes; keep universal key + backend.
    wipe_cache(&sb.repo);

    let mut hat = open_hat(&sb.repo, &sb.backend);
    match hat.recover() {
        Err(_) => {}
        Ok(()) => {
            let check = hat.check();
            let checkout = hat.checkout_in_dir("family".into(), sb.restore.clone());
            assert!(
                check.is_err() || checkout.is_err(),
                "recover must not report a healthy full restore with missing blobs"
            );
        }
    }
}

#[test]
fn check_command_detects_missing_hash() {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());
    init_repo(&sb.repo);
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &sb.fixture);
        hat.check().unwrap();
    }

    let blobs: Vec<_> = fs::read_dir(&sb.backend)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .collect();
    assert!(!blobs.is_empty());
    fs::remove_file(&blobs[0]).unwrap();

    let hat = open_hat(&sb.repo, &sb.backend);
    assert!(hat.check().is_err(), "check must fail when a blob is missing");
}

#[test]
fn recover_ignores_stray_short_and_corrupt_hex_backend_files() {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());
    init_repo(&sb.repo);
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &sb.fixture);
    }

    // Inject stray hex filenames that previously crashed BlobIndex::id_of_name:
    // - 1-byte hex name (< SEALBYTES -> integer underflow)
    // - 64-byte random hex name (>= SEALBYTES -> crypto_box_seal_open auth failure panic)
    fs::write(sb.backend.join("aa"), b"junk").unwrap();
    fs::write(sb.backend.join("deadbeef".repeat(16)), b"more-junk").unwrap();

    // Wipe local indexes; keep universal key + backend.
    wipe_cache(&sb.repo);

    let mut hat = open_hat(&sb.repo, &sb.backend);
    hat.recover().expect("recover must ignore stray hex backend files");
    hat.check().expect("recovered repo must pass check");
    let leaf = checkout_and_leaf(&mut hat, "family", &sb.fixture, &sb.restore);
    assert_tree_eq(&sb.fixture, &leaf);
}

#[test]
fn recover_with_wrong_key_returns_err_instead_of_panicking() {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());
    init_repo(&sb.repo);
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &sb.fixture);
    }

    // Fresh repo dir with a different universal key pointing at the same backend.
    let other_repo = sb.root.path().join("other-repo");
    init_repo(&other_repo);

    let mut hat = open_hat(&other_repo, &sb.backend);
    assert!(
        hat.recover().is_err(),
        "recover with wrong key must return Err cleanly, not panic"
    );
}

#[test]
fn open_repository_rejects_empty_and_oversized_universal_key() {
    use hat::backend::FileBackend;
    use hat::Hat;
    use std::sync::Arc;

    let sb = sandbox();
    init_repo(&sb.repo);
    let key_path = sb.repo.join("secret-universal-key");

    // 0-byte key must fail closed (never silently fall back to unkeyed BLAKE2b).
    fs::write(&key_path, b"").unwrap();
    let backend = Arc::new(FileBackend::new(sb.backend.clone()));
    assert!(Hat::open_repository(sb.repo.clone(), backend, MAX_BLOB_SIZE).is_err());

    // Oversized (> 64 bytes) key must return Err instead of panicking in libsodium.
    fs::write(&key_path, vec![0x99u8; 128]).unwrap();
    let backend = Arc::new(FileBackend::new(sb.backend.clone()));
    assert!(Hat::open_repository(sb.repo.clone(), backend, MAX_BLOB_SIZE).is_err());
}

#[test]
fn swapping_two_valid_blobs_is_rejected_by_check_and_checkout() {
    let sb = sandbox();
    // Create enough data across multiple files to ensure >= 2 backend blobs.
    write_fixture(
        &sb.fixture,
        &[
            ("a.bin", FileSpec::file(vec![0x11u8; 700 * 1024])),
            ("b.bin", FileSpec::file(vec![0x22u8; 700 * 1024])),
        ],
    );
    init_repo(&sb.repo);
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &sb.fixture);
        hat.check().expect("check passes before blob swap");
    }

    let mut blobs: Vec<_> = fs::read_dir(&sb.backend)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .collect();
    blobs.sort();
    assert!(blobs.len() >= 2, "expected at least 2 blobs, got {}", blobs.len());

    // Swap contents of two valid blobs on the backend.
    let bytes0 = fs::read(&blobs[0]).unwrap();
    let bytes1 = fs::read(&blobs[1]).unwrap();
    fs::write(&blobs[0], &bytes1).unwrap();
    fs::write(&blobs[1], &bytes0).unwrap();

    let mut hat = open_hat(&sb.repo, &sb.backend);
    assert!(
        hat.check().is_err(),
        "check must reject swapped blobs because MAC binds blob_name"
    );
    fs::create_dir_all(&sb.restore).unwrap();
    assert!(
        hat.checkout_in_dir("family".into(), sb.restore.clone()).is_err(),
        "checkout must return Err on swapped blobs"
    );
}

#[test]
fn recover_with_expected_root_detects_backend_rollback() {
    let sb = sandbox();
    init_repo(&sb.repo);

    let fix1 = sb.root.path().join("fix1");
    let fix2 = sb.root.path().join("fix2");
    fs::create_dir_all(&fix1).unwrap();
    fs::create_dir_all(&fix2).unwrap();
    write_fixture(&fix1, &[("v1.txt", FileSpec::file(b"version-1".to_vec()))]);
    write_fixture(&fix2, &[("v2.txt", FileSpec::file(b"version-2".to_vec()))]);

    let latest_root_hash = {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &fix1);
        commit_tree(&mut hat, "family", &fix2);
        hat.list_snapshots()
            .into_iter()
            .filter(|s| s.family_name == "__hat__roots__")
            .max_by_key(|s| s.info.snapshot_id)
            .and_then(|s| s.hash)
            .expect("synthetic root hash")
            .bytes
    };

    // Wrong pinned root hash (simulating a backend rollback to an older SnapshotList) fails closed.
    wipe_cache(&sb.repo);
    let mut wrong_hash = latest_root_hash.clone();
    wrong_hash[0] ^= 0xFF;
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        assert!(
            hat.recover_with_expected_root(Some(&wrong_hash)).is_err(),
            "recover_with_expected_root must fail on root hash mismatch"
        );
    }

    // Matching pinned root hash succeeds and restores latest snapshot.
    wipe_cache(&sb.repo);
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        hat.recover_with_expected_root(Some(&latest_root_hash))
            .expect("recover_with_expected_root should succeed with matching root hash");
        let leaf = checkout_and_leaf(&mut hat, "family", &fix2, &sb.restore);
        assert_tree_eq(&fix2, &leaf);
    }
}

#[test]
fn backend_blobs_do_not_leak_plaintext_or_filenames() {
    let sb = sandbox();
    init_repo(&sb.repo);

    let secret_payload = b"SUPER_SECRET_SENTINEL_PAYLOAD_9876543210".to_vec();
    let secret_filename = "TOP_SECRET_FILENAME_SENTINEL.txt";
    let secret_target = "SECRET_SYMLINK_TARGET_SENTINEL";
    write_fixture(
        &sb.fixture,
        &[
            (secret_filename, FileSpec::file(secret_payload.clone())),
            ("link", FileSpec::symlink(secret_target)),
        ],
    );

    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &sb.fixture);
    }

    let blobs = backend_files(&sb.backend);
    assert!(!blobs.is_empty());

    let needles: &[&[u8]] = &[
        &secret_payload,
        secret_filename.as_bytes(),
        secret_target.as_bytes(),
    ];
    for blob in &blobs {
        let fname = blob.file_name().unwrap().as_encoded_bytes();
        let data = fs::read(blob).unwrap();
        for needle in needles {
            assert!(
                !fname.windows(needle.len()).any(|w| w == *needle),
                "backend filename leaked plaintext sentinel"
            );
            assert!(
                !data.windows(needle.len()).any(|w| w == *needle),
                "backend blob {} leaked plaintext sentinel",
                blob.display()
            );
        }
    }
}

#[test]
fn single_bit_flip_detected_by_check_and_checkout() {
    for flip_end in [false, true] {
        let sb = sandbox();
        write_fixture(
            &sb.fixture,
            &[("payload.bin", FileSpec::file(random_bytes_seeded(7, 64 * 1024)))],
        );
        init_repo(&sb.repo);
        {
            let mut hat = open_hat(&sb.repo, &sb.backend);
            commit_tree(&mut hat, "family", &sb.fixture);
            hat.check().unwrap();
        }

        let blobs = backend_files(&sb.backend);
        assert!(!blobs.is_empty());
        let mut checkout_failures = 0usize;
        for victim in &blobs {
            let orig = fs::read(victim).unwrap();
            let mut bytes = orig.clone();
            let idx = if flip_end {
                bytes.len() - 1
            } else {
                bytes.len() / 2
            };
            bytes[idx] ^= 0x01;
            fs::write(victim, &bytes).unwrap();

            let mut hat = open_hat(&sb.repo, &sb.backend);
            assert!(
                hat.check().is_err(),
                "check must fail on 1-bit flip (flip_end={}, blob={})",
                flip_end,
                victim.display()
            );
            if sb.restore.exists() {
                fs::remove_dir_all(&sb.restore).ok();
            }
            fs::create_dir_all(&sb.restore).unwrap();
            if hat.checkout_in_dir("family".into(), sb.restore.clone()).is_err() {
                checkout_failures += 1;
            }
            drop(hat);

            fs::write(victim, &orig).unwrap();
        }
        assert!(
            checkout_failures > 0,
            "1-bit flip on live data/tree blobs must fail checkout (flip_end={})",
            flip_end
        );
    }
}

#[test]
fn wrong_key_fails_check_and_checkout() {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());
    init_repo(&sb.repo);
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &sb.fixture);
        hat.check().unwrap();
    }

    // Replace the 32-byte universal key with a different 32-byte key.
    let key_path = sb.repo.join("secret-universal-key");
    let mut key = fs::read(&key_path).unwrap();
    key[0] ^= 0xFF;
    fs::write(&key_path, &key).unwrap();

    let mut hat = open_hat(&sb.repo, &sb.backend);
    assert!(hat.check().is_err(), "check with wrong key must fail");
    fs::create_dir_all(&sb.restore).unwrap();
    assert!(
        hat.checkout_in_dir("family".into(), sb.restore.clone()).is_err(),
        "checkout with wrong key must return Err"
    );
}

#[test]
fn open_repository_rejects_all_non_32_byte_keys() {
    use hat::backend::FileBackend;
    use hat::Hat;
    use std::sync::Arc;

    let sb = sandbox();
    init_repo(&sb.repo);
    let key_path = sb.repo.join("secret-universal-key");

    for bad_len in [0usize, 1, 16, 31, 33, 64, 128] {
        fs::write(&key_path, vec![0x42u8; bad_len]).unwrap();
        let backend = Arc::new(FileBackend::new(sb.backend.clone()));
        assert!(
            Hat::open_repository(sb.repo.clone(), backend, MAX_BLOB_SIZE).is_err(),
            "open_repository must reject {}-byte key",
            bad_len
        );
    }
}

#[test]
fn real_backend_rollback_replay_detected() {
    let sb = sandbox();
    init_repo(&sb.repo);

    let fix1 = sb.root.path().join("fix1");
    let fix2 = sb.root.path().join("fix2");
    fs::create_dir_all(&fix1).unwrap();
    fs::create_dir_all(&fix2).unwrap();
    write_fixture(&fix1, &[("v1.txt", FileSpec::file(b"state-1".to_vec()))]);
    write_fixture(&fix2, &[("v2.txt", FileSpec::file(b"state-2".to_vec()))]);

    let root_v1 = {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &fix1);
        hat.list_snapshots()
            .into_iter()
            .filter(|s| s.family_name == "__hat__roots__")
            .max_by_key(|s| s.info.snapshot_id)
            .and_then(|s| s.hash)
            .unwrap()
            .bytes
    };

    // Snapshot the exact backend state at v1.
    let v1_backend_copy: Vec<(std::ffi::OsString, Vec<u8>)> = backend_files(&sb.backend)
        .into_iter()
        .map(|p| (p.file_name().unwrap().to_owned(), fs::read(&p).unwrap()))
        .collect();

    let root_v2 = {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &fix2);
        hat.list_snapshots()
            .into_iter()
            .filter(|s| s.family_name == "__hat__roots__")
            .max_by_key(|s| s.info.snapshot_id)
            .and_then(|s| s.hash)
            .unwrap()
            .bytes
    };
    assert_ne!(root_v1, root_v2);

    // Malicious backend rolls back storage to the real, valid v1 state.
    for p in backend_files(&sb.backend) {
        fs::remove_file(p).unwrap();
    }
    for (name, bytes) in &v1_backend_copy {
        fs::write(sb.backend.join(name), bytes).unwrap();
    }

    // Client expecting v2 root must reject the replayed v1 backend.
    wipe_cache(&sb.repo);
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        assert!(
            hat.recover_with_expected_root(Some(&root_v2)).is_err(),
            "replayed older valid backend must fail pinned v2 root check"
        );
    }

    // Client expecting v1 root recovers v1 cleanly.
    wipe_cache(&sb.repo);
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        hat.recover_with_expected_root(Some(&root_v1)).unwrap();
        let leaf = checkout_and_leaf(&mut hat, "family", &fix1, &sb.restore);
        assert_tree_eq(&fix1, &leaf);
    }
}

#[test]
fn recover_then_commit_and_gc_lifecycle() {
    let sb = sandbox();
    init_repo(&sb.repo);

    let fix1 = sb.root.path().join("fix1");
    let fix2 = sb.root.path().join("fix2");
    fs::create_dir_all(&fix1).unwrap();
    fs::create_dir_all(&fix2).unwrap();
    write_fixture(&fix1, &[("v1.bin", FileSpec::file(random_bytes_seeded(11, 200 * 1024)))]);
    write_fixture(&fix2, &[("v2.bin", FileSpec::file(random_bytes_seeded(12, 200 * 1024)))]);

    let snap1 = {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &fix1);
        hat.latest_snapshot_id("family").unwrap()
    };

    wipe_cache(&sb.repo);
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        hat.recover().unwrap();
        hat.check().unwrap();

        // Commit a brand-new snapshot on the SAME Hat handle right after recover()
        // (regression test for stale blob_id_counter / pre-reserved blob_desc collision).
        commit_tree(&mut hat, "family", &fix2);
        let snap2 = hat.latest_snapshot_id("family").unwrap();
        assert!(snap2 > snap1);

        hat.deregister_by_name("family".into(), snap1).unwrap();
        let (deleted, live) = hat.gc().unwrap();
        assert!(deleted > 0);
        assert!(live > 0);
        hat.check().unwrap();

        let leaf = checkout_and_leaf(&mut hat, "family", &fix2, &sb.restore);
        assert_tree_eq(&fix2, &leaf);
    }
}

#[test]
fn recover_multi_family_and_multi_snapshot() {
    let sb = sandbox();
    init_repo(&sb.repo);

    let fix_a1 = sb.root.path().join("fix_a1");
    let fix_a2 = sb.root.path().join("fix_a2");
    let fix_b1 = sb.root.path().join("fix_b1");
    fs::create_dir_all(&fix_a1).unwrap();
    fs::create_dir_all(&fix_a2).unwrap();
    fs::create_dir_all(&fix_b1).unwrap();
    write_fixture(&fix_a1, &[("a1.txt", FileSpec::file(b"a-one".to_vec()))]);
    write_fixture(&fix_a2, &[("a2.txt", FileSpec::file(b"a-two".to_vec()))]);
    write_fixture(&fix_b1, &[("b1.txt", FileSpec::file(b"b-one".to_vec()))]);

    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family_a", &fix_a1);
        commit_tree(&mut hat, "family_a", &fix_a2);
        commit_tree(&mut hat, "family_b", &fix_b1);
    }

    wipe_cache(&sb.repo);
    let mut hat = open_hat(&sb.repo, &sb.backend);
    hat.recover().unwrap();
    hat.check().unwrap();

    let out_a = sb.root.path().join("out_a");
    let out_b = sb.root.path().join("out_b");
    let leaf_a = checkout_and_leaf(&mut hat, "family_a", &fix_a2, &out_a);
    let leaf_b = checkout_and_leaf(&mut hat, "family_b", &fix_b1, &out_b);
    assert_tree_eq(&fix_a2, &leaf_a);
    assert_tree_eq(&fix_b1, &leaf_b);
}

#[test]
fn corrupt_local_sqlite_cache_returns_err() {
    use hat::backend::FileBackend;
    use hat::Hat;
    use std::sync::Arc;

    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());
    init_repo(&sb.repo);
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &sb.fixture);
    }

    let db_path = sb.repo.join("cache").join("index.sqlite3");
    fs::write(&db_path, b"not a valid sqlite database file at all!!!").unwrap();

    let backend = Arc::new(FileBackend::new(sb.backend.clone()));
    assert!(
        Hat::open_repository(sb.repo.clone(), backend, MAX_BLOB_SIZE).is_err(),
        "corrupt local sqlite index must fail open_repository"
    );
}
