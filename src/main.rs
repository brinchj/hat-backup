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

use clap::{Arg, Command};
use hat::backend;
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

fn main() {
    // Initialize libraries
    unsafe {
        libsodium_sys::sodium_init();
    }
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
                .arg(Arg::new("PATH").required(true).help("The path of the snapshot")),
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
        .subcommand(Command::new("resume").about("Resume previous failed command."))
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
            .unwrap_or_else(|| panic!("{} required", name))
    };

    // Special cased one-off commands
    if let Some(("init", dir)) = matches.subcommand() {
        let dir = PathBuf::from(dir.get_one::<String>("DIR").expect("missing DIR to initialize"));
        if dir.exists() {
            eprintln!("Error: directory already exists ({})", dir.display());
            std::process::exit(1);
        }

        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(dir.join("cache")).unwrap();
        hat::crypto::keys::Keeper::write_new_universal_key(&dir).unwrap();

        std::process::exit(0);
    }

    // Setup config variables that can take their value from either flag or environment.
    let cache_dir = PathBuf::from(flag_or_env("hat_state_dir"));

    match matches.subcommand() {
        Some(("resume", _)) => {
            // Setting up the repository triggers automatic resume.
            let backend = Arc::new(backend::CmdBackend::new());
            hat::Hat::open_repository(cache_dir, backend, MAX_BLOB_SIZE).unwrap();
        }
        Some(("commit", cmd)) => {
            let name = cmd.get_one::<String>("NAME").unwrap().to_owned();
            let path = cmd.get_one::<String>("PATH").unwrap();

            let backend = Arc::new(backend::CmdBackend::new());
            let mut hat = hat::Hat::open_repository(cache_dir, backend, MAX_BLOB_SIZE).unwrap();

            // Update the family index.
            let mut family = hat
                .open_family(name.clone())
                .unwrap_or_else(|_| panic!("Could not open family '{}'", name));
            family.snapshot_dir(PathBuf::from(path));

            // Commit the updated index.
            hat.commit(&mut family, None).unwrap();

            // Meta commit.
            hat.meta_commit().unwrap();

            // Flush any remaining blobs.
            hat.data_flush().unwrap();
        }
        Some(("checkout", cmd)) => {
            let name = cmd.get_one::<String>("NAME").unwrap().to_owned();
            let path = cmd.get_one::<String>("PATH").unwrap();

            let backend = Arc::new(backend::CmdBackend::new());
            let mut hat = hat::Hat::open_repository(cache_dir, backend, MAX_BLOB_SIZE).unwrap();

            hat.checkout_in_dir(name, PathBuf::from(path)).unwrap();
        }
        Some(("recover", _)) => {
            let backend = Arc::new(backend::CmdBackend::new());
            let mut hat = hat::Hat::open_repository(cache_dir, backend, MAX_BLOB_SIZE).unwrap();

            hat.recover().unwrap();
        }
        Some(("delete", cmd)) => {
            let name = cmd.get_one::<String>("NAME").unwrap().to_owned();
            let id = cmd.get_one::<String>("ID").unwrap().to_owned();

            let backend = Arc::new(backend::CmdBackend::new());
            let mut hat = hat::Hat::open_repository(cache_dir, backend, MAX_BLOB_SIZE).unwrap();

            hat.deregister_by_name(name, id.parse::<u64>().unwrap())
                .unwrap();
        }
        Some(("gc", _)) => {
            let backend = Arc::new(backend::CmdBackend::new());
            let mut hat = hat::Hat::open_repository(cache_dir, backend, MAX_BLOB_SIZE).unwrap();
            let (deleted_hashes, live_blobs) = hat.gc().unwrap();
            println!("Deleted hashes: {:?}", deleted_hashes);
            println!("Live data blobs after deletion: {:?}", live_blobs);
        }
        Some(("mount", cmd)) => {
            #[cfg(feature = "fuse")]
            {
                let path = cmd.get_one::<String>("PATH").unwrap();
                let backend = Arc::new(backend::CmdBackend::new());

                let hat = hat::Hat::open_repository(cache_dir, backend, MAX_BLOB_SIZE).unwrap();
                hat::vfs::Fuse::new(hat).mount(&path).unwrap();
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
            let backend = Arc::new(backend::CmdBackend::new());

            let hat = hat::Hat::open_repository(cache_dir, backend, MAX_BLOB_SIZE).unwrap();
            if let Some(f) = hat::vfs::Filesystem::new(hat).ls(&path).unwrap() {
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
        _ => {
            println!("No subcommand specified\nFor more information re-run with --help");
            std::process::exit(1);
        }
    }
}
