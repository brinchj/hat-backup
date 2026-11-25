# Security and confidentiality

Hat is pre-alpha. This document records **design measures that exist in the
code today** so we do not accidentally remove them for performance, and
**known gaps** so we do not over-claim readiness.

The project README still warns that Hat is not ready for trusted use. Treat
this file as an engineering checklist, not a completed security audit.

## Goals (confidentiality / privacy)

Against an observer who can see **backend blob storage** (object names, sizes,
upload timing) but **not** the local key material, we aim to limit leakage of:

1. File contents
2. How much new or changed data each flush/commit wrote (fill level / delta size)
3. Trivial tampering with stored blobs without detection

Local SQLite indexes and the on-disk key file are a different trust boundary
(see [Known gaps](#known-gaps)).

## Measures that must not be “optimized away”

### Fixed-size blobs (size hiding)

**What:** Every flushed blob is random-padded to exactly `max_blob_size` before
it is written to the backend (`Blob::to_ciphertext` → `random_pad_upto` →
assert length == `max_len`). See `src/blob/blob.rs`.

**Why:** Without padding, ciphertext length tracks how full the blob was.
An observer of the backend (or of upload sizes) could infer approximate
amounts of new/changed data—especially on commit barriers that flush
partially filled blobs. Fixed size hides fill level.

**Do not:** Ship “tight packing” / variable-length blobs for throughput unless
there is an explicit, reviewed replacement for this size-hiding property
(and tests that encode the invariant).

**Trade-off:** Partial flushes still write a full `max_blob_size` object
(write amplification). That cost is accepted for confidentiality.

### Chunk encryption inside blobs

**What:** File/chunks are sealed with per-chunk keys tied into `HashRef`
metadata (`RefKey::seal` / AEAD XChaCha20-Poly1305-IETF with 192-bit random
nonces via libsodium; AAD binds node/leaf type, chunk offset, full 64-byte
content hash, and blob name). Blob footers that list chunk refs are sealed
with an access key (`FixedKey::seal`, AAD bound to blob name). See
`src/crypto/` and `src/blob/blob.rs`.

**Why:** Backend operators should not read plaintext chunk bytes from stored
objects, and chunks/footers cannot be transplanted across blobs or offsets.

### Blob authentication

**What:** Each blob ciphertext ends with a keyed BLAKE2b-512 authentication
tag (`append_authentication` / `strip_authentication`, verified in constant
time via `sodium_memcmp`) covering the blob name and full ciphertext bytes.

**Why:** Detect accidental or malicious modification, truncation, or renaming
of stored blob objects.

### Content-addressed hashing with keyed fingerprints

**What:** Hashes used for dedup/identity are produced with keyed hashing
(Blake2b personalization via the keeper), not a naked unkeyed digest of
plaintext alone in the public layout.

**Why:** Reduce trivial offline guessing / cross-repo correlation from hash
values alone (not a full anonymity guarantee).

### Local key material

**What:** `init` writes `secret-universal-key`; `Hat::open_repository` loads
it into `crypto::keys::Keeper`, which derives further keys (fingerprint,
blob authentication, box keypairs, etc.).

**Why:** Confidentiality of backend data depends on keeping this key secret.

## Related durability note (not confidentiality)

Commit paths flush blob ciphertext **before** recording hash refs in SQLite
so a crash cannot leave indexes pointing at data that never hit the backend.
That is a **integrity/durability** property. It must not be confused with
size hiding: barrier flushes of partial blobs remain **full-sized** on
purpose.

## Known gaps

Be honest in reviews and releases:

- **Not a finished privacy product.** README status still applies.
- **Universal key on disk** is a raw local file; no passphrase wrapping,
  HSM, or key rotation story.
- **Local metadata** (SQLite family/key/hash indexes) holds paths, structure,
  and references in clearer form than backend blobs—protect the state dir.
- **CmdBackend** (default CLI) runs external put/get helpers; treat those
  programs and their environment as part of the trust boundary.
- **Traffic analysis:** Fixed blob size hides *fill level per object*, not
  *how many* blobs were uploaded or *when*. Timing and object-count side
  channels remain.
- **Compression side channel:** Zstd chunk compression is enabled by default
  (chunks that do not shrink are stored raw). Compress-then-encrypt can leak
  approximate data compressibility through total blob counts across large
  uploads; pass `--compression none` (or configure it at `hat init`) when
  strict volume/compressibility hiding is preferred over storage savings.
- **Keyed chunking & dedup:** FastCDC uses a per-repository keyed seed derived
  from the master key so chunk boundaries are not predictable without the key,
  but identical content still collapses to shared hashes; a capable adversary
  with chosen-plaintext access may learn about content sharing.
- **Recovery rollback:** Without an out-of-band pinned root hash
  (`Hat::recover_with_expected_root`), a backend attacker who deletes recent
  blobs can cause `recover` to select an older surviving `SnapshotList`.
- **No formal threat model / audit** yet.

## Checklist before changing crypto or blob layout

- [ ] Does the change alter blob **wire length** or padding? If yes, document
      the new size-hiding story or keep fixed `max_blob_size` padding.
- [ ] Do unit tests still assert flushed blob length == `max_blob_size`
      where that invariant is required?
- [ ] Do process-crash / resume tests still pass if flush ordering changes?
- [ ] Update this file when adding or removing a confidentiality measure.

## Reporting

This repository is experimental. Prefer opening an issue describing the
issue, impact, and affected versions/commits. Do not assume a funded
vulnerability-response process exists yet.
