mod common;

use common::*;
use std::fs;

#[test]
fn filebackend_checkout_roundtrip() {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());
    init_repo(&sb.repo);
    let mut hat = open_hat(&sb.repo, &sb.backend);
    commit_tree(&mut hat, "family", &sb.fixture);
    let leaf = checkout_and_leaf(&mut hat, "family", &sb.fixture, &sb.restore);
    assert_tree_eq(&sb.fixture, &leaf);
}

#[test]
fn symlink_roundtrip() {
    let sb = sandbox();
    write_fixture(
        &sb.fixture,
        &[
            ("target.txt", FileSpec::file(b"payload".to_vec())),
            ("rel_link", FileSpec::symlink("target.txt")),
            ("abs_link", FileSpec::symlink("/tmp")),
        ],
    );
    init_repo(&sb.repo);
    let mut hat = open_hat(&sb.repo, &sb.backend);
    commit_tree(&mut hat, "family", &sb.fixture);
    let leaf = checkout_and_leaf(&mut hat, "family", &sb.fixture, &sb.restore);
    assert_tree_eq(&sb.fixture, &leaf);
}

#[test]
fn reopen_repo_reads_same() {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());
    init_repo(&sb.repo);
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &sb.fixture);
    }
    let mut hat = open_hat(&sb.repo, &sb.backend);
    let leaf = checkout_and_leaf(&mut hat, "family", &sb.fixture, &sb.restore);
    assert_tree_eq(&sb.fixture, &leaf);
}

#[test]
fn two_snapshots_independent_checkout() {
    let sb = sandbox();
    init_repo(&sb.repo);

    let fix1 = sb.root.path().join("fix1");
    let fix2 = sb.root.path().join("fix2");
    fs::create_dir_all(&fix1).unwrap();
    fs::create_dir_all(&fix2).unwrap();
    write_fixture(
        &fix1,
        &[
            ("only1.txt", FileSpec::file(b"one".to_vec())),
            ("shared.txt", FileSpec::file(b"shared-bytes".to_vec())),
        ],
    );
    write_fixture(
        &fix2,
        &[
            ("only2.txt", FileSpec::file(b"two".to_vec())),
            ("shared.txt", FileSpec::file(b"shared-bytes".to_vec())),
        ],
    );

    let mut hat = open_hat(&sb.repo, &sb.backend);
    commit_tree(&mut hat, "family", &fix1);
    let snap1 = hat.latest_snapshot_id("family").unwrap();
    commit_tree(&mut hat, "family", &fix2);
    let snap2 = hat.latest_snapshot_id("family").unwrap();
    assert!(snap2 > snap1);

    hat.deregister_by_name("family".into(), snap1).unwrap();
    let (_deleted, live) = hat.gc().unwrap();
    assert!(live > 0);

    let restore = sb.root.path().join("restore2");
    let leaf = checkout_and_leaf(&mut hat, "family", &fix2, &restore);
    assert_tree_eq(&fix2, &leaf);
    assert!(!leaf.join("only1.txt").exists());
}

#[test]
fn shared_chunk_gc_ondisk() {
    let sb = sandbox();
    init_repo(&sb.repo);

    let fix1 = sb.root.path().join("fix1");
    let fix2 = sb.root.path().join("fix2");
    fs::create_dir_all(&fix1).unwrap();
    fs::create_dir_all(&fix2).unwrap();
    let shared = b"dedup-me-please".to_vec();
    write_fixture(
        &fix1,
        &[
            ("shared.dat", FileSpec::file(shared.clone())),
            ("only1.dat", FileSpec::file(b"exclusive-1".to_vec())),
        ],
    );
    write_fixture(
        &fix2,
        &[
            ("shared.dat", FileSpec::file(shared)),
            ("only2.dat", FileSpec::file(b"exclusive-2".to_vec())),
        ],
    );

    let mut hat = open_hat(&sb.repo, &sb.backend);
    commit_tree(&mut hat, "family", &fix1);
    let snap1 = hat.latest_snapshot_id("family").unwrap();
    commit_tree(&mut hat, "family", &fix2);

    hat.deregister_by_name("family".into(), snap1).unwrap();
    hat.gc().unwrap();

    let restore = sb.root.path().join("restore_shared");
    let leaf = checkout_and_leaf(&mut hat, "family", &fix2, &restore);
    assert_tree_eq(&fix2, &leaf);
}
