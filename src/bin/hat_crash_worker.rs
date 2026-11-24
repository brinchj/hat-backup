//! Test helper: commit or delete against an on-disk repo, optionally hanging at a fault barrier.
//! Used by `tests/process_crash.rs` (SIGKILL + reopen).

use hat::backend::FileBackend;
use hat::Hat;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;

fn usage() -> ! {
    eprintln!(
        "usage: hat-crash-worker commit <repo> <backend> <family> <fixture>\n\
         usage: hat-crash-worker delete <repo> <backend> <family> <snapshot_id>\n\
         usage: hat-crash-worker done-barrier <repo> <backend> <family> <fixture> <barrier>"
    );
    std::process::exit(2);
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
            // Durability before commit stages: process-kill tests need blobs on disk.
            fam.flush().expect("family flush");
            hat.data_flush().expect("data_flush before commit");
            // Fault env (HAT_TEST_FAULT / HAT_TEST_BARRIER) is honored inside commit.
            match hat.commit(&mut fam, None) {
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
        "done-barrier" => {
            let fixture = PathBuf::from(args.next().unwrap_or_else(|| usage()));
            let barrier = PathBuf::from(args.next().unwrap_or_else(|| usage()));
            let mut fam = hat.open_family(family).expect("open_family");
            fam.snapshot_dir(fixture).expect("snapshot_dir");
            hat.commit(&mut fam, None).expect("commit");
            hat.meta_commit().expect("meta_commit");
            hat.data_flush().expect("data_flush");
            std::fs::write(&barrier, b"ready").expect("barrier");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
        "delete" => {
            let id: u64 = args
                .next()
                .unwrap_or_else(|| usage())
                .parse()
                .expect("snapshot id");
            match hat.deregister_by_name(family, id) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("delete stopped: {}", e);
                    std::process::exit(1);
                }
            }
        }
        _ => usage(),
    }
}
