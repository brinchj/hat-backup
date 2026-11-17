mod common;

use common::*;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn worker() -> Command {
    Command::new(env!("CARGO_BIN_EXE_hat-crash-worker"))
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < timeout,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn kill_process(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn kill_after_full_commit_reopen_ok() {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());
    init_repo(&sb.repo);
    let barrier = sb.root.path().join("barrier");

    let mut child = worker()
        .args([
            "done-barrier",
            sb.repo.to_str().unwrap(),
            sb.backend.to_str().unwrap(),
            "family",
            sb.fixture.to_str().unwrap(),
            barrier.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn worker");

    wait_for_file(&barrier, Duration::from_secs(60));
    kill_process(&mut child);

    let mut hat = open_hat(&sb.repo, &sb.backend);
    assert_eq!(hat.unfinished_snapshot_count(), 0);
    let leaf = checkout_and_leaf(&mut hat, "family", &sb.fixture, &sb.restore);
    assert_tree_eq(&sb.fixture, &leaf);
}

fn kill_mid_commit_at(fault: &str) {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());
    init_repo(&sb.repo);
    // Pre-index so commit has work past reserve without requiring a prior flush in child.
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        let fam = hat.open_family("family".into()).unwrap();
        fam.snapshot_dir(sb.fixture.clone()).unwrap();
        fam.flush().unwrap();
        hat.data_flush().unwrap();
    }

    let barrier = sb.root.path().join("barrier");
    let mut child = worker()
        .env("HAT_TEST_FAULT", fault)
        .env("HAT_TEST_BARRIER", &barrier)
        .args([
            "commit",
            sb.repo.to_str().unwrap(),
            sb.backend.to_str().unwrap(),
            "family",
            sb.fixture.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn worker");

    wait_for_file(&barrier, Duration::from_secs(60));
    kill_process(&mut child);

    let mut hat = open_hat(&sb.repo, &sb.backend);
    // open_repository already calls resume().
    assert_eq!(
        hat.unfinished_snapshot_count(),
        0,
        "resume should clear unfinished work after fault {}",
        fault
    );
    let leaf = checkout_and_leaf(&mut hat, "family", &sb.fixture, &sb.restore);
    assert_tree_eq(&sb.fixture, &leaf);
}

#[test]
fn kill_mid_commit_resume_repairs_after_hash_update() {
    kill_mid_commit_at("AfterHashUpdate");
}

#[test]
fn kill_mid_commit_resume_repairs_after_register_final() {
    kill_mid_commit_at("AfterRegisterFinal");
}

#[test]
fn kill_mid_commit_resume_repairs_after_ready_commit() {
    kill_mid_commit_at("AfterReadyCommit");
}

#[test]
fn kill_mid_delete_resume_then_gc() {
    let sb = sandbox();
    init_repo(&sb.repo);

    let fix1 = sb.root.path().join("fix1");
    let fix2 = sb.root.path().join("fix2");
    fs::create_dir_all(&fix1).unwrap();
    fs::create_dir_all(&fix2).unwrap();
    write_fixture(&fix1, &[("a.txt", FileSpec::file(b"aaa".to_vec()))]);
    write_fixture(&fix2, &[("b.txt", FileSpec::file(b"bbb".to_vec()))]);

    let snap1;
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &fix1);
        snap1 = hat.latest_snapshot_id("family").unwrap();
        commit_tree(&mut hat, "family", &fix2);
    }

    let barrier = sb.root.path().join("barrier");
    let mut child = worker()
        .env("HAT_TEST_FAULT", "AfterGcDeregister")
        .env("HAT_TEST_BARRIER", &barrier)
        .args([
            "delete",
            sb.repo.to_str().unwrap(),
            sb.backend.to_str().unwrap(),
            "family",
            &snap1.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn worker");

    wait_for_file(&barrier, Duration::from_secs(60));
    kill_process(&mut child);

    let mut hat = open_hat(&sb.repo, &sb.backend);
    assert_eq!(hat.unfinished_snapshot_count(), 0);
    let (_deleted, live) = hat.gc().unwrap();
    assert!(live > 0);

    let restore = sb.root.path().join("restore_b");
    let leaf = checkout_and_leaf(&mut hat, "family", &fix2, &restore);
    assert_tree_eq(&fix2, &leaf);
}
