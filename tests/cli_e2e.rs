mod common;

use assert_cmd::Command;
use common::*;
use std::fs;

fn hatbin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_hat"))
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
    assert!(
        !root_stdout.lines().any(|l| l.contains("__hat__roots__")),
        "ls / must not expose internal __hat__roots__ family: {:?}",
        root_stdout
    );

    // Direct access to internal __hat__roots__ must fail cleanly.
    with_file_backend(&mut hatbin(), &sb)
        .args(["ls", "__hat__roots__"])
        .assert()
        .failure();
    with_file_backend(&mut hatbin(), &sb)
        .args(["checkout", "__hat__roots__", sb.restore.to_str().unwrap()])
        .assert()
        .failure();
    with_file_backend(&mut hatbin(), &sb)
        .args(["delete", "__hat__roots__/1"])
        .assert()
        .failure();
    with_file_backend(&mut hatbin(), &sb)
        .args(["commit", "__hat__roots__", sb.fixture.to_str().unwrap()])
        .assert()
        .failure();

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
        slash_fam_stdout
            .lines()
            .any(|l| l.trim().starts_with("alpha/") && l.trim().ends_with("Z-1")),
        "expected 'alpha/<ts>Z-1' in ls /alpha output: {:?}",
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

    // Invalid snapshot ID under a leading-slash path must fail non-zero and NOT return root families.
    let out_bad = with_file_backend(&mut hatbin(), &sb)
        .args(["ls", "/alpha/alpha/"])
        .output()
        .unwrap();
    assert!(!out_bad.status.success());
    assert!(
        String::from_utf8_lossy(&out_bad.stdout).trim().is_empty(),
        "ls /alpha/alpha/ must produce empty stdout, got: {:?}",
        String::from_utf8_lossy(&out_bad.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out_bad.stderr).contains("path not found"),
        "expected path not found error on stderr, got: {:?}",
        String::from_utf8_lossy(&out_bad.stderr)
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
    let dry_run_out = with_file_backend(&mut hatbin(), &sb)
        .args(["gc", "--dry-run"])
        .output()
        .unwrap();
    assert!(dry_run_out.status.success());
    let stdout = String::from_utf8_lossy(&dry_run_out.stdout);
    assert!(
        stdout.contains("Would delete ") && !stdout.contains("Would delete 0 "),
        "expected non-zero dry-run delete count, got: {:?}",
        stdout
    );
    assert_eq!(
        backend_files(&sb.backend),
        blobs_before,
        "gc --dry-run must not modify backend blobs"
    );
    assert_eq!(backend_total_bytes(&sb.backend), bytes_before);

    // Real gc afterwards actually reclaims the unused blobs.
    with_file_backend(&mut hatbin(), &sb)
        .arg("gc")
        .assert()
        .success();
    assert!(
        backend_files(&sb.backend).len() < blobs_before.len(),
        "real gc must reclaim blobs left untouched by gc --dry-run"
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

#[test]
fn cli_ls_long_shows_timestamps_and_metadata() {
    let sb = sandbox();
    write_fixture(
        &sb.fixture,
        &[
            ("pinned.txt", FileSpec::file_mode(b"hello world", 0o640)),
            ("sub/inner.txt", FileSpec::file(b"x")),
            ("link", FileSpec::Symlink {
                target: std::path::PathBuf::from("pinned.txt"),
            }),
        ],
    );
    // Set deterministic mtime = 1_700_000_000 -> 2023-11-14 22:13:20Z.
    hat::util::set_times_nofollow(&sb.fixture.join("pinned.txt"), 1_700_000_000, 1_700_000_000)
        .unwrap();

    hatbin()
        .args(["init", sb.repo.to_str().unwrap()])
        .assert()
        .success();

    with_file_backend(&mut hatbin(), &sb)
        .args(["commit", "alpha", sb.fixture.to_str().unwrap()])
        .assert()
        .success();

    // 1. `ls -l` (defaulting to `/`) shows family with latest snapshot created time.
    let out_root = with_file_backend(&mut hatbin(), &sb)
        .args(["ls", "-l"])
        .output()
        .unwrap();
    assert!(out_root.status.success());
    let root_stdout = String::from_utf8_lossy(&out_root.stdout);
    let alpha_line = root_stdout
        .lines()
        .find(|l| l.ends_with(" alpha"))
        .unwrap_or_else(|| panic!("expected family 'alpha' line in ls -l: {:?}", root_stdout));
    let root_cols: Vec<&str> = alpha_line.split_whitespace().collect();
    assert_eq!(root_cols.len(), 5, "unexpected ls -l root row: {:?}", alpha_line);
    assert_eq!(root_cols[0], "dr-xr-xr-x");
    assert_eq!(root_cols[1], "-");
    assert!(root_cols[2].len() == 10 && root_cols[3].ends_with('Z'));
    assert_eq!(root_cols[4], "alpha");

    // 2. `ls -l alpha` shows snapshot created time on `alpha/<ts>Z-1`.
    let out_snap = with_file_backend(&mut hatbin(), &sb)
        .args(["ls", "-l", "alpha"])
        .output()
        .unwrap();
    assert!(out_snap.status.success());
    let snap_stdout = String::from_utf8_lossy(&out_snap.stdout);
    let snap_line = snap_stdout
        .lines()
        .find(|l| l.ends_with("Z-1"))
        .unwrap_or_else(|| {
            panic!(
                "expected 'alpha/<ts>Z-1' line in ls -l alpha: {:?}",
                snap_stdout
            )
        });
    let snap_cols: Vec<&str> = snap_line.split_whitespace().collect();
    assert_eq!(snap_cols.len(), 5, "unexpected ls -l snap row: {:?}", snap_line);
    assert_eq!(snap_cols[0], "dr-xr-xr-x");
    assert_eq!(snap_cols[1], "-");
    assert_eq!(snap_cols[2], root_cols[2]);
    assert_eq!(snap_cols[3], root_cols[3]);
    assert!(
        snap_cols[4].starts_with("alpha/") && snap_cols[4].ends_with("Z-1"),
        "unexpected composite snapshot path: {:?}",
        snap_cols[4]
    );

    // 3. `ls -l alpha/1` shows file mode, size, exact mtime, dir mode, and symlink target.
    let out_dir = with_file_backend(&mut hatbin(), &sb)
        .args(["ls", "-l", "alpha/1"])
        .output()
        .unwrap();
    assert!(out_dir.status.success());
    let dir_stdout = String::from_utf8_lossy(&out_dir.stdout);

    let file_line = dir_stdout
        .lines()
        .find(|l| l.ends_with(" alpha/1/pinned.txt"))
        .unwrap_or_else(|| panic!("expected pinned.txt in ls -l alpha/1: {:?}", dir_stdout));
    assert!(
        file_line.starts_with("-rw-r----- ")
            && file_line.contains(" 11 ")
            && file_line.contains(" 2023-11-14 22:13:20Z "),
        "expected mode -rw-r-----, size 11, and exact UTC mtime on pinned.txt, got: {:?}",
        file_line
    );

    let sub_line = dir_stdout
        .lines()
        .find(|l| l.ends_with(" alpha/1/sub"))
        .unwrap_or_else(|| panic!("expected sub dir in ls -l alpha/1: {:?}", dir_stdout));
    assert!(
        sub_line.starts_with('d') && sub_line.contains("Z alpha/1/sub"),
        "expected dir mode and timestamp on sub, got: {:?}",
        sub_line
    );

    let link_line = dir_stdout
        .lines()
        .find(|l| l.contains(" alpha/1/link -> pinned.txt"))
        .unwrap_or_else(|| panic!("expected symlink arrow in ls -l alpha/1: {:?}", dir_stdout));
    assert!(
        link_line.starts_with('l') && link_line.contains("Z alpha/1/link -> pinned.txt"),
        "expected symlink mode, timestamp, and target, got: {:?}",
        link_line
    );

    // 4. `license` subcommand prints license text cleanly.
    let lic = hatbin().arg("license").output().unwrap();
    assert!(lic.status.success());
    let lic_stdout = String::from_utf8_lossy(&lic.stdout);
    assert!(lic_stdout.contains("Apache License") || lic_stdout.contains("clap"));
}

#[test]
fn cli_ergonomics_help_global_flags_config_and_delete_dry_run() {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());

    // 1. Bare `hat` (no args) shows help + examples, not `HAT_STATE_DIR required`.
    let no_args = hatbin()
        .env_remove("HAT_STATE_DIR")
        .env_remove("HAT_BACKEND")
        .env_remove("HAT_BACKEND_PATH")
        .output()
        .unwrap();
    assert!(!no_args.status.success());
    let help_err = String::from_utf8_lossy(&no_args.stderr);
    assert!(
        help_err.contains("Usage: hat") && help_err.contains("Examples:"),
        "expected help with usage and examples on bare `hat`, got: {:?}",
        help_err
    );

    // 2. Invalid `--backend` value is rejected immediately by clap.
    let bad_backend = hatbin()
        .args(["check", "--backend", "not-a-backend"])
        .output()
        .unwrap();
    assert!(!bad_backend.status.success());
    let bad_backend_err = String::from_utf8_lossy(&bad_backend.stderr);
    assert!(
        bad_backend_err.contains("possible values") && bad_backend_err.contains("file"),
        "expected clap possible-values error for bad backend, got: {:?}",
        bad_backend_err
    );

    // 3. `init` with backend flags persists `0600` config.json so later commands only need `--state-dir`
    //    and accept global flags *after* the subcommand.
    hatbin()
        .env_remove("HAT_STATE_DIR")
        .env_remove("HAT_BACKEND")
        .env_remove("HAT_BACKEND_PATH")
        .args([
            "init",
            sb.repo.to_str().unwrap(),
            "--backend",
            "file",
            "--backend-path",
            sb.backend.to_str().unwrap(),
        ])
        .assert()
        .success();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let cfg_mode = fs::metadata(sb.repo.join("config.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(cfg_mode, 0o600, "config.json must be 0600");
    }

    // Commit with global `--state-dir` placed *after* the subcommand; stdout stays completely empty.
    let commit_out = hatbin()
        .env_remove("HAT_STATE_DIR")
        .env_remove("HAT_BACKEND")
        .env_remove("HAT_BACKEND_PATH")
        .args([
            "commit",
            "docs",
            sb.fixture.to_str().unwrap(),
            "--state-dir",
            sb.repo.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(commit_out.status.success());
    assert!(
        commit_out.stdout.is_empty(),
        "commit must not pollute stdout, got: {:?}",
        String::from_utf8_lossy(&commit_out.stdout)
    );
    let commit_err = String::from_utf8_lossy(&commit_out.stderr);
    assert!(
        commit_err.contains("Committed docs/") && commit_err.trim().ends_with("Z-1"),
        "expected confirmation on stderr, got: {:?}",
        commit_err
    );

    // `-q` suppresses status confirmation on stderr.
    let quiet_commit = hatbin()
        .env_remove("HAT_STATE_DIR")
        .env_remove("HAT_BACKEND")
        .env_remove("HAT_BACKEND_PATH")
        .args([
            "commit",
            "docs",
            sb.fixture.to_str().unwrap(),
            "--state-dir",
            sb.repo.to_str().unwrap(),
            "-q",
        ])
        .output()
        .unwrap();
    assert!(quiet_commit.status.success());
    assert!(quiet_commit.stdout.is_empty());
    assert!(quiet_commit.stderr.is_empty());

    // `delete <family>/<id> --dry-run` checks existence and does not mutate.
    let del_dry = hatbin()
        .env_remove("HAT_BACKEND")
        .env_remove("HAT_BACKEND_PATH")
        .args([
            "delete",
            "docs/1",
            "--dry-run",
            "--state-dir",
            sb.repo.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(del_dry.status.success());
    let del_dry_out = String::from_utf8_lossy(&del_dry.stdout);
    assert!(
        del_dry_out.contains("Would delete docs/") && del_dry_out.trim().ends_with("Z-1"),
        "expected composite snapshot name in dry-run delete, got: {:?}",
        del_dry_out
    );

    // Snapshot 1 still exists after dry-run.
    hatbin()
        .env_remove("HAT_BACKEND")
        .env_remove("HAT_BACKEND_PATH")
        .args(["ls", "docs/1", "--state-dir", sb.repo.to_str().unwrap()])
        .assert()
        .success();

    // Nonexistent snapshot fails on `--dry-run`.
    hatbin()
        .env_remove("HAT_BACKEND")
        .env_remove("HAT_BACKEND_PATH")
        .args([
            "delete",
            "docs/999",
            "--dry-run",
            "--state-dir",
            sb.repo.to_str().unwrap(),
        ])
        .assert()
        .failure();

    // Real `delete docs/1` succeeds and hints `hat gc`.
    let del_real = hatbin()
        .env_remove("HAT_BACKEND")
        .env_remove("HAT_BACKEND_PATH")
        .args(["delete", "docs/1", "--state-dir", sb.repo.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(del_real.status.success());
    let del_err = String::from_utf8_lossy(&del_real.stderr);
    assert!(
        del_err.contains("Deleted docs/") && del_err.contains("Z-1") && del_err.contains("hat gc"),
        "expected delete confirmation and gc hint on stderr, got: {:?}",
        del_err
    );
}

#[test]
fn cli_json_output_across_commands() {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());

    let init_out = hatbin()
        .args([
            "init",
            sb.repo.to_str().unwrap(),
            "--backend",
            "file",
            "--backend-path",
            sb.backend.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(init_out.status.success());
    let init_json: serde_json::Value = serde_json::from_slice(&init_out.stdout).unwrap();
    assert_eq!(init_json["backend"], "file");

    let commit_out = hatbin()
        .args([
            "commit",
            "docs",
            sb.fixture.to_str().unwrap(),
            "--state-dir",
            sb.repo.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(commit_out.status.success());
    let commit_json: serde_json::Value = serde_json::from_slice(&commit_out.stdout).unwrap();
    assert_eq!(commit_json["family"], "docs");
    assert_eq!(commit_json["snapshot_id"], 1);

    // `ls --json` at root, family, and snapshot dir.
    let ls_root = hatbin()
        .args(["ls", "--json", "--state-dir", sb.repo.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(ls_root.status.success());
    let root_arr: serde_json::Value = serde_json::from_slice(&ls_root.stdout).unwrap();
    let root_items = root_arr.as_array().unwrap();
    assert_eq!(root_items.len(), 1);
    assert_eq!(root_items[0]["kind"], "family");
    assert_eq!(root_items[0]["name"], "docs");

    let ls_fam = hatbin()
        .args([
            "ls",
            "docs",
            "--json",
            "--state-dir",
            sb.repo.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(ls_fam.status.success());
    let fam_arr: serde_json::Value = serde_json::from_slice(&ls_fam.stdout).unwrap();
    assert_eq!(fam_arr[0]["kind"], "snapshot");
    assert_eq!(fam_arr[0]["snapshot_id"], 1);

    let ls_dir = hatbin()
        .args([
            "ls",
            "docs/1",
            "--json",
            "--state-dir",
            sb.repo.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(ls_dir.status.success());
    let dir_arr: serde_json::Value = serde_json::from_slice(&ls_dir.stdout).unwrap();
    assert!(dir_arr
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["kind"] == "file" && v["name"] == "readme.txt"));

    // `check --json`
    let check_out = hatbin()
        .args(["check", "--json", "--state-dir", sb.repo.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(check_out.status.success());
    let check_json: serde_json::Value = serde_json::from_slice(&check_out.stdout).unwrap();
    assert_eq!(check_json["ok"], true);

    // `gc --dry-run --json`
    let gc_out = hatbin()
        .args([
            "gc",
            "-n",
            "--json",
            "--state-dir",
            sb.repo.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(gc_out.status.success());
    let gc_json: serde_json::Value = serde_json::from_slice(&gc_out.stdout).unwrap();
    assert_eq!(gc_json["dry_run"], true);
    assert!(gc_json["live_data_blobs"].as_u64().unwrap() > 0);
}

#[test]
fn cli_ls_recursive_and_latest_and_browse_non_tty() {
    let sb = sandbox();
    write_fixture(
        &sb.fixture,
        &[
            ("top.txt", FileSpec::file(b"top")),
            ("sub/a.txt", FileSpec::file(b"a")),
            ("sub/deep/b.txt", FileSpec::file(b"b")),
        ],
    );

    hatbin()
        .args([
            "init",
            sb.repo.to_str().unwrap(),
            "--backend",
            "file",
            "--backend-path",
            sb.backend.to_str().unwrap(),
        ])
        .assert()
        .success();

    hatbin()
        .args([
            "commit",
            "docs",
            sb.fixture.to_str().unwrap(),
            "--state-dir",
            sb.repo.to_str().unwrap(),
        ])
        .assert()
        .success();

    // `ls --recursive` from `/` walks family -> snapshot -> nested dirs & files without `__hat__roots__`.
    let root_rec = hatbin()
        .args(["ls", "--recursive", "--state-dir", sb.repo.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(root_rec.status.success());
    let root_lines: Vec<&str> = std::str::from_utf8(&root_rec.stdout)
        .unwrap()
        .lines()
        .collect();
    assert_eq!(root_lines.len(), 7, "got: {:?}", root_lines);
    assert_eq!(root_lines[0], "docs");
    let snap_path = root_lines[1];
    assert!(
        snap_path.starts_with("docs/") && snap_path.ends_with("Z-1"),
        "expected composite snapshot path, got: {:?}",
        snap_path
    );
    assert_eq!(
        root_lines,
        vec![
            "docs".to_string(),
            snap_path.to_string(),
            format!("{}/sub", snap_path),
            format!("{}/sub/a.txt", snap_path),
            format!("{}/sub/deep", snap_path),
            format!("{}/sub/deep/b.txt", snap_path),
            format!("{}/top.txt", snap_path),
        ]
    );

    // Composite snapshot name AND unique date prefix work for `ls`, `checkout`, and `delete --dry-run`.
    let date_prefix = &snap_path["docs/".len().."docs/".len() + 10]; // YYYY-MM-DD
    hatbin()
        .args([
            "ls",
            &format!("docs/{}", date_prefix),
            "--state-dir",
            sb.repo.to_str().unwrap(),
        ])
        .assert()
        .success();

    let prefix_restore = sb.root.path().join("prefix_restore");
    hatbin()
        .args([
            "checkout",
            &format!("docs/{}/sub/deep", date_prefix),
            prefix_restore.to_str().unwrap(),
            "--state-dir",
            sb.repo.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert_eq!(fs::read(prefix_restore.join("b.txt")).unwrap(), b"b");

    hatbin()
        .args([
            "delete",
            snap_path,
            "--dry-run",
            "--state-dir",
            sb.repo.to_str().unwrap(),
        ])
        .assert()
        .success();

    // Commit a second snapshot on the same day -> date prefix becomes ambiguous and fails cleanly listing both!
    hatbin()
        .args([
            "commit",
            "docs",
            sb.fixture.to_str().unwrap(),
            "--state-dir",
            sb.repo.to_str().unwrap(),
        ])
        .assert()
        .success();
    let ambig = hatbin()
        .args([
            "checkout",
            &format!("docs/{}", date_prefix),
            sb.restore.to_str().unwrap(),
            "--state-dir",
            sb.repo.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!ambig.status.success());
    let ambig_err = String::from_utf8_lossy(&ambig.stderr);
    assert!(
        ambig_err.contains("Ambiguous") && ambig_err.contains("Z-1") && ambig_err.contains("Z-2"),
        "expected ambiguity error listing both snapshots, got: {:?}",
        ambig_err
    );

    // `ls -lR docs/latest` resolves `latest`, shows modes & timestamps recursively.
    let latest_rec = hatbin()
        .args([
            "ls",
            "-lR",
            "docs/latest",
            "--state-dir",
            sb.repo.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(latest_rec.status.success());
    let latest_out = String::from_utf8_lossy(&latest_rec.stdout);
    assert!(
        latest_out.contains("Z docs/latest/sub/deep/b.txt"),
        "expected deep file with timestamp in ls -lR docs/latest, got: {:?}",
        latest_out
    );

    // `ls -R` on a single file does not double the filename.
    let single_rec = hatbin()
        .args([
            "ls",
            "-R",
            "docs/1/sub/a.txt",
            "--state-dir",
            sb.repo.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(single_rec.status.success());
    assert_eq!(
        String::from_utf8_lossy(&single_rec.stdout).trim(),
        "docs/1/sub/a.txt"
    );

    // `ls -R --json docs/1` emits valid JSON array containing deep entries.
    let json_rec = hatbin()
        .args([
            "ls",
            "-R",
            "--json",
            "docs/1",
            "--state-dir",
            sb.repo.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(json_rec.status.success());
    let arr: serde_json::Value = serde_json::from_slice(&json_rec.stdout).unwrap();
    assert!(arr
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["path"] == "docs/1/sub/deep/b.txt" && v["kind"] == "file"));

    // `ls -R` on nonexistent path fails non-zero.
    hatbin()
        .args([
            "ls",
            "-R",
            "docs/1/no_such_dir",
            "--state-dir",
            sb.repo.to_str().unwrap(),
        ])
        .assert()
        .failure();

    // `browse --help` renders cleanly, and non-TTY `browse` exits non-zero with helpful error.
    let help = hatbin().args(["browse", "--help"]).output().unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("recursive find"));

    let non_tty = hatbin()
        .args(["browse", "--state-dir", sb.repo.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!non_tty.status.success());
    let non_tty_err = String::from_utf8_lossy(&non_tty.stderr);
    assert!(
        non_tty_err.contains("interactive terminal"),
        "expected interactive terminal error in non-TTY browse, got: {:?}",
        non_tty_err
    );
}
