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

use backend::StoreBackend;
use crypto::CipherText;
use hex::{self, FromHex};
use std::collections::BTreeMap;
use std::mem;
use std::process;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::Duration;
use util::FnBox;

const HAT_CMD_PUT: &str = "hat-backup-put";
const HAT_CMD_GET: &str = "hat-backup-get";
const HAT_CMD_DELETE: &str = "hat-backup-delete";
const HAT_CMD_LIST: &str = "hat-backup-list";

pub struct CmdBackend {
    read_cache: Mutex<BTreeMap<Vec<u8>, Result<Option<Vec<u8>>, String>>>,
    max_cache_size: usize,
    max_concurrent: usize,
    queue: Mutex<Vec<CmdPut>>,
}

struct CmdPutContext {
    hex_key: String,
    text: CipherText,
    done_callback: Box<FnBox<(), ()>>,
}

impl CmdPutContext {
    fn start_child(&self) -> Result<process::Child, String> {
        use std::io::Write;

        let mut child = process::Command::new(HAT_CMD_PUT)
            .arg(&self.hex_key[..])
            .stdin(process::Stdio::piped())
            .spawn()
            .map_err(|err| format!("failed to spawn sub-process {}: {}", HAT_CMD_PUT, err))?;

        {
            let mut stdin = mem::replace(&mut child.stdin, None).expect("failed to get stdin");
            for block in self.text.slices() {
                if let Err(err) = stdin.write_all(block) {
                    return Err(err.to_string());
                }
            }
        }

        Ok(child)
    }
}

struct CmdPut {
    child: process::Child,
    context: CmdPutContext,
}

impl CmdPut {
    fn new(context: CmdPutContext) -> Result<Self, String> {
        let child = context.start_child()?;

        Ok(CmdPut {
            child: child,
            context: context,
        })
    }

    fn try_wait(&mut self) -> Result<Option<process::ExitStatus>, String> {
        self.child.try_wait().map_err(|err| {
            format!(
                "failed to query sub-process {}: {}",
                HAT_CMD_PUT,
                err.to_string()
            )
        })
    }

    fn wait(mut self) -> Result<(), (String, CmdPutContext)> {
        let status = match self.child.wait() {
            Ok(status) => status,
            Err(err) => {
                return Err((
                    format!(
                        "failed to query sub-process {}: {}",
                        HAT_CMD_PUT,
                        err.to_string()
                    ),
                    self.context,
                ))
            }
        };

        if status.success() {
            self.context.done_callback.call(());
            Ok(())
        } else {
            let why = status
                .code()
                .map(|c| format!("failed with exit code: {}", c))
                .unwrap_or_else(|| "killed by signal".into());

            let err = format!("sub-process {} {}", HAT_CMD_PUT, why);
            Err((err, self.context))
        }
    }
}

impl CmdBackend {
    pub fn new() -> CmdBackend {
        CmdBackend {
            read_cache: Mutex::new(BTreeMap::new()),
            max_cache_size: 10,
            max_concurrent: 5,
            queue: Mutex::new(vec![]),
        }
    }

    fn guarded_cache_get(&self, name: &[u8]) -> Option<Result<Option<Vec<u8>>, String>> {
        match self.read_cache.lock() {
            Err(e) => Some(Err(e.to_string())),
            Ok(cache) => cache.get(name).cloned(),
        }
    }

    fn get(&self, name: &[u8]) -> Result<Option<Vec<u8>>, String> {
        // Read key:
        let hex_key = hex::encode(&name);

        match process::Command::new(HAT_CMD_GET)
            .arg(&hex_key[..])
            .stdout(process::Stdio::piped())
            .output()
        {
            Ok(out) => {
                if out.stdout.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(out.stdout))
                }
            }
            Err(err) => Err(format!(
                "{} failed while getting file {}: {}",
                HAT_CMD_GET,
                hex_key,
                err.to_string()
            )),
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

    fn new_put(&self, ctx: CmdPutContext) -> Result<(), String> {
        let mut queue = self.queue.lock().unwrap();

        while queue.len() >= self.max_concurrent {
            self.try_flush(&mut queue);
            thread::sleep(Duration::from_millis(10));
        }

        queue.push(CmdPut::new(ctx)?);

        Ok(())
    }

    fn try_flush(&self, queue: &mut MutexGuard<Vec<CmdPut>>) {
        let mut old = mem::replace(&mut **queue, vec![]);

        let mut restart = vec![];

        for mut c in old.drain(..) {
            match c.try_wait() {
                Ok(None) => queue.push(c),
                Err(err) => {
                    eprintln!("error: {}", err.to_string());
                    queue.push(c);
                }
                Ok(..) => {
                    // Process seems ready.
                    if let Err((err, ctx)) = c.wait() {
                        eprintln!("error: {}", err);
                        restart.push(ctx);
                    }
                }
            }
        }

        for ctx in restart {
            queue.push(CmdPut::new(ctx).expect("failed to restart failed sub-process"));
        }
    }
}

impl StoreBackend for CmdBackend {
    fn store(&self, name: &[u8], text: CipherText, done: Box<FnBox<(), ()>>) -> Result<(), String> {
        let hex_key = hex::encode(&name);

        let context = CmdPutContext {
            hex_key,
            text,
            done_callback: done,
        };

        self.new_put(context)?;

        Ok(())
    }

    fn retrieve(&self, name: &[u8]) -> Result<Option<Vec<u8>>, String> {
        // Check for key in cache:
        let value_opt = self.guarded_cache_get(name);
        if let Some(r) = value_opt {
            r
        } else {
            let res = self.get(name);

            // Update cache to contain key:
            self.guarded_cache_put(name.to_vec(), res.clone());
            res
        }
    }

    fn delete(&self, name: &[u8]) -> Result<(), String> {
        let name = name.to_vec();
        self.guarded_cache_delete(&name);

        let hex_key = hex::encode(&name);

        match process::Command::new(HAT_CMD_DELETE).arg(&hex_key).output() {
            Ok(..) => Ok(()),
            Err(err) => Err(format!(
                "{} failed while deleting file {}: {}",
                HAT_CMD_DELETE,
                hex_key,
                err.to_string()
            )),
        }
    }

    fn list(&self) -> Result<Vec<Box<[u8]>>, String> {
        let listing = match process::Command::new(HAT_CMD_LIST)
            .stdout(process::Stdio::piped())
            .output()
        {
            Ok(out) => match String::from_utf8(out.stdout) {
                Ok(utf8) => utf8,
                Err(err) => {
                    return Err(format!(
                        "{} result encoding is not valid utf8: {}",
                        HAT_CMD_LIST,
                        err.to_string()
                    ));
                }
            },
            Err(err) => return Err(format!("{} failed: {}", HAT_CMD_LIST, err.to_string())),
        };

        let mut out = vec![];
        for f in listing.lines() {
            if let Ok(bytes) = Vec::from_hex(&f) {
                out.push(bytes.into_boxed_slice());
            } else {
                eprintln!("WARNING: ignoring unexpected files name: {}", f);
            }
        }

        Ok(out)
    }

    fn flush(&self) -> Result<(), String> {
        loop {
            {
                let mut queue = self.queue.lock().unwrap();
                if queue.is_empty() {
                    break;
                }
                self.try_flush(&mut queue);
            }

            thread::sleep(Duration::from_millis(100));
        }

        Ok(())
    }
}
