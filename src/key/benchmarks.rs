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

use crate::backend::DevNullBackend;
use crate::key::tests::*;
use crate::key::*;
use std::sync::Arc;
use test::Bencher;

#[bench]
fn insert_1_key_x_128000_zeros(bench: &mut Bencher) {
    let backend = Arc::new(DevNullBackend);
    let ks = Store::new_for_testing(backend, 4 * 1024 * 1024).unwrap();

    let bytes = vec![0u8; 128 * 1024];

    let mut i = 0i32;
    bench.iter(|| {
        i += 1;

        let entry = EntryStub {
            data: Some(vec![bytes.clone()]),
            key_entry: Entry::new(None, format!("{}", i).into(), Data::FilePlaceholder, None),
        };

        ks.insert(entry.key_entry.clone(), Some(move || Some(entry)))
            .unwrap();
    });

    ks.flush().unwrap();
    bench.bytes = 128 * 1024;
}

#[bench]
fn insert_1_key_x_128000_unique(bench: &mut Bencher) {
    let backend = Arc::new(DevNullBackend);
    let ks = Store::new_for_testing(backend, 4 * 1024 * 1024).unwrap();

    let bytes = vec![0u8; 128 * 1024];

    let mut i = 0i32;
    bench.iter(|| {
        i += 1;

        let mut my_bytes = bytes.clone();
        my_bytes[0] = i as u8;
        my_bytes[1] = (i / 256) as u8;
        my_bytes[2] = (i / 65536) as u8;

        let entry = EntryStub {
            data: Some(vec![my_bytes]),
            key_entry: Entry::new(None, format!("{}", i).into(), Data::FilePlaceholder, None),
        };

        ks.insert(entry.key_entry.clone(), Some(move || Some(entry)))
            .unwrap();
    });

    ks.flush().unwrap();
    bench.bytes = 128 * 1024;
}

#[bench]
fn insert_1_key_x_16_x_128000_zeros(bench: &mut Bencher) {
    let backend = Arc::new(DevNullBackend);
    let ks = Store::new_for_testing(backend, 4 * 1024 * 1024).unwrap();

    bench.iter(|| {
        let bytes = vec![0u8; 128 * 1024];

        let entry = EntryStub {
            data: Some(vec![bytes; 16]),
            key_entry: Entry::new(None, "abc".to_string().into(), Data::FilePlaceholder, None),
        };
        ks.insert(entry.key_entry.clone(), Some(move || Some(entry)))
            .unwrap();
        ks.flush().unwrap();
    });

    ks.flush().unwrap();
    bench.bytes = 16 * (128 * 1024);
}

#[bench]
fn insert_1_key_x_16_x_128000_unique(bench: &mut Bencher) {
    let backend = Arc::new(DevNullBackend);
    let ks = Store::new_for_testing(backend, 4 * 1024 * 1024).unwrap();

    let bytes = vec![0u8; 128 * 1024];
    let mut i = 0i32;

    bench.iter(|| {
        i += 1;

        let mut my_bytes = bytes.clone();
        my_bytes[0] = i as u8;
        my_bytes[1] = (i / 256) as u8;
        my_bytes[2] = (i / 65536) as u8;

        let mut chunks = vec![];
        for i in 0..16 {
            let mut local_bytes = my_bytes.clone();
            local_bytes[3] = i as u8;
            chunks.push(local_bytes);
        }

        let entry = EntryStub {
            data: Some(chunks),
            key_entry: Entry::new(None, "abc".to_string().into(), Data::FilePlaceholder, None),
        };

        ks.insert(entry.key_entry.clone(), Some(move || Some(entry)))
            .unwrap();
        ks.flush().unwrap();
    });

    bench.bytes = 16 * (128 * 1024);
}

#[bench]
fn insert_1_key_unchanged_empty(bench: &mut Bencher) {
    let backend = Arc::new(DevNullBackend);
    let ks = Store::new_for_testing(backend, 4 * 1024 * 1024).unwrap();

    bench.iter(|| {
        let entry = EntryStub {
            data: None,
            key_entry: Entry::new(None, "abc".to_string().into(), Data::FilePlaceholder, None),
        };
        ks.insert_entry(entry.key_entry.clone()).unwrap();
    });

    ks.flush().unwrap();
}

#[bench]
fn insert_1_key_updated_empty(bench: &mut Bencher) {
    let backend = Arc::new(DevNullBackend);
    let ks = Store::new_for_testing(backend, 4 * 1024 * 1024).unwrap();

    let mut i = 0;
    bench.iter(|| {
        i += 1;
        let entry = EntryStub {
            data: None,
            key_entry: Entry {
                parent_id: None,
                node_id: None,
                data: Data::DirPlaceholder,
                info: Info {
                    name: "abc".to_string().into(),
                    created_ts_secs: Some(i),
                    modified_ts_secs: Some(i),
                    accessed_ts_secs: Some(i),
                    group_id: None,
                    user_id: None,
                    permissions: None,
                    byte_length: None,
                    snapshot_ts_utc: 0,
                    dev: None,
                    ino: None,
                    ctime_secs: None,
                    ctime_nsec: None,
                    mtime_nsec: None,
                },
            },
        };
        ks.insert_entry(entry.key_entry.clone()).unwrap();
    });

    ks.flush().unwrap();
}

#[bench]
fn insert_1_key_unique_empty(bench: &mut Bencher) {
    let backend = Arc::new(DevNullBackend);
    let ks = Store::new_for_testing(backend, 4 * 1024 * 1024).unwrap();

    let mut i = 0;
    bench.iter(|| {
        i += 1;
        let entry = EntryStub {
            data: None,
            key_entry: Entry::new(None, format!("{}", i).into(), Data::DirPlaceholder, None),
        };
        ks.insert_entry(entry.key_entry.clone()).unwrap();
    });

    ks.flush().unwrap();
}
