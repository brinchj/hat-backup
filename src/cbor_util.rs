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

//! Thin CBOR helpers replacing the deprecated `serde_cbor` crate.

use serde::{de::DeserializeOwned, Serialize};
use std::io;

pub type DeError = ciborium::de::Error<io::Error>;
pub type SerError = ciborium::ser::Error<io::Error>;

pub fn from_slice<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, DeError> {
    ciborium::from_reader(bytes)
}

pub fn to_vec<T: Serialize>(value: &T) -> Result<Vec<u8>, SerError> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf)?;
    Ok(buf)
}
