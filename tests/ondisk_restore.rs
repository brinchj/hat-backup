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
    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0, "gc must delete unused hashes from snap1");
    assert!(live > 0);
    hat.check().unwrap();

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
    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0, "gc must delete exclusive snap1 hashes");
    assert!(live > 0);
    hat.check().unwrap();

    let restore = sb.root.path().join("restore_shared");
    let leaf = checkout_and_leaf(&mut hat, "family", &fix2, &restore);
    assert_tree_eq(&fix2, &leaf);
}

#[test]
fn incremental_mutations_and_historical_vfs_read() {
    use hat::hat::walker::Content;
    use hat::vfs::fs::{FileReader, List};
    use hat::vfs::Filesystem;
    use std::path::Path;

    let sb = sandbox();
    init_repo(&sb.repo);

    // Commit v1
    write_fixture(
        &sb.fixture,
        &[
            ("same_size.txt", FileSpec::file(b"aaaa1111".to_vec())),
            ("grow.txt", FileSpec::file(b"short".to_vec())),
            ("shrink.txt", FileSpec::file(b"a much longer initial payload".to_vec())),
            ("delete_me.txt", FileSpec::file(b"gone in v2".to_vec())),
            ("rename_src.txt", FileSpec::file(b"moved content".to_vec())),
            ("file_to_dir", FileSpec::file(b"was a file".to_vec())),
            ("dir_to_file/inside.txt", FileSpec::file(b"was a dir".to_vec())),
            ("file_to_link", FileSpec::file(b"was file now link".to_vec())),
        ],
    );

    let snap1 = {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &sb.fixture);
        hat.latest_snapshot_id("family").unwrap()
    };

    // Mutate the same directory in place for v2 (no sleep: exercises same-second mutations).
    fs::write(sb.fixture.join("same_size.txt"), b"bbbb2222").unwrap();
    fs::write(sb.fixture.join("grow.txt"), b"short grew into a much bigger file").unwrap();
    fs::write(sb.fixture.join("shrink.txt"), b"tiny").unwrap();
    fs::remove_file(sb.fixture.join("delete_me.txt")).unwrap();
    fs::rename(
        sb.fixture.join("rename_src.txt"),
        sb.fixture.join("rename_dst.txt"),
    )
    .unwrap();
    fs::remove_file(sb.fixture.join("file_to_dir")).unwrap();
    fs::create_dir_all(sb.fixture.join("file_to_dir")).unwrap();
    fs::write(sb.fixture.join("file_to_dir/child.txt"), b"now a dir").unwrap();
    fs::remove_dir_all(sb.fixture.join("dir_to_file")).unwrap();
    fs::write(sb.fixture.join("dir_to_file"), b"now a file").unwrap();
    fs::remove_file(sb.fixture.join("file_to_link")).unwrap();
    std::os::unix::fs::symlink("rename_dst.txt", sb.fixture.join("file_to_link")).unwrap();
    fs::write(sb.fixture.join("added.txt"), b"brand new").unwrap();

    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &sb.fixture);
        hat.check().unwrap();
    }

    // Reopen and verify latest checkout matches v2 exactly.
    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        let leaf = checkout_and_leaf(&mut hat, "family", &sb.fixture, &sb.restore);
        assert_tree_eq(&sb.fixture, &leaf);
    }

    // Verify historical snapshot (snap1) is still readable via VFS with original bytes.
    {
        let hat = open_hat(&sb.repo, &sb.backend);
        let hash_backend = hat.hash_backend();
        let mut vfs = Filesystem::new(hat);
        let snap1_path = format!("family/{}/delete_me.txt", snap1);
        let listing = vfs
            .ls(Path::new(&snap1_path))
            .unwrap()
            .expect("delete_me.txt must still exist in snap1");
        let List::Dir(entries) = listing else {
            panic!("expected Dir listing for file entry");
        };
        assert_eq!(entries.len(), 1);
        let Content::Data(href) = entries[0].1.clone() else {
            panic!("expected Content::Data for delete_me.txt");
        };
        let mut reader = FileReader::new(hash_backend, href).unwrap();
        let bytes = reader.read(0, 1024).unwrap();
        assert_eq!(&*bytes, b"gone in v2");
    }
}

#[test]
fn multi_family_isolation_and_gc() {
    let sb = sandbox();
    init_repo(&sb.repo);

    let fix_a = sb.root.path().join("fix_a");
    let fix_b = sb.root.path().join("fix_b");
    fs::create_dir_all(&fix_a).unwrap();
    fs::create_dir_all(&fix_b).unwrap();
    write_fixture(
        &fix_a,
        &[
            ("a.bin", FileSpec::file(random_bytes_seeded(101, 256 * 1024))),
            ("shared.txt", FileSpec::file(b"shared across families".to_vec())),
        ],
    );
    write_fixture(
        &fix_b,
        &[
            ("b.bin", FileSpec::file(random_bytes_seeded(202, 256 * 1024))),
            ("shared.txt", FileSpec::file(b"shared across families".to_vec())),
        ],
    );

    let mut hat = open_hat(&sb.repo, &sb.backend);
    commit_tree(&mut hat, "family_a", &fix_a);
    let snap_a = hat.latest_snapshot_id("family_a").unwrap();
    commit_tree(&mut hat, "family_b", &fix_b);

    // Delete family_a and GC; family_b must remain completely intact.
    hat.deregister_by_name("family_a".into(), snap_a).unwrap();
    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0);
    assert!(live > 0);
    hat.check().unwrap();

    let restore_b = sb.root.path().join("restore_b");
    let leaf_b = checkout_and_leaf(&mut hat, "family_b", &fix_b, &restore_b);
    assert_tree_eq(&fix_b, &leaf_b);
    assert!(
        hat.checkout_in_dir("family_a".into(), sb.root.path().join("restore_a"))
            .is_err(),
        "deleted family_a must fail checkout"
    );
}

#[test]
fn multi_megabyte_incompressible_roundtrip() {
    let sb = sandbox();
    init_repo(&sb.repo);

    // 3.2 MiB of seeded random bytes across files forces multi-chunk, multi-blob storage
    // (MAX_BLOB_SIZE = 1 MiB).
    write_fixture(
        &sb.fixture,
        &[
            ("large.bin", FileSpec::file(random_bytes_seeded(1, 3 * 1024 * 1024 + 12345))),
            ("nested/part1.bin", FileSpec::file(random_bytes_seeded(2, 200 * 1024))),
            ("nested/part2.bin", FileSpec::file(random_bytes_seeded(3, 50 * 1024))),
        ],
    );

    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &sb.fixture);
        hat.check().unwrap();
    }

    assert!(
        backend_files(&sb.backend).len() >= 3,
        "expected >= 3 backend blobs for >3 MiB incompressible data"
    );

    let mut hat = open_hat(&sb.repo, &sb.backend);
    hat.check().unwrap();
    let leaf = checkout_and_leaf(&mut hat, "family", &sb.fixture, &sb.restore);
    assert_tree_eq(&sb.fixture, &leaf);
}

#[test]
fn dedup_and_gc_reclaims_backend_blobs() {
    let sb = sandbox();
    init_repo(&sb.repo);

    // Every backend blob is padded to MAX_BLOB_SIZE (1 MiB). Use a 4 MiB shared
    // incompressible file and 1.5 MiB exclusive files so dedup and GC deltas are unambiguous.
    let shared = random_bytes_seeded(42, 4 * 1024 * 1024);
    let excl1 = random_bytes_seeded(43, 1500 * 1024);
    let excl2 = random_bytes_seeded(44, 1500 * 1024);

    let fix1 = sb.root.path().join("fix1");
    let fix2 = sb.root.path().join("fix2");
    fs::create_dir_all(&fix1).unwrap();
    fs::create_dir_all(&fix2).unwrap();
    write_fixture(
        &fix1,
        &[
            ("shared.bin", FileSpec::file(shared.clone())),
            ("excl1.bin", FileSpec::file(excl1)),
        ],
    );
    write_fixture(
        &fix2,
        &[
            ("shared.bin", FileSpec::file(shared)),
            ("excl2.bin", FileSpec::file(excl2)),
        ],
    );

    let mut hat = open_hat(&sb.repo, &sb.backend);
    commit_tree(&mut hat, "family", &fix1);
    let snap1 = hat.latest_snapshot_id("family").unwrap();
    let blobs_after_snap1 = backend_files(&sb.backend).len();
    let bytes_after_snap1 = backend_total_bytes(&sb.backend);

    commit_tree(&mut hat, "family", &fix2);
    let blobs_after_snap2 = backend_files(&sb.backend).len();
    let bytes_after_snap2 = backend_total_bytes(&sb.backend);

    // Dedup check: snap2 added ~1.5 MiB exclusive data (+ metadata blobs), not ~5.5 MiB.
    let blob_delta = blobs_after_snap2 - blobs_after_snap1;
    assert!(
        blob_delta <= 4,
        "expected shared 4 MiB file to deduplicate; snap2 added {} blobs ({} bytes)",
        blob_delta,
        bytes_after_snap2 - bytes_after_snap1
    );

    hat.deregister_by_name("family".into(), snap1).unwrap();
    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0, "expected gc to delete snap1 exclusive hashes");
    assert!(live > 0);

    let blobs_after_gc = backend_files(&sb.backend).len();
    let bytes_after_gc = backend_total_bytes(&sb.backend);
    assert!(
        blobs_after_gc < blobs_after_snap2 && bytes_after_gc < bytes_after_snap2,
        "gc must reclaim unreferenced backend blobs (before: {} blobs / {} bytes, after: {} blobs / {} bytes)",
        blobs_after_snap2,
        bytes_after_snap2,
        blobs_after_gc,
        bytes_after_gc
    );

    hat.check().unwrap();
    let leaf = checkout_and_leaf(&mut hat, "family", &fix2, &sb.restore);
    assert_tree_eq(&fix2, &leaf);
}

#[test]
fn delete_all_snapshots_and_gc_empties_backend() {
    let sb = sandbox();
    write_fixture(&sb.fixture, &basic_fixture());
    init_repo(&sb.repo);

    let mut hat = open_hat(&sb.repo, &sb.backend);
    commit_tree(&mut hat, "family", &sb.fixture);
    assert!(!backend_files(&sb.backend).is_empty());

    hat.delete_all_snapshots().unwrap();
    let (deleted, live) = hat.gc().unwrap();
    assert!(deleted > 0);
    assert_eq!(live, 0);
    assert!(
        backend_files(&sb.backend).is_empty(),
        "expected all backend blobs removed after delete_all_snapshots + gc"
    );
    hat.check().unwrap();
    assert!(hat.checkout_in_dir("family".into(), sb.restore.clone()).is_err());
}

#[test]
fn filesystem_edge_cases_roundtrip() {
    let sb = sandbox();
    init_repo(&sb.repo);

    write_fixture(
        &sb.fixture,
        &[
            ("empty_dir", FileSpec::dir()),
            ("nested/deep/empty_leaf", FileSpec::dir()),
            ("dir_target/file.txt", FileSpec::file(b"inside".to_vec())),
            ("link_to_dir", FileSpec::symlink("dir_target")),
            ("broken_link", FileSpec::symlink("does/not/exist")),
            (
                "unicode/café file (1) — 日本語.txt",
                FileSpec::file(b"unicode payload".to_vec()),
            ),
            ("readonly.txt", FileSpec::file_mode(b"ro".to_vec(), 0o444)),
        ],
    );

    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &sb.fixture);
        hat.check().unwrap();
    }

    let mut hat = open_hat(&sb.repo, &sb.backend);
    let leaf = checkout_and_leaf(&mut hat, "family", &sb.fixture, &sb.restore);
    assert_tree_eq(&sb.fixture, &leaf);
}

#[test]
fn same_second_racily_clean_and_metadata_mutations() {
    use hat::util::{set_times_nofollow, system_time_secs};
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;

    let sb = sandbox();
    init_repo(&sb.repo);

    write_fixture(
        &sb.fixture,
        &[
            ("racy.txt", FileSpec::file(b"11111111".to_vec())),
            ("chmod.txt", FileSpec::file_mode(b"same-bytes".to_vec(), 0o644)),
            ("retarget", FileSpec::symlink("target_v1")),
            ("swap_to_dir", FileSpec::file(b"file-v1".to_vec())),
        ],
    );

    let now = system_time_secs(SystemTime::now());
    let old_ts = now - 60;
    let racy_ts = now + 60;

    // Pin chmod/retarget/swap_to_dir to an old mtime so they are outside the racily-clean
    // window — proving metadata, symlink-target, and kind checks fire independently of mtime.
    for name in ["chmod.txt", "retarget", "swap_to_dir"] {
        set_times_nofollow(&sb.fixture.join(name), old_ts, old_ts).unwrap();
    }
    set_times_nofollow(&sb.fixture.join("racy.txt"), racy_ts, racy_ts).unwrap();

    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &sb.fixture);
    }

    // Mutate racy.txt with identical byte length and identical non-past mtime.
    fs::write(sb.fixture.join("racy.txt"), b"22222222").unwrap();
    set_times_nofollow(&sb.fixture.join("racy.txt"), racy_ts, racy_ts).unwrap();

    // Mutate permissions while keeping identical bytes and old mtime.
    fs::set_permissions(sb.fixture.join("chmod.txt"), fs::Permissions::from_mode(0o755)).unwrap();
    set_times_nofollow(&sb.fixture.join("chmod.txt"), old_ts, old_ts).unwrap();

    // Retarget symlink at same path with identical old mtime.
    fs::remove_file(sb.fixture.join("retarget")).unwrap();
    std::os::unix::fs::symlink("target_v2", sb.fixture.join("retarget")).unwrap();
    set_times_nofollow(&sb.fixture.join("retarget"), old_ts, old_ts).unwrap();

    // Replace file with a directory containing a file, pinning dir mtime to identical old_ts.
    fs::remove_file(sb.fixture.join("swap_to_dir")).unwrap();
    fs::create_dir_all(sb.fixture.join("swap_to_dir")).unwrap();
    fs::write(sb.fixture.join("swap_to_dir/nested.txt"), b"nested-v2").unwrap();
    set_times_nofollow(&sb.fixture.join("swap_to_dir"), old_ts, old_ts).unwrap();

    {
        let mut hat = open_hat(&sb.repo, &sb.backend);
        commit_tree(&mut hat, "family", &sb.fixture);
        hat.check().unwrap();
        let leaf = checkout_and_leaf(&mut hat, "family", &sb.fixture, &sb.restore);
        assert_tree_eq(&sb.fixture, &leaf);
    }
}
