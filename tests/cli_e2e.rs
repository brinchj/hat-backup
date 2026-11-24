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

    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        let fam = hat.open_family("family".into()).unwrap();
        fam.snapshot_dir(sb.fixture.clone()).unwrap();
        fam.flush().unwrap();
        hat.data_flush().unwrap();
    }

    let barrier = sb.root.path().join("barrier");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_hat-crash-worker"))
        .env("HAT_TEST_FAULT", "AfterHashUpdate")
        .env("HAT_TEST_BARRIER", &barrier)
        .args([
            "commit",
            sb.repo.to_str().unwrap(),
            sb.backend.to_str().unwrap(),
            "family",
            sb.fixture.to_str().unwrap(),
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

    // Resume finishes unfinished commit; flush meta/data like a normal commit end.
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        assert_eq!(hat.unfinished_snapshot_count(), 0);
        hat.meta_commit().unwrap();
        hat.data_flush().unwrap();
        let leaf = checkout_and_leaf(&mut hat, "family", &sb.fixture, &sb.restore);
        assert_tree_eq(&sb.fixture, &leaf);
    }
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
}
