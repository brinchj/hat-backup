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

use crate::models;
pub use crate::models::LeafType;

use crate::cbor_util;
use std::fmt;
use std::str::FromStr;

pub const DEFAULT_ZSTD_LEVEL: i32 = 3;
pub const MIN_ZSTD_LEVEL: i32 = 1;
pub const MAX_ZSTD_LEVEL: i32 = 19;

/// Write-side chunk compression policy.
///
/// Chunks are always hashed before compression so deduplication works across
/// compression setting changes. Chunks that do not shrink under compression are
/// stored raw (`Packing::Raw`).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Compression {
    None,
    Zstd(i32),
}

impl Default for Compression {
    fn default() -> Self {
        Compression::Zstd(DEFAULT_ZSTD_LEVEL)
    }
}

impl fmt::Display for Compression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Compression::None => f.write_str("none"),
            Compression::Zstd(DEFAULT_ZSTD_LEVEL) => f.write_str("zstd"),
            Compression::Zstd(level) => write!(f, "zstd:{}", level),
        }
    }
}

impl FromStr for Compression {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.trim().to_ascii_lowercase();
        match lower.as_str() {
            "none" | "off" => Ok(Compression::None),
            "zstd" => Ok(Compression::Zstd(DEFAULT_ZSTD_LEVEL)),
            _ => {
                if let Some(level_str) = lower.strip_prefix("zstd:") {
                    let level: i32 = level_str.parse().map_err(|_| {
                        format!(
                            "invalid zstd compression level '{}' (expected integer {}..={})",
                            level_str, MIN_ZSTD_LEVEL, MAX_ZSTD_LEVEL
                        )
                    })?;
                    if !(MIN_ZSTD_LEVEL..=MAX_ZSTD_LEVEL).contains(&level) {
                        return Err(format!(
                            "zstd compression level {} out of range (expected {}..={})",
                            level, MIN_ZSTD_LEVEL, MAX_ZSTD_LEVEL
                        ));
                    }
                    Ok(Compression::Zstd(level))
                } else {
                    Err(format!(
                        "unsupported compression '{}' (expected 'zstd', 'zstd:<{}..={}>', or 'none')",
                        s, MIN_ZSTD_LEVEL, MAX_ZSTD_LEVEL
                    ))
                }
            }
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Packing {
    GZip,
    Snappy,
    Zstd,
}

#[derive(Debug, Clone)]
pub enum Key {
    AeadChacha20Poly1305(secstr::SecStr),
    AeadXChaCha20Poly1305(secstr::SecStr),
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum NodeType {
    Branch(u64),
    Leaf,
}

impl From<u64> for NodeType {
    fn from(n: u64) -> NodeType {
        match n {
            0 => NodeType::Leaf,
            _ => NodeType::Branch(n),
        }
    }
}

impl From<NodeType> for u64 {
    fn from(t: NodeType) -> u64 {
        match t {
            NodeType::Branch(height) => height,
            NodeType::Leaf => 0,
        }
    }
}

impl From<u64> for LeafType {
    fn from(n: u64) -> LeafType {
        match n {
            1 => LeafType::FileChunk,
            2 => LeafType::TreeList,
            3 => LeafType::SnapshotList,
            _ => unreachable!("Corrupt LeafType tag: {}", n),
        }
    }
}

impl From<LeafType> for u64 {
    fn from(t: LeafType) -> u64 {
        match t {
            LeafType::FileChunk => 1,
            LeafType::TreeList => 2,
            LeafType::SnapshotList => 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChunkRef {
    pub blob_id: Option<i64>,
    pub blob_name: Vec<u8>,
    pub offset: usize,
    pub length: usize,
    pub packing: Option<Packing>,
    pub key: Option<Key>,
}

impl From<models::ChunkRef> for ChunkRef {
    fn from(chunk_ref: models::ChunkRef) -> ChunkRef {
        ChunkRef {
            blob_id: None,
            blob_name: chunk_ref.blob_name,
            offset: chunk_ref.offset as usize,
            length: chunk_ref.length as usize,
            packing: match chunk_ref.packing {
                models::Packing::Raw => None,
                models::Packing::GZip => Some(Packing::GZip),
                models::Packing::Snappy => Some(Packing::Snappy),
                models::Packing::Zstd => Some(Packing::Zstd),
            },
            key: match chunk_ref.key {
                models::Key::None => None,
                models::Key::AeadChacha20Poly1305(key) => {
                    Some(Key::AeadChacha20Poly1305(secstr::SecVec::new(key)))
                }
                models::Key::AeadXChaCha20Poly1305(key) => {
                    Some(Key::AeadXChaCha20Poly1305(secstr::SecVec::new(key)))
                }
            },
        }
    }
}

impl ChunkRef {
    pub fn to_model(&self) -> models::ChunkRef {
        models::ChunkRef {
            blob_name: self.blob_name.clone(),
            offset: self.offset as u64,
            length: self.length as u64,
            packing: match self.packing {
                None => models::Packing::Raw,
                Some(Packing::GZip) => models::Packing::GZip,
                Some(Packing::Snappy) => models::Packing::Snappy,
                Some(Packing::Zstd) => models::Packing::Zstd,
            },
            key: match self.key {
                None => models::Key::None,
                Some(Key::AeadChacha20Poly1305(ref key)) => {
                    models::Key::AeadChacha20Poly1305(key.unsecure().to_owned())
                }
                Some(Key::AeadXChaCha20Poly1305(ref key)) => {
                    models::Key::AeadXChaCha20Poly1305(key.unsecure().to_owned())
                }
            },
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<ChunkRef, cbor_util::DeError> {
        let model: models::ChunkRef = cbor_util::from_slice(bytes)?;
        Ok(From::from(model))
    }

    pub fn as_bytes(&self) -> Vec<u8> {
        cbor_util::to_vec(&self.to_model()).unwrap()
    }

    pub fn as_bytes_no_name(&self) -> Vec<u8> {
        let mut model = self.to_model();
        model.blob_name = "".into();
        cbor_util::to_vec(&model).unwrap()
    }
}
