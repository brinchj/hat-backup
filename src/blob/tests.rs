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
// limitations under the License

use crate::backend::{MemoryBackend, StoreBackend};
use crate::blob::{
    Blob, BlobError, BlobIndex, BlobReader, BlobStore, ChunkRef, Compression, LeafType, NodeType,
    Packing,
};
use crate::crypto;
use crate::db;
use crate::hash;
use quickcheck;

use std::collections::HashSet;
use std::sync::Arc;

#[test]
fn identity() {
    fn prop(chunks: Vec<Vec<u8>>) -> bool {
        let backend = Arc::new(MemoryBackend::new());

        let keys = Arc::new(crypto::keys::Keeper::new_for_testing());
        let db = Arc::new(db::Index::new_for_testing());
        let blob_index = Arc::new(BlobIndex::new(keys.clone(), db).unwrap());
        let bs_p = BlobStore::new(keys.clone(), blob_index, backend.clone(), 1024);

        let mut ids = Vec::new();
        for chunk in chunks.iter() {
            let node = NodeType::Leaf;
            let leaf = LeafType::FileChunk;
            ids.push((
                bs_p.store(
                    &chunk[..],
                    hash::Hash::new(&keys, node, leaf, chunk),
                    node,
                    leaf,
                    None,
                    Box::new(move || {}),
                ),
                chunk,
            ));
        }

        bs_p.flush();

        // Non-empty chunks must be in the backend now:
        for &(ref id, chunk) in ids.iter() {
            if !chunk.is_empty() {
                match backend.retrieve(&id.persistent_ref.blob_name[..]) {
                    Ok(_) => (),
                    Err(e) => panic!("{:?}", e),
                }
            }
        }

        // All chunks must be available through the blob store:
        for &(ref id, chunk) in ids.iter() {
            assert_eq!(bs_p.retrieve(id).unwrap().unwrap(), &chunk[..]);
        }

        true
    }
    quickcheck::quickcheck(prop as fn(Vec<Vec<u8>>) -> bool);
}

#[test]
fn identity_with_excessive_flushing() {
    fn prop(chunks: Vec<Vec<u8>>) -> bool {
        let backend = Arc::new(MemoryBackend::new());

        let keys = Arc::new(crypto::keys::Keeper::new_for_testing());
        let db = Arc::new(db::Index::new_for_testing());
        let blob_index = Arc::new(BlobIndex::new(keys.clone(), db).unwrap());
        let bs_p = BlobStore::new(keys.clone(), blob_index, backend.clone(), 1024);

        let mut ids = Vec::new();
        for chunk in chunks.iter() {
            let node = NodeType::Leaf;
            let leaf = LeafType::FileChunk;
            ids.push((
                bs_p.store(
                    &chunk[..],
                    hash::Hash::new(&keys, node, leaf, chunk),
                    node,
                    leaf,
                    None,
                    Box::new(move || {}),
                ),
                chunk,
            ));
            bs_p.flush();
            let &(ref id, chunk) = ids.last().unwrap();
            assert_eq!(bs_p.retrieve(id).unwrap().unwrap(), &chunk[..]);
        }

        // Non-empty chunks must be in the backend now:
        for &(ref id, chunk) in ids.iter() {
            if !chunk.is_empty() {
                match backend.retrieve(&id.persistent_ref.blob_name[..]) {
                    Ok(_) => (),
                    Err(e) => panic!("{:?}", e),
                }
            }
        }

        // All chunks must be available through the blob store:
        for &(ref id, chunk) in ids.iter() {
            assert_eq!(bs_p.retrieve(id).unwrap().unwrap(), &chunk[..]);
        }

        true
    }
    quickcheck::quickcheck(prop as fn(Vec<Vec<u8>>) -> bool);
}

#[test]
fn blobid_identity() {
    fn prop(name: Vec<u8>, offset: usize, length: usize) -> bool {
        let blob_name = ChunkRef {
            blob_id: None,
            blob_name: name.to_vec(),
            offset,
            length,
            packing: None,
            key: None,
        };
        let blob_name_bytes = blob_name.as_bytes();
        let recovered = ChunkRef::from_bytes(&blob_name_bytes[..]).unwrap();
        assert_eq!(blob_name.blob_name, recovered.blob_name);
        assert_eq!(blob_name.offset, recovered.offset);
        assert_eq!(blob_name.length, recovered.length);
        assert!(recovered.blob_id.is_none());
        assert!(recovered.packing.is_none());
        assert!(recovered.key.is_none());

        true
    }
    quickcheck::quickcheck(prop as fn(Vec<u8>, usize, usize) -> bool);
}

#[test]
fn blob_reuse() {
    let keys = Arc::new(crypto::keys::Keeper::new_for_testing());

    let node = NodeType::Leaf;
    let leaf = LeafType::FileChunk;
    let mut c1 = hash::tree::HashRef {
        hash: hash::Hash::new(&keys, node, leaf, &[]),
        node,
        leaf,
        info: None,
        persistent_ref: ChunkRef {
            blob_id: None,
            blob_name: Vec::new(),
            offset: 0,
            length: 0,
            packing: None,
            key: None,
        },
    };
    let mut c2 = c1.clone();

    let mut b = Blob::new(keys.clone(), 1200);
    b.try_append(&[1, 2, 3], &mut c1).unwrap();
    b.try_append(&[4, 5, 6], &mut c2).unwrap();

    let out = b.finish(b"blob-1").unwrap().to_vec();
    let mut reader = BlobReader::new(keys.clone(), b"blob-1", crypto::CipherTextRef::new(&out[..])).unwrap();
    assert_eq!(vec![1, 2, 3], reader.read_chunk(&c1).unwrap());
    assert_eq!(vec![4, 5, 6], reader.read_chunk(&c2).unwrap());

    let mut c3 = c2.clone();

    b.try_append(&[1, 2], &mut c1).unwrap();
    b.try_append(&[1, 2], &mut c2).unwrap();
    b.try_append(&[1, 2], &mut c3).unwrap();

    let out = b.finish(b"blob-2").unwrap().to_vec();
    let mut reader = BlobReader::new(keys.clone(), b"blob-2", crypto::CipherTextRef::new(&out[..])).unwrap();
    assert_eq!(vec![1, 2], reader.read_chunk(&c1).unwrap());
    assert_eq!(vec![1, 2], reader.read_chunk(&c2).unwrap());
    assert_eq!(vec![1, 2], reader.read_chunk(&c3).unwrap());
}

#[test]
fn blob_identity() {
    fn prop(chunks: Vec<Vec<u8>>) -> bool {
        let max_size = 10000;
        let keys = Arc::new(crypto::keys::Keeper::new_for_testing());

        let node = NodeType::Leaf;
        let leaf = LeafType::FileChunk;

        let mut b = Blob::new(keys.clone(), max_size);
        let mut n = 0;
        for chunk in chunks.iter() {
            let mut cref = hash::tree::HashRef {
                hash: hash::Hash::new(&keys, node, leaf, &[]),
                node,
                leaf,
                info: None,
                persistent_ref: ChunkRef {
                    blob_id: None,
                    blob_name: Vec::new(),
                    offset: 0,
                    length: 0,
                    packing: None,
                    key: None,
                },
            };
            if b.try_append(&chunk[..], &mut cref).is_err() {
                assert!(b.upperbound_len() + chunk.len() + cref.as_bytes().len() + 50 >= max_size);
                break;
            }
            n += 1;
        }

        let out = match b.finish(b"") {
            None => {
                assert_eq!(n, 0);
                return true;
            }
            Some(ct) => ct.to_vec(),
        };

        assert_eq!(max_size, out.len());

        let keys = Arc::new(crypto::keys::Keeper::new_for_testing());
        let mut reader =
            BlobReader::new(keys.clone(), b"", crypto::CipherTextRef::new(&out[..])).unwrap();
        let hrefs = reader.refs().unwrap();
        assert_eq!(n, hrefs.len());

        // Check recovered ChunkRefs.
        for (i, href) in hrefs.into_iter().enumerate() {
            assert!(chunks[i].len() < href.persistent_ref.length);
            let chunk = reader.read_chunk(&href).unwrap();
            assert_eq!(chunks[i].len(), chunk.len());
            assert_eq!(&chunks[i], &chunk);
        }

        true
    }
    quickcheck::quickcheck(prop as fn(Vec<Vec<u8>>) -> bool);
}

#[test]
fn random_input_fails() {
    fn prop(data: Vec<u8>) -> bool {
        let keys = Arc::new(crypto::keys::Keeper::new_for_testing());
        BlobReader::new(keys, b"blob", crypto::CipherTextRef::new(&data[..])).is_err()
    }
    quickcheck::quickcheck(prop as fn(Vec<u8>) -> bool);
}

fn empty_blocks_blob_ciphertext(blob: &mut Blob, blocksize: usize) -> Vec<u8> {
    let keys = Arc::new(crypto::keys::Keeper::new_for_testing());
    let block = vec![0u8; blocksize];
    let node = NodeType::Leaf;
    let leaf = LeafType::FileChunk;
    loop {
        let mut cref = hash::tree::HashRef {
            hash: hash::Hash::new(&keys, node, leaf, &block[..]),
            node,
            leaf,
            info: None,
            persistent_ref: ChunkRef {
                blob_id: None,
                blob_name: Vec::new(),
                offset: 0,
                length: block.len(),
                packing: None,
                key: None,
            },
        };
        match blob.try_append(&block[..], &mut cref) {
            Ok(()) => continue,
            Err(_) => break,
        }
    }
    blob.finish(b"").unwrap().to_vec()
}

#[test]
fn blob_ciphertext_uniqueblocks() {
    // If every inserted block gets a unique (nonce, key) combination, they should produce unique
    // blocks in the out-coming ciphertext (by high enough probability to assert it).
    let keys = Arc::new(crypto::keys::Keeper::new_for_testing());
    let mut blob = Blob::new(keys, 1024 * 1024);
    let mut blocks = HashSet::new();

    for _ in 1..10 {
        for c in empty_blocks_blob_ciphertext(&mut blob, 16).chunks(16) {
            let v = c.to_owned();
            assert!(!blocks.contains(&v));
            blocks.insert(v);
        }
    }
}

#[test]
fn blob_ciphertext_authed_allbytes() {
    let keys = Arc::new(crypto::keys::Keeper::new_for_testing());
    let mut blob = Blob::new(keys.clone(), 1024);
    let mut bytes = empty_blocks_blob_ciphertext(&mut blob, 1);

    fn verify(keys: &Arc<crypto::keys::Keeper>, bs: &[u8]) -> Result<Vec<Vec<u8>>, BlobError> {
        let mut vs = vec![];
        let mut reader = BlobReader::new(keys.clone(), b"", crypto::CipherTextRef::new(bs))?;
        for r in reader.refs()? {
            vs.push(reader.read_chunk(&r)?);
        }
        Ok(vs)
    }

    fn with_modified<F>(bytes: &mut [u8], i: usize, b: u8, f: F) -> Result<Vec<Vec<u8>>, BlobError>
    where
        F: FnOnce(&[u8]) -> Result<Vec<Vec<u8>>, BlobError>,
    {
        {
            bytes[i] ^= b;
        }
        let res = f(bytes);
        {
            bytes[i] ^= b;
        }
        res
    }

    // Blob is valid.
    let vs = verify(&keys, &bytes[..]).unwrap();
    for i in 0..bytes.len() {
        assert!(with_modified(&mut bytes[..], i, 1, |bs| verify(&keys, bs)).is_err());
    }
    // We did not corrupt the blob.
    assert_eq!(vs, verify(&keys, &bytes[..]).unwrap());
}

#[test]
fn compression_roundtrip_and_incompressible_fallback() {
    let backend = Arc::new(MemoryBackend::new());
    let keys = Arc::new(crypto::keys::Keeper::new_for_testing());
    let db = Arc::new(db::Index::new_for_testing());
    let blob_index = Arc::new(BlobIndex::new(keys.clone(), db).unwrap());
    let bs = BlobStore::new(keys.clone(), blob_index, backend.clone(), 256 * 1024);

    assert_eq!(bs.compression(), Compression::Zstd(3));

    // Compressible chunk shrinks significantly and records Packing::Zstd.
    let repetitive = b"hat-backup-compressible-log-line\n".repeat(2048);
    let rep_hash = hash::Hash::new(&keys, NodeType::Leaf, LeafType::FileChunk, &repetitive);
    let rep_ref = bs.store(
        &repetitive,
        rep_hash,
        NodeType::Leaf,
        LeafType::FileChunk,
        None,
        Box::new(|| {}),
    );
    assert_eq!(rep_ref.persistent_ref.packing, Some(Packing::Zstd));
    assert!(
        rep_ref.persistent_ref.length < repetitive.len() / 10,
        "expected strong zstd compression, got {} bytes from {}",
        rep_ref.persistent_ref.length,
        repetitive.len()
    );

    // High-entropy chunk does not inflate: falls back to raw (packing = None).
    let mut state: u64 = 0x1234_5678_9abc_def0;
    let incompressible: Vec<u8> = (0..65536)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect();
    let inc_hash = hash::Hash::new(&keys, NodeType::Leaf, LeafType::FileChunk, &incompressible);
    let inc_ref = bs.store(
        &incompressible,
        inc_hash,
        NodeType::Leaf,
        LeafType::FileChunk,
        None,
        Box::new(|| {}),
    );
    assert_eq!(inc_ref.persistent_ref.packing, None);

    // Switching to Compression::None stores even repetitive data raw.
    bs.set_compression(Compression::None);
    let raw_hash = hash::Hash::new(&keys, NodeType::Leaf, LeafType::FileChunk, &repetitive);
    let raw_ref = bs.store(
        &repetitive,
        raw_hash,
        NodeType::Leaf,
        LeafType::FileChunk,
        None,
        Box::new(|| {}),
    );
    assert_eq!(raw_ref.persistent_ref.packing, None);

    bs.flush();

    assert_eq!(bs.retrieve(&rep_ref).unwrap().unwrap(), repetitive);
    assert_eq!(bs.retrieve(&inc_ref).unwrap().unwrap(), incompressible);
    assert_eq!(bs.retrieve(&raw_ref).unwrap().unwrap(), repetitive);
}

#[test]
fn compression_parsing_and_display() {
    assert_eq!("none".parse::<Compression>().unwrap(), Compression::None);
    assert_eq!("off".parse::<Compression>().unwrap(), Compression::None);
    assert_eq!("zstd".parse::<Compression>().unwrap(), Compression::Zstd(3));
    assert_eq!("ZSTD:1".parse::<Compression>().unwrap(), Compression::Zstd(1));
    assert_eq!(
        "zstd:19".parse::<Compression>().unwrap(),
        Compression::Zstd(19)
    );

    assert_eq!(Compression::None.to_string(), "none");
    assert_eq!(Compression::Zstd(3).to_string(), "zstd");
    assert_eq!(Compression::Zstd(7).to_string(), "zstd:7");

    assert!("zstd:0".parse::<Compression>().is_err());
    assert!("zstd:20".parse::<Compression>().is_err());
    assert!("zstd:abc".parse::<Compression>().is_err());
    assert!("gzip".parse::<Compression>().is_err());
}

#[test]
fn corrupt_zstd_payload_returns_error() {
    let keys = Arc::new(crypto::keys::Keeper::new_for_testing());
    let mut blob = Blob::new(keys.clone(), 4096);
    let mut href = crate::hash::tree::HashRef {
        hash: hash::Hash { bytes: vec![1; 64] },
        node: NodeType::Leaf,
        leaf: LeafType::FileChunk,
        info: None,
        persistent_ref: ChunkRef {
            blob_id: Some(1),
            blob_name: b"testblob".to_vec(),
            offset: 0,
            length: 0,
            packing: Some(Packing::Zstd),
            key: None,
        },
    };
    blob.try_append(b"definitely-not-valid-zstd-frame", &mut href)
        .unwrap();
    let ct = blob.finish(b"testblob").unwrap().to_vec();

    let mut reader =
        BlobReader::new(keys, b"testblob", crypto::CipherTextRef::new(&ct)).unwrap();
    let err = reader.read_chunk(&href).unwrap_err();
    assert!(
        err.to_string().contains("zstd"),
        "expected zstd decode error, got: {}",
        err
    );
}

#[test]
fn concurrent_flushes_overlap_and_flush_waits_for_in_flight_writes() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;

    struct BlockingStoreBackend {
        inner: MemoryBackend,
        in_store: AtomicUsize,
        max_concurrent: AtomicUsize,
        entered: AtomicUsize,
        barrier: Barrier,
    }

    impl StoreBackend for BlockingStoreBackend {
        fn store(
            &self,
            name: &[u8],
            data: crypto::CipherText,
            done_callback: Box<dyn FnOnce() + Send>,
        ) -> Result<(), String> {
            let cur = self.in_store.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_concurrent.fetch_max(cur, Ordering::SeqCst);
            // Synchronize the exact first two concurrent blob stores to prove they overlap.
            if self.entered.fetch_add(1, Ordering::SeqCst) < 2 {
                self.barrier.wait();
            }
            let res = self.inner.store(name, data, done_callback);
            self.in_store.fetch_sub(1, Ordering::SeqCst);
            res
        }
        fn retrieve(&self, name: &[u8]) -> Result<Option<Vec<u8>>, String> {
            self.inner.retrieve(name)
        }
        fn delete(&self, name: &[u8]) -> Result<(), String> {
            self.inner.delete(name)
        }
        fn list(&self) -> Result<Vec<Box<[u8]>>, String> {
            self.inner.list()
        }
        fn flush(&self) -> Result<(), String> {
            self.inner.flush()
        }
    }

    let backend = Arc::new(BlockingStoreBackend {
        inner: MemoryBackend::new(),
        in_store: AtomicUsize::new(0),
        max_concurrent: AtomicUsize::new(0),
        entered: AtomicUsize::new(0),
        barrier: Barrier::new(2),
    });
    let keys = Arc::new(crypto::keys::Keeper::new_for_testing());
    let db = Arc::new(db::Index::new_for_testing());
    let blob_index = Arc::new(BlobIndex::new(keys.clone(), db).unwrap());
    // Small blob size (4 KiB) + Compression::None so every 2.5 KiB chunk fills a blob.
    let bs = Arc::new(BlobStore::new(
        keys.clone(),
        blob_index,
        backend.clone(),
        4096,
    ));
    bs.set_compression(Compression::None);

    let callbacks_ran = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for t in 0..4usize {
        let bs = bs.clone();
        let keys = keys.clone();
        let callbacks_ran = callbacks_ran.clone();
        handles.push(std::thread::spawn(move || {
            let mut refs = Vec::new();
            for i in 0..3usize {
                let chunk = vec![(t * 16 + i + 1) as u8; 2500];
                let h = hash::Hash::new(&keys, NodeType::Leaf, LeafType::FileChunk, &chunk);
                let cb_count = callbacks_ran.clone();
                let href = bs.store(
                    &chunk,
                    h,
                    NodeType::Leaf,
                    LeafType::FileChunk,
                    None,
                    Box::new(move || {
                        cb_count.fetch_add(1, Ordering::SeqCst);
                    }),
                );
                refs.push((href, chunk));
            }
            refs
        }));
    }

    let mut all_refs = Vec::new();
    for h in handles {
        all_refs.extend(h.join().unwrap());
    }
    bs.flush();

    assert!(
        backend.max_concurrent.load(Ordering::SeqCst) >= 2,
        "expected >= 2 overlapping backend.store() calls"
    );
    assert_eq!(callbacks_ran.load(Ordering::SeqCst), all_refs.len());
    for (href, expected) in &all_refs {
        assert_eq!(bs.retrieve(href).unwrap().unwrap(), *expected);
    }
}
