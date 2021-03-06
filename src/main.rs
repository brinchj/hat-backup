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

// Import the hat library
extern crate hat;

// Rust crates.
extern crate env_logger;
extern crate libsodium_sys;

// We use Clap for argument parsing.
#[macro_use]
extern crate clap;

use clap::{App, SubCommand};
use std::env;

use hat::backend;
use std::borrow::ToOwned;
use std::collections::BTreeSet;
use std::convert::From;
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
    unsafe { libsodium_sys::sodium_init() };
    env_logger::init();

    // Because "snapshot" and "checkout" use the exact same type of arguments, we can make a
    // template. This template defines two positional arguments, both are required
    let arg_template = "<NAME> 'Name of the snapshot'
                        <PATH> 'The path of the snapshot'";

    // Create valid arguments
    let matches = App::new("hat")
        .version(&format!("v{}", crate_version!())[..])
        .about("Create backup snapshots")
        .args_from_usage(
            "-l, --license 'Display the license'
            --hat_state_dir=[DIR] 'Location of Hat\'s local state'",
        )
        .subcommand(
            SubCommand::with_name("init")
                .about("Init state directory with a new key and cache dir")
                .args_from_usage("<DIR> 'New state directory to initialize'"),
        )
        .subcommand(
            SubCommand::with_name("commit")
                .about("Commit a new snapshot")
                .args_from_usage(arg_template),
        )
        .subcommand(
            SubCommand::with_name("checkout")
                .about("Checkout a snapshot")
                .args_from_usage(arg_template),
        )
        .subcommand(SubCommand::with_name("recover").about("Recover list of commit'ed snapshots"))
        .subcommand(
            SubCommand::with_name("delete")
                .about("Delete a snapshot")
                .args_from_usage(
                    "<NAME> 'Name of the snapshot family'
                     <ID> 'The snapshot id to delete'",
                ),
        )
        .subcommand(
            SubCommand::with_name("gc")
                .about("Garbage collect: identify and remove unused data blocks.")
                .args_from_usage("-p --pretend 'Do not modify any data'"),
        )
        .subcommand(SubCommand::with_name("resume").about("Resume previous failed command."))
        .subcommand(
            SubCommand::with_name("mount")
                .about("Mount Hat snapshots on a mountpoint path using FUSE")
                .args_from_usage("<PATH> 'Path of the mount point'"),
        )
        .subcommand(
            SubCommand::with_name("ls")
                .about("List Hat snapshots paths")
                .args_from_usage("<PATH> 'Path to list inside hat'"),
        )
        .get_matches();

    // Check for license flag
    if matches.is_present("license") {
        license();
        std::process::exit(0);
    }

    let flag_or_env = |name: &str| {
        matches
            .value_of(name)
            .map(|x| x.to_string())
            .or_else(|| env::var_os(name.to_uppercase()).map(|s| s.into_string().unwrap()))
            .expect(&format!("{} required", name))
    };

    // Special cased one-off commands
    match matches.subcommand() {
        ("init", Some(dir)) => {
            let dir = PathBuf::from(dir.value_of("DIR").expect("missing DIR to initialize"));
            if dir.exists() {
                eprintln!("Error: directory already exists ({})", dir.display());
                std::process::exit(1);
            }

            fs::create_dir_all(&dir).unwrap();
            fs::create_dir_all(dir.join("cache")).unwrap();
            hat::crypto::keys::Keeper::write_new_universal_key(&dir).unwrap();

            std::process::exit(0);
        }
        _ => (),
    }

    // Setup config variables that can take their value from either flag or environment.
    let cache_dir = PathBuf::from(flag_or_env("hat_state_dir"));

    match matches.subcommand() {
        ("resume", Some(_cmd)) => {
            // Setting up the repository triggers automatic resume.
            let backend = Arc::new(backend::CmdBackend::new());
            hat::Hat::open_repository(cache_dir, backend, MAX_BLOB_SIZE).unwrap();
        }
        ("commit", Some(cmd)) => {
            let name = cmd.value_of("NAME").unwrap().to_owned();
            let path = cmd.value_of("PATH").unwrap();

            let backend = Arc::new(backend::CmdBackend::new());
            let mut hat = hat::Hat::open_repository(cache_dir, backend, MAX_BLOB_SIZE).unwrap();

            // Update the family index.
            let mut family = hat
                .open_family(name.clone())
                .expect(&format!("Could not open family '{}'", name));
            family.snapshot_dir(PathBuf::from(path));

            // Commit the updated index.
            hat.commit(&mut family, None).unwrap();

            // Meta commit.
            hat.meta_commit().unwrap();

            // Flush any remaining blobs.
            hat.data_flush().unwrap();
        }
        ("checkout", Some(cmd)) => {
            let name = cmd.value_of("NAME").unwrap().to_owned();
            let path = cmd.value_of("PATH").unwrap();

            let backend = Arc::new(backend::CmdBackend::new());
            let mut hat = hat::Hat::open_repository(cache_dir, backend, MAX_BLOB_SIZE).unwrap();

            hat.checkout_in_dir(name, PathBuf::from(path)).unwrap();
        }
        ("recover", Some(_cmd)) => {
            let backend = Arc::new(backend::CmdBackend::new());
            let mut hat = hat::Hat::open_repository(cache_dir, backend, MAX_BLOB_SIZE).unwrap();

            hat.recover().unwrap();
        }
        ("delete", Some(cmd)) => {
            let name = cmd.value_of("NAME").unwrap().to_owned();
            let id = cmd.value_of("ID").unwrap().to_owned();

            let backend = Arc::new(backend::CmdBackend::new());
            let mut hat = hat::Hat::open_repository(cache_dir, backend, MAX_BLOB_SIZE).unwrap();

            hat.deregister_by_name(name, id.parse::<u64>().unwrap())
                .unwrap();
        }
        ("gc", Some(_cmd)) => {
            let backend = Arc::new(backend::CmdBackend::new());
            let mut hat = hat::Hat::open_repository(cache_dir, backend, MAX_BLOB_SIZE).unwrap();
            let (deleted_hashes, live_blobs) = hat.gc().unwrap();
            println!("Deleted hashes: {:?}", deleted_hashes);
            println!("Live data blobs after deletion: {:?}", live_blobs);
        }
        ("mount", Some(cmd)) => {
            let path = cmd.value_of("PATH").unwrap();
            let backend = Arc::new(backend::CmdBackend::new());

            let hat = hat::Hat::open_repository(cache_dir, backend, MAX_BLOB_SIZE).unwrap();
            hat::vfs::Fuse::new(hat).mount(&path).unwrap();
        }
        ("ls", Some(cmd)) => {
            let path: PathBuf = cmd.value_of("PATH").unwrap().into();
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
                    hat::vfs::fs::List::Snapshots(snapshots) => for si in snapshots {
                        println!(
                            "{}",
                            PathBuf::from(si.family_name)
                                .join(format!("{}", si.info.snapshot_id))
                                .display()
                        );
                    },
                    hat::vfs::fs::List::Dir(files) => for (entry, _) in files {
                        let name_os_string: ffi::OsString = entry.info.name.into();
                        println!("{}", path.join(name_os_string).display());
                    },
                }
            }
        }
        _ => {
            println!(
                "No subcommand specified\n{}\nFor more information re-run with --help",
                matches.usage()
            );
            std::process::exit(1);
        }
    }
}
