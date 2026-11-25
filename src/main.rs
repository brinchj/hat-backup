// Copyright 2014 Google Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![forbid(unsafe_code)]

use clap::{Arg, ArgMatches, Command};
use hat::backend::{self, StoreBackend};
use hat::hat::HatRc;
use hat::Hat;
use std::collections::BTreeSet;
use std::env;
use std::ffi;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

static MAX_BLOB_SIZE: usize = 4 * 1024 * 1024;

fn license() {
    println!(include_str!("../LICENSE"));
    println!("clap (Command Line Argument Parser) License:");
    println!(include_str!("../LICENSE-CLAP"));
}

fn backend_kind(matches: &ArgMatches) -> &str {
    matches
        .get_one::<String>("backend")
        .map(|s| s.as_str())
        .unwrap_or("cmd")
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("Error: {}", msg);
    std::process::exit(1);
}

fn open_hat_cmd(cache_dir: PathBuf) -> HatRc<backend::CmdBackend> {
    let backend = Arc::new(backend::CmdBackend::new());
    Hat::open_repository(cache_dir, backend, MAX_BLOB_SIZE).unwrap_or_else(|e| fail(e))
}

fn open_hat_file(cache_dir: PathBuf, matches: &ArgMatches) -> HatRc<backend::FileBackend> {
    let path = matches
        .get_one::<String>("backend_path")
        .cloned()
        .or_else(|| env::var("HAT_BACKEND_PATH").ok())
        .unwrap_or_else(|| fail("--backend-path or HAT_BACKEND_PATH required for file backend"));
    let backend = Arc::new(backend::FileBackend::new(PathBuf::from(path)));
    Hat::open_repository(cache_dir, backend, MAX_BLOB_SIZE).unwrap_or_else(|e| fail(e))
}

fn do_commit<B: StoreBackend>(mut hat: HatRc<B>, name: String, path: PathBuf) {
    let mut family = hat
        .open_family(name.clone())
        .unwrap_or_else(|e| fail(format!("could not open family '{}': {}", name, e)));
    family.snapshot_dir(path).unwrap_or_else(|e| fail(e));
    hat.commit(&mut family).unwrap_or_else(|e| fail(e));
    hat.meta_commit().unwrap_or_else(|e| fail(e));
    hat.data_flush().unwrap_or_else(|e| fail(e));
}

fn do_checkout<B: StoreBackend>(mut hat: HatRc<B>, name: String, id: Option<u64>, path: PathBuf) {
    match id {
        Some(snapshot_id) => hat
            .checkout_snapshot_in_dir(name, snapshot_id, path)
            .unwrap_or_else(|e| fail(e)),
        None => hat
            .checkout_in_dir(name, path)
            .unwrap_or_else(|e| fail(e)),
    }
}

fn do_recover<B: StoreBackend>(mut hat: HatRc<B>) {
    hat.recover().unwrap_or_else(|e| fail(e));
}

fn do_delete<B: StoreBackend>(mut hat: HatRc<B>, name: String, id: u64) {
    hat.deregister_by_name(name, id).unwrap_or_else(|e| fail(e));
    hat.meta_commit().unwrap_or_else(|e| fail(e));
    hat.data_flush().unwrap_or_else(|e| fail(e));
}

fn do_gc<B: StoreBackend>(mut hat: HatRc<B>, pretend: bool) {
    let (deleted_hashes, live_blobs) = if pretend {
        hat.gc_pretend().unwrap_or_else(|e| fail(e))
    } else {
        hat.gc().unwrap_or_else(|e| fail(e))
    };
    if pretend {
        println!("Would delete hashes: {:?}", deleted_hashes);
    } else {
        println!("Deleted hashes: {:?}", deleted_hashes);
    }
    println!("Live data blobs after deletion: {:?}", live_blobs);
}

fn do_check<B: StoreBackend>(hat: HatRc<B>) {
    match hat.check() {
        Ok(()) => println!("check: ok"),
        Err(e) => {
            eprintln!("check: {}", e);
            std::process::exit(1);
        }
    }
}

fn do_resume<B: StoreBackend>(mut hat: HatRc<B>) {
    // open_repository() already ran hat.resume(). Publish an updated root listing
    // and flush pending blobs so resumed commits/deletes survive cache loss.
    if !hat.list_snapshots().is_empty() {
        hat.meta_commit().unwrap_or_else(|e| fail(e));
        hat.data_flush().unwrap_or_else(|e| fail(e));
    }
}

fn do_ls<B: StoreBackend>(hat: HatRc<B>, path: PathBuf) {
    if let Some(f) = hat::vfs::Filesystem::new(hat)
        .ls(&path)
        .unwrap_or_else(|e| fail(e))
    {
        match f {
            hat::vfs::fs::List::Root(snapshots) => {
                snapshots
                    .into_iter()
                    .map(|s| s.family_name)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .for_each(|name| println!("{}", name));
            }
            hat::vfs::fs::List::Snapshots(snapshots) => {
                for si in snapshots {
                    println!(
                        "{}",
                        PathBuf::from(si.family_name)
                            .join(format!("{}", si.info.snapshot_id))
                            .display()
                    );
                }
            }
            hat::vfs::fs::List::Dir(files) => {
                for (entry, _) in files {
                    let name_os_string: ffi::OsString = entry.info.name.into();
                    println!("{}", path.join(name_os_string).display());
                }
            }
        }
    }
}

fn main() {
    hat::crypto::keys::init_sodium();
    env_logger::init();

    let matches = Command::new("hat")
        .version(concat!("v", env!("CARGO_PKG_VERSION")))
        .about("Create backup snapshots")
        .arg(
            Arg::new("license")
                .short('l')
                .long("license")
                .action(clap::ArgAction::SetTrue)
                .help("Display the license"),
        )
        .arg(
            Arg::new("hat_state_dir")
                .long("hat_state_dir")
                .value_name("DIR")
                .help("Location of Hat's local state")
                .env("HAT_STATE_DIR"),
        )
        .arg(
            Arg::new("backend")
                .long("backend")
                .value_name("KIND")
                .help("Storage backend: cmd (default) or file")
                .env("HAT_BACKEND"),
        )
        .arg(
            Arg::new("backend_path")
                .long("backend-path")
                .value_name("DIR")
                .help("Directory for file backend blob storage")
                .env("HAT_BACKEND_PATH"),
        )
        .subcommand(
            Command::new("init")
                .about("Init state directory with a new key and cache dir")
                .arg(Arg::new("DIR").required(true).help("New state directory to initialize")),
        )
        .subcommand(
            Command::new("commit")
                .about("Commit a new snapshot")
                .arg(Arg::new("NAME").required(true).help("Name of the snapshot"))
                .arg(Arg::new("PATH").required(true).help("The path of the snapshot")),
        )
        .subcommand(
            Command::new("checkout")
                .about("Checkout a snapshot")
                .arg(Arg::new("NAME").required(true).help("Name of the snapshot"))
                .arg(Arg::new("PATH").required(true).help("The path of the snapshot"))
                .arg(
                    Arg::new("id")
                        .long("id")
                        .value_name("ID")
                        .help("Snapshot ID to checkout (defaults to latest)"),
                ),
        )
        .subcommand(Command::new("recover").about("Recover list of commit'ed snapshots"))
        .subcommand(
            Command::new("delete")
                .about("Delete a snapshot")
                .arg(Arg::new("NAME").required(true).help("Name of the snapshot family"))
                .arg(Arg::new("ID").required(true).help("The snapshot id to delete")),
        )
        .subcommand(
            Command::new("gc")
                .about("Garbage collect: identify and remove unused data blocks.")
                .arg(
                    Arg::new("pretend")
                        .short('p')
                        .long("pretend")
                        .action(clap::ArgAction::SetTrue)
                        .help("Do not modify any data"),
                ),
        )
        .subcommand(Command::new("check").about("Verify live hash chunks against the backend"))
        .subcommand(
            Command::new("resume")
                .about("Clean up incomplete local state and republish snapshot metadata"),
        )
        .subcommand(
            Command::new("mount")
                .about("Mount Hat snapshots on a mountpoint path using FUSE")
                .arg(Arg::new("PATH").required(true).help("Path of the mount point")),
        )
        .subcommand(
            Command::new("ls")
                .about("List Hat snapshots paths")
                .arg(Arg::new("PATH").required(true).help("Path to list inside hat")),
        )
        .get_matches();

    if matches.get_flag("license") {
        license();
        std::process::exit(0);
    }

    let flag_or_env = |name: &str| {
        matches
            .get_one::<String>(name)
            .cloned()
            .or_else(|| env::var_os(name.to_uppercase()).map(|s| s.into_string().unwrap()))
            .unwrap_or_else(|| {
                eprintln!(
                    "Error: {} required (pass --{} or set {})",
                    name,
                    name,
                    name.to_uppercase()
                );
                std::process::exit(1);
            })
    };

    if let Some(("init", dir)) = matches.subcommand() {
        let dir = PathBuf::from(dir.get_one::<String>("DIR").expect("missing DIR to initialize"));
        if let Some(parent) = dir.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).unwrap_or_else(|e| fail(e));
            }
        }
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        if let Err(e) = builder.create(&dir) {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                fail(format!("directory already exists ({})", dir.display()));
            }
            fail(format!("failed to create directory {}: {}", dir.display(), e));
        }
        builder
            .create(dir.join("cache"))
            .unwrap_or_else(|e| fail(e));
        hat::crypto::keys::Keeper::write_new_universal_key(&dir).unwrap_or_else(|e| fail(e));

        std::process::exit(0);
    }

    let cache_dir = PathBuf::from(flag_or_env("hat_state_dir"));
    let kind = backend_kind(&matches);

    match matches.subcommand() {
        Some(("resume", _)) => match kind {
            "file" => do_resume(open_hat_file(cache_dir, &matches)),
            _ => do_resume(open_hat_cmd(cache_dir)),
        },
        Some(("commit", cmd)) => {
            let name = cmd.get_one::<String>("NAME").unwrap().to_owned();
            let path = PathBuf::from(cmd.get_one::<String>("PATH").unwrap());
            match kind {
                "file" => do_commit(open_hat_file(cache_dir, &matches), name, path),
                _ => do_commit(open_hat_cmd(cache_dir), name, path),
            }
        }
        Some(("checkout", cmd)) => {
            let name = cmd.get_one::<String>("NAME").unwrap().to_owned();
            let path = PathBuf::from(cmd.get_one::<String>("PATH").unwrap());
            let id = cmd.get_one::<String>("id").map(|s| {
                s.parse::<u64>()
                    .unwrap_or_else(|e| fail(format!("invalid snapshot id '{}': {}", s, e)))
            });
            match kind {
                "file" => do_checkout(open_hat_file(cache_dir, &matches), name, id, path),
                _ => do_checkout(open_hat_cmd(cache_dir), name, id, path),
            }
        }
        Some(("recover", _)) => match kind {
            "file" => do_recover(open_hat_file(cache_dir, &matches)),
            _ => do_recover(open_hat_cmd(cache_dir)),
        },
        Some(("delete", cmd)) => {
            let name = cmd.get_one::<String>("NAME").unwrap().to_owned();
            let id_str = cmd.get_one::<String>("ID").unwrap();
            let id = id_str
                .parse::<u64>()
                .unwrap_or_else(|e| fail(format!("invalid snapshot id '{}': {}", id_str, e)));
            match kind {
                "file" => do_delete(open_hat_file(cache_dir, &matches), name, id),
                _ => do_delete(open_hat_cmd(cache_dir), name, id),
            }
        }
        Some(("gc", cmd)) => {
            let pretend = cmd.get_flag("pretend");
            match kind {
                "file" => do_gc(open_hat_file(cache_dir, &matches), pretend),
                _ => do_gc(open_hat_cmd(cache_dir), pretend),
            }
        }
        Some(("check", _)) => match kind {
            "file" => do_check(open_hat_file(cache_dir, &matches)),
            _ => do_check(open_hat_cmd(cache_dir)),
        },
        Some(("mount", cmd)) => {
            #[cfg(feature = "fuse")]
            {
                let path = cmd.get_one::<String>("PATH").unwrap();
                match kind {
                    "file" => {
                        let hat = open_hat_file(cache_dir, &matches);
                        hat::vfs::Fuse::new(hat)
                            .mount(&path)
                            .unwrap_or_else(|e| fail(e));
                    }
                    _ => {
                        let hat = open_hat_cmd(cache_dir);
                        hat::vfs::Fuse::new(hat)
                            .mount(&path)
                            .unwrap_or_else(|e| fail(e));
                    }
                }
            }
            #[cfg(not(feature = "fuse"))]
            {
                let _ = cmd;
                let _ = cache_dir;
                eprintln!(
                    "Error: FUSE support was not enabled at build time. Rebuild with `--features fuse`."
                );
                std::process::exit(1);
            }
        }
        Some(("ls", cmd)) => {
            let path: PathBuf = cmd.get_one::<String>("PATH").unwrap().into();
            match kind {
                "file" => do_ls(open_hat_file(cache_dir, &matches), path),
                _ => do_ls(open_hat_cmd(cache_dir), path),
            }
        }
        _ => {
            println!("No subcommand specified\nFor more information re-run with --help");
            std::process::exit(1);
        }
    }
}
