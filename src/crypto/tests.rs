use crate::blob::{BlobReader, ChunkRef, Key, LeafType, NodeType};
use crate::crypto::keys::{Keeper, UNIVERSAL_KEY_BYTES};
use crate::crypto::{authed, sealed, CipherText, CipherTextRef, FixedKey, PlainText, PlainTextRef, RefKey};
use crate::hash::tree::HashRef;
use crate::hash::Hash;
use std::fs;
use std::io;
use std::sync::Arc;

fn test_keeper(seed_byte: u8) -> Keeper {
    Keeper::new(secstr::SecStr::new(vec![seed_byte; UNIVERSAL_KEY_BYTES]))
}

#[test]
fn asymmetric_unlock_rejects_short_ciphertext() {
    let keeper = test_keeper(1);
    for len in [0usize, 1, 16, sealed::desc::SEALBYTES - 1] {
        let short = vec![0xAAu8; len];
        assert!(
            keeper.data_unlock(&short).is_err(),
            "data_unlock must reject {len}-byte input"
        );
        assert!(
            keeper.access_unlock(&short).is_err(),
            "access_unlock must reject {len}-byte input"
        );
        assert!(
            keeper.naming_unlock(&short).is_err(),
            "naming_unlock must reject {len}-byte input"
        );
    }
}

#[test]
fn asymmetric_unlock_rejects_tampered_ciphertext_and_wrong_key() {
    let k1 = test_keeper(1);
    let k2 = test_keeper(2);

    let mut data_ct = k1.data_lock(b"secret-data");
    assert_eq!(k1.data_unlock(&data_ct).unwrap(), b"secret-data");
    assert!(k2.data_unlock(&data_ct).is_err());
    data_ct[0] ^= 0x01;
    assert!(k1.data_unlock(&data_ct).is_err());

    let mut access_ct = k1.access_lock(b"secret-access");
    assert_eq!(k1.access_unlock(&access_ct).unwrap(), b"secret-access");
    assert!(k2.access_unlock(&access_ct).is_err());
    let last = access_ct.len() - 1;
    access_ct[last] ^= 0x80;
    assert!(k1.access_unlock(&access_ct).is_err());

    let mut name_ct = k1.naming_lock(&42i64.to_le_bytes());
    assert_eq!(k1.naming_unlock(&name_ct).unwrap(), 42i64.to_le_bytes());
    assert!(k2.naming_unlock(&name_ct).is_err());
    name_ct[10] ^= 0xFF;
    assert!(k1.naming_unlock(&name_ct).is_err());
}

#[test]
fn symmetric_lock_and_unlock_reject_invalid_key_and_nonce_lengths() {
    let valid_key = [0x11u8; authed::desc::KEYBYTES];
    let valid_nonce = [0x22u8; authed::desc::NONCEBYTES];
    let valid_ct = Keeper::symmetric_lock(b"hello", b"ad", &valid_nonce, &valid_key).unwrap();
    assert_eq!(
        Keeper::symmetric_unlock(&valid_key, &valid_ct, b"ad", &valid_nonce).unwrap(),
        b"hello"
    );

    for bad_key_len in [0usize, 1, 16, authed::desc::KEYBYTES - 1, authed::desc::KEYBYTES + 1] {
        let bad_key = vec![0x11u8; bad_key_len];
        assert!(Keeper::symmetric_lock(b"hello", b"ad", &valid_nonce, &bad_key).is_err());
        assert!(Keeper::symmetric_unlock(&bad_key, &valid_ct, b"ad", &valid_nonce).is_err());
    }

    for bad_nonce_len in [0usize, 1, authed::desc::NONCEBYTES - 1, authed::desc::NONCEBYTES + 1] {
        let bad_nonce = vec![0x22u8; bad_nonce_len];
        assert!(Keeper::symmetric_lock(b"hello", b"ad", &bad_nonce, &valid_key).is_err());
        assert!(Keeper::symmetric_unlock(&valid_key, &valid_ct, b"ad", &bad_nonce).is_err());
    }
}

#[test]
fn symmetric_unlock_rejects_short_and_tampered_ciphertext() {
    let key = [0x33u8; authed::desc::KEYBYTES];
    let nonce = [0x44u8; authed::desc::NONCEBYTES];

    for len in [0usize, 1, authed::desc::MACBYTES - 1] {
        let short = vec![0u8; len];
        assert!(Keeper::symmetric_unlock(&key, &short, b"ad", &nonce).is_err());
    }

    let mut ct = Keeper::symmetric_lock(b"payload", b"ad", &nonce, &key).unwrap();
    assert!(Keeper::symmetric_unlock(&key, &ct, b"wrong-ad", &nonce).is_err());
    ct[0] ^= 0x01;
    assert!(Keeper::symmetric_unlock(&key, &ct, b"ad", &nonce).is_err());
}

#[test]
fn refkey_unseal_handles_bounds_overflow_bad_key_and_exact_end() {
    let keeper = test_keeper(3);
    let access_key = FixedKey::new_access_partial_key();
    let payload = b"chunk-body";

    let mut href = HashRef {
        hash: Hash::new(&keeper, NodeType::Leaf, LeafType::FileChunk, payload),
        node: NodeType::Leaf,
        leaf: LeafType::FileChunk,
        info: None,
        persistent_ref: ChunkRef {
            blob_id: None,
            blob_name: vec![],
            offset: 0,
            length: 0,
            packing: None,
            key: None,
        },
    };

    let ct = RefKey::seal(&mut href, &access_key, PlainTextRef::new(payload)).to_vec();

    // Exact end boundary (offset + length == ct.len()) must succeed.
    assert_eq!(
        RefKey::unseal(&access_key, &href, CipherTextRef::new(&ct))
            .unwrap()
            .into_vec(),
        payload
    );

    // Overflow in offset + length must return Err, not panic.
    let mut overflow_href = href.clone();
    overflow_href.persistent_ref.offset = usize::MAX;
    overflow_href.persistent_ref.length = 1;
    assert!(RefKey::unseal(&access_key, &overflow_href, CipherTextRef::new(&ct)).is_err());

    // Out-of-bounds range must return Err, not panic.
    let mut oob_href = href.clone();
    oob_href.persistent_ref.offset = 1;
    assert!(RefKey::unseal(&access_key, &oob_href, CipherTextRef::new(&ct)).is_err());

    // Wrong partial_key length must return Err.
    let mut bad_key_href = href.clone();
    bad_key_href.persistent_ref.key = Some(Key::AeadChacha20Poly1305(secstr::SecStr::new(vec![1; 8])));
    assert!(RefKey::unseal(&access_key, &bad_key_href, CipherTextRef::new(&ct)).is_err());

    // Short access_key must return Err.
    let short_access = secstr::SecStr::new(vec![0u8; 4]);
    assert!(RefKey::unseal(&short_access, &href, CipherTextRef::new(&ct)).is_err());

    // Tampered chunk ciphertext must return Err, not panic.
    let mut tampered = ct.clone();
    tampered[0] ^= 0x01;
    assert!(RefKey::unseal(&access_key, &href, CipherTextRef::new(&tampered)).is_err());
}

#[test]
fn fixedkey_unseal_rejects_corrupt_access_footer_and_invalid_lengths() {
    let keeper = test_keeper(4);
    let fk = FixedKey::new(&keeper);
    let access_key = FixedKey::new_access_partial_key();

    let sealed_ct = fk.seal(&access_key, PlainTextRef::new(b"footer-data")).to_vec();

    // Corrupted access box returns Err.
    let mut bad_access = sealed_ct.clone();
    let last = bad_access.len() - 1;
    bad_access[last] ^= 0x01;
    assert!(fk.unseal_access_ctx(CipherTextRef::new(&bad_access)).is_err());

    // Valid access box, corrupted footer box returns Err.
    let (_ak, footer_ct, rest) = fk.unseal_access_ctx(CipherTextRef::new(&sealed_ct)).unwrap();
    let mut bad_footer = footer_ct.to_vec();
    bad_footer[0] ^= 0x01;
    assert!(fk.unseal(CipherTextRef::new(&bad_footer), rest.as_ref()).is_err());

    // Wrong-length footer box returns Err.
    assert!(fk.unseal(CipherTextRef::new(&bad_footer[..4]), rest.as_ref()).is_err());

    // Validly sealed footer claiming negative/zero ct_len returns Err instead of panicking.
    for invalid_len in [0i64, -1i64, -100i64] {
        let mut foot_pt = PlainText::from_i64(invalid_len).into_vec();
        foot_pt.extend_from_slice(&[0x55u8; authed::desc::KEYBYTES]);
        let forged_footer = fk.seal_blob_data(PlainTextRef::new(&foot_pt)).to_vec();
        assert!(fk.unseal(CipherTextRef::new(&forged_footer), rest.as_ref()).is_err());
    }
}

#[test]
fn blob_reader_refs_rejects_truncated_and_off_by_one_footers() {
    let keeper = Arc::new(test_keeper(5));
    let fk = FixedKey::new(&keeper);

    // 1) 1-byte footer (missing second length byte)
    // 2) 6-byte footer claiming entry length 5: len = 5, footer_pos.len() = 6 == len + 1
    //    (previously bypassed `assert!(footer_pos.len() > len)` and panicked on `&footer_pos[2..7]`)
    for bad_footer_plaintext in [vec![0x05u8], vec![0x05, 0x00, 1, 2, 3, 4]] {
        let access_key = FixedKey::new_access_partial_key();
        let mut blob_ct = fk.seal(&access_key, PlainTextRef::new(&bad_footer_plaintext));
        blob_ct.append_authentication(&keeper);
        let raw = blob_ct.to_vec();

        let mut reader = BlobReader::new(keeper.clone(), CipherTextRef::new(&raw)).unwrap();
        assert!(
            reader.refs().is_err(),
            "refs() must return Err for malformed footer {:?}",
            bad_footer_plaintext
        );
    }
}

#[test]
fn load_from_universal_key_rejects_empty_short_and_oversized_files() {
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("secret-universal-key");

    for bad_len in [0usize, 1, 16, UNIVERSAL_KEY_BYTES - 1, UNIVERSAL_KEY_BYTES + 1, 65, 1024] {
        fs::write(&key_path, vec![0x42u8; bad_len]).unwrap();
        let err = match Keeper::load_from_universal_key(dir.path()) {
            Ok(_) => panic!("expected error for {bad_len}-byte universal key"),
            Err(e) => e,
        };
        assert_eq!(
            err.kind(),
            io::ErrorKind::InvalidData,
            "expected InvalidData for {bad_len}-byte universal key"
        );
    }

    // Valid 32-byte key succeeds.
    fs::write(&key_path, vec![0x42u8; UNIVERSAL_KEY_BYTES]).unwrap();
    assert!(Keeper::load_from_universal_key(dir.path()).is_ok());
}

#[test]
fn write_new_universal_key_sets_0600_prevents_overwrite_and_matches_direct_keeper() {
    let dir = tempfile::tempdir().unwrap();
    Keeper::write_new_universal_key(dir.path()).unwrap();

    let key_path = dir.path().join("secret-universal-key");
    let raw = fs::read(&key_path).unwrap();
    assert_eq!(raw.len(), UNIVERSAL_KEY_BYTES);

    // No temp files left behind.
    let entries: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries.len(), 1);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret-universal-key must be mode 0600, got {:o}", mode);
    }

    // Second call must refuse to overwrite existing key.
    let err = Keeper::write_new_universal_key(dir.path()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&key_path).unwrap(), raw);

    // Loaded keeper must match Keeper::new(disk_bytes) with single derivation.
    let loaded = Keeper::load_from_universal_key(dir.path()).unwrap();
    let direct = Keeper::new(secstr::SecStr::new(raw));
    let mut fp1 = [0u8; 64];
    let mut fp2 = [0u8; 64];
    let salt = [0u8; 16];
    loaded.fingerprint(b"check", &salt, &mut fp1);
    direct.fingerprint(b"check", &salt, &mut fp2);
    assert_eq!(fp1, fp2);
}

#[test]
#[should_panic]
fn keeper_new_rejects_empty_key() {
    let _ = Keeper::new(secstr::SecStr::new(vec![]));
}

#[test]
fn collapse_empty_ciphertext_does_not_panic() {
    let mut empty = CipherText::empty();
    assert!(empty.collapse().is_empty());
}
