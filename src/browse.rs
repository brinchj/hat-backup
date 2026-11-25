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

use crate::{format_entry_mode, format_unix_ts_utc, format_utc_datetime};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{
    self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, queue};
use hat::backend::StoreBackend;
use hat::hat::walker::Content;
use hat::hat::HatRc;
use hat::vfs::fs::{Filesystem, List, WalkItem};
use hat::HatError;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowseItemKind {
    ParentDir,
    Family,
    Snapshot { family: String, snapshot_id: u64 },
    Dir,
    File,
    Symlink { target: PathBuf },
}

impl BrowseItemKind {
    pub fn is_selectable(&self) -> bool {
        matches!(
            self,
            BrowseItemKind::Snapshot { .. }
                | BrowseItemKind::Dir
                | BrowseItemKind::File
                | BrowseItemKind::Symlink { .. }
        )
    }
}

#[derive(Clone, Debug)]
pub struct BrowseItem {
    pub kind: BrowseItemKind,
    pub label: String,
    pub full_path: PathBuf,
    pub mode: String,
    pub size: Option<u64>,
    pub timestamp: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedCheckout {
    /// Hat VFS path to pass to `Hat::checkout_path_in_dir`.
    pub snapshot_path: PathBuf,
    /// Directory argument passed as `output_dir` to `Hat::checkout_path_in_dir`.
    pub checkout_target_dir: PathBuf,
    /// Concrete path on local disk that will be created/written.
    pub display_dest: PathBuf,
    /// Whether `display_dest` already exists on disk.
    pub already_exists: bool,
}

pub enum Mode {
    Browse,
    FilterInput {
        buffer: String,
    },
    FindInput {
        buffer: String,
    },
    FindResults {
        query: String,
        base_path: PathBuf,
        results: Vec<BrowseItem>,
    },
    ConfirmCheckout {
        plan: Vec<PlannedCheckout>,
    },
    Help,
}

pub struct BrowserState<B: StoreBackend> {
    fs: Filesystem<B>,
    dest_dir: PathBuf,
    cwd: Vec<OsString>,
    items: Vec<BrowseItem>,
    filter: String,
    cursor: usize,
    scroll: usize,
    selected: BTreeMap<PathBuf, BrowseItemKind>,
    mode: Mode,
    status: Option<String>,
    should_quit: bool,
}

fn path_from_parts(parts: &[OsString]) -> PathBuf {
    if parts.is_empty() {
        PathBuf::from("/")
    } else {
        let mut p = PathBuf::new();
        for part in parts {
            p.push(part);
        }
        p
    }
}

fn display_breadcrumb(parts: &[OsString]) -> String {
    if parts.is_empty() {
        "/".to_string()
    } else {
        let mut out = String::new();
        for part in parts {
            out.push('/');
            out.push_str(&part.to_string_lossy());
        }
        out
    }
}

fn parse_start_parts<B: StoreBackend>(
    fs: &mut Filesystem<B>,
    start_path: &Path,
) -> Result<Vec<OsString>, HatError> {
    let mut parts = Vec::new();
    for comp in start_path.components() {
        match comp {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(os) => parts.push(os.to_os_string()),
            Component::ParentDir | Component::Prefix(_) => {
                return Err(format!("invalid browse path '{}'", start_path.display()).into())
            }
        }
    }

    // Resolve any snapshot selector (`latest`, numeric ID, date prefix, or composite name)
    // into its canonical composite snapshot name so breadcrumbs and checkout paths stay canonical.
    if parts.len() >= 2 {
        let family = parts[0].to_string_lossy().into_owned();
        let selector = parts[1].to_string_lossy().into_owned();
        let family_snaps: Vec<_> = fs
            .hat_mut()
            .list_user_snapshots()
            .into_iter()
            .filter(|s| s.family_name == family && s.hash_ref.is_some())
            .collect();
        match hat::hat::match_snapshot_selector(&family_snaps, &selector) {
            hat::hat::SelectorLookup::Found(s) => {
                parts[1] = OsString::from(s.display_name());
            }
            hat::hat::SelectorLookup::Ambiguous(candidates) => {
                return Err(format!(
                    "Ambiguous snapshot selector '{}' in family '{}'; matches: {}",
                    selector,
                    family,
                    candidates.join(", ")
                )
                .into());
            }
            hat::hat::SelectorLookup::NotFound => {}
        }
    }

    Ok(parts)
}

fn format_size(size: Option<u64>) -> String {
    match size {
        None => "-".to_string(),
        Some(n) if n < 1024 => format!("{}B", n),
        Some(n) if n < 1024 * 1024 => format!("{:.1}K", n as f64 / 1024.0),
        Some(n) if n < 1024 * 1024 * 1024 => format!("{:.1}M", n as f64 / (1024.0 * 1024.0)),
        Some(n) => format!("{:.1}G", n as f64 / (1024.0 * 1024.0 * 1024.0)),
    }
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else if max_chars == 1 {
        "…".to_string()
    } else {
        let mut out: String = s.chars().take(max_chars - 1).collect();
        out.push('…');
        out
    }
}

/// Extract `(family, snapshot_id, relative_subpath_components)` from a canonical Hat VFS path.
fn split_snapshot_path(path: &Path) -> Option<(String, String, Vec<OsString>)> {
    let mut comps = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(os) => Some(os.to_os_string()),
            _ => None,
        });
    let family = comps.next()?.to_string_lossy().into_owned();
    let snap_id = comps.next()?.to_string_lossy().into_owned();
    let sub: Vec<OsString> = comps.collect();
    Some((family, snap_id, sub))
}

/// Plan local destination paths for one or more selected items without silently clobbering
/// colliding basenames from different subdirectories or different snapshots.
pub fn plan_checkouts(
    targets: &[(PathBuf, BrowseItemKind)],
    dest_dir: &Path,
) -> Vec<PlannedCheckout> {
    // First pass: count default basenames and snapshot-relative paths to detect collisions.
    let mut default_names: HashMap<OsString, usize> = HashMap::new();
    let mut rel_paths: HashMap<PathBuf, usize> = HashMap::new();
    let mut snap_rel_paths: HashMap<(String, PathBuf), usize> = HashMap::new();
    for (snap_path, kind) in targets {
        if let Some((family, snap_name, sub)) = split_snapshot_path(snap_path) {
            let key = match kind {
                BrowseItemKind::Snapshot { .. } => {
                    OsString::from(format!("{}-{}", family, snap_name))
                }
                _ => sub
                    .last()
                    .cloned()
                    .unwrap_or_else(|| OsString::from(format!("{}-{}", family, snap_name))),
            };
            *default_names.entry(key).or_insert(0) += 1;

            let mut rel = PathBuf::new();
            for c in &sub {
                rel.push(c);
            }
            *rel_paths.entry(rel.clone()).or_insert(0) += 1;
            *snap_rel_paths.entry((snap_name, rel)).or_insert(0) += 1;
        }
    }

    let mut plan = Vec::with_capacity(targets.len());
    for (snap_path, kind) in targets {
        let Some((family, snap_name, sub)) = split_snapshot_path(snap_path) else {
            continue;
        };

        let mut rel = PathBuf::new();
        for c in &sub {
            rel.push(c);
        }
        let rel_collides = rel_paths.get(&rel).copied().unwrap_or(0) > 1;
        let snap_rel_collides = snap_rel_paths
            .get(&(snap_name.clone(), rel.clone()))
            .copied()
            .unwrap_or(0)
            > 1;

        let (checkout_target_dir, display_dest) = match kind {
            BrowseItemKind::Snapshot { .. } => {
                let folder = format!("{}-{}", family, snap_name);
                let target = dest_dir.join(&folder);
                (target.clone(), target)
            }
            BrowseItemKind::Dir => {
                let base = sub
                    .last()
                    .cloned()
                    .unwrap_or_else(|| OsString::from(format!("{}-{}", family, snap_name)));
                let base_collides = default_names.get(&base).copied().unwrap_or(0) > 1;
                let target = if rel_collides {
                    let prefix = if snap_rel_collides {
                        format!("{}-{}", family, snap_name)
                    } else {
                        snap_name.clone()
                    };
                    dest_dir.join(prefix).join(&rel)
                } else if base_collides && sub.len() > 1 {
                    dest_dir.join(&rel)
                } else {
                    dest_dir.join(&base)
                };
                (target.clone(), target)
            }
            BrowseItemKind::File | BrowseItemKind::Symlink { .. } => {
                let Some(file_name) = sub.last().cloned() else {
                    continue;
                };
                let base_collides = default_names.get(&file_name).copied().unwrap_or(0) > 1;
                if rel_collides {
                    let prefix = if snap_rel_collides {
                        format!("{}-{}", family, snap_name)
                    } else {
                        snap_name.clone()
                    };
                    let mut parent = dest_dir.join(prefix);
                    for c in &sub[..sub.len() - 1] {
                        parent.push(c);
                    }
                    let display = parent.join(&file_name);
                    (parent, display)
                } else if base_collides && sub.len() > 1 {
                    let mut parent = dest_dir.to_path_buf();
                    for c in &sub[..sub.len() - 1] {
                        parent.push(c);
                    }
                    let display = parent.join(&file_name);
                    (parent, display)
                } else {
                    let display = dest_dir.join(&file_name);
                    (dest_dir.to_path_buf(), display)
                }
            }
            BrowseItemKind::ParentDir | BrowseItemKind::Family => continue,
        };

        let already_exists = fs::symlink_metadata(&display_dest).is_ok();
        plan.push(PlannedCheckout {
            snapshot_path: snap_path.clone(),
            checkout_target_dir,
            display_dest,
            already_exists,
        });
    }

    plan
}

impl<B: StoreBackend> BrowserState<B> {
    pub fn new(hat: HatRc<B>, start_path: &Path, dest_dir: PathBuf) -> Result<Self, HatError> {
        let mut fs = Filesystem::new(hat);
        let cwd = parse_start_parts(&mut fs, start_path)?;

        let mut state = BrowserState {
            fs,
            dest_dir,
            cwd,
            items: Vec::new(),
            filter: String::new(),
            cursor: 0,
            scroll: 0,
            selected: BTreeMap::new(),
            mode: Mode::Browse,
            status: None,
            should_quit: false,
        };

        state.reload(None)?;
        Ok(state)
    }

    pub fn current_path(&self) -> PathBuf {
        path_from_parts(&self.cwd)
    }

    #[cfg(test)]
    pub fn selected_count(&self) -> usize {
        self.selected.len()
    }

    #[cfg(test)]
    pub fn is_find_results(&self) -> bool {
        matches!(self.mode, Mode::FindResults { .. })
    }

    fn reload(&mut self, focus_path: Option<&Path>) -> Result<(), HatError> {
        let path = self.current_path();
        let Some(listing) = self.fs.ls(&path)? else {
            return Err(format!("path not found: {}", path.display()).into());
        };

        let mut items = Vec::new();
        if !self.cwd.is_empty() {
            let parent_path = if self.cwd.len() == 1 {
                PathBuf::from("/")
            } else {
                path_from_parts(&self.cwd[..self.cwd.len() - 1])
            };
            items.push(BrowseItem {
                kind: BrowseItemKind::ParentDir,
                label: "..".to_string(),
                full_path: parent_path,
                mode: "drwxr-xr-x".to_string(),
                size: None,
                timestamp: format!("{:>20}", ""),
            });
        }

        match listing {
            List::Root(snapshots) => {
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
                for (name, created) in families {
                    items.push(BrowseItem {
                        kind: BrowseItemKind::Family,
                        label: format!("{}/", name),
                        full_path: PathBuf::from(&name),
                        mode: "dr-xr-xr-x".to_string(),
                        size: None,
                        timestamp: format_utc_datetime(created),
                    });
                }
            }
            List::Snapshots(mut snapshots) => {
                // Show newest snapshots first for quick browsing.
                snapshots.sort_by(|a, b| b.info.snapshot_id.cmp(&a.info.snapshot_id));
                for si in snapshots {
                    let snap_name = si.display_name();
                    let full_path = PathBuf::from(&si.family_name).join(&snap_name);
                    items.push(BrowseItem {
                        kind: BrowseItemKind::Snapshot {
                            family: si.family_name,
                            snapshot_id: si.info.snapshot_id,
                        },
                        label: format!("{}/", snap_name),
                        full_path,
                        mode: "dr-xr-xr-x".to_string(),
                        size: None,
                        timestamp: format_utc_datetime(si.created),
                    });
                }
            }
            List::Dir(entries) => {
                // If `start_path` pointed directly at a single file rather than a directory,
                // move up to its parent directory and focus the file.
                if self.cwd.len() > 2
                    && entries.len() == 1
                    && !matches!(entries[0].1, Content::Dir(_))
                {
                    let only_name: OsString = entries[0].0.info.name.clone().into();
                    if self.cwd.last() == Some(&only_name) {
                        let target_file = self.current_path();
                        self.cwd.pop();
                        return self.reload(Some(&target_file));
                    }
                }

                let mut dir_items = Vec::new();
                let mut file_items = Vec::new();
                for (entry, content) in entries {
                    let name_os: OsString = entry.info.name.into();
                    let name_str = name_os.to_string_lossy().into_owned();
                    let full_path = path.join(&name_os);
                    let mode = format_entry_mode(&content, entry.info.permissions.as_ref());
                    let timestamp = format_unix_ts_utc(entry.info.modified_ts_secs);
                    match content {
                        Content::Dir(_) => dir_items.push(BrowseItem {
                            kind: BrowseItemKind::Dir,
                            label: format!("{}/", name_str),
                            full_path,
                            mode,
                            size: None,
                            timestamp,
                        }),
                        Content::Link(target) => file_items.push(BrowseItem {
                            kind: BrowseItemKind::Symlink {
                                target: target.clone(),
                            },
                            label: format!("{} -> {}", name_str, target.display()),
                            full_path,
                            mode,
                            size: entry.info.byte_length,
                            timestamp,
                        }),
                        Content::Data(_) => file_items.push(BrowseItem {
                            kind: BrowseItemKind::File,
                            label: name_str,
                            full_path,
                            mode,
                            size: entry.info.byte_length,
                            timestamp,
                        }),
                    }
                }
                dir_items.sort_by(|a, b| a.label.cmp(&b.label));
                file_items.sort_by(|a, b| a.label.cmp(&b.label));
                items.extend(dir_items);
                items.extend(file_items);
            }
        }

        self.items = items;
        self.filter.clear();
        self.scroll = 0;
        self.cursor = if let Some(target) = focus_path {
            self.items
                .iter()
                .position(|it| it.full_path == target)
                .unwrap_or(0)
        } else {
            0
        };

        Ok(())
    }

    pub fn visible_items(&self) -> Vec<&BrowseItem> {
        match &self.mode {
            Mode::FindResults { results, .. } => results.iter().collect(),
            Mode::FilterInput { buffer } => self.filtered_browse_items(buffer),
            _ => self.filtered_browse_items(&self.filter),
        }
    }

    fn filtered_browse_items(&self, needle: &str) -> Vec<&BrowseItem> {
        if needle.is_empty() {
            return self.items.iter().collect();
        }
        let lower = needle.to_lowercase();
        self.items
            .iter()
            .filter(|it| {
                it.kind == BrowseItemKind::ParentDir || it.label.to_lowercase().contains(&lower)
            })
            .collect()
    }

    fn clamp_cursor(&mut self) {
        let len = self.visible_items().len();
        if len == 0 {
            self.cursor = 0;
            self.scroll = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let len = self.visible_items().len();
        if len == 0 {
            self.cursor = 0;
            return;
        }
        let next = (self.cursor as isize + delta).clamp(0, (len - 1) as isize) as usize;
        self.cursor = next;
    }

    pub fn move_top(&mut self) {
        self.cursor = 0;
    }

    pub fn move_bottom(&mut self) {
        let len = self.visible_items().len();
        if len > 0 {
            self.cursor = len - 1;
        }
    }

    pub fn go_up(&mut self) {
        if matches!(self.mode, Mode::FindResults { .. }) {
            self.mode = Mode::Browse;
            self.clamp_cursor();
            self.status = None;
            return;
        }
        if self.cwd.is_empty() {
            return;
        }
        let prev = self.current_path();
        self.cwd.pop();
        if let Err(e) = self.reload(Some(&prev)) {
            self.status = Some(format!("Error: {}", e));
        } else {
            self.status = None;
        }
    }

    /// Switch to the previous (`delta = -1`, older) or next (`delta = 1`, newer) snapshot
    /// in the current family while staying in the same relative directory (or nearest existing
    /// ancestor if the directory does not exist in the target snapshot).
    pub fn switch_snapshot(&mut self, delta: isize) {
        if self.cwd.len() < 2 {
            self.status =
                Some("Enter a snapshot first to switch between snapshots with [ / ].".to_string());
            return;
        }

        if matches!(self.mode, Mode::FindResults { .. }) {
            self.mode = Mode::Browse;
        }

        let family = self.cwd[0].to_string_lossy().into_owned();
        let current_snap = self.cwd[1].to_string_lossy().into_owned();

        let mut snaps: Vec<_> = self
            .fs
            .hat_mut()
            .list_user_snapshots()
            .into_iter()
            .filter(|s| s.family_name == family && s.hash_ref.is_some())
            .collect();
        snaps.sort_by_key(|s| s.info.snapshot_id);

        if snaps.is_empty() {
            self.status = Some(format!("No snapshots in family '{}'.", family));
            return;
        }

        let Some(cur_idx) = snaps.iter().position(|s| s.display_name() == current_snap) else {
            self.status = Some(format!("Current snapshot '{}' not found.", current_snap));
            return;
        };

        let target_idx = cur_idx as isize + delta;
        if target_idx < 0 {
            self.status = Some(format!("Already at oldest snapshot (1/{}).", snaps.len()));
            return;
        }
        let target_idx = target_idx as usize;
        if target_idx >= snaps.len() {
            self.status = Some(format!(
                "Already at newest snapshot ({}/{}).",
                snaps.len(),
                snaps.len()
            ));
            return;
        }

        let target_name = snaps[target_idx].display_name();
        let prev_focus_name: Option<OsString> = self
            .visible_items()
            .get(self.cursor)
            .filter(|it| it.kind != BrowseItemKind::ParentDir)
            .and_then(|it| it.full_path.file_name().map(|n| n.to_os_string()));

        let orig_rel = self.cwd[2..].to_vec();
        let orig_rel_display = path_from_parts(&orig_rel).display().to_string();

        for keep in (0..=orig_rel.len()).rev() {
            let mut candidate_parts = Vec::with_capacity(2 + keep);
            candidate_parts.push(self.cwd[0].clone());
            candidate_parts.push(OsString::from(&target_name));
            candidate_parts.extend(orig_rel[..keep].iter().cloned());

            let candidate_path = path_from_parts(&candidate_parts);
            match self.fs.ls(&candidate_path) {
                Ok(Some(List::Dir(entries))) => {
                    // Guard against a path that was a directory in the previous snapshot
                    // but is a regular file or symlink in the target snapshot.
                    if keep > 0
                        && entries.len() == 1
                        && !matches!(entries[0].1, Content::Dir(_))
                    {
                        let only_name: OsString = entries[0].0.info.name.clone().into();
                        if only_name == orig_rel[keep - 1] {
                            continue;
                        }
                    }

                    self.cwd = candidate_parts;
                    let focus_path = if keep == orig_rel.len() {
                        prev_focus_name.as_ref().map(|n| candidate_path.join(n))
                    } else {
                        None
                    };
                    if let Err(e) = self.reload(focus_path.as_deref()) {
                        self.status = Some(format!("Error: {}", e));
                        return;
                    }

                    self.status = if keep == orig_rel.len() {
                        Some(format!(
                            "Snapshot {}/{}: {}",
                            target_idx + 1,
                            snaps.len(),
                            target_name
                        ))
                    } else if keep == 0 {
                        Some(format!(
                            "Snapshot {}/{}: {} ('{}' not present; moved to snapshot root)",
                            target_idx + 1,
                            snaps.len(),
                            target_name,
                            orig_rel_display
                        ))
                    } else {
                        let fallback_display =
                            path_from_parts(&orig_rel[..keep]).display().to_string();
                        Some(format!(
                            "Snapshot {}/{}: {} ('{}' not present; moved to '{}')",
                            target_idx + 1,
                            snaps.len(),
                            target_name,
                            orig_rel_display,
                            fallback_display
                        ))
                    };
                    return;
                }
                Ok(_) => continue,
                Err(e) => {
                    self.status = Some(format!("Error switching snapshot: {}", e));
                    return;
                }
            }
        }
    }

    pub fn enter(&mut self) {
        let Some(item) = self.visible_items().get(self.cursor).cloned().cloned() else {
            return;
        };

        if matches!(self.mode, Mode::FindResults { .. }) {
            // Jump straight to the selected find result in normal Browse mode.
            let parts: Vec<OsString> = item
                .full_path
                .components()
                .filter_map(|c| match c {
                    Component::Normal(os) => Some(os.to_os_string()),
                    _ => None,
                })
                .collect();
            self.mode = Mode::Browse;
            match item.kind {
                BrowseItemKind::Family | BrowseItemKind::Snapshot { .. } | BrowseItemKind::Dir => {
                    self.cwd = parts;
                    if let Err(e) = self.reload(None) {
                        self.status = Some(format!("Error: {}", e));
                    } else {
                        self.status = None;
                    }
                }
                BrowseItemKind::File | BrowseItemKind::Symlink { .. } => {
                    if parts.len() > 1 {
                        self.cwd = parts[..parts.len() - 1].to_vec();
                    } else {
                        self.cwd.clear();
                    }
                    if let Err(e) = self.reload(Some(&item.full_path)) {
                        self.status = Some(format!("Error: {}", e));
                    } else {
                        self.status = None;
                    }
                }
                BrowseItemKind::ParentDir => {}
            }
            return;
        }

        match item.kind {
            BrowseItemKind::ParentDir => self.go_up(),
            BrowseItemKind::Family | BrowseItemKind::Snapshot { .. } | BrowseItemKind::Dir => {
                let parts: Vec<OsString> = item
                    .full_path
                    .components()
                    .filter_map(|c| match c {
                        Component::Normal(os) => Some(os.to_os_string()),
                        _ => None,
                    })
                    .collect();
                self.cwd = parts;
                if let Err(e) = self.reload(None) {
                    self.status = Some(format!("Error: {}", e));
                } else {
                    self.status = None;
                }
            }
            BrowseItemKind::File | BrowseItemKind::Symlink { .. } => {
                self.status = Some("Press Space to select, or 'c' to checkout.".to_string());
            }
        }
    }

    pub fn toggle_select(&mut self) {
        let Some(item) = self.visible_items().get(self.cursor).cloned().cloned() else {
            return;
        };
        if !item.kind.is_selectable() {
            self.status = Some("Select a snapshot, directory, or file to checkout.".to_string());
            return;
        }
        if self.selected.remove(&item.full_path).is_none() {
            self.selected.insert(item.full_path, item.kind);
        }
        self.status = None;
        self.move_cursor(1);
    }

    pub fn toggle_select_all(&mut self) {
        let selectable: Vec<(PathBuf, BrowseItemKind)> = self
            .visible_items()
            .into_iter()
            .filter(|it| it.kind.is_selectable())
            .map(|it| (it.full_path.clone(), it.kind.clone()))
            .collect();
        if selectable.is_empty() {
            self.status = Some("No selectable items in this view.".to_string());
            return;
        }
        let all_selected = selectable
            .iter()
            .all(|(p, _)| self.selected.contains_key(p));
        if all_selected {
            for (p, _) in selectable {
                self.selected.remove(&p);
            }
        } else {
            for (p, kind) in selectable {
                self.selected.insert(p, kind);
            }
        }
        self.status = None;
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
        self.status = Some("Selection cleared.".to_string());
    }

    pub fn run_find(&mut self, query: String) {
        let base_path = match &self.mode {
            Mode::FindResults { base_path, .. } => base_path.clone(),
            _ => self.current_path(),
        };
        let found = match self.fs.find(&base_path, &query) {
            Ok(Some(items)) => items,
            Ok(None) => {
                self.mode = Mode::Browse;
                self.status = Some(format!("Path not found: {}", base_path.display()));
                return;
            }
            Err(e) => {
                self.mode = Mode::Browse;
                self.status = Some(format!("Find error: {}", e));
                return;
            }
        };

        let mut results = Vec::new();
        for item in found {
            match item {
                WalkItem::Family { name, created } => {
                    results.push(BrowseItem {
                        kind: BrowseItemKind::Family,
                        label: format!("{}/", name),
                        full_path: PathBuf::from(&name),
                        mode: "dr-xr-xr-x".to_string(),
                        size: None,
                        timestamp: format_utc_datetime(created),
                    });
                }
                WalkItem::Snapshot {
                    family,
                    snapshot_id,
                    path,
                    created,
                    ..
                } => {
                    let rel = path
                        .strip_prefix(&base_path)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned();
                    results.push(BrowseItem {
                        kind: BrowseItemKind::Snapshot {
                            family,
                            snapshot_id,
                        },
                        label: format!("{}/", rel),
                        full_path: path,
                        mode: "dr-xr-xr-x".to_string(),
                        size: None,
                        timestamp: format_utc_datetime(created),
                    });
                }
                WalkItem::Entry {
                    path,
                    entry,
                    content,
                } => {
                    let rel = path
                        .strip_prefix(&base_path)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned();
                    let mode = format_entry_mode(&content, entry.info.permissions.as_ref());
                    let timestamp = format_unix_ts_utc(entry.info.modified_ts_secs);
                    match content {
                        Content::Dir(_) => results.push(BrowseItem {
                            kind: BrowseItemKind::Dir,
                            label: format!("{}/", rel),
                            full_path: path,
                            mode,
                            size: None,
                            timestamp,
                        }),
                        Content::Link(target) => results.push(BrowseItem {
                            kind: BrowseItemKind::Symlink {
                                target: target.clone(),
                            },
                            label: format!("{} -> {}", rel, target.display()),
                            full_path: path,
                            mode,
                            size: entry.info.byte_length,
                            timestamp,
                        }),
                        Content::Data(_) => results.push(BrowseItem {
                            kind: BrowseItemKind::File,
                            label: rel,
                            full_path: path,
                            mode,
                            size: entry.info.byte_length,
                            timestamp,
                        }),
                    }
                }
            }
        }

        if results.is_empty() {
            self.mode = Mode::Browse;
            self.status = Some(format!(
                "No matches for '{}' under {}",
                query,
                base_path.display()
            ));
            return;
        }

        let count = results.len();
        self.mode = Mode::FindResults {
            query: query.clone(),
            base_path,
            results,
        };
        self.cursor = 0;
        self.scroll = 0;
        self.status = Some(format!(
            "Found {} match(es) for '{}' (Enter: jump, Space: mark, c: checkout, Esc: back)",
            count, query
        ));
    }

    pub fn begin_checkout(&mut self) {
        let targets: Vec<(PathBuf, BrowseItemKind)> = if !self.selected.is_empty() {
            self.selected
                .iter()
                .map(|(p, k)| (p.clone(), k.clone()))
                .collect()
        } else if let Some(item) = self.visible_items().get(self.cursor).cloned().cloned() {
            if item.kind.is_selectable() {
                vec![(item.full_path, item.kind)]
            } else {
                self.status =
                    Some("Move to a snapshot, directory, or file (or mark with Space).".to_string());
                return;
            }
        } else {
            self.status = Some("Nothing selected to checkout.".to_string());
            return;
        };

        let plan = plan_checkouts(&targets, &self.dest_dir);
        if plan.is_empty() {
            self.status = Some("Nothing selected to checkout.".to_string());
            return;
        }
        self.mode = Mode::ConfirmCheckout { plan };
        self.status = None;
    }

    pub fn confirm_checkout(&mut self) -> Result<usize, HatError> {
        let plan = match std::mem::replace(&mut self.mode, Mode::Browse) {
            Mode::ConfirmCheckout { plan } => plan,
            other => {
                self.mode = other;
                return Ok(0);
            }
        };

        let mut count = 0usize;
        for item in &plan {
            self.fs.hat_mut().checkout_path_in_dir(
                &item.snapshot_path,
                None,
                item.checkout_target_dir.clone(),
            )?;
            count += 1;
        }

        self.selected.clear();
        self.status = Some(format!(
            "Checked out {} item(s) into {}",
            count,
            self.dest_dir.display()
        ));
        self.clamp_cursor();
        Ok(count)
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }

        // Ctrl-C always exits cleanly.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        match &mut self.mode {
            Mode::Help => match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter => {
                    self.mode = Mode::Browse;
                }
                _ => {}
            },
            Mode::FilterInput { buffer } => match key.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.mode = Mode::Browse;
                    self.clamp_cursor();
                }
                KeyCode::Enter => {
                    self.filter = buffer.clone();
                    self.mode = Mode::Browse;
                    self.clamp_cursor();
                }
                KeyCode::Backspace => {
                    buffer.pop();
                    self.cursor = 0;
                }
                KeyCode::Char(ch) => {
                    buffer.push(ch);
                    self.cursor = 0;
                }
                _ => {}
            },
            Mode::FindInput { buffer } => match key.code {
                KeyCode::Esc => {
                    self.mode = Mode::Browse;
                    self.status = None;
                }
                KeyCode::Enter => {
                    let query = buffer.clone();
                    self.run_find(query);
                }
                KeyCode::Backspace => {
                    buffer.pop();
                }
                KeyCode::Char(ch) => {
                    buffer.push(ch);
                }
                _ => {}
            },
            Mode::ConfirmCheckout { .. } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    if let Err(e) = self.confirm_checkout() {
                        self.status = Some(format!("Checkout error: {}", e));
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.mode = Mode::Browse;
                    self.status = Some("Checkout cancelled.".to_string());
                }
                _ => {}
            },
            Mode::Browse | Mode::FindResults { .. } => match key.code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
                KeyCode::PageUp => self.move_cursor(-15),
                KeyCode::PageDown => self.move_cursor(15),
                KeyCode::Home | KeyCode::Char('g') => self.move_top(),
                KeyCode::End | KeyCode::Char('G') => self.move_bottom(),
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.enter(),
                KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.go_up(),
                KeyCode::Char('[') => self.switch_snapshot(-1),
                KeyCode::Char(']') => self.switch_snapshot(1),
                KeyCode::Char(' ') => self.toggle_select(),
                KeyCode::Char('a') => self.toggle_select_all(),
                KeyCode::Char('u') => self.clear_selection(),
                KeyCode::Char('c') => self.begin_checkout(),
                KeyCode::Char('?') => self.mode = Mode::Help,
                KeyCode::Char('/') => {
                    if matches!(self.mode, Mode::Browse) {
                        self.mode = Mode::FilterInput {
                            buffer: self.filter.clone(),
                        };
                    }
                }
                KeyCode::Char('f') => {
                    self.mode = Mode::FindInput {
                        buffer: String::new(),
                    };
                }
                KeyCode::Esc => {
                    if matches!(self.mode, Mode::FindResults { .. }) {
                        self.mode = Mode::Browse;
                        self.clamp_cursor();
                        self.status = None;
                    } else if !self.filter.is_empty() {
                        self.filter.clear();
                        self.clamp_cursor();
                    }
                }
                _ => {}
            },
        }
    }

    fn render<W: Write>(&mut self, out: &mut W, use_color: bool) -> io::Result<()> {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let cols = cols as usize;
        let rows = rows as usize;
        let list_height = rows.saturating_sub(4).max(1);

        // Adjust scroll window so cursor is always visible.
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + list_height {
            self.scroll = self.cursor + 1 - list_height;
        }

        queue!(out, MoveTo(0, 0), Clear(ClearType::All))?;

        // Line 0: Header bar.
        let header_left = match &self.mode {
            Mode::FindResults {
                query, base_path, ..
            } => format!(
                "hat browse  find '{}' in {}",
                query,
                base_path.display()
            ),
            _ => format!("hat browse  {}", display_breadcrumb(&self.cwd)),
        };
        let header_right = format!(
            "dest: {}  marked: {}",
            self.dest_dir.display(),
            self.selected.len()
        );
        let header = if header_left.chars().count() + header_right.chars().count() + 2 <= cols {
            let pad = cols - header_left.chars().count() - header_right.chars().count();
            format!("{}{}{}", header_left, " ".repeat(pad), header_right)
        } else {
            truncate_str(&format!("{} | {}", header_left, header_right), cols)
        };
        queue!(out, SetAttribute(Attribute::Reverse))?;
        write!(out, "{:<width$}", header, width = cols)?;
        queue!(out, SetAttribute(Attribute::Reset))?;

        // Line 1: Sub-header (filter or column titles).
        queue!(out, MoveTo(0, 1))?;
        if let Mode::FilterInput { buffer } = &self.mode {
            let line = format!("filter> {}_", buffer);
            write!(out, "{}", truncate_str(&line, cols))?;
        } else if !self.filter.is_empty() && matches!(self.mode, Mode::Browse) {
            let line = format!("filter: {} (Esc to clear)", self.filter);
            write!(out, "{}", truncate_str(&line, cols))?;
        } else {
            let col_hdr = format!(
                "    {:<10} {:>7}  {:<20}  {}",
                "MODE", "SIZE", "MODIFIED / CREATED", "NAME"
            );
            queue!(out, SetAttribute(Attribute::Dim))?;
            write!(out, "{}", truncate_str(&col_hdr, cols))?;
            queue!(out, SetAttribute(Attribute::Reset))?;
        }

        // Body lines (2 .. 2 + list_height).
        match &self.mode {
            Mode::Help => {
                let help_lines = [
                    "Keybindings:",
                    "  Up / Down, k / j      Move cursor",
                    "  Enter / Right / l     Open directory, snapshot, or jump to find match",
                    "  Backspace / Left / h  Go up one level (or exit find results)",
                    "  [ / ]                 Previous (older) / next (newer) snapshot in same dir",
                    "  Space                 Mark / unmark item for checkout (persists across dirs)",
                    "  a / u                 Toggle select all in view / clear all selections",
                    "  c                     Checkout marked items (or highlighted item) into dest",
                    "  f                     Recursive find by filename under current path",
                    "  /                     Filter current directory listing",
                    "  g / G, PgUp / PgDn    Jump to top / bottom / page",
                    "  ?                     Toggle this help",
                    "  q / Ctrl-C            Quit",
                    "",
                    "Press Esc, Enter, or '?' to close help.",
                ];
                for (i, line) in help_lines.iter().take(list_height).enumerate() {
                    queue!(out, MoveTo(0, (2 + i) as u16))?;
                    write!(out, "{}", truncate_str(line, cols))?;
                }
            }
            Mode::ConfirmCheckout { plan } => {
                let mut row = 2usize;
                queue!(out, MoveTo(0, row as u16), SetAttribute(Attribute::Bold))?;
                let title = format!(
                    "Checkout {} item(s) into {}?",
                    plan.len(),
                    self.dest_dir.display()
                );
                write!(out, "{}", truncate_str(&title, cols))?;
                queue!(out, SetAttribute(Attribute::Reset))?;
                row += 2;

                let max_preview = list_height.saturating_sub(4);
                for item in plan.iter().take(max_preview) {
                    queue!(out, MoveTo(0, row as u16))?;
                    let exists_tag = if item.already_exists {
                        " [exists: will overwrite]"
                    } else {
                        ""
                    };
                    let line = format!(
                        "  {} -> {}{}",
                        item.snapshot_path.display(),
                        item.display_dest.display(),
                        exists_tag
                    );
                    if item.already_exists && use_color {
                        queue!(out, SetForegroundColor(Color::Yellow))?;
                    }
                    write!(out, "{}", truncate_str(&line, cols))?;
                    if item.already_exists && use_color {
                        queue!(out, ResetColor)?;
                    }
                    row += 1;
                }
                if plan.len() > max_preview {
                    queue!(out, MoveTo(0, row as u16))?;
                    write!(out, "  ... and {} more item(s)", plan.len() - max_preview)?;
                    row += 1;
                }
                if row + 1 < 2 + list_height {
                    queue!(out, MoveTo(0, (row + 1) as u16), SetAttribute(Attribute::Bold))?;
                    write!(out, "Press 'y' or Enter to confirm, 'n' or Esc to cancel.")?;
                    queue!(out, SetAttribute(Attribute::Reset))?;
                }
            }
            _ => {
                let visible = self.visible_items();
                if visible.is_empty() {
                    queue!(out, MoveTo(0, 2), SetAttribute(Attribute::Dim))?;
                    write!(out, "  (empty)")?;
                    queue!(out, SetAttribute(Attribute::Reset))?;
                } else {
                    for (offset, item) in visible
                        .iter()
                        .skip(self.scroll)
                        .take(list_height)
                        .enumerate()
                    {
                        let idx = self.scroll + offset;
                        let is_cursor = idx == self.cursor;
                        let is_marked = self.selected.contains_key(&item.full_path);
                        let mark = if is_marked {
                            "[x]"
                        } else if item.kind.is_selectable() {
                            "[ ]"
                        } else {
                            "   "
                        };

                        let row_str = format!(
                            "{} {:<10} {:>7}  {:<20}  {}",
                            mark,
                            item.mode,
                            format_size(item.size),
                            item.timestamp,
                            item.label
                        );

                        queue!(out, MoveTo(0, (2 + offset) as u16))?;
                        if is_cursor {
                            queue!(out, SetAttribute(Attribute::Reverse))?;
                        } else if is_marked && use_color {
                            queue!(out, SetForegroundColor(Color::Green))?;
                        } else if use_color {
                            match item.kind {
                                BrowseItemKind::Dir
                                | BrowseItemKind::Family
                                | BrowseItemKind::Snapshot { .. }
                                | BrowseItemKind::ParentDir => {
                                    queue!(out, SetForegroundColor(Color::Cyan))?;
                                }
                                BrowseItemKind::Symlink { .. } => {
                                    queue!(out, SetForegroundColor(Color::Magenta))?;
                                }
                                BrowseItemKind::File => {}
                            }
                        }
                        let rendered = truncate_str(&row_str, cols);
                        if is_cursor {
                            write!(out, "{:<width$}", rendered, width = cols)?;
                        } else {
                            write!(out, "{}", rendered)?;
                        }
                        queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
                    }
                }
            }
        }

        // Second-to-last line: prompt or status message.
        if rows >= 3 {
            queue!(out, MoveTo(0, (rows - 2) as u16))?;
            if let Mode::FindInput { buffer } = &self.mode {
                let prompt = format!(
                    "find (under {})> {}_",
                    display_breadcrumb(&self.cwd),
                    buffer
                );
                queue!(out, SetAttribute(Attribute::Bold))?;
                write!(out, "{}", truncate_str(&prompt, cols))?;
                queue!(out, SetAttribute(Attribute::Reset))?;
            } else if let Some(msg) = &self.status {
                write!(out, "{}", truncate_str(msg, cols))?;
            }
        }

        // Bottom line: compact key legend.
        if rows >= 2 {
            queue!(
                out,
                MoveTo(0, (rows - 1) as u16),
                SetAttribute(Attribute::Reverse)
            )?;
            let legend = match &self.mode {
                Mode::FindInput { .. } => "Enter: search  Esc: cancel",
                Mode::FilterInput { .. } => "Enter: apply filter  Esc: clear",
                Mode::ConfirmCheckout { .. } => "y/Enter: confirm checkout  n/Esc: cancel",
                Mode::Help => "Esc/Enter/?: close help",
                Mode::FindResults { .. } => {
                    "Enter: jump  Space: mark  a: all  c: checkout  f: find again  Esc: back  q: quit"
                }
                Mode::Browse => {
                    "Enter: open  Bksp: up  [/]: prev/next snap  Space: mark  c: checkout  f: find  ?: help  q: quit"
                }
            };
            write!(out, "{:<width$}", truncate_str(legend, cols), width = cols)?;
            queue!(out, SetAttribute(Attribute::Reset))?;
        }

        out.flush()
    }
}

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(e);
        }
        Ok(RawModeGuard)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

pub fn run_browse<B: StoreBackend>(
    hat: HatRc<B>,
    start_path: &Path,
    dest_dir: PathBuf,
) -> Result<(), HatError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(
            "'hat browse' requires an interactive terminal (use 'hat ls' and 'hat checkout' in scripts)"
                .into(),
        );
    }

    let mut state = BrowserState::new(hat, start_path, dest_dir)?;
    let _guard = RawModeGuard::enter()?;
    let mut stdout = io::stdout();
    let use_color = env::var_os("NO_COLOR").is_none();

    loop {
        state.render(&mut stdout, use_color)?;
        if state.should_quit {
            break;
        }
        match event::read()? {
            Event::Key(key) => state.handle_key(key),
            Event::Resize(_, _) => {}
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hat::backend::FileBackend;
    use hat::crypto::keys::Keeper;
    use hat::Hat;
    use std::sync::Arc;

    fn make_test_hat() -> (HatRc<FileBackend>, tempfile::TempDir) {
        hat::crypto::keys::init_sodium();
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path().join("repo");
        let backend_dir = dir.path().join("backend");
        let src1 = dir.path().join("src1");
        let src2 = dir.path().join("src2");
        fs::create_dir_all(repo_dir.join("cache")).unwrap();
        fs::create_dir_all(&backend_dir).unwrap();
        Keeper::write_new_universal_key(&repo_dir).unwrap();
        fs::create_dir_all(src1.join("alpha/deep")).unwrap();
        fs::create_dir_all(src1.join("beta")).unwrap();
        fs::create_dir_all(src2.join("alpha")).unwrap();

        fs::write(src1.join("top.txt"), b"v1-top").unwrap();
        fs::write(src1.join("alpha/shared.txt"), b"from-alpha").unwrap();
        fs::write(src1.join("alpha/deep/needle_notes.txt"), b"deep-needle").unwrap();
        fs::write(src1.join("beta/shared.txt"), b"from-beta").unwrap();
        fs::write(src2.join("alpha/shared.txt"), b"v2-alpha").unwrap();

        let backend = Arc::new(FileBackend::new(backend_dir));
        let mut hat = Hat::open_repository(repo_dir, backend, 1024 * 1024).unwrap();

        let mut fam = hat.open_family("docs".to_string()).unwrap();
        fam.snapshot_dir(src1).unwrap();
        hat.commit(&mut fam).unwrap();
        hat.meta_commit().unwrap();

        let mut fam = hat.open_family("docs".to_string()).unwrap();
        fam.snapshot_dir(src2).unwrap();
        hat.commit(&mut fam).unwrap();
        hat.meta_commit().unwrap();

        (hat, dir)
    }

    #[test]
    fn browse_navigate_find_multi_select_and_checkout_with_collision_safety() {
        let (hat, tmp) = make_test_hat();
        let dest = tmp.path().join("cwd_out");
        fs::create_dir_all(&dest).unwrap();

        let mut state = BrowserState::new(hat, Path::new("/"), dest.clone()).unwrap();
        assert_eq!(state.current_path(), PathBuf::from("/"));
        assert_eq!(state.visible_items().len(), 1);
        assert_eq!(state.visible_items()[0].label, "docs/");

        // Enter family `docs` -> shows `..`, `<ts>-2/`, `<ts>-1/` (newest first).
        state.enter();
        assert_eq!(state.current_path(), PathBuf::from("docs"));
        let labels: Vec<String> = state
            .visible_items()
            .iter()
            .map(|i| i.label.clone())
            .collect();
        assert_eq!(labels.len(), 3);
        assert_eq!(labels[0], "..");
        assert!(labels[1].ends_with("Z-2/"), "got {:?}", labels);
        assert!(labels[2].ends_with("Z-1/"), "got {:?}", labels);
        let snap1_name = labels[2].trim_end_matches('/').to_string();

        // Move to snapshot 1 and enter it.
        state.move_cursor(2);
        state.enter();
        assert_eq!(state.current_path(), PathBuf::from("docs").join(&snap1_name));

        // Run recursive `find` for "shared" -> finds both alpha/shared.txt and beta/shared.txt!
        state.run_find("SHARED".to_string());
        assert!(state.is_find_results());
        assert_eq!(state.visible_items().len(), 2);

        // Select BOTH colliding `shared.txt` files directly from find results!
        state.toggle_select_all();
        assert_eq!(state.selected_count(), 2);

        // Also run find for "needle", mark `alpha/deep/needle_notes.txt`, and jump into it.
        state.run_find("needle".to_string());
        assert_eq!(state.visible_items().len(), 1);
        assert!(state.visible_items()[0]
            .label
            .contains("alpha/deep/needle_notes.txt"));
        state.toggle_select();
        assert_eq!(state.selected_count(), 3);

        // Enter jumps from find results to parent dir `docs/<snap1>/alpha/deep` focusing `needle_notes.txt`.
        state.enter();
        assert!(!state.is_find_results());
        assert_eq!(
            state.current_path(),
            PathBuf::from("docs").join(&snap1_name).join("alpha/deep")
        );

        // Go up to `docs/<snap1>/alpha` -> cursor restores onto `deep/`.
        state.go_up();
        assert_eq!(
            state.current_path(),
            PathBuf::from("docs").join(&snap1_name).join("alpha")
        );
        assert_eq!(state.visible_items()[state.cursor].label, "deep/");

        // Begin and confirm checkout of all 3 marked items!
        state.begin_checkout();
        let checked_out = state.confirm_checkout().unwrap();
        assert_eq!(checked_out, 3);
        assert_eq!(state.selected_count(), 0);

        // Colliding `shared.txt` files were placed under `alpha/shared.txt` and `beta/shared.txt`
        // instead of clobbering each other, while non-colliding `needle_notes.txt` went to cwd root!
        assert_eq!(fs::read(dest.join("alpha/shared.txt")).unwrap(), b"from-alpha");
        assert_eq!(fs::read(dest.join("beta/shared.txt")).unwrap(), b"from-beta");
        assert_eq!(fs::read(dest.join("needle_notes.txt")).unwrap(), b"deep-needle");
    }

    #[test]
    fn browse_checkout_directory_preserves_directory_name_in_cwd() {
        let (hat, tmp) = make_test_hat();
        let dest = tmp.path().join("cwd_dir");
        fs::create_dir_all(&dest).unwrap();

        // Start directly at `docs/1` and select directory `alpha/`.
        let mut state = BrowserState::new(hat, Path::new("docs/1"), dest.clone()).unwrap();
        let alpha_idx = state
            .visible_items()
            .iter()
            .position(|i| i.label == "alpha/")
            .unwrap();
        state.cursor = alpha_idx;
        state.toggle_select();

        state.begin_checkout();
        assert_eq!(state.confirm_checkout().unwrap(), 1);

        // Directory is checked out as `<dest>/alpha/...` instead of spilling loose files into `<dest>`.
        assert_eq!(fs::read(dest.join("alpha/shared.txt")).unwrap(), b"from-alpha");
        assert_eq!(
            fs::read(dest.join("alpha/deep/needle_notes.txt")).unwrap(),
            b"deep-needle"
        );
    }

    #[test]
    fn browse_start_path_resolves_latest_alias() {
        let (hat, tmp) = make_test_hat();
        let dest = tmp.path().join("cwd_latest");
        fs::create_dir_all(&dest).unwrap();

        let state = BrowserState::new(hat, Path::new("docs/latest"), dest).unwrap();
        let cur = state.current_path().to_string_lossy().into_owned();
        assert!(
            cur.starts_with("docs/") && cur.ends_with("Z-2"),
            "expected canonical composite snapshot path ending in Z-2, got {:?}",
            cur
        );
    }

    #[test]
    fn browse_switch_snapshot_stays_in_dir_and_handles_missing_subdir_and_cross_snap_checkout() {
        let (hat, tmp) = make_test_hat();
        let dest = tmp.path().join("cwd_switch");
        fs::create_dir_all(&dest).unwrap();

        // Start inside `docs/1/alpha`.
        let mut state = BrowserState::new(hat, Path::new("docs/1/alpha"), dest.clone()).unwrap();
        let snap1_dir = state.current_path();
        assert!(snap1_dir.to_string_lossy().ends_with("Z-1/alpha"));

        // Focus `shared.txt` and mark it in snapshot 1.
        let shared_idx = state
            .visible_items()
            .iter()
            .position(|i| i.label == "shared.txt")
            .unwrap();
        state.cursor = shared_idx;
        state.toggle_select();
        // Put cursor back on `shared.txt` before switching snapshot.
        state.cursor = shared_idx;

        // Trying `[` at oldest snapshot stays put with clear boundary message.
        state.switch_snapshot(-1);
        assert_eq!(state.current_path(), snap1_dir);
        assert!(state.status.as_deref().unwrap().contains("oldest snapshot"));

        // Press `]` to move to snapshot 2 while staying in `alpha/` with cursor on `shared.txt`!
        state.switch_snapshot(1);
        let snap2_dir = state.current_path();
        assert!(
            snap2_dir.to_string_lossy().ends_with("Z-2/alpha"),
            "expected to stay in alpha/ under snap 2, got {:?}",
            snap2_dir
        );
        assert_eq!(state.visible_items()[state.cursor].label, "shared.txt");

        // Mark `shared.txt` in snapshot 2 as well!
        state.toggle_select();
        assert_eq!(state.selected_count(), 2);

        // Press `]` at newest snapshot -> boundary status message.
        state.switch_snapshot(1);
        assert_eq!(state.current_path(), snap2_dir);
        assert!(state.status.as_deref().unwrap().contains("newest snapshot"));

        // Checkout BOTH versions of `alpha/shared.txt` across snapshots -> neither overwrites the other!
        state.begin_checkout();
        assert_eq!(state.confirm_checkout().unwrap(), 2);
        let snap1_name = snap1_dir
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_os_string();
        let snap2_name = snap2_dir
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_os_string();
        assert_eq!(
            fs::read(dest.join(&snap1_name).join("alpha/shared.txt")).unwrap(),
            b"from-alpha"
        );
        assert_eq!(
            fs::read(dest.join(&snap2_name).join("alpha/shared.txt")).unwrap(),
            b"v2-alpha"
        );

        // Switch back to snap 1, enter `deep/` (which does NOT exist in snap 2), then press `]`.
        state.switch_snapshot(-1);
        let deep_idx = state
            .visible_items()
            .iter()
            .position(|i| i.label == "deep/")
            .unwrap();
        state.cursor = deep_idx;
        state.enter();
        assert!(state.current_path().to_string_lossy().ends_with("Z-1/alpha/deep"));

        state.switch_snapshot(1);
        assert!(
            state.current_path().to_string_lossy().ends_with("Z-2/alpha"),
            "expected graceful fallback to nearest ancestor alpha/, got {:?}",
            state.current_path()
        );
        assert!(state.status.as_deref().unwrap().contains("not present"));
    }
}
