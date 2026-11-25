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

use crate::crypto;
use crate::crypto::{CipherText, CipherTextRef, PlainTextRef};
use crate::hash::tree::HashRef;

use std::borrow::Cow;
use std::cell::RefCell;
use std::io::Read;
use std::mem;
use std::sync::Arc;

use super::{BlobError, Compression, Packing};

/// Upper bound on a single decompressed chunk to guard against corrupt or
/// malicious zstd frames expanding without limit.
pub const MAX_DECOMPRESSED_CHUNK_BYTES: usize = 16 * 1024 * 1024;

thread_local! {
    /// Per-thread reusable zstd compressor so worker threads avoid allocating
    /// and freeing a `ZSTD_CCtx` on every chunk.
    static ZSTD_COMPRESSOR: RefCell<Option<(i32, zstd::bulk::Compressor<'static>)>> =
        const { RefCell::new(None) };
}

/// Compress `chunk` according to `compression`, returning the payload bytes to
/// seal and the resulting wire `Packing`. If compression does not strictly
/// reduce byte length, the original uncompressed bytes are returned with `None`.
pub(crate) fn compress_chunk(
    chunk: &[u8],
    compression: Compression,
) -> Result<(Cow<'_, [u8]>, Option<Packing>), BlobError> {
    if chunk.is_empty() {
        return Ok((Cow::Borrowed(chunk), None));
    }
    match compression {
        Compression::None => Ok((Cow::Borrowed(chunk), None)),
        Compression::Zstd(level) => {
            let compressed = ZSTD_COMPRESSOR.with(|cell| {
                let mut slot = cell.borrow_mut();
                let need_new = match slot.as_ref() {
                    Some((cached_level, _)) => *cached_level != level,
                    None => true,
                };
                if need_new {
                    let comp = zstd::bulk::Compressor::new(level)
                        .map_err(|e| BlobError::from(format!("zstd init failed: {}", e)))?;
                    *slot = Some((level, comp));
                }
                let (_, comp) = slot.as_mut().expect("compressor initialized");
                comp.compress(chunk)
                    .map_err(|e| BlobError::from(format!("zstd compress failed: {}", e)))
            })?;
            if compressed.len() < chunk.len() {
                Ok((Cow::Owned(compressed), Some(Packing::Zstd)))
            } else {
                Ok((Cow::Borrowed(chunk), None))
            }
        }
    }
}

/// Decompress a decrypted chunk payload according to its recorded `Packing`.
pub(crate) fn decompress_chunk(
    data: Vec<u8>,
    packing: Option<Packing>,
) -> Result<Vec<u8>, BlobError> {
    match packing {
        None => Ok(data),
        Some(Packing::Zstd) => {
            let decoder = zstd::stream::read::Decoder::new(&data[..])
                .map_err(|e| BlobError::from(format!("zstd decoder init failed: {}", e)))?;
            let mut limited = decoder.take((MAX_DECOMPRESSED_CHUNK_BYTES + 1) as u64);
            let mut out = Vec::new();
            limited
                .read_to_end(&mut out)
                .map_err(|e| BlobError::from(format!("zstd decompress failed: {}", e)))?;
            if out.len() > MAX_DECOMPRESSED_CHUNK_BYTES {
                return Err(BlobError::from(format!(
                    "zstd decompressed chunk exceeds limit of {} bytes",
                    MAX_DECOMPRESSED_CHUNK_BYTES
                )));
            }
            Ok(out)
        }
        Some(Packing::GZip) => Err(BlobError::from("unsupported chunk packing: gzip")),
        Some(Packing::Snappy) => Err(BlobError::from("unsupported chunk packing: snappy")),
    }
}

pub struct Blob {
    keys: Arc<crypto::keys::Keeper>,
    access_key: crypto::authed::desc::Key,
    chunks: CipherText,
    footer: Vec<u8>,
    overhead: usize,
    max_len: usize,
}

impl Blob {
    pub fn new(keys: Arc<crypto::keys::Keeper>, max_len: usize) -> Blob {
        Blob {
            keys,
            access_key: crypto::FixedKey::new_access_partial_key(),
            chunks: CipherText::empty(),
            footer: Vec::with_capacity(max_len / 2),
            overhead: crypto::sealed::desc::overhead() + crypto::authed::hash::DIGESTBYTES,
            max_len,
        }
    }

    pub fn upperbound_len(&self) -> usize {
        if self.chunks.is_empty() {
            0
        } else {
            self.chunks.len() + self.footer.len() + self.overhead
        }
    }

    #[cfg_attr(feature = "flame_it", flame)]
    pub fn try_append(&mut self, chunk: &[u8], href: &mut HashRef) -> Result<(), ()> {
        href.persistent_ref.offset = self.chunks.len();
        let ct = crypto::RefKey::seal(href, &self.access_key, PlainTextRef::new(chunk));

        let mut href_bytes = href.as_bytes();
        assert!(href_bytes.len() < 65535);

        let new_len = self.chunks.len()
            + ct.len()
            + self.footer.len()
            + 2
            + href_bytes.len()
            + self.overhead;
        if new_len > self.max_len {
            if self.chunks.is_empty() {
                panic!(
                    "Can never fit chunk of size {} in blob of size {}",
                    chunk.len(),
                    self.max_len
                );
            }
            return Err(());
        }

        self.chunks.append(ct);

        // Generate footer entry.
        self.footer.push((href_bytes.len() % 256) as u8);
        self.footer.push((href_bytes.len() / 256) as u8);
        self.footer.append(&mut href_bytes);

        Ok(())
    }

    #[cfg_attr(feature = "flame_it", flame)]
    pub fn finish(&mut self, blob_name: &[u8]) -> Option<CipherText> {
        if self.chunks.is_empty() {
            return None;
        }

        // Generate a new access key for the next blob to use.
        let access_key = mem::replace(
            &mut self.access_key,
            crypto::FixedKey::new_access_partial_key(),
        );

        let footer_overhead = self.footer.len() + self.overhead;
        let footer = crypto::FixedKey::new(&self.keys).seal(
            blob_name,
            &access_key,
            PlainTextRef::new(&self.footer[..]),
        );
        self.footer.clear();

        assert!(self.chunks.len() + footer_overhead <= self.max_len);

        // Always pad to max_len so ciphertext size does not leak how much real data
        // was in this blob (partial fills / small deltas would otherwise be visible).
        let mut out = mem::replace(&mut self.chunks, CipherText::empty());
        out.random_pad_upto(self.max_len - footer_overhead);
        out.append(footer);
        out.append_authentication(blob_name, &self.keys);

        assert_eq!(out.len(), self.max_len);

        // Everything has been reset. We are ready to go again.
        assert_eq!(0, self.chunks.len());
        assert_eq!(0, self.footer.len());

        Some(out)
    }
}

pub struct BlobReader {
    keys: Arc<crypto::keys::Keeper>,
    blob_name: Vec<u8>,
    access_key: crypto::authed::desc::Key,
    footer_ct: Vec<u8>,
    blob: CipherText,
}

impl BlobReader {
    pub fn new<'b>(
        keys: Arc<crypto::keys::Keeper>,
        blob_name: &[u8],
        blob: CipherTextRef<'b>,
    ) -> Result<BlobReader, crypto::CryptoError> {
        let rest = blob.strip_authentication(blob_name, &keys)?;
        let (access_key, footer_ct, rest) = crypto::FixedKey::new(&keys).unseal_access_ctx(rest)?;

        Ok(BlobReader {
            keys,
            blob_name: blob_name.to_vec(),
            access_key,
            footer_ct: footer_ct.to_vec(),
            blob: rest.to_owned(),
        })
    }

    pub fn refs(&mut self) -> Result<Vec<HashRef>, BlobError> {
        let (_rest, footer_vec) = crypto::FixedKey::new(&self.keys).unseal(
            &self.blob_name,
            CipherTextRef::new(&self.footer_ct[..]),
            self.blob.collapse(),
        )?;
        let mut footer_pos = footer_vec.as_bytes();

        let mut hrefs = Vec::new();
        while !footer_pos.is_empty() {
            if footer_pos.len() < 2 {
                return Err(BlobError::from(
                    "corrupt blob footer: truncated entry length",
                ));
            }
            let len = footer_pos[0] as usize + 256 * (footer_pos[1] as usize);
            let entry_end = len
                .checked_add(2)
                .ok_or_else(|| BlobError::from("corrupt blob footer: entry length overflow"))?;
            if footer_pos.len() < entry_end {
                return Err(BlobError::from("corrupt blob footer: truncated entry"));
            }

            hrefs.push(HashRef::from_bytes(&footer_pos[2..entry_end])?);
            footer_pos = &footer_pos[entry_end..];
        }

        Ok(hrefs)
    }

    pub fn read_chunk(&mut self, href: &HashRef) -> Result<Vec<u8>, BlobError> {
        let raw = crypto::RefKey::unseal(&self.access_key, href, self.blob.collapse())?.into_vec();
        decompress_chunk(raw, href.persistent_ref.packing.clone())
    }
}
