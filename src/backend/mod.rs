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

mod cmd;
mod devnull;
mod file;
mod memory;

use crypto::CipherText;
use util::FnBox;

pub use self::cmd::CmdBackend;
pub use self::devnull::DevNullBackend;
pub use self::file::FileBackend;
pub use self::memory::MemoryBackend;

pub trait StoreBackend: Sync + Send + 'static {
    fn store(
        &self,
        name: &[u8],
        data: CipherText,
        done_callback: Box<FnBox<(), ()>>,
    ) -> Result<(), String>;
    fn retrieve(&self, name: &[u8]) -> Result<Option<Vec<u8>>, String>;
    fn delete(&self, name: &[u8]) -> Result<(), String>;
    fn list(&self) -> Result<Vec<Box<[u8]>>, String>;
    fn flush(&self) -> Result<(), String>;
}
