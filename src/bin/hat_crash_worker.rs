//! Test helper: commit or delete against an on-disk repo, optionally hanging at a fault barrier.
//! Used by `tests/process_crash.rs` and `tests/cli_e2e.rs` (SIGKILL + reopen).

#![forbid(unsafe_code)]

use hat::backend::FileBackend;
use hat::Hat;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;

fn usage() -> ! {
    eprintln!(
        "usage: hat-crash-worker commit <repo> <backend> <family> <fixture>\n\
         usage: hat-crash-worker commit-before-meta <repo> <backend> <family> <fixture> <barrier>\n\
         usage: hat-crash-worker done-barrier <repo> <backend> <family> <fixture> <barrier>\n\
         usage: hat-crash-worker delete-before-meta <repo> <backend> <family> <snapshot_id> <barrier>"
    );
    std::process::exit(2);
}

fn hang_at_barrier(barrier: &PathBuf) -> ! {
    std::fs::write(barrier, b"ready").expect("barrier");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

fn main() {
    hat::crypto::keys::init_sodium();

    let mut args = env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| usage());
    let repo = PathBuf::from(args.next().unwrap_or_else(|| usage()));
    let backend_dir = PathBuf::from(args.next().unwrap_or_else(|| usage()));
    let family = args.next().unwrap_or_else(|| usage());

    let backend = Arc::new(FileBackend::new(backend_dir));
    let mut hat = Hat::open_repository(repo, backend, 1024 * 1024).expect("open_repository");

    match cmd.as_str() {
        "commit" => {
            let fixture = PathBuf::from(args.next().unwrap_or_else(|| usage()));
            let mut fam = hat.open_family(family).expect("open_family");
            fam.snapshot_dir(fixture).expect("snapshot_dir");
            // Fault env (HAT_TEST_FAULT / HAT_TEST_BARRIER) is honored inside commit.
            match hat.commit(&mut fam) {
                Ok(()) => {
                    hat.meta_commit().expect("meta_commit");
                    hat.data_flush().expect("data_flush");
                }
                Err(e) => {
                    eprintln!("commit stopped: {}", e);
                    std::process::exit(1);
                }
            }
        }
        "commit-before-meta" => {
            let fixture = PathBuf::from(args.next().unwrap_or_else(|| usage()));
            let barrier = PathBuf::from(args.next().unwrap_or_else(|| usage()));
            let mut fam = hat.open_family(family).expect("open_family");
            fam.snapshot_dir(fixture).expect("snapshot_dir");
            hat.commit(&mut fam).expect("commit");
            // Killed after local atomic commit, before meta_commit publishes the new root.
            hang_at_barrier(&barrier);
        }
        "done-barrier" => {
            let fixture = PathBuf::from(args.next().unwrap_or_else(|| usage()));
            let barrier = PathBuf::from(args.next().unwrap_or_else(|| usage()));
            let mut fam = hat.open_family(family).expect("open_family");
            fam.snapshot_dir(fixture).expect("snapshot_dir");
            hat.commit(&mut fam).expect("commit");
            hat.meta_commit().expect("meta_commit");
            hat.data_flush().expect("data_flush");
            hang_at_barrier(&barrier);
        }
        "delete-before-meta" => {
            let id: u64 = args
                .next()
                .unwrap_or_else(|| usage())
                .parse()
                .expect("snapshot id");
            let barrier = PathBuf::from(args.next().unwrap_or_else(|| usage()));
            hat.deregister_by_name(family, id).expect("deregister");
            // Killed after local O(1) snapshot delete, before meta_commit / gc.
            hang_at_barrier(&barrier);
        }
        _ => usage(),
    }
}
