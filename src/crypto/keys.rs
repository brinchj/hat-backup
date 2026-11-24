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

use crate::blob;
use crate::errors::CryptoError;
use libsodium_rs::{
    crypto_aead::xchacha20poly1305, crypto_box, crypto_generichash::blake2b, ensure_init, random,
    utils,
};
use secstr;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Once;

const UNIVERSAL_KEY_FILENAME: &str = "secret-universal-key";
pub const UNIVERSAL_KEY_BYTES: usize = 32;

static SODIUM_INIT: Once = Once::new();

/// Initialize libsodium once and abort if initialization fails.
pub fn init_sodium() {
    SODIUM_INIT.call_once(|| {
        ensure_init().expect("libsodium initialization failed");
    });
}

/// Constant-time equality comparison using `libsodium_rs::utils::memcmp`.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    if a.is_empty() {
        return true;
    }
    init_sodium();
    utils::memcmp(a, b)
}

// Crypto personalizations. Do not change these.
const UNIVERSAL_KEY_MSG: &[u8] = b"hat-backup:universal-key:rust";

const HAT_PERSONALIZATION: &[u8; blake2b::PERSONALBYTES] = b"hat-backup~~rust";

#[cfg_attr(feature = "flame_it", flame)]
pub fn compute_salt(node_type: blob::NodeType, leaf_type: blob::LeafType) -> Box<[u8]> {
    use byteorder::{LittleEndian, WriteBytesExt};

    let mut salt = [0u8; 16];
    {
        let (mut left, mut right) = salt.split_at_mut(8);
        left.write_u64::<LittleEndian>(From::from(node_type))
            .unwrap();
        right
            .write_u64::<LittleEndian>(From::from(leaf_type))
            .unwrap();
    }

    Box::new(salt)
}

#[cfg_attr(feature = "flame_it", flame)]
pub fn random_bytes(size: usize) -> secstr::SecStr {
    init_sodium();
    secstr::SecStr::new(random::bytes(size))
}

pub fn keyed_fingerprint_simple(sk: &[u8], msg: &[u8], out: &mut [u8]) {
    keyed_fingerprint(sk, msg, &HAT_PERSONALIZATION[..], out)
}

#[cfg_attr(feature = "flame_it", flame)]
pub fn keyed_fingerprint(sk: &[u8], msg: &[u8], salt: &[u8], out: &mut [u8]) {
    init_sodium();
    assert_eq!(blake2b::SALTBYTES, salt.len());
    assert!(
        !sk.is_empty() && sk.len() <= blake2b::KEYBYTES_MAX,
        "invalid BLAKE2b key length: {}",
        sk.len()
    );
    assert!(
        out.len() >= blake2b::BYTES_MIN && out.len() <= blake2b::BYTES_MAX,
        "invalid BLAKE2b output length: {}",
        out.len()
    );

    let mut digest = blake2b::hash_with_salt_and_personal(
        msg,
        Some(sk),
        out.len(),
        Some(salt),
        Some(&HAT_PERSONALIZATION[..]),
    )
    .expect("BLAKE2b keyed_fingerprint failed");
    out.copy_from_slice(&digest);
    utils::memzero(&mut digest);
}

pub struct Keeper {
    universal_key: secstr::SecStr,
    fingerprint_key: Option<secstr::SecStr>,
    blob_authentication_key: Option<secstr::SecStr>,

    data_key_pk: Option<crypto_box::PublicKey>,
    data_key_sk: Option<crypto_box::SecretKey>,

    naming_key_pk: Option<crypto_box::PublicKey>,
    naming_key_sk: Option<crypto_box::SecretKey>,

    access_key_pk: Option<crypto_box::PublicKey>,
    access_key_sk: Option<crypto_box::SecretKey>,
}

impl Keeper {
    pub fn load_from_universal_key(dir: &Path) -> Result<Keeper, io::Error> {
        let key_path = dir.join(UNIVERSAL_KEY_FILENAME);
        let f = fs::File::open(&key_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = f.metadata()?.permissions().mode();
            if mode & 0o077 != 0 {
                warn!(
                    "universal key {} has permissive mode {:o}; expected 0600",
                    key_path.display(),
                    mode & 0o777
                );
            }
        }
        let mut buf = Vec::with_capacity(UNIVERSAL_KEY_BYTES + 1);
        f.take((UNIVERSAL_KEY_BYTES + 1) as u64)
            .read_to_end(&mut buf)?;

        let key = secstr::SecStr::new(buf);
        if key.unsecure().len() != UNIVERSAL_KEY_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid universal key length: expected {} bytes, got {}",
                    UNIVERSAL_KEY_BYTES,
                    key.unsecure().len()
                ),
            ));
        }

        Ok(Keeper::new(key))
    }

    pub fn write_new_universal_key(dir: &Path) -> Result<(), io::Error> {
        let final_path = dir.join(UNIVERSAL_KEY_FILENAME);
        if final_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("universal key already exists: {}", final_path.display()),
            ));
        }

        let suffix = hex::encode(random_bytes(8).unsecure());
        let tmp_path = dir.join(format!(".{}.tmp-{}", UNIVERSAL_KEY_FILENAME, suffix));
        let raw_key = random_bytes(UNIVERSAL_KEY_BYTES);

        let write_result = (|| -> Result<(), io::Error> {
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp_path)?;
            f.write_all(raw_key.unsecure())?;
            f.sync_all()?;
            drop(f);

            // hard_link fails atomically with AlreadyExists if final_path appeared concurrently.
            fs::hard_link(&tmp_path, &final_path)?;
            Ok(())
        })();

        let _ = fs::remove_file(&tmp_path);
        write_result?;

        #[cfg(unix)]
        {
            if let Ok(d) = fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }

        Ok(())
    }

    pub fn new(key: secstr::SecStr) -> Keeper {
        init_sodium();
        assert_eq!(
            key.unsecure().len(),
            UNIVERSAL_KEY_BYTES,
            "universal key must be {} bytes",
            UNIVERSAL_KEY_BYTES
        );
        // Personalize key for Hat and make it 256-bit (32 bytes).
        let universal_key = Keeper::from_key_and_nonce(&key, UNIVERSAL_KEY_MSG, 32);

        let mut keeper = Keeper {
            universal_key,
            fingerprint_key: None,
            blob_authentication_key: None,
            data_key_pk: None,
            data_key_sk: None,
            access_key_pk: None,
            access_key_sk: None,
            naming_key_pk: None,
            naming_key_sk: None,
        };

        keeper.init();

        keeper
    }

    #[cfg(test)]
    pub fn new_for_testing() -> Keeper {
        Keeper::new(secstr::SecStr::new(vec![0; 32]))
    }

    fn init(&mut self) {
        // Generate key used for fingerprinting.
        self.fingerprint_key = Some(self.from_nonce("hat:FINGERPRINT-key".as_bytes(), 64));

        // Generate key for authenticating blob data.
        self.blob_authentication_key =
            Some(self.from_nonce("hat:BLOB-AUTHENTICATION-key".as_bytes(), 64));

        // Generate data key.
        // Required for reading blob data without a direct reference.
        let (pk, sk) = self.x25519_key_pair_from_nonce("hat:DATA-key-x25519".as_bytes());
        self.data_key_pk = Some(pk);
        self.data_key_sk = Some(sk);

        // Generate access key.
        // Required for reading any blob data (with direct reference or with data key).
        let (pk, sk) = self.x25519_key_pair_from_nonce("hat:ACCESS-key-x25519".as_bytes());
        self.access_key_pk = Some(pk);
        self.access_key_sk = Some(sk);

        // Generate naming key.
        // Required for reading blob names.
        let (pk, sk) = self.x25519_key_pair_from_nonce("hat:NAMING-key-x25519".as_bytes());
        self.naming_key_pk = Some(pk);
        self.naming_key_sk = Some(sk);
    }

    fn from_key_and_nonce(key: &secstr::SecStr, nonce: &[u8], outlen: usize) -> secstr::SecStr {
        let mut out = secstr::SecStr::new(vec![0; outlen]);
        keyed_fingerprint_simple(
            key.unsecure(),
            nonce,
            out.unsecure_mut(),
        );
        out
    }

    pub fn from_nonce(&self, nonce: &[u8], outlen: usize) -> secstr::SecStr {
        Self::from_key_and_nonce(&self.universal_key, nonce, outlen)
    }

    fn x25519_key_pair_from_nonce(
        &self,
        nonce: &[u8],
    ) -> (crypto_box::PublicKey, crypto_box::SecretKey) {
        let seed = self.from_nonce(nonce, crypto_box::SECRETKEYBYTES);
        crypto_box::KeyPair::from_seed(seed.unsecure())
            .expect("x25519 keypair from 32-byte seed")
            .into_tuple()
    }

    fn asymmetric_lock(pk: &crypto_box::PublicKey, msg: &[u8]) -> Vec<u8> {
        init_sodium();
        crypto_box::seal_box(msg, pk).expect("sealed box encryption failed")
    }

    fn asymmetric_unlock(
        pk: &crypto_box::PublicKey,
        sk: &crypto_box::SecretKey,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        init_sodium();
        crypto_box::open_sealed_box(ciphertext, pk, sk)
            .map_err(|_| CryptoError::from("crypto read failed: sealed box open failed"))
    }

    pub fn data_lock(&self, msg: &[u8]) -> Vec<u8> {
        Keeper::asymmetric_lock(
            self.data_key_pk.as_ref().expect("need data public key"),
            msg,
        )
    }

    pub fn data_unlock(&self, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let pk = self
            .data_key_pk
            .as_ref()
            .ok_or_else(|| CryptoError::from("missing data public key"))?;
        let sk = self
            .data_key_sk
            .as_ref()
            .ok_or_else(|| CryptoError::from("missing data private key"))?;
        Keeper::asymmetric_unlock(pk, sk, ciphertext)
    }

    pub fn access_lock(&self, msg: &[u8]) -> Vec<u8> {
        Keeper::asymmetric_lock(
            self.access_key_pk
                .as_ref()
                .expect("need access public key"),
            msg,
        )
    }

    pub fn access_unlock(&self, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let pk = self
            .access_key_pk
            .as_ref()
            .ok_or_else(|| CryptoError::from("missing access public key"))?;
        let sk = self
            .access_key_sk
            .as_ref()
            .ok_or_else(|| CryptoError::from("missing access private key"))?;
        Keeper::asymmetric_unlock(pk, sk, ciphertext)
    }

    pub fn naming_lock(&self, msg: &[u8]) -> Vec<u8> {
        Keeper::asymmetric_lock(
            self.naming_key_pk.as_ref().expect("need naming public key"),
            msg,
        )
    }

    pub fn naming_unlock(&self, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let pk = self
            .naming_key_pk
            .as_ref()
            .ok_or_else(|| CryptoError::from("missing naming public key"))?;
        let sk = self
            .naming_key_sk
            .as_ref()
            .ok_or_else(|| CryptoError::from("missing naming private key"))?;
        Keeper::asymmetric_unlock(pk, sk, ciphertext)
    }

    pub fn fingerprint(&self, msg: &[u8], salt: &[u8], out: &mut [u8]) {
        let key = self.fingerprint_key.as_ref().expect("need fingerprint key");
        keyed_fingerprint(key.unsecure(), msg, salt, out);
    }

    pub fn blob_authentication(&self, blob_name: &[u8], blob: &[u8], out: &mut [u8]) {
        init_sodium();
        assert!(
            out.len() >= blake2b::BYTES_MIN && out.len() <= blake2b::BYTES_MAX,
            "invalid BLAKE2b output length: {}",
            out.len()
        );
        let key = self
            .blob_authentication_key
            .as_ref()
            .expect("need blob authentication key");
        let salt: &[u8; blake2b::SALTBYTES] = b"blob~~~~blob~~~~";
        let name_len = (blob_name.len() as u64).to_le_bytes();

        let mut state = blake2b::State::new_with_salt_and_personal(
            Some(key.unsecure()),
            out.len(),
            Some(salt),
            Some(&HAT_PERSONALIZATION[..]),
        )
        .expect("BLAKE2b state init failed");
        state.update(&name_len);
        state.update(blob_name);
        state.update(blob);
        let tag = state.finalize();
        out.copy_from_slice(&tag);
    }

    pub fn symmetric_lock(
        msg: &[u8],
        ad: &[u8],
        nonce: &[u8],
        key: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        init_sodium();
        let key = xchacha20poly1305::Key::from_bytes(key).map_err(|_| {
            CryptoError::from(format!(
                "crypto write failed: invalid key length {} (expected {})",
                key.len(),
                xchacha20poly1305::KEYBYTES
            ))
        })?;
        let nonce = xchacha20poly1305::Nonce::try_from(nonce).map_err(|_| {
            CryptoError::from(format!(
                "crypto write failed: invalid nonce length {} (expected {})",
                nonce.len(),
                xchacha20poly1305::NPUBBYTES
            ))
        })?;
        msg.len()
            .checked_add(xchacha20poly1305::ABYTES)
            .ok_or_else(|| CryptoError::from("crypto write failed: ciphertext length overflow"))?;

        xchacha20poly1305::encrypt(msg, Some(ad), &nonce, &key)
            .map_err(|_| CryptoError::from("crypto write failed: symmetric encryption failed"))
    }

    pub fn symmetric_unlock(
        key: &[u8],
        ciphertext: &[u8],
        ad: &[u8],
        nonce: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        init_sodium();
        let key = xchacha20poly1305::Key::from_bytes(key).map_err(|_| {
            CryptoError::from(format!(
                "crypto read failed: invalid key length {} (expected {})",
                key.len(),
                xchacha20poly1305::KEYBYTES
            ))
        })?;
        let nonce = xchacha20poly1305::Nonce::try_from(nonce).map_err(|_| {
            CryptoError::from(format!(
                "crypto read failed: invalid nonce length {} (expected {})",
                nonce.len(),
                xchacha20poly1305::NPUBBYTES
            ))
        })?;

        xchacha20poly1305::decrypt(ciphertext, Some(ad), &nonce, &key)
            .map_err(|_| CryptoError::from("crypto read failed: symmetric decryption failed"))
    }
}
