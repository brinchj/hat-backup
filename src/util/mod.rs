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

mod counter;
mod file_iterator;
mod listdir;
mod ordered_collection;
mod periodic_timer;
mod process;
mod sync_pool;
mod unique_priority_queue;

pub use self::counter::Counter;
pub use self::file_iterator::FileIterator;
pub use self::listdir::{HasPath, PathHandler};
pub use self::periodic_timer::PeriodicTimer;
pub use self::process::{MsgHandler, Process};
pub use self::sync_pool::{SyncPool, SyncPoolGuard};
pub use self::unique_priority_queue::UniquePriorityQueue;

/// Convert whole seconds since the Unix epoch (possibly negative) to a `SystemTime`.
fn unix_secs_to_system_time(secs: i64) -> std::time::SystemTime {
    use std::time::{Duration, UNIX_EPOCH};
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

/// Set access and modification times (in Unix seconds) on `path` without following symlinks.
pub fn set_times_nofollow(
    path: &std::path::Path,
    atime_secs: i64,
    mtime_secs: i64,
) -> std::io::Result<()> {
    let times = std::fs::FileTimes::new()
        .set_accessed(unix_secs_to_system_time(atime_secs))
        .set_modified(unix_secs_to_system_time(mtime_secs));
    std::fs::set_times_nofollow(path, times)
}

/// Convert a `SystemTime` to whole seconds since the Unix epoch (0 if before the epoch).
pub fn system_time_secs(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Validate that a single snapshot directory entry name is a normal filename
/// (non-empty, not `.` or `..`, no path separators or NUL bytes) to prevent path traversal.
pub fn validate_entry_name(name: &std::ffi::OsStr) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = name.as_bytes();
        if bytes.is_empty()
            || bytes == b"."
            || bytes == b".."
            || bytes.contains(&b'/')
            || bytes.contains(&0)
        {
            return Err(format!("unsafe checkout entry name: {:?}", name));
        }
    }
    let mut comps = std::path::Path::new(name).components();
    match (comps.next(), comps.next()) {
        (Some(std::path::Component::Normal(_)), None) => Ok(()),
        _ => Err(format!("unsafe checkout entry name: {:?}", name)),
    }
}
