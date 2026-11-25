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
    hat.check().unwrap();
    let leaf = checkout_and_leaf(&mut hat, "family", &sb.fixture, &sb.restore);
    assert_tree_eq(&sb.fixture, &leaf);
}

#[test]
fn kill_before_publish_preserves_prior_state_and_gc_reclaims_orphans() {
    let sb = sandbox();
    init_repo(&sb.repo);

    let fix1 = sb.root.path().join("fix1");
    let fix2 = sb.root.path().join("fix2");
    fs::create_dir_all(&fix1).unwrap();
    fs::create_dir_all(&fix2).unwrap();
    write_fixture(&fix1, &[("v1.txt", FileSpec::file(b"version-1".to_vec()))]);
    write_fixture(&fix2, &[("v2.txt", FileSpec::file(b"version-2-orphan".to_vec()))]);

    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &fix1);
    }

    let barrier = sb.root.path().join("barrier");
    let mut child = worker()
        .env("HAT_TEST_FAULT", "BeforePublish")
        .env("HAT_TEST_BARRIER", &barrier)
        .args([
            "commit",
            sb.repo.to_str().unwrap(),
            sb.backend.to_str().unwrap(),
            "family",
            fix2.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn worker");

    wait_for_file(&barrier, Duration::from_secs(60));
    kill_process(&mut child);

    let mut hat = open_hat(&sb.repo, &sb.backend);
    assert_eq!(hat.unfinished_snapshot_count(), 0);
    // Only snapshot 1 should be published; snapshot 2 was aborted atomically before publish.
    assert_eq!(hat.latest_snapshot_id("family"), Some(1));

    // Mark-and-sweep GC reclaims the orphaned chunks uploaded by the aborted snapshot 2.
    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0, "gc must reclaim orphaned chunks from aborted commit");
    assert!(live > 0);
    hat.check().unwrap();

    let leaf = checkout_and_leaf(&mut hat, "family", &fix1, &sb.restore);
    assert_tree_eq(&fix1, &leaf);
}

#[test]
fn kill_between_local_commit_and_meta_commit_recovers_cleanly() {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());
    init_repo(&sb.repo);
    let barrier = sb.root.path().join("barrier");

    let mut child = worker()
        .args([
            "commit-before-meta",
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
    hat.meta_commit().unwrap();
    hat.check().unwrap();
    let leaf = checkout_and_leaf(&mut hat, "family", &sb.fixture, &sb.restore);
    assert_tree_eq(&sb.fixture, &leaf);
}

#[test]
fn kill_after_local_delete_before_meta_commit_then_gc() {
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
        .args([
            "delete-before-meta",
            sb.repo.to_str().unwrap(),
            sb.backend.to_str().unwrap(),
            "family",
            &snap1.to_string(),
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
    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0, "gc must delete snap1 hashes after local delete");
    assert!(live > 0);
    hat.meta_commit().unwrap();
    hat.check().unwrap();

    let restore = sb.root.path().join("restore_b");
    let leaf = checkout_and_leaf(&mut hat, "family", &fix2, &restore);
    assert_tree_eq(&fix2, &leaf);
}
