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

use crate::backend::StoreBackend;
use crate::crypto::CipherText;
use hex::{self, FromHex};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

type ReadCache = BTreeMap<Vec<u8>, Result<Option<Vec<u8>>, String>>;

pub struct FileBackend {
    root: PathBuf,
    read_cache: Mutex<ReadCache>,
    max_cache_size: usize,
}

impl FileBackend {
    pub fn new(root: PathBuf) -> FileBackend {
        FileBackend {
            root,
            read_cache: Mutex::new(BTreeMap::new()),
            // Checkout/`check` touch many blobs; a tiny cache thrashed on FS-shaped trees.
            max_cache_size: 64,
        }
    }

    fn guarded_cache_get(&self, name: &[u8]) -> Option<Result<Option<Vec<u8>>, String>> {
        match self.read_cache.lock() {
            Err(e) => Some(Err(e.to_string())),
            Ok(cache) => cache.get(name).cloned(),
        }
    }

    fn get(&self, name: &[u8]) -> Result<Option<Vec<u8>>, String> {
        use self::io::Read;

        // Read key:
        let path = {
            let mut p = self.root.clone();
            p.push(hex::encode(name));
            p
        };

        match fs::File::open(&path) {
            Err(_) => Ok(None),
            Ok(mut fd) => {
                let mut buf = Vec::new();
                match fd.read_to_end(&mut buf) {
                    Ok(_) => Ok(Some(buf)),
                    Err(e) => Err(e.to_string()),
                }
            }
        }
    }

    fn guarded_cache_delete(&self, name: &[u8]) {
        self.read_cache.lock().unwrap().remove(name);
    }

    fn guarded_cache_put(&self, name: Vec<u8>, result: Result<Option<Vec<u8>>, String>) {
        let mut cache = self.read_cache.lock().unwrap();
        if cache.len() >= self.max_cache_size {
            cache.clear();
        }
        cache.insert(name, result);
    }
}

/// Prefix for in-progress writes. Never valid hex, so `list` skips it.
const TMP_PREFIX: &str = ".tmp-";

/// Force written bytes to stable storage, ordered before any later write.
///
/// On macOS, `fsync` does not flush the drive cache. `F_BARRIERFSYNC` orders
/// this write before later writes without the full cache flush that
/// `F_FULLFSYNC` pays for. Elsewhere, `sync_all` is the barrier.
fn sync_to_stable(file: &fs::File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_BARRIERFSYNC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        file.sync_all()
    }
}

impl FileBackend {
    fn blob_path(&self, name: &[u8]) -> PathBuf {
        self.root.join(hex::encode(name))
    }

    fn tmp_path(&self, name: &[u8]) -> PathBuf {
        self.root.join(format!("{}{}", TMP_PREFIX, hex::encode(name)))
    }

    /// Write the blob under its temp name and sync the bytes.
    fn write_tmp(&self, tmp: &Path, data: &CipherText) -> io::Result<()> {
        use self::io::Write;

        let mut file = fs::File::create(tmp)?;
        for r in data.slices() {
            file.write_all(r)?;
        }
        sync_to_stable(&file)
    }
}

impl StoreBackend for FileBackend {
    /// Publish a blob atomically and durably, then call `done`.
    ///
    /// Order: write temp, sync bytes, rename to the final name, sync the
    /// directory, then `done`. `done` commits the hash ref, so a ref can only
    /// point at a blob already on disk. A failure before the rename leaves no
    /// final name behind.
    fn store(&self, name: &[u8], data: CipherText, done: Box<dyn FnOnce() + Send>) -> Result<(), String> {
        let tmp = self.tmp_path(name);
        let dst = self.blob_path(name);

        if let Err(e) = self.write_tmp(&tmp, &data) {
            let _ = fs::remove_file(&tmp);
            return Err(e.to_string());
        }
        if let Err(e) = fs::rename(&tmp, &dst) {
            let _ = fs::remove_file(&tmp);
            return Err(e.to_string());
        }
        // The rename is only durable once the directory entry is synced.
        let dir = fs::File::open(&self.root).map_err(|e| e.to_string())?;
        sync_to_stable(&dir).map_err(|e| e.to_string())?;

        done();
        Ok(())
    }

    fn retrieve(&self, name: &[u8]) -> Result<Option<Vec<u8>>, String> {
        // Check for key in cache:
        let value_opt = self.guarded_cache_get(name);
        if let Some(r) = value_opt {
            return r;
        }

        let res = self.get(name);

        // Update cache to contain key:
        self.guarded_cache_put(name.to_vec(), res.clone());

        res
    }

    fn delete(&self, name: &[u8]) -> Result<(), String> {
        let name = name.to_vec();
        self.guarded_cache_delete(&name);

        let path = {
            let mut p = self.root.clone();
            p.push(hex::encode(&name));
            p
        };

        match fs::remove_file(&path) {
            Ok(_) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn list(&self) -> Result<Vec<Box<[u8]>>, String> {
        let es = &|e: io::Error| e.to_string();

        let mut out = vec![];
        for p in fs::read_dir(&self.root).map_err(es)? {
            if let Some(name) = p.map_err(es)?.path().file_name() {
                // Skip in-progress temp writes and any name that is not a blob id.
                // Parsing with `unwrap` here would panic recovery on a stray file.
                if let Some(b) = name
                    .to_str()
                    .filter(|s| !s.starts_with(TMP_PREFIX))
                    .and_then(|s| Vec::from_hex(s).ok())
                {
                    out.push(b.into_boxed_slice())
                }
            }
        }
        Ok(out)
    }

    fn flush(&self) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn done_runs_only_after_final_name_holds_full_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let backend = FileBackend::new(root.clone());

        let final_path = backend.blob_path(b"blob-1");
        let tmp_path = backend.tmp_path(b"blob-1");
        let seen = Arc::new(AtomicBool::new(false));
        let seen_cb = seen.clone();
        let check_path = final_path.clone();
        let check_tmp = tmp_path.clone();

        backend
            .store(
                b"blob-1",
                CipherText::new(b"hello".to_vec()),
                Box::new(move || {
                    // Inside done(): the blob must already be published in full.
                    assert_eq!(fs::read(&check_path).unwrap(), b"hello");
                    assert!(!check_tmp.exists(), "temp file must be renamed away");
                    seen_cb.store(true, Ordering::SeqCst);
                }),
            )
            .unwrap();

        assert!(seen.load(Ordering::SeqCst), "done() was not called");
        assert_eq!(fs::read(&final_path).unwrap(), b"hello");
        assert!(!tmp_path.exists());
    }

    #[test]
    fn failed_write_does_not_publish_or_call_done() {
        let dir = tempfile::tempdir().unwrap();
        // Root does not exist, so the temp create fails before anything is published.
        let backend = FileBackend::new(dir.path().join("missing"));

        let called = Arc::new(AtomicBool::new(false));
        let called_cb = called.clone();
        let res = backend.store(
            b"blob-2",
            CipherText::new(b"data".to_vec()),
            Box::new(move || called_cb.store(true, Ordering::SeqCst)),
        );

        assert!(res.is_err());
        assert!(!called.load(Ordering::SeqCst), "done() must not run on failure");
    }

    #[test]
    fn list_skips_temp_and_non_hex_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let backend = FileBackend::new(root.clone());

        backend
            .store(b"real", CipherText::new(b"x".to_vec()), Box::new(|| {}))
            .unwrap();
        // Stray files that used to panic `list` (non-hex) or look like blobs (temp).
        fs::write(backend.tmp_path(b"inflight"), b"partial").unwrap();
        fs::write(root.join("not-hex-at-all"), b"junk").unwrap();

        let names = backend.list().unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(&names[0][..], b"real");
    }
}
