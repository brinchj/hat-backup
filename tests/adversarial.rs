mod common;

use common::*;
use std::fs;
use std::io::Write;

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

    let mut blobs: Vec<_> = fs::read_dir(&sb.backend)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .collect();
    blobs.sort();
    assert!(!blobs.is_empty(), "expected backend blobs");
    // Truncate the largest blob to maximize chance of hitting live data.
    let victim = blobs
        .iter()
        .max_by_key(|p| fs::metadata(p).unwrap().len())
        .unwrap();
    let mut f = fs::OpenOptions::new()
        .write(true)
        .open(victim)
        .unwrap();
    f.set_len(16).unwrap();
    f.flush().unwrap();

    let mut hat = open_hat(&sb.repo, &sb.backend);
    let check = hat.check();
    // Checkout may return Err or panic on corrupt ciphertext; both are safe (not silent Ok).
    let checkout = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fs::create_dir_all(&sb.restore).ok();
        hat.checkout_in_dir("family".into(), sb.restore.clone())
    }));
    let checkout_failed = match &checkout {
        Err(_) => true,
        Ok(Err(_)) => true,
        Ok(Ok(())) => false,
    };
    assert!(
        check.is_err() || checkout_failed,
        "truncated blob must not look fully healthy (check={:?}, checkout={:?})",
        check,
        checkout
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
    let cache = sb.repo.join("cache");
    for entry in fs::read_dir(&cache).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            fs::remove_file(&path).unwrap();
        } else if path.is_dir() {
            fs::remove_dir_all(&path).unwrap();
        }
    }

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
    let cache = sb.repo.join("cache");
    for entry in fs::read_dir(&cache).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            fs::remove_file(&path).unwrap();
        } else if path.is_dir() {
            fs::remove_dir_all(&path).unwrap();
        }
    }

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
