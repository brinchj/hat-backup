extern crate bincode;
extern crate hat;
extern crate serde_cbor;
extern crate serde_json;

use hat::backend::{MemoryBackend, StoreBackend};
use hat::hat::{Family, HatRc};
use hat::key;
use hat::models;
use hat::vfs::{self, Filesystem};

use std::path;
use std::sync::Arc;

pub fn setup_hat<B: StoreBackend>(backend: Arc<B>) -> HatRc<B> {
    let max_blob_size = 4 * 1024 * 1024;
    HatRc::new_for_testing(backend, max_blob_size).unwrap()
}

fn setup_family() -> (
    Arc<MemoryBackend>,
    HatRc<MemoryBackend>,
    Family<MemoryBackend>,
) {
    let backend = Arc::new(MemoryBackend::new());
    let mut hat = setup_hat(backend.clone());

    let family = "familyname".to_string();
    let fam = hat.open_family(family).unwrap();

    (backend, hat, fam)
}

fn metadata_test(info: models::FileInfo) {
    println!("OK: MESSAGE PARSED");
    if !info.name.is_empty() {
        panic!("OK: MESSAGE HAS FILENAME");
        let entry = key::Entry::new_from_model(None, key::Data::FilePlaceholder, info);
        let (_backend, mut hat, mut fam) = setup_family();

        fam.snapshot_direct(entry.clone(), false, None).unwrap();
        hat.commit(&mut fam, None).unwrap();
        hat.meta_commit().unwrap();
        hat.data_flush().unwrap();

        let mut fs = Filesystem::new(hat);

        if let vfs::fs::List::Dir(files) = fs.ls(&path::PathBuf::from("familyname/1"))
            .unwrap()
            .expect("no files found")
        {
            assert_eq!(files.len(), 1);
            let mut want = entry.info;
            want.snapshot_ts_utc = files[0].0.info.snapshot_ts_utc;
            assert_eq!(want, files[0].0.info);
        } else {
            panic!("familyname/1 is not a directory");
        }
    }
}

pub fn metadata_test_bincode(data: &[u8]) {
    bincode::deserialize(data).ok().map(metadata_test);
}

pub fn metadata_test_json(data: &[u8]) {
    serde_json::from_slice(data).ok().map(metadata_test);
}

pub fn metadata_test_cbor(data: &[u8]) {
    serde_cbor::from_slice(data).ok().map(metadata_test);
}
