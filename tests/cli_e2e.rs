mod common;

use assert_cmd::Command;
use common::*;
use std::fs;

fn hatbin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_hatbin"))
}

fn with_file_backend<'a>(cmd: &'a mut Command, sb: &Sandbox) -> &'a mut Command {
    cmd.env("HAT_STATE_DIR", &sb.repo)
        .env("HAT_BACKEND", "file")
        .env("HAT_BACKEND_PATH", &sb.backend)
}

#[test]
fn cli_init_commit_checkout() {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());

    hatbin()
        .args(["init", sb.repo.to_str().unwrap()])
        .assert()
        .success();

    with_file_backend(&mut hatbin(), &sb)
        .args(["commit", "family", sb.fixture.to_str().unwrap()])
        .assert()
        .success();

    with_file_backend(&mut hatbin(), &sb)
        .args(["checkout", "family", sb.restore.to_str().unwrap()])
        .assert()
        .success();

    assert_tree_eq(&sb.fixture, &restored_tree_path(&sb.fixture, &sb.restore));

    with_file_backend(&mut hatbin(), &sb)
        .arg("check")
        .assert()
        .success();
}

#[test]
fn cli_gc_after_delete() {
    let sb = sandbox();
    init_repo(&sb.repo);

    let fix1 = sb.root.path().join("fix1");
    let fix2 = sb.root.path().join("fix2");
    fs::create_dir_all(&fix1).unwrap();
    fs::create_dir_all(&fix2).unwrap();
    write_fixture(&fix1, &[("a.txt", FileSpec::file(b"a".to_vec()))]);
    write_fixture(&fix2, &[("b.txt", FileSpec::file(b"b".to_vec()))]);

    with_file_backend(&mut hatbin(), &sb)
        .args(["commit", "family", fix1.to_str().unwrap()])
        .assert()
        .success();

    let snap1 = {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        hat.latest_snapshot_id("family").unwrap()
    };

    with_file_backend(&mut hatbin(), &sb)
        .args(["commit", "family", fix2.to_str().unwrap()])
        .assert()
        .success();

    with_file_backend(&mut hatbin(), &sb)
        .args(["delete", "family", &snap1.to_string()])
        .assert()
        .success();

    with_file_backend(&mut hatbin(), &sb)
        .arg("gc")
        .assert()
        .success();

    let restore = sb.root.path().join("out");
    fs::create_dir_all(&restore).unwrap();
    with_file_backend(&mut hatbin(), &sb)
        .args(["checkout", "family", restore.to_str().unwrap()])
        .assert()
        .success();
    assert_tree_eq(&fix2, &restored_tree_path(&fix2, &restore));
}

#[test]
fn cli_resume_after_kill() {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());
    init_repo(&sb.repo);

    let barrier = sb.root.path().join("barrier");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_hat-crash-worker"))
        .args([
            "commit-before-meta",
            sb.repo.to_str().unwrap(),
            sb.backend.to_str().unwrap(),
            "family",
            sb.fixture.to_str().unwrap(),
            barrier.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let start = std::time::Instant::now();
    while !barrier.exists() {
        assert!(start.elapsed() < std::time::Duration::from_secs(60));
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();

    with_file_backend(&mut hatbin(), &sb)
        .arg("resume")
        .assert()
        .success();

    with_file_backend(&mut hatbin(), &sb)
        .arg("check")
        .assert()
        .success();

    with_file_backend(&mut hatbin(), &sb)
        .args(["checkout", "family", sb.restore.to_str().unwrap()])
        .assert()
        .success();

    assert_tree_eq(&sb.fixture, &restored_tree_path(&sb.fixture, &sb.restore));

    // Verify `hatbin resume` also published meta_commit() so the resumed snapshot survives cache wipe.
    for entry in fs::read_dir(sb.repo.join("cache")).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            fs::remove_dir_all(&path).unwrap();
        } else {
            fs::remove_file(&path).unwrap();
        }
    }
    let restore2 = sb.root.path().join("restore_after_recover");
    fs::create_dir_all(&restore2).unwrap();
    with_file_backend(&mut hatbin(), &sb)
        .arg("recover")
        .assert()
        .success();
    with_file_backend(&mut hatbin(), &sb)
        .args(["checkout", "family", restore2.to_str().unwrap()])
        .assert()
        .success();
    assert_tree_eq(&sb.fixture, &restored_tree_path(&sb.fixture, &restore2));
}

#[test]
fn cli_init_sets_restrictive_permissions_and_refuses_overwrite() {
    let sb = sandbox();

    hatbin()
        .args(["init", sb.repo.to_str().unwrap()])
        .assert()
        .success();

    let key_path = sb.repo.join("secret-universal-key");
    let orig_key = fs::read(&key_path).unwrap();
    assert_eq!(orig_key.len(), 32);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&sb.repo).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(sb.repo.join("cache")).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // Re-running init on the same directory must fail and keep original key untouched.
    hatbin()
        .args(["init", sb.repo.to_str().unwrap()])
        .assert()
        .failure();
    assert_eq!(fs::read(&key_path).unwrap(), orig_key);

    // Committing multiple families uses a single SQLite cache file (`index.sqlite3` + WAL/SHM);
    // every file inside cache/ must be 0600 and cache/ must remain 0700.
    write_fixture(&sb.fixture, &[("hello.txt", FileSpec::file(b"secret-ish".to_vec()))]);
    for fam in ["family", "family2"] {
        let mut cmd = hatbin();
        with_file_backend(&mut cmd, &sb)
            .args(["commit", fam, sb.fixture.to_str().unwrap()])
            .assert()
            .success();
    }

    let cache = sb.repo.join("cache");
    assert!(cache.join("index.sqlite3").is_file(), "expected single cache/index.sqlite3");
    let sqlite_files: Vec<_> = fs::read_dir(&cache)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("sqlite3"))
        .collect();
    assert_eq!(
        sqlite_files.len(),
        1,
        "expected exactly one .sqlite3 cache file across multiple families, found: {:?}",
        sqlite_files
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&cache).unwrap().permissions().mode() & 0o777, 0o700);
        for entry in fs::read_dir(&cache).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                assert_eq!(
                    mode,
                    0o600,
                    "cache file {} must have mode 0600, got {:o}",
                    path.display(),
                    mode
                );
            }
        }
    }
}

#[test]
fn cli_recover_and_checkout_after_cache_wipe() {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());

    hatbin()
        .args(["init", sb.repo.to_str().unwrap()])
        .assert()
        .success();

    with_file_backend(&mut hatbin(), &sb)
        .args(["commit", "family", sb.fixture.to_str().unwrap()])
        .assert()
        .success();

    // Wipe local cache while keeping universal key and backend.
    for entry in fs::read_dir(sb.repo.join("cache")).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            fs::remove_dir_all(&path).unwrap();
        } else {
            fs::remove_file(&path).unwrap();
        }
    }

    with_file_backend(&mut hatbin(), &sb)
        .arg("recover")
        .assert()
        .success();

    with_file_backend(&mut hatbin(), &sb)
        .arg("check")
        .assert()
        .success();

    with_file_backend(&mut hatbin(), &sb)
        .args(["checkout", "family", sb.restore.to_str().unwrap()])
        .assert()
        .success();

    assert_tree_eq(&sb.fixture, &restored_tree_path(&sb.fixture, &sb.restore));
}

#[test]
fn cli_ls_lists_families_and_snapshots() {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());

    hatbin()
        .args(["init", sb.repo.to_str().unwrap()])
        .assert()
        .success();

    with_file_backend(&mut hatbin(), &sb)
        .args(["commit", "alpha", sb.fixture.to_str().unwrap()])
        .assert()
        .success();

    // `ls` with no path argument defaults to `/`.
    let out_default = with_file_backend(&mut hatbin(), &sb)
        .arg("ls")
        .output()
        .unwrap();
    assert!(out_default.status.success());
    let default_stdout = String::from_utf8_lossy(&out_default.stdout);
    assert!(
        default_stdout.lines().any(|l| l.trim() == "alpha"),
        "expected 'alpha' in bare ls output: {:?}",
        default_stdout
    );

    let out_root = with_file_backend(&mut hatbin(), &sb)
        .args(["ls", "/"])
        .output()
        .unwrap();
    assert!(out_root.status.success());
    let root_stdout = String::from_utf8_lossy(&out_root.stdout);
    assert_eq!(default_stdout, root_stdout);
    assert!(
        root_stdout.lines().any(|l| l.trim() == "alpha"),
        "expected 'alpha' in ls / output: {:?}",
        root_stdout
    );

    let out_fam = with_file_backend(&mut hatbin(), &sb)
        .args(["ls", "alpha"])
        .output()
        .unwrap();
    assert!(out_fam.status.success());
    let fam_stdout = String::from_utf8_lossy(&out_fam.stdout);
    assert!(
        fam_stdout.lines().any(|l| l.starts_with("alpha/")),
        "expected 'alpha/<id>' in ls alpha output: {:?}",
        fam_stdout
    );

    // Leading `/` must resolve into the family / snapshot / subdir rather than falling back to root.
    let out_slash_fam = with_file_backend(&mut hatbin(), &sb)
        .args(["ls", "/alpha"])
        .output()
        .unwrap();
    assert!(out_slash_fam.status.success());
    let slash_fam_stdout = String::from_utf8_lossy(&out_slash_fam.stdout);
    assert!(
        slash_fam_stdout.lines().any(|l| l.trim() == "alpha/1"),
        "expected 'alpha/1' in ls /alpha output: {:?}",
        slash_fam_stdout
    );
    assert!(
        !slash_fam_stdout.lines().any(|l| l.trim() == "__hat__roots__"),
        "ls /alpha must not return root families: {:?}",
        slash_fam_stdout
    );

    let out_snap_root = with_file_backend(&mut hatbin(), &sb)
        .args(["ls", "/alpha/1"])
        .output()
        .unwrap();
    assert!(out_snap_root.status.success());
    let snap_root_stdout = String::from_utf8_lossy(&out_snap_root.stdout);
    assert!(
        snap_root_stdout
            .lines()
            .any(|l| l.trim() == "/alpha/1/readme.txt"),
        "expected '/alpha/1/readme.txt' in ls /alpha/1 output: {:?}",
        snap_root_stdout
    );

    let out_subdir = with_file_backend(&mut hatbin(), &sb)
        .args(["ls", "/alpha/1/nested"])
        .output()
        .unwrap();
    assert!(out_subdir.status.success());
    let subdir_stdout = String::from_utf8_lossy(&out_subdir.stdout);
    assert!(
        subdir_stdout
            .lines()
            .any(|l| l.trim() == "/alpha/1/nested/a.txt"),
        "expected '/alpha/1/nested/a.txt' in ls /alpha/1/nested output: {:?}",
        subdir_stdout
    );

    // Invalid snapshot ID under a leading-slash path must NOT return root families.
    let out_bad = with_file_backend(&mut hatbin(), &sb)
        .args(["ls", "/alpha/alpha/"])
        .output()
        .unwrap();
    assert!(out_bad.status.success());
    assert!(
        String::from_utf8_lossy(&out_bad.stdout).trim().is_empty(),
        "ls /alpha/alpha/ must produce empty output, got: {:?}",
        String::from_utf8_lossy(&out_bad.stdout)
    );
}

#[test]
fn cli_fails_nonzero_on_bad_usage_and_corrupt_backend() {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());

    // Missing HAT_STATE_DIR fails non-zero.
    hatbin()
        .env_remove("HAT_STATE_DIR")
        .arg("check")
        .assert()
        .failure();

    hatbin()
        .args(["init", sb.repo.to_str().unwrap()])
        .assert()
        .success();

    // Missing HAT_BACKEND_PATH with file backend fails non-zero.
    hatbin()
        .env("HAT_STATE_DIR", &sb.repo)
        .env("HAT_BACKEND", "file")
        .env_remove("HAT_BACKEND_PATH")
        .arg("check")
        .assert()
        .failure();

    // Checkout of nonexistent family fails non-zero.
    with_file_backend(&mut hatbin(), &sb)
        .args(["checkout", "no_such_family", sb.restore.to_str().unwrap()])
        .assert()
        .failure();

    // Commit, corrupt a blob, verify `hatbin check` exits non-zero.
    with_file_backend(&mut hatbin(), &sb)
        .args(["commit", "family", sb.fixture.to_str().unwrap()])
        .assert()
        .success();

    let blobs = backend_files(&sb.backend);
    assert!(!blobs.is_empty());
    let mut data = fs::read(&blobs[0]).unwrap();
    data[0] ^= 0xFF;
    fs::write(&blobs[0], &data).unwrap();

    with_file_backend(&mut hatbin(), &sb)
        .arg("check")
        .assert()
        .failure();
}

#[test]
fn cli_gc_pretend_does_not_delete_blobs() {
    let sb = sandbox();
    init_repo(&sb.repo);

    let fix1 = sb.root.path().join("fix1");
    let fix2 = sb.root.path().join("fix2");
    fs::create_dir_all(&fix1).unwrap();
    fs::create_dir_all(&fix2).unwrap();
    write_fixture(
        &fix1,
        &[("only1.bin", FileSpec::file(random_bytes_seeded(51, 1500 * 1024)))],
    );
    write_fixture(
        &fix2,
        &[("only2.bin", FileSpec::file(random_bytes_seeded(52, 1500 * 1024)))],
    );

    with_file_backend(&mut hatbin(), &sb)
        .args(["commit", "family", fix1.to_str().unwrap()])
        .assert()
        .success();

    let snap1 = {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        hat.latest_snapshot_id("family").unwrap()
    };

    with_file_backend(&mut hatbin(), &sb)
        .args(["commit", "family", fix2.to_str().unwrap()])
        .assert()
        .success();

    with_file_backend(&mut hatbin(), &sb)
        .args(["delete", "family", &snap1.to_string()])
        .assert()
        .success();

    let blobs_before = backend_files(&sb.backend);
    let bytes_before = backend_total_bytes(&sb.backend);

    // Dry-run gc must NOT remove any backend blobs or hashes.
    let pretend_out = with_file_backend(&mut hatbin(), &sb)
        .args(["gc", "--pretend"])
        .output()
        .unwrap();
    assert!(pretend_out.status.success());
    let stdout = String::from_utf8_lossy(&pretend_out.stdout);
    assert!(
        stdout.contains("Would delete hashes:") && !stdout.contains("Would delete hashes: 0"),
        "expected non-zero pretend delete count, got: {:?}",
        stdout
    );
    assert_eq!(
        backend_files(&sb.backend),
        blobs_before,
        "gc --pretend must not modify backend blobs"
    );
    assert_eq!(backend_total_bytes(&sb.backend), bytes_before);

    // Real gc afterwards actually reclaims the unused blobs.
    with_file_backend(&mut hatbin(), &sb)
        .arg("gc")
        .assert()
        .success();
    assert!(
        backend_files(&sb.backend).len() < blobs_before.len(),
        "real gc must reclaim blobs left untouched by gc --pretend"
    );

    with_file_backend(&mut hatbin(), &sb)
        .arg("check")
        .assert()
        .success();
    with_file_backend(&mut hatbin(), &sb)
        .args(["checkout", "family", sb.restore.to_str().unwrap()])
        .assert()
        .success();
    assert_tree_eq(&fix2, &restored_tree_path(&fix2, &sb.restore));
}

#[test]
fn cli_delete_gc_then_recover_after_cache_wipe() {
    let sb = sandbox();
    init_repo(&sb.repo);

    let fix1 = sb.root.path().join("fix1");
    let fix2 = sb.root.path().join("fix2");
    fs::create_dir_all(&fix1).unwrap();
    fs::create_dir_all(&fix2).unwrap();
    write_fixture(&fix1, &[("a.txt", FileSpec::file(b"snap1-only".to_vec()))]);
    write_fixture(&fix2, &[("b.txt", FileSpec::file(b"snap2-only".to_vec()))]);

    with_file_backend(&mut hatbin(), &sb)
        .args(["commit", "family", fix1.to_str().unwrap()])
        .assert()
        .success();

    let snap1 = {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        hat.latest_snapshot_id("family").unwrap()
    };

    with_file_backend(&mut hatbin(), &sb)
        .args(["commit", "family", fix2.to_str().unwrap()])
        .assert()
        .success();

    with_file_backend(&mut hatbin(), &sb)
        .args(["delete", "family", &snap1.to_string()])
        .assert()
        .success();

    with_file_backend(&mut hatbin(), &sb)
        .arg("gc")
        .assert()
        .success();

    // Wipe local cache after delete+gc; recover must succeed using the updated root listing.
    for entry in fs::read_dir(sb.repo.join("cache")).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            fs::remove_dir_all(&path).unwrap();
        } else {
            fs::remove_file(&path).unwrap();
        }
    }

    with_file_backend(&mut hatbin(), &sb)
        .arg("recover")
        .assert()
        .success();
    with_file_backend(&mut hatbin(), &sb)
        .arg("check")
        .assert()
        .success();
    with_file_backend(&mut hatbin(), &sb)
        .args(["checkout", "family", sb.restore.to_str().unwrap()])
        .assert()
        .success();
    assert_tree_eq(&fix2, &restored_tree_path(&fix2, &sb.restore));
}

#[test]
fn cli_checkout_by_snapshot_id_and_clean_error_exits() {
    let sb = sandbox();
    init_repo(&sb.repo);

    let fix1 = sb.root.path().join("fix1");
    let fix2 = sb.root.path().join("fix2");
    fs::create_dir_all(&fix1).unwrap();
    fs::create_dir_all(&fix2).unwrap();
    write_fixture(&fix1, &[("v.txt", FileSpec::file(b"version-one".to_vec()))]);
    write_fixture(&fix2, &[("v.txt", FileSpec::file(b"version-two".to_vec()))]);

    with_file_backend(&mut hatbin(), &sb)
        .args(["commit", "family", fix1.to_str().unwrap()])
        .assert()
        .success();
    let snap1 = {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        hat.latest_snapshot_id("family").unwrap()
    };

    with_file_backend(&mut hatbin(), &sb)
        .args(["commit", "family", fix2.to_str().unwrap()])
        .assert()
        .success();

    let restore_v1 = sb.root.path().join("restore_v1");
    fs::create_dir_all(&restore_v1).unwrap();
    with_file_backend(&mut hatbin(), &sb)
        .args([
            "checkout",
            "family",
            restore_v1.to_str().unwrap(),
            "--id",
            &snap1.to_string(),
        ])
        .assert()
        .success();
    assert_tree_eq(&fix1, &restored_tree_path(&fix1, &restore_v1));

    // Default checkout (without --id) still restores latest (v2).
    with_file_backend(&mut hatbin(), &sb)
        .args(["checkout", "family", sb.restore.to_str().unwrap()])
        .assert()
        .success();
    assert_tree_eq(&fix2, &restored_tree_path(&fix2, &sb.restore));

    // Errors exit non-zero cleanly (no Rust panic backtrace).
    let bad_checkout = with_file_backend(&mut hatbin(), &sb)
        .args([
            "checkout",
            "family",
            sb.root.path().join("restore_bad").to_str().unwrap(),
            "--id",
            "999999",
        ])
        .output()
        .unwrap();
    assert!(!bad_checkout.status.success());
    let stderr = String::from_utf8_lossy(&bad_checkout.stderr);
    assert!(
        stderr.contains("Error:") && !stderr.contains("panicked at"),
        "expected clean Error message without panic, got: {:?}",
        stderr
    );
}

#[test]
fn cli_fails_fast_when_repository_is_locked() {
    let sb = sandbox();
    init_repo(&sb.repo);

    // Hold the repository lock in this process while invoking hatbin in a child process.
    let held = open_hat(&sb.repo, &sb.backend);

    let out = with_file_backend(&mut hatbin(), &sb)
        .arg("check")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "child hatbin must fail fast when another process holds the repo lock"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.to_lowercase().contains("locked") && !stderr.contains("panicked at"),
        "expected clean lock error without panic, got: {:?}",
        stderr
    );

    drop(held);

    // Once released, hatbin succeeds.
    with_file_backend(&mut hatbin(), &sb)
        .arg("check")
        .assert()
        .success();
}

#[test]
fn cli_checkout_by_snapshot_path_and_subpath() {
    let sb = sandbox();
    hatbin()
        .args(["init", sb.repo.to_str().unwrap()])
        .assert()
        .success();

    let fix1 = sb.root.path().join("fix1");
    let fix2 = sb.root.path().join("fix2");
    fs::create_dir_all(&fix1).unwrap();
    fs::create_dir_all(&fix2).unwrap();
    write_fixture(
        &fix1,
        &[
            ("top.txt", FileSpec::file(b"v1-top".to_vec())),
            ("sub/a.txt", FileSpec::file(b"v1-a".to_vec())),
            ("sub/deep/b.txt", FileSpec::file(b"v1-b".to_vec())),
        ],
    );
    write_fixture(
        &fix2,
        &[
            ("top.txt", FileSpec::file(b"v2-top".to_vec())),
            ("sub/a.txt", FileSpec::file(b"v2-a".to_vec())),
            ("sub/deep/b.txt", FileSpec::file(b"v2-b".to_vec())),
        ],
    );

    with_file_backend(&mut hatbin(), &sb)
        .args(["commit", "myfam", fix1.to_str().unwrap()])
        .assert()
        .success();
    with_file_backend(&mut hatbin(), &sb)
        .args(["commit", "myfam", fix2.to_str().unwrap()])
        .assert()
        .success();

    // 1. Checkout snapshot 1 via `myfam/1` (and with leading `/`).
    let r_snap1 = sb.root.path().join("r_snap1");
    with_file_backend(&mut hatbin(), &sb)
        .args(["checkout", "/myfam/1", r_snap1.to_str().unwrap()])
        .assert()
        .success();
    assert_tree_eq(&fix1, &r_snap1);

    // 2. Checkout a subdirectory via `myfam/1/sub` into a local destination dir.
    let r_sub1 = sb.root.path().join("r_sub1");
    with_file_backend(&mut hatbin(), &sb)
        .args(["checkout", "myfam/1/sub", r_sub1.to_str().unwrap()])
        .assert()
        .success();
    assert_eq!(fs::read(r_sub1.join("a.txt")).unwrap(), b"v1-a");
    assert_eq!(fs::read(r_sub1.join("deep/b.txt")).unwrap(), b"v1-b");
    assert!(!r_sub1.join("top.txt").exists());

    // 3. Checkout a subdirectory from latest via `myfam/latest/sub/deep`.
    let r_latest_deep = sb.root.path().join("r_latest_deep");
    with_file_backend(&mut hatbin(), &sb)
        .args([
            "checkout",
            "myfam/latest/sub/deep",
            r_latest_deep.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert_eq!(fs::read(r_latest_deep.join("b.txt")).unwrap(), b"v2-b");

    // 4. Checkout a single file via `myfam/1/sub/a.txt`.
    let r_single = sb.root.path().join("r_single");
    with_file_backend(&mut hatbin(), &sb)
        .args(["checkout", "myfam/1/sub/a.txt", r_single.to_str().unwrap()])
        .assert()
        .success();
    assert_eq!(fs::read(r_single.join("a.txt")).unwrap(), b"v1-a");
    assert!(!r_single.join("deep").exists());

    // 5. Usage text clearly names SNAPSHOT_PATH and DEST_DIR when args are missing.
    let missing = hatbin().args(["checkout", "myfam"]).output().unwrap();
    assert!(!missing.status.success());
    let missing_stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(
        missing_stderr.contains("<DEST_DIR>") && missing_stderr.contains("<SNAPSHOT_PATH>"),
        "expected <SNAPSHOT_PATH> and <DEST_DIR> in usage, got: {:?}",
        missing_stderr
    );

    // 6. Conflicting snapshot IDs, bad IDs, and nonexistent subpaths fail cleanly.
    let r_err = sb.root.path().join("r_err");
    for args in [
        vec!["checkout", "myfam/1", r_err.to_str().unwrap(), "--id", "1"],
        vec!["checkout", "myfam/notanid", r_err.to_str().unwrap()],
        vec!["checkout", "myfam/1/missing", r_err.to_str().unwrap()],
        vec!["checkout", "/", r_err.to_str().unwrap()],
    ] {
        let out = with_file_backend(&mut hatbin(), &sb)
            .args(args)
            .output()
            .unwrap();
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("Error:") && !stderr.contains("panicked at"),
            "expected clean Error without panic, got: {:?}",
            stderr
        );
    }
}
