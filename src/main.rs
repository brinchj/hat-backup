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

mod browse;

use clap::{Arg, ArgAction, ArgMatches, Command};
use hat::backend::{self, StoreBackend};
use hat::hat::HatRc;
use hat::Hat;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

static MAX_BLOB_SIZE: usize = 4 * 1024 * 1024;
static CONFIG_FILE_NAME: &str = "config.json";

#[derive(Clone, Copy)]
struct OutputConfig {
    quiet: bool,
    json: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RepoConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backend_path: Option<PathBuf>,
}

enum BackendSpec {
    File(PathBuf),
    Cmd,
}

fn use_stderr_color() -> bool {
    std::io::stderr().is_terminal() && env::var_os("NO_COLOR").is_none()
}

fn fail(msg: impl std::fmt::Display) -> ! {
    if use_stderr_color() {
        eprintln!("\x1b[1;31mError:\x1b[0m {}", msg);
    } else {
        eprintln!("Error: {}", msg);
    }
    std::process::exit(1);
}

fn print_json(value: &impl Serialize) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{}", s),
        Err(e) => fail(format!("failed to serialize JSON output: {}", e)),
    }
}

fn init_logging(quiet: bool, verbose: u8) {
    let mut builder = env_logger::Builder::new();
    if let Ok(spec) = env::var("RUST_LOG") {
        builder.parse_filters(&spec);
    } else {
        let level = if quiet {
            log::LevelFilter::Error
        } else {
            match verbose {
                0 => log::LevelFilter::Warn,
                1 => log::LevelFilter::Info,
                _ => log::LevelFilter::Debug,
            }
        };
        builder.filter_level(level);
    }
    builder.init();
}

fn license() {
    println!(include_str!("../LICENSE"));
    println!("clap (Command Line Argument Parser) License:");
    println!(include_str!("../LICENSE-CLAP"));
}

fn load_repo_config(state_dir: &Path) -> RepoConfig {
    let config_path = state_dir.join(CONFIG_FILE_NAME);
    match fs::read_to_string(&config_path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            fail(format!(
                "invalid repository config at {}: {}",
                config_path.display(),
                e
            ))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => RepoConfig::default(),
        Err(e) => fail(format!(
            "failed to read repository config {}: {}",
            config_path.display(),
            e
        )),
    }
}

fn write_repo_config(state_dir: &Path, cfg: &RepoConfig) {
    let config_path = state_dir.join(CONFIG_FILE_NAME);
    let data = serde_json::to_string_pretty(cfg)
        .unwrap_or_else(|e| fail(format!("failed to serialize config: {}", e)));

    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&config_path).unwrap_or_else(|e| {
        fail(format!(
            "failed to write repository config {}: {}",
            config_path.display(),
            e
        ))
    });
    file.write_all(data.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .unwrap_or_else(|e| {
            fail(format!(
                "failed to write repository config {}: {}",
                config_path.display(),
                e
            ))
        });
}

fn require_state_dir(matches: &ArgMatches) -> PathBuf {
    matches
        .get_one::<PathBuf>("state_dir")
        .cloned()
        .unwrap_or_else(|| {
            fail(
                "repository state directory required. Pass --state-dir <DIR>, set HAT_STATE_DIR, \
                 or create one with 'hat init <DIR>'",
            )
        })
}

fn resolve_backend(state_dir: &Path, matches: &ArgMatches) -> BackendSpec {
    let file_cfg = load_repo_config(state_dir);

    let cli_or_env_backend = matches.get_one::<String>("backend").cloned();
    let cli_or_env_path = matches.get_one::<PathBuf>("backend_path").cloned();

    let backend_path = cli_or_env_path.or(file_cfg.backend_path);
    let kind = cli_or_env_backend.or(file_cfg.backend).or_else(|| {
        if backend_path.is_some() {
            Some("file".to_string())
        } else {
            None
        }
    });

    match kind.as_deref() {
        Some("file") => {
            let path = backend_path.unwrap_or_else(|| {
                fail(
                    "--backend-path <DIR> or HAT_BACKEND_PATH is required for the file backend",
                )
            });
            BackendSpec::File(path)
        }
        Some("cmd") => BackendSpec::Cmd,
        Some(other) => fail(format!(
            "unsupported backend '{}' (expected 'file' or 'cmd')",
            other
        )),
        None => fail(
            "no storage backend configured. Pass --backend file --backend-path <DIR> \
             (or --backend cmd), set HAT_BACKEND/HAT_BACKEND_PATH, or pass them to 'hat init'",
        ),
    }
}

fn open_hat_cmd(state_dir: PathBuf) -> HatRc<backend::CmdBackend> {
    let backend = Arc::new(backend::CmdBackend::new());
    Hat::open_repository(state_dir, backend, MAX_BLOB_SIZE).unwrap_or_else(|e| fail(e))
}

fn open_hat_file(state_dir: PathBuf, backend_path: PathBuf) -> HatRc<backend::FileBackend> {
    let backend = Arc::new(backend::FileBackend::new(backend_path));
    Hat::open_repository(state_dir, backend, MAX_BLOB_SIZE).unwrap_or_else(|e| fail(e))
}

fn do_init(dir: PathBuf, matches: &ArgMatches, out: &OutputConfig) {
    let backend = matches.get_one::<String>("backend").cloned();
    let backend_path = matches.get_one::<PathBuf>("backend_path").cloned();

    let effective_backend = match (backend.as_deref(), backend_path.as_ref()) {
        (Some("file"), None) => {
            fail("--backend-path <DIR> is required when initializing with --backend file")
        }
        (Some("cmd"), Some(_)) => {
            fail("--backend-path cannot be used with --backend cmd")
        }
        (None, Some(_)) => Some("file".to_string()),
        (Some(k), _) => Some(k.to_string()),
        (None, None) => None,
    };

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
            fail(format!("directory already exists: {}", dir.display()));
        }
        fail(format!("failed to create directory {}: {}", dir.display(), e));
    }
    builder
        .create(dir.join("cache"))
        .unwrap_or_else(|e| fail(e));
    hat::crypto::keys::Keeper::write_new_universal_key(&dir).unwrap_or_else(|e| fail(e));

    if effective_backend.is_some() || backend_path.is_some() {
        let cfg = RepoConfig {
            backend: effective_backend.clone(),
            backend_path: backend_path.clone(),
        };
        write_repo_config(&dir, &cfg);
    }

    if out.json {
        print_json(&serde_json::json!({
            "state_dir": dir,
            "backend": effective_backend,
            "backend_path": backend_path,
        }));
    } else if !out.quiet {
        eprintln!("Initialized repository at {}", dir.display());
    }
}

fn do_commit<B: StoreBackend>(
    mut hat: HatRc<B>,
    family_name: String,
    source_dir: PathBuf,
    out: &OutputConfig,
) {
    if family_name == hat::hat::SYNTHETIC_ROOTS_FAMILY {
        fail(format!(
            "family name '{}' is reserved for internal metadata",
            family_name
        ));
    }
    let mut family = hat
        .open_family(family_name.clone())
        .unwrap_or_else(|e| fail(format!("could not open family '{}': {}", family_name, e)));
    family.snapshot_dir(source_dir).unwrap_or_else(|e| fail(e));
    hat.commit(&mut family).unwrap_or_else(|e| fail(e));
    hat.meta_commit().unwrap_or_else(|e| fail(e));
    hat.data_flush().unwrap_or_else(|e| fail(e));

    let committed = hat
        .list_user_snapshots()
        .into_iter()
        .filter(|s| s.family_name == family_name)
        .max_by_key(|s| s.info.snapshot_id);

    if out.json {
        print_json(&serde_json::json!({
            "family": family_name,
            "snapshot": committed.as_ref().map(|s| s.display_name()),
            "snapshot_id": committed.as_ref().map(|s| s.info.snapshot_id),
        }));
    } else if !out.quiet {
        match committed {
            Some(s) => eprintln!("Committed {}/{}", family_name, s.display_name()),
            None => eprintln!("Committed {}", family_name),
        }
    }
}

fn do_checkout<B: StoreBackend>(
    mut hat: HatRc<B>,
    snapshot_path: PathBuf,
    id: Option<u64>,
    dest_dir: PathBuf,
    out: &OutputConfig,
) {
    hat.checkout_path_in_dir(&snapshot_path, id, dest_dir.clone())
        .unwrap_or_else(|e| fail(e));

    if out.json {
        print_json(&serde_json::json!({
            "snapshot_path": snapshot_path,
            "id": id,
            "dest_dir": dest_dir,
        }));
    } else if !out.quiet {
        eprintln!(
            "Checked out {} to {}",
            snapshot_path.display(),
            dest_dir.display()
        );
    }
}

fn do_recover<B: StoreBackend>(mut hat: HatRc<B>, out: &OutputConfig) {
    hat.recover().unwrap_or_else(|e| fail(e));
    let snapshots = hat.list_user_snapshots();
    let family_count = snapshots
        .iter()
        .map(|s| s.family_name.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    let snapshot_count = snapshots.len();

    if out.json {
        print_json(&serde_json::json!({
            "snapshots": snapshot_count,
            "families": family_count,
        }));
    } else if !out.quiet {
        eprintln!(
            "Recovered {} snapshot(s) across {} family(ies).",
            snapshot_count, family_count
        );
    }
}

fn parse_delete_target(snapshot_arg: &str, selector_arg: Option<&str>) -> (String, String) {
    let path = Path::new(snapshot_arg);
    let mut parts = Vec::new();
    for comp in path.components() {
        match comp {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(os) => parts.push(os.to_string_lossy().into_owned()),
            Component::ParentDir | Component::Prefix(_) => {
                fail(format!("invalid snapshot target '{}'", snapshot_arg))
            }
        }
    }

    match (parts.as_slice(), selector_arg) {
        ([family, selector], None) => (family.clone(), selector.clone()),
        ([family], Some(selector)) => (family.clone(), selector.to_string()),
        ([_, _], Some(_)) => fail(format!(
            "snapshot selector specified twice (in '{}' and positional argument)",
            snapshot_arg
        )),
        ([family], None) => fail(format!(
            "missing snapshot selector. Use 'hat delete {}/<snapshot>' (name, ID, date prefix, or 'latest')",
            family
        )),
        _ => fail(format!(
            "invalid snapshot target '{}'. Expected '<family>/<snapshot>' or '<family> <snapshot>'",
            snapshot_arg
        )),
    }
}

fn do_delete<B: StoreBackend>(
    mut hat: HatRc<B>,
    family: String,
    selector: String,
    dry_run: bool,
    out: &OutputConfig,
) {
    if family == hat::hat::SYNTHETIC_ROOTS_FAMILY {
        fail(format!("snapshot '{}/{}' not found", family, selector));
    }

    let family_snaps: Vec<_> = hat
        .list_user_snapshots()
        .into_iter()
        .filter(|s| s.family_name == family)
        .collect();
    let (id, display_name) = match hat::hat::match_snapshot_selector(&family_snaps, &selector) {
        hat::hat::SelectorLookup::Found(s) => (s.info.snapshot_id, s.display_name()),
        hat::hat::SelectorLookup::Ambiguous(candidates) => fail(format!(
            "ambiguous snapshot selector '{}' in family '{}'; matches: {}",
            selector,
            family,
            candidates.join(", ")
        )),
        hat::hat::SelectorLookup::NotFound => {
            fail(format!("snapshot '{}/{}' not found", family, selector))
        }
    };

    if dry_run {
        if out.json {
            print_json(&serde_json::json!({
                "dry_run": true,
                "family": family,
                "snapshot": display_name,
                "snapshot_id": id,
            }));
        } else if !out.quiet {
            println!("Would delete {}/{}", family, display_name);
        }
        return;
    }

    hat.deregister_by_name(family.clone(), id)
        .unwrap_or_else(|e| fail(e));
    hat.meta_commit().unwrap_or_else(|e| fail(e));
    hat.data_flush().unwrap_or_else(|e| fail(e));

    if out.json {
        print_json(&serde_json::json!({
            "dry_run": false,
            "family": family,
            "snapshot": display_name,
            "snapshot_id": id,
        }));
    } else if !out.quiet {
        eprintln!(
            "Deleted {}/{}. Run 'hat gc' to reclaim unused blobs.",
            family, display_name
        );
    }
}

fn do_gc<B: StoreBackend>(mut hat: HatRc<B>, dry_run: bool, out: &OutputConfig) {
    let (deleted_hashes, live_blobs) = if dry_run {
        hat.gc_pretend().unwrap_or_else(|e| fail(e))
    } else {
        hat.gc().unwrap_or_else(|e| fail(e))
    };

    if out.json {
        print_json(&serde_json::json!({
            "dry_run": dry_run,
            "deleted_hashes": deleted_hashes,
            "live_data_blobs": live_blobs,
        }));
    } else if !out.quiet {
        if dry_run {
            println!(
                "Would delete {} unused hash(es) ({} live data blob(s) remaining).",
                deleted_hashes, live_blobs
            );
        } else {
            println!(
                "Deleted {} unused hash(es) ({} live data blob(s) remaining).",
                deleted_hashes, live_blobs
            );
        }
    }
}

fn do_check<B: StoreBackend>(hat: HatRc<B>, out: &OutputConfig) {
    match hat.check() {
        Ok(()) => {
            if out.json {
                print_json(&serde_json::json!({ "ok": true }));
            } else if !out.quiet {
                println!("check: ok");
            }
        }
        Err(e) => {
            if out.json {
                print_json(&serde_json::json!({
                    "ok": false,
                    "error": e.to_string(),
                }));
            }
            fail(format!("check failed: {}", e));
        }
    }
}

fn do_resume<B: StoreBackend>(mut hat: HatRc<B>, out: &OutputConfig) {
    // open_repository() already ran hat.resume(). Publish an updated root listing
    // and flush pending blobs so resumed commits/deletes survive cache loss.
    if !hat.list_snapshots().is_empty() {
        hat.meta_commit().unwrap_or_else(|e| fail(e));
        hat.data_flush().unwrap_or_else(|e| fail(e));
    }
    let snapshot_count = hat.list_user_snapshots().len();

    if out.json {
        print_json(&serde_json::json!({
            "snapshots": snapshot_count,
        }));
    } else if !out.quiet {
        eprintln!("Resumed repository ({} snapshot(s)).", snapshot_count);
    }
}

#[cfg(unix)]
fn format_unix_perm_bits(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    s.push(if mode & 0o400 != 0 { 'r' } else { '-' });
    s.push(if mode & 0o200 != 0 { 'w' } else { '-' });
    s.push(match (mode & 0o100 != 0, mode & 0o4000 != 0) {
        (true, true) => 's',
        (false, true) => 'S',
        (true, false) => 'x',
        (false, false) => '-',
    });
    s.push(if mode & 0o040 != 0 { 'r' } else { '-' });
    s.push(if mode & 0o020 != 0 { 'w' } else { '-' });
    s.push(match (mode & 0o010 != 0, mode & 0o2000 != 0) {
        (true, true) => 's',
        (false, true) => 'S',
        (true, false) => 'x',
        (false, false) => '-',
    });
    s.push(if mode & 0o004 != 0 { 'r' } else { '-' });
    s.push(if mode & 0o002 != 0 { 'w' } else { '-' });
    s.push(match (mode & 0o001 != 0, mode & 0o1000 != 0) {
        (true, true) => 't',
        (false, true) => 'T',
        (true, false) => 'x',
        (false, false) => '-',
    });
    s
}

pub(crate) fn format_entry_mode(
    content: &hat::hat::walker::Content,
    perms: Option<&fs::Permissions>,
) -> String {
    let kind = match content {
        hat::hat::walker::Content::Dir(_) => 'd',
        hat::hat::walker::Content::Link(_) => 'l',
        hat::hat::walker::Content::Data(_) => '-',
    };
    #[cfg(unix)]
    let perm_str = if let Some(p) = perms {
        use std::os::unix::fs::PermissionsExt;
        format_unix_perm_bits(p.mode())
    } else {
        "---------".to_string()
    };
    #[cfg(not(unix))]
    let perm_str = {
        let _ = perms;
        "---------".to_string()
    };
    format!("{}{}", kind, perm_str)
}

pub(crate) fn format_utc_datetime(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M:%SZ").to_string()
}

fn format_utc_rfc3339(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub(crate) fn format_unix_ts_utc(ts_secs: Option<i64>) -> String {
    ts_secs
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map(format_utc_datetime)
        .unwrap_or_else(|| format!("{:>20}", "-"))
}

fn format_unix_ts_rfc3339(ts_secs: Option<i64>) -> Option<String> {
    ts_secs
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map(format_utc_rfc3339)
}

fn entry_to_json(
    full_path: &Path,
    entry: &hat::KeyEntry,
    content: &hat::hat::walker::Content,
) -> serde_json::Value {
    let name_os_string: ffi::OsString = entry.info.name.clone().into();
    let name_str = name_os_string.to_string_lossy().into_owned();
    let mode = format_entry_mode(content, entry.info.permissions.as_ref());
    let kind = match content {
        hat::hat::walker::Content::Dir(_) => "dir",
        hat::hat::walker::Content::Link(_) => "symlink",
        hat::hat::walker::Content::Data(_) => "file",
    };
    let symlink_target = match content {
        hat::hat::walker::Content::Link(t) => Some(t.to_string_lossy().into_owned()),
        _ => None,
    };
    let mut obj = serde_json::json!({
        "kind": kind,
        "name": name_str,
        "path": full_path.to_string_lossy(),
        "mode": mode,
        "size": entry.info.byte_length,
        "mtime": format_unix_ts_rfc3339(entry.info.modified_ts_secs),
    });
    if let Some(target) = symlink_target {
        obj["symlink_target"] = serde_json::Value::String(target);
    }
    obj
}

fn print_entry_line(
    full_path: &Path,
    entry: &hat::KeyEntry,
    content: &hat::hat::walker::Content,
    long: bool,
) {
    if long {
        let mode = format_entry_mode(content, entry.info.permissions.as_ref());
        let size = entry
            .info
            .byte_length
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".to_string());
        let mtime = format_unix_ts_utc(entry.info.modified_ts_secs);
        match content {
            hat::hat::walker::Content::Link(target) => {
                println!(
                    "{} {:>10} {} {} -> {}",
                    mode,
                    size,
                    mtime,
                    full_path.display(),
                    target.display()
                );
            }
            _ => {
                println!(
                    "{} {:>10} {} {}",
                    mode,
                    size,
                    mtime,
                    full_path.display()
                );
            }
        }
    } else {
        println!("{}", full_path.display());
    }
}

fn do_ls_recursive<B: StoreBackend>(
    hat: HatRc<B>,
    path: PathBuf,
    long: bool,
    out: &OutputConfig,
) {
    let Some(items) = hat::vfs::Filesystem::new(hat)
        .walk(&path)
        .unwrap_or_else(|e| fail(e))
    else {
        fail(format!("path not found: {}", path.display()));
    };

    if out.json {
        let json_items: Vec<_> = items
            .into_iter()
            .map(|item| match item {
                hat::vfs::fs::WalkItem::Family { name, created } => serde_json::json!({
                    "kind": "family",
                    "name": name,
                    "path": name,
                    "created": format_utc_rfc3339(created),
                }),
                hat::vfs::fs::WalkItem::Snapshot {
                    family,
                    snapshot_id,
                    name,
                    path,
                    created,
                } => serde_json::json!({
                    "kind": "snapshot",
                    "family": family,
                    "name": name,
                    "snapshot_id": snapshot_id,
                    "path": path.to_string_lossy(),
                    "created": format_utc_rfc3339(created),
                }),
                hat::vfs::fs::WalkItem::Entry {
                    path,
                    entry,
                    content,
                } => entry_to_json(&path, &entry, &content),
            })
            .collect();
        print_json(&json_items);
        return;
    }

    for item in items {
        match item {
            hat::vfs::fs::WalkItem::Family { name, created } => {
                if long {
                    println!(
                        "dr-xr-xr-x {:>10} {} {}",
                        "-",
                        format_utc_datetime(created),
                        name
                    );
                } else {
                    println!("{}", name);
                }
            }
            hat::vfs::fs::WalkItem::Snapshot { path, created, .. } => {
                if long {
                    println!(
                        "dr-xr-xr-x {:>10} {} {}",
                        "-",
                        format_utc_datetime(created),
                        path.display()
                    );
                } else {
                    println!("{}", path.display());
                }
            }
            hat::vfs::fs::WalkItem::Entry {
                path,
                entry,
                content,
            } => print_entry_line(&path, &entry, &content, long),
        }
    }
}

fn do_ls<B: StoreBackend>(
    hat: HatRc<B>,
    path: PathBuf,
    long: bool,
    recursive: bool,
    out: &OutputConfig,
) {
    if recursive {
        do_ls_recursive(hat, path, long, out);
        return;
    }

    let Some(listing) = hat::vfs::Filesystem::new(hat)
        .ls(&path)
        .unwrap_or_else(|e| fail(e))
    else {
        fail(format!("path not found: {}", path.display()));
    };

    match listing {
        hat::vfs::fs::List::Root(snapshots) => {
            let mut families: BTreeMap<String, chrono::DateTime<chrono::Utc>> = BTreeMap::new();
            for s in snapshots {
                families
                    .entry(s.family_name)
                    .and_modify(|latest| {
                        if s.created > *latest {
                            *latest = s.created;
                        }
                    })
                    .or_insert(s.created);
            }
            if out.json {
                let items: Vec<_> = families
                    .into_iter()
                    .map(|(name, latest_created)| {
                        serde_json::json!({
                            "kind": "family",
                            "name": name,
                            "path": name,
                            "created": format_utc_rfc3339(latest_created),
                        })
                    })
                    .collect();
                print_json(&items);
            } else if long {
                for (name, latest_created) in families {
                    println!(
                        "dr-xr-xr-x {:>10} {} {}",
                        "-",
                        format_utc_datetime(latest_created),
                        name
                    );
                }
            } else {
                for name in families.keys() {
                    println!("{}", name);
                }
            }
        }
        hat::vfs::fs::List::Snapshots(snapshots) => {
            if out.json {
                let items: Vec<_> = snapshots
                    .into_iter()
                    .map(|si| {
                        let snap_name = si.display_name();
                        let snap_path = format!("{}/{}", si.family_name, snap_name);
                        serde_json::json!({
                            "kind": "snapshot",
                            "family": si.family_name,
                            "name": snap_name,
                            "snapshot_id": si.info.snapshot_id,
                            "path": snap_path,
                            "created": format_utc_rfc3339(si.created),
                        })
                    })
                    .collect();
                print_json(&items);
            } else {
                for si in snapshots {
                    let snap_path = PathBuf::from(&si.family_name).join(si.display_name());
                    if long {
                        println!(
                            "dr-xr-xr-x {:>10} {} {}",
                            "-",
                            format_utc_datetime(si.created),
                            snap_path.display()
                        );
                    } else {
                        println!("{}", snap_path.display());
                    }
                }
            }
        }
        hat::vfs::fs::List::Dir(files) => {
            if out.json {
                let items: Vec<_> = files
                    .into_iter()
                    .map(|(entry, content)| {
                        let name_os_string: ffi::OsString = entry.info.name.clone().into();
                        let full_path = path.join(&name_os_string);
                        entry_to_json(&full_path, &entry, &content)
                    })
                    .collect();
                print_json(&items);
            } else {
                for (entry, content) in files {
                    let name_os_string: ffi::OsString = entry.info.name.clone().into();
                    let full_path = path.join(name_os_string);
                    print_entry_line(&full_path, &entry, &content, long);
                }
            }
        }
    }
}

fn do_browse<B: StoreBackend>(hat: HatRc<B>, start_path: PathBuf, dest_dir: PathBuf) {
    browse::run_browse(hat, &start_path, dest_dir).unwrap_or_else(|e| fail(e));
}

fn build_cli() -> Command {
    Command::new("hat")
        .version(concat!("v", env!("CARGO_PKG_VERSION")))
        .about("Encrypted, deduplicated backup snapshots")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .after_help(
            "Examples:\n  \
             hat init ~/.hat --backend file --backend-path /mnt/backup/blobs\n  \
             export HAT_STATE_DIR=~/.hat\n  \
             hat commit docs ~/Documents\n  \
             hat ls -lR docs/latest\n  \
             hat browse docs/latest\n  \
             hat checkout docs/1/notes ./restored-notes\n  \
             hat delete docs/1 --dry-run\n  \
             hat gc --dry-run",
        )
        .arg(
            Arg::new("state_dir")
                .long("state-dir")
                .value_name("DIR")
                .value_parser(clap::value_parser!(PathBuf))
                .global(true)
                .env("HAT_STATE_DIR")
                .help("Path to Hat's local repository state directory"),
        )
        .arg(
            Arg::new("backend")
                .long("backend")
                .value_name("KIND")
                .value_parser(["file", "cmd"])
                .global(true)
                .env("HAT_BACKEND")
                .help("Storage backend kind"),
        )
        .arg(
            Arg::new("backend_path")
                .long("backend-path")
                .value_name("DIR")
                .value_parser(clap::value_parser!(PathBuf))
                .global(true)
                .env("HAT_BACKEND_PATH")
                .help("Blob storage directory for the file backend"),
        )
        .arg(
            Arg::new("quiet")
                .short('q')
                .long("quiet")
                .action(ArgAction::SetTrue)
                .global(true)
                .help("Suppress non-essential status and warning output"),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .action(ArgAction::Count)
                .global(true)
                .help("Increase logging verbosity (-v for info, -vv for debug)"),
        )
        .arg(
            Arg::new("json")
                .long("json")
                .action(ArgAction::SetTrue)
                .global(true)
                .help("Emit machine-readable JSON to stdout"),
        )
        .subcommand(
            Command::new("init")
                .about("Initialize a new repository state directory and key")
                .after_help(
                    "Examples:\n  \
                     hat init ~/.hat --backend file --backend-path /mnt/backup/blobs\n  \
                     hat init ~/.hat",
                )
                .arg(
                    Arg::new("DIR")
                        .required(true)
                        .value_parser(clap::value_parser!(PathBuf))
                        .help("New state directory to initialize"),
                ),
        )
        .subcommand(
            Command::new("commit")
                .about("Create a new snapshot of a local directory")
                .after_help("Example:\n  hat commit docs ~/Documents")
                .arg(
                    Arg::new("FAMILY")
                        .required(true)
                        .help("Snapshot family name"),
                )
                .arg(
                    Arg::new("SOURCE_DIR")
                        .required(true)
                        .value_parser(clap::value_parser!(PathBuf))
                        .help("Local directory to back up"),
                ),
        )
        .subcommand(
            Command::new("checkout")
                .about("Restore a snapshot or subpath into a local destination directory")
                .after_help(
                    "Examples:\n  \
                     hat checkout docs ./restore\n  \
                     hat checkout docs/1 ./restore\n  \
                     hat checkout docs/latest/subdir ./restore",
                )
                .arg(
                    Arg::new("SNAPSHOT_PATH")
                        .required(true)
                        .value_parser(clap::value_parser!(PathBuf))
                        .help(
                            "Snapshot path: <family>, <family>/<snapshot>, or <family>/<snapshot>/<subpath> \
                             (<snapshot> may be full name, numeric ID, date prefix, or 'latest')",
                        ),
                )
                .arg(
                    Arg::new("DEST_DIR")
                        .required(true)
                        .value_parser(clap::value_parser!(PathBuf))
                        .help("Local destination directory to restore into"),
                )
                .arg(
                    Arg::new("id")
                        .long("id")
                        .value_name("ID")
                        .value_parser(clap::value_parser!(u64))
                        .help("Snapshot ID to checkout when SNAPSHOT_PATH is a family name"),
                ),
        )
        .subcommand(
            Command::new("ls")
                .about("List snapshot families, snapshots, or files inside a snapshot")
                .after_help(
                    "Examples:\n  \
                     hat ls\n  \
                     hat ls -l docs\n  \
                     hat ls -lR docs/latest",
                )
                .arg(
                    Arg::new("long")
                        .short('l')
                        .long("long")
                        .action(ArgAction::SetTrue)
                        .help("Use a long listing format with mode, size, and timestamps"),
                )
                .arg(
                    Arg::new("recursive")
                        .short('R')
                        .long("recursive")
                        .action(ArgAction::SetTrue)
                        .help("List subdirectories and snapshots recursively"),
                )
                .arg(
                    Arg::new("PATH")
                        .default_value("/")
                        .value_parser(clap::value_parser!(PathBuf))
                        .help("Path inside Hat to list"),
                ),
        )
        .subcommand(
            Command::new("browse")
                .about("Interactively navigate snapshots, find files, and checkout into local directory")
                .after_help(
                    "Examples:\n  \
                     hat browse\n  \
                     hat browse docs/latest\n  \
                     hat browse docs/1 --dest ./restored\n\n\
                     Keys inside browse:\n  \
                     Arrows / j k : move   Enter / l : open   Bksp / h : up\n  \
                     [ / ] : prev/next snapshot in same dir\n  \
                     Space : mark item     f : recursive find  / : filter\n  \
                     c : checkout marked   ? : help            q : quit",
                )
                .arg(
                    Arg::new("PATH")
                        .default_value("/")
                        .value_parser(clap::value_parser!(PathBuf))
                        .help("Starting path inside Hat (defaults to /)"),
                )
                .arg(
                    Arg::new("dest")
                        .short('d')
                        .long("dest")
                        .value_name("DIR")
                        .value_parser(clap::value_parser!(PathBuf))
                        .help("Local destination directory for checkouts (defaults to current directory)"),
                ),
        )
        .subcommand(
            Command::new("delete")
                .about("Delete a snapshot")
                .after_help(
                    "Examples:\n  \
                     hat delete docs/1 --dry-run\n  \
                     hat delete docs/1",
                )
                .arg(
                    Arg::new("SNAPSHOT")
                        .required(true)
                        .help("Snapshot target as <family>/<snapshot> (full name, ID, date prefix, or 'latest')"),
                )
                .arg(
                    Arg::new("ID")
                        .help("Snapshot selector when SNAPSHOT is a family name"),
                )
                .arg(
                    Arg::new("dry_run")
                        .short('n')
                        .long("dry-run")
                        .action(ArgAction::SetTrue)
                        .help("Show what would be deleted without modifying the repository"),
                ),
        )
        .subcommand(
            Command::new("gc")
                .about("Garbage-collect unreferenced hashes and data blobs")
                .after_help("Examples:\n  hat gc --dry-run\n  hat gc")
                .arg(
                    Arg::new("dry_run")
                        .short('n')
                        .long("dry-run")
                        .action(ArgAction::SetTrue)
                        .help("Report what would be deleted without modifying any data"),
                ),
        )
        .subcommand(
            Command::new("check").about("Verify all live hash chunks against the storage backend"),
        )
        .subcommand(
            Command::new("recover")
                .about("Rebuild local cache from committed snapshots on the storage backend"),
        )
        .subcommand(
            Command::new("resume")
                .about("Clean up incomplete local state and republish snapshot metadata"),
        )
        .subcommand(
            Command::new("mount")
                .about("Mount Hat snapshots as a read-only FUSE filesystem")
                .arg(
                    Arg::new("MOUNTPOINT")
                        .required(true)
                        .value_parser(clap::value_parser!(PathBuf))
                        .help("Local directory mountpoint"),
                ),
        )
        .subcommand(Command::new("license").about("Display software license information"))
}

fn main() {
    hat::crypto::keys::init_sodium();

    let matches = build_cli().get_matches();
    let out = OutputConfig {
        quiet: matches.get_flag("quiet"),
        json: matches.get_flag("json"),
    };
    init_logging(out.quiet, matches.get_count("verbose"));

    match matches.subcommand() {
        Some(("license", _)) => {
            license();
            return;
        }
        Some(("init", cmd)) => {
            let dir = cmd
                .get_one::<PathBuf>("DIR")
                .expect("required DIR")
                .clone();
            do_init(dir, &matches, &out);
            return;
        }
        _ => {}
    }

    let state_dir = require_state_dir(&matches);
    let backend = resolve_backend(&state_dir, &matches);

    match matches.subcommand() {
        Some(("resume", _)) => match backend {
            BackendSpec::File(p) => do_resume(open_hat_file(state_dir, p), &out),
            BackendSpec::Cmd => do_resume(open_hat_cmd(state_dir), &out),
        },
        Some(("commit", cmd)) => {
            let family = cmd.get_one::<String>("FAMILY").unwrap().to_owned();
            let source_dir = cmd.get_one::<PathBuf>("SOURCE_DIR").unwrap().clone();
            match backend {
                BackendSpec::File(p) => {
                    do_commit(open_hat_file(state_dir, p), family, source_dir, &out)
                }
                BackendSpec::Cmd => do_commit(open_hat_cmd(state_dir), family, source_dir, &out),
            }
        }
        Some(("checkout", cmd)) => {
            let snapshot_path = cmd.get_one::<PathBuf>("SNAPSHOT_PATH").unwrap().clone();
            let dest_dir = cmd.get_one::<PathBuf>("DEST_DIR").unwrap().clone();
            let id = cmd.get_one::<u64>("id").copied();
            match backend {
                BackendSpec::File(p) => {
                    do_checkout(open_hat_file(state_dir, p), snapshot_path, id, dest_dir, &out)
                }
                BackendSpec::Cmd => {
                    do_checkout(open_hat_cmd(state_dir), snapshot_path, id, dest_dir, &out)
                }
            }
        }
        Some(("recover", _)) => match backend {
            BackendSpec::File(p) => do_recover(open_hat_file(state_dir, p), &out),
            BackendSpec::Cmd => do_recover(open_hat_cmd(state_dir), &out),
        },
        Some(("delete", cmd)) => {
            let snapshot_arg = cmd.get_one::<String>("SNAPSHOT").unwrap();
            let id_arg = cmd.get_one::<String>("ID").map(|s| s.as_str());
            let (family, selector) = parse_delete_target(snapshot_arg, id_arg);
            let dry_run = cmd.get_flag("dry_run");
            match backend {
                BackendSpec::File(p) => {
                    do_delete(open_hat_file(state_dir, p), family, selector, dry_run, &out)
                }
                BackendSpec::Cmd => {
                    do_delete(open_hat_cmd(state_dir), family, selector, dry_run, &out)
                }
            }
        }
        Some(("gc", cmd)) => {
            let dry_run = cmd.get_flag("dry_run");
            match backend {
                BackendSpec::File(p) => do_gc(open_hat_file(state_dir, p), dry_run, &out),
                BackendSpec::Cmd => do_gc(open_hat_cmd(state_dir), dry_run, &out),
            }
        }
        Some(("check", _)) => match backend {
            BackendSpec::File(p) => do_check(open_hat_file(state_dir, p), &out),
            BackendSpec::Cmd => do_check(open_hat_cmd(state_dir), &out),
        },
        Some(("mount", cmd)) => {
            #[cfg(feature = "fuse")]
            {
                let mountpoint = cmd.get_one::<PathBuf>("MOUNTPOINT").unwrap();
                match backend {
                    BackendSpec::File(p) => {
                        let hat = open_hat_file(state_dir, p);
                        hat::vfs::Fuse::new(hat)
                            .mount(mountpoint)
                            .unwrap_or_else(|e| fail(e));
                    }
                    BackendSpec::Cmd => {
                        let hat = open_hat_cmd(state_dir);
                        hat::vfs::Fuse::new(hat)
                            .mount(mountpoint)
                            .unwrap_or_else(|e| fail(e));
                    }
                }
            }
            #[cfg(not(feature = "fuse"))]
            {
                let _ = (cmd, state_dir, backend);
                fail(
                    "FUSE support was not enabled at build time. Rebuild with '--features fuse'",
                );
            }
        }
        Some(("ls", cmd)) => {
            let path = cmd.get_one::<PathBuf>("PATH").unwrap().clone();
            let long = cmd.get_flag("long");
            let recursive = cmd.get_flag("recursive");
            match backend {
                BackendSpec::File(p) => {
                    do_ls(open_hat_file(state_dir, p), path, long, recursive, &out)
                }
                BackendSpec::Cmd => do_ls(open_hat_cmd(state_dir), path, long, recursive, &out),
            }
        }
        Some(("browse", cmd)) => {
            let start_path = cmd.get_one::<PathBuf>("PATH").unwrap().clone();
            let dest_dir = cmd
                .get_one::<PathBuf>("dest")
                .cloned()
                .unwrap_or_else(|| env::current_dir().unwrap_or_else(|e| fail(e)));
            match backend {
                BackendSpec::File(p) => do_browse(open_hat_file(state_dir, p), start_path, dest_dir),
                BackendSpec::Cmd => do_browse(open_hat_cmd(state_dir), start_path, dest_dir),
            }
        }
        _ => unreachable!("clap enforces subcommand_required(true)"),
    }
}
