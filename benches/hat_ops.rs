//! End-to-end benchmarks for the backup operations that matter for user trust:
//! ingest/commit, dedup ingest, checkout, GC after delete, and `check`.
//!
//! Uses on-disk `FileBackend` + `Hat::open_repository` (same path as confidence tests),
//! not `DevNullBackend` / `:memory:` micro-benches.
//!
//! Two fixture shapes:
//! - **bulk MiB:** few large files (raw ingest / crypto throughput)
//! - **filesystem tree:** many small files, deep dirs, empties, symlinks, mixed modes
//!   (walk / SQLite / syscall-dominated — closer to real backup UX)
//!
//! Run: `cargo bench --bench hat_ops`
//! Quick smoke: `cargo bench --bench hat_ops -- --quick`
//! Filesystem only: `cargo bench --bench hat_ops -- filesystem`

use criterion::{
    black_box, criterion_group, criterion_main, BatchSize, Criterion, SamplingMode, Throughput,
};
use hat::backend::FileBackend;
use hat::crypto::keys::Keeper;
use hat::Hat;
use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::Duration;
use tempfile::TempDir;

static SODIUM: Once = Once::new();

fn init_sodium() {
    SODIUM.call_once(|| unsafe {
        libsodium_sys::sodium_init();
    });
}

const MAX_BLOB_SIZE: usize = 1024 * 1024;
/// Unique payload size per file in the standard fixture (4 files → ~4 MiB unique).
const FILE_BYTES: usize = 1024 * 1024;

struct BenchRepo {
    _tmp: TempDir,
    repo: PathBuf,
    backend: PathBuf,
    fixture: PathBuf,
    restore: PathBuf,
}

fn new_repo() -> BenchRepo {
    init_sodium();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    let backend = tmp.path().join("backend");
    let fixture = tmp.path().join("fixture");
    let restore = tmp.path().join("restore");
    fs::create_dir_all(&backend).unwrap();
    fs::create_dir_all(&fixture).unwrap();
    fs::create_dir_all(&restore).unwrap();
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(repo.join("cache")).unwrap();
    Keeper::write_new_universal_key(&repo).unwrap();
    BenchRepo {
        _tmp: tmp,
        repo,
        backend,
        fixture,
        restore,
    }
}

fn open_hat(repo: &Path, backend: &Path) -> hat::hat::HatRc<FileBackend> {
    let backend = Arc::new(FileBackend::new(backend.to_path_buf()));
    Hat::open_repository(repo.to_path_buf(), backend, MAX_BLOB_SIZE).unwrap()
}

fn write_unique_fixture(root: &Path, salt: u64) {
    // Overwrite in place when possible — avoid delete/recreate thrash between samples.
    fs::create_dir_all(root.join("nested")).unwrap();
    for i in 0..4u64 {
        let mut data = vec![0u8; FILE_BYTES];
        // Make each file and each bench iteration unique so we measure ingest, not pure dedup.
        for (idx, byte) in data.iter_mut().enumerate() {
            *byte = ((salt as usize)
                .wrapping_mul(131)
                .wrapping_add(i as usize * 17)
                .wrapping_add(idx)) as u8;
        }
        let name = if i == 0 {
            format!("file_{}.bin", i)
        } else {
            format!("nested/file_{}.bin", i)
        };
        fs::write(root.join(name), data).unwrap();
    }
}

fn write_identical_fixture(root: &Path) {
    fs::create_dir_all(root.join("nested")).unwrap();
    let data = vec![0xABu8; FILE_BYTES];
    for i in 0..4u64 {
        let name = if i == 0 {
            format!("file_{}.bin", i)
        } else {
            format!("nested/file_{}.bin", i)
        };
        fs::write(root.join(name), &data).unwrap();
    }
}

/// I/O benches are noisy on shared machines; prefer more samples + flat sampling
/// over Criterion's default adaptive mode (which amplifies mild outliers at n=10).
fn configure_io_group(group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>) {
    group.sample_size(50);
    group.warm_up_time(Duration::from_secs(3));
    group.measurement_time(Duration::from_secs(20));
    group.sampling_mode(SamplingMode::Flat);
    group.noise_threshold(0.05);
}

fn commit_tree(hat: &mut hat::hat::HatRc<FileBackend>, family: &str, fixture: &Path) {
    let mut fam = hat.open_family(family.to_string()).unwrap();
    fam.snapshot_dir(fixture.to_path_buf()).unwrap();
    hat.commit(&mut fam, None).unwrap();
    hat.meta_commit().unwrap();
    hat.data_flush().unwrap();
}

fn fixture_bytes() -> u64 {
    (4 * FILE_BYTES) as u64
}

/// Shape of a small-but-noisy project/home tree: deep paths, many tiny files,
/// empties, symlinks, mixed modes. Total payload stays modest; entry count is the point.
#[derive(Clone, Copy, Debug)]
struct FsTreeStats {
    files: u64,
    dirs: u64,
    symlinks: u64,
    bytes: u64,
}

fn ensure_dir(path: &Path, dirs: &mut u64) {
    fs::create_dir_all(path).unwrap();
    *dirs += 1;
}

fn write_file(path: &Path, data: &[u8], mode: u32, files: &mut u64, bytes: &mut u64) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, data).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    *files += 1;
    *bytes += data.len() as u64;
}

fn write_filesystem_fixture(root: &Path, salt: u64) -> FsTreeStats {
    if root.exists() {
        fs::remove_dir_all(root).ok();
    }
    fs::create_dir_all(root).unwrap();

    let mut files = 0u64;
    let mut dirs = 0u64;
    let mut symlinks = 0u64;
    let mut bytes = 0u64;

    // Deep chain: Documents/work/year/month/...
    let mut deep = root.join("Documents");
    ensure_dir(&deep, &mut dirs);
    for part in ["work", "2024", "03", "15", "notes", "archive", "old"] {
        deep.push(part);
        ensure_dir(&deep, &mut dirs);
    }
    for i in 0..40u64 {
        let mut body = format!("note-{salt}-{i}\n").into_bytes();
        body.extend(std::iter::repeat((i as u8).wrapping_add(salt as u8)).take(64));
        write_file(
            &deep.join(format!("note_{i}.txt")),
            &body,
            0o644,
            &mut files,
            &mut bytes,
        );
    }
    for i in 0..20u64 {
        write_file(
            &deep.join(format!("empty_{i}.dat")),
            b"",
            0o644,
            &mut files,
            &mut bytes,
        );
    }

    // Wide project tree (src / tests / docs / scripts).
    for top in ["src", "tests", "docs", "scripts"] {
        let base = root.join("Projects").join("app").join(top);
        ensure_dir(&base, &mut dirs);
        for mod_i in 0..12u64 {
            let mdir = base.join(format!("mod_{mod_i}"));
            ensure_dir(&mdir, &mut dirs);
            for f in 0..10u64 {
                let size = match (mod_i + f) % 5 {
                    0 => 16,
                    1 => 200,
                    2 => 1500,
                    3 => 8192,
                    _ => 48 * 1024,
                };
                let mut data = vec![0u8; size];
                for (idx, b) in data.iter_mut().enumerate() {
                    *b = ((salt as usize)
                        .wrapping_mul(31)
                        .wrapping_add(mod_i as usize * 13)
                        .wrapping_add(f as usize)
                        .wrapping_add(idx)) as u8;
                }
                let mode = if top == "scripts" { 0o755 } else { 0o644 };
                write_file(
                    &mdir.join(format!("f_{f}.rs")),
                    &data,
                    mode,
                    &mut files,
                    &mut bytes,
                );
            }
            ensure_dir(&mdir.join(".cache"), &mut dirs);
        }
    }

    // Vendor-ish fan-out: many tiny leaves (metadata pressure).
    for pkg in 0..30u64 {
        let pdir = root
            .join("vendor")
            .join(format!("pkg_{pkg}"))
            .join("lib");
        ensure_dir(&pdir, &mut dirs);
        for leaf in 0..8u64 {
            let data = format!("{salt}-pkg{pkg}-leaf{leaf}\n").into_bytes();
            write_file(
                &pdir.join(format!("index_{leaf}.js")),
                &data,
                0o644,
                &mut files,
                &mut bytes,
            );
        }
    }

    // Symlinks (relative + into deep tree).
    let link_dir = root.join("links");
    ensure_dir(&link_dir, &mut dirs);
    symlink("../Projects/app/src", link_dir.join("to_src")).unwrap();
    symlinks += 1;
    symlink("missing-target", link_dir.join("dangling")).unwrap();
    symlinks += 1;
    symlink(
        deep.strip_prefix(root).unwrap(),
        link_dir.join("to_notes"),
    )
    .unwrap();
    symlinks += 1;

    // Dotfile / config clutter.
    write_file(
        &root.join(".editorconfig"),
        b"root = true\n[*]\ncharset = utf-8\n",
        0o644,
        &mut files,
        &mut bytes,
    );
    write_file(
        &root.join("README.md"),
        b"# bench fixture\n",
        0o644,
        &mut files,
        &mut bytes,
    );
    ensure_dir(&root.join(".git").join("objects"), &mut dirs);
    write_file(
        &root.join(".git").join("HEAD"),
        b"ref: refs/heads/main\n",
        0o644,
        &mut files,
        &mut bytes,
    );

    FsTreeStats {
        files,
        dirs,
        symlinks,
        bytes,
    }
}

fn filesystem_stats() -> FsTreeStats {
    // Stable counts from a throwaway build (salt does not change topology).
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("probe");
    write_filesystem_fixture(&root, 0)
}

fn bench_commit_unique(c: &mut Criterion) {
    let mut group = c.benchmark_group("commit");
    group.throughput(Throughput::Bytes(fixture_bytes()));
    configure_io_group(&mut group);
    group.bench_function("unique_4x1MiB_filebackend", |b| {
        // One repo for the whole run: setup only rewrites payload bytes (not timed).
        // Avoids tempfile create/destroy noise that showed up as high-mild outliers at n=10.
        let repo = new_repo();
        let hat = RefCell::new(open_hat(&repo.repo, &repo.backend));
        let salt = RefCell::new(0u64);
        b.iter_batched(
            || {
                *salt.borrow_mut() += 1;
                write_unique_fixture(&repo.fixture, *salt.borrow());
            },
            |_| {
                commit_tree(&mut hat.borrow_mut(), "family", &repo.fixture);
                black_box(());
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

fn bench_commit_dedup(c: &mut Criterion) {
    let mut group = c.benchmark_group("commit");
    group.throughput(Throughput::Bytes(fixture_bytes()));
    configure_io_group(&mut group);
    group.bench_function("dedup_identical_4x1MiB_filebackend", |b| {
        let repo = new_repo();
        write_identical_fixture(&repo.fixture);
        let mut hat = open_hat(&repo.repo, &repo.backend);
        // Prime indexes / chunk store once (not timed).
        commit_tree(&mut hat, "family", &repo.fixture);
        let hat = RefCell::new(hat);
        b.iter(|| {
            // Same bytes again — dedup path; warm repo keeps sample variance down.
            commit_tree(&mut hat.borrow_mut(), "family", &repo.fixture);
            black_box(());
        });
    });
    group.finish();
}

fn bench_checkout(c: &mut Criterion) {
    let mut group = c.benchmark_group("checkout");
    group.throughput(Throughput::Bytes(fixture_bytes()));
    configure_io_group(&mut group);
    group.bench_function("restore_4x1MiB_filebackend", |b| {
        let repo = new_repo();
        write_unique_fixture(&repo.fixture, 42);
        let mut hat = open_hat(&repo.repo, &repo.backend);
        commit_tree(&mut hat, "family", &repo.fixture);
        let hat = RefCell::new(hat);
        b.iter_batched(
            || {
                if repo.restore.exists() {
                    fs::remove_dir_all(&repo.restore).ok();
                }
                fs::create_dir_all(&repo.restore).unwrap();
            },
            |_| {
                hat.borrow_mut()
                    .checkout_in_dir("family".into(), repo.restore.clone())
                    .unwrap();
                black_box(());
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

fn bench_gc_after_delete(c: &mut Criterion) {
    let mut group = c.benchmark_group("gc");
    configure_io_group(&mut group);
    group.bench_function("delete_snap_then_gc_filebackend", |b| {
        b.iter_batched(
            || {
                let repo = new_repo();
                let fix1 = repo._tmp.path().join("fix1");
                let fix2 = repo._tmp.path().join("fix2");
                write_unique_fixture(&fix1, 1);
                write_unique_fixture(&fix2, 2);
                let mut hat = open_hat(&repo.repo, &repo.backend);
                commit_tree(&mut hat, "family", &fix1);
                let snap1 = hat.latest_snapshot_id("family").unwrap();
                commit_tree(&mut hat, "family", &fix2);
                (repo, hat, snap1)
            },
            |(_repo, mut hat, snap1)| {
                hat.deregister_by_name("family".into(), snap1).unwrap();
                let result = hat.gc().unwrap();
                black_box(result);
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

fn bench_check(c: &mut Criterion) {
    let mut group = c.benchmark_group("check");
    group.throughput(Throughput::Bytes(fixture_bytes()));
    configure_io_group(&mut group);
    group.bench_function("verify_live_hashes_4x1MiB", |b| {
        let repo = new_repo();
        write_unique_fixture(&repo.fixture, 7);
        let mut hat = open_hat(&repo.repo, &repo.backend);
        commit_tree(&mut hat, "family", &repo.fixture);
        let hat = RefCell::new(hat);
        b.iter(|| {
            hat.borrow().check().unwrap();
            black_box(());
        });
    });
    group.finish();
}

fn bench_filesystem_commit(c: &mut Criterion) {
    let stats = filesystem_stats();
    let mut group = c.benchmark_group("filesystem");
    // Entry count is the meaningful unit (walk/SQLite/syscalls), not MiB/s.
    group.throughput(Throughput::Elements(stats.files));
    configure_io_group(&mut group);
    // FS tree commits are slower; keep flat sampling but allow enough wall time.
    group.measurement_time(Duration::from_secs(30));

    group.bench_function(
        format!(
            "commit_tree_{}files_{}dirs_{}links_{}KiB",
            stats.files,
            stats.dirs,
            stats.symlinks,
            stats.bytes / 1024
        ),
        |b| {
            let repo = new_repo();
            let hat = RefCell::new(open_hat(&repo.repo, &repo.backend));
            let salt = RefCell::new(0u64);
            b.iter_batched(
                || {
                    *salt.borrow_mut() += 1;
                    let built = write_filesystem_fixture(&repo.fixture, *salt.borrow());
                    assert_eq!(built.files, stats.files);
                },
                |_| {
                    commit_tree(&mut hat.borrow_mut(), "family", &repo.fixture);
                    black_box(());
                },
                BatchSize::LargeInput,
            );
        },
    );
    group.finish();
}

fn bench_filesystem_checkout(c: &mut Criterion) {
    let stats = filesystem_stats();
    let mut group = c.benchmark_group("filesystem");
    group.throughput(Throughput::Elements(stats.files));
    configure_io_group(&mut group);
    group.measurement_time(Duration::from_secs(30));

    group.bench_function(
        format!(
            "checkout_tree_{}files_{}KiB",
            stats.files,
            stats.bytes / 1024
        ),
        |b| {
            let repo = new_repo();
            write_filesystem_fixture(&repo.fixture, 99);
            let mut hat = open_hat(&repo.repo, &repo.backend);
            commit_tree(&mut hat, "family", &repo.fixture);
            let hat = RefCell::new(hat);
            b.iter_batched(
                || {
                    if repo.restore.exists() {
                        fs::remove_dir_all(&repo.restore).ok();
                    }
                    fs::create_dir_all(&repo.restore).unwrap();
                },
                |_| {
                    hat.borrow_mut()
                        .checkout_in_dir("family".into(), repo.restore.clone())
                        .unwrap();
                    black_box(());
                },
                BatchSize::LargeInput,
            );
        },
    );
    group.finish();
}

fn bench_filesystem_dedup_commit(c: &mut Criterion) {
    let stats = filesystem_stats();
    let mut group = c.benchmark_group("filesystem");
    group.throughput(Throughput::Elements(stats.files));
    configure_io_group(&mut group);
    group.measurement_time(Duration::from_secs(30));

    group.bench_function(
        format!(
            "commit_unchanged_tree_{}files_{}KiB",
            stats.files,
            stats.bytes / 1024
        ),
        |b| {
            let repo = new_repo();
            write_filesystem_fixture(&repo.fixture, 1);
            let mut hat = open_hat(&repo.repo, &repo.backend);
            commit_tree(&mut hat, "family", &repo.fixture);
            let hat = RefCell::new(hat);
            b.iter(|| {
                // Same tree again: stresses metadata walk + dedup, not bulk encrypt.
                commit_tree(&mut hat.borrow_mut(), "family", &repo.fixture);
                black_box(());
            });
        },
    );
    group.finish();
}

criterion_group!(
    hat_ops,
    bench_commit_unique,
    bench_commit_dedup,
    bench_checkout,
    bench_gc_after_delete,
    bench_check,
    bench_filesystem_commit,
    bench_filesystem_checkout,
    bench_filesystem_dedup_commit
);
criterion_main!(hat_ops);
