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
