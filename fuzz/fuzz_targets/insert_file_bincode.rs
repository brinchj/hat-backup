#![no_main]
extern crate bincode;
extern crate hat;
#[macro_use]
extern crate libfuzzer_sys;

use hat::hat::{Family, HatRc};
use hat::backend::{MemoryBackend, StoreBackend};
use hat::key;
use hat::models;

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

fuzz_target!(|data: &[u8]| {
    if let Ok(info) = bincode::deserialize::<models::FileInfo>(data) {
        if !info.name.is_empty() {
            let entry = key::Entry::new_from_model(None, key::Data::FilePlaceholder, info);
            let (_backend, _hat, fam) = setup_family();
            fam.snapshot_direct(entry, false, None).unwrap();
        }
    }
});
