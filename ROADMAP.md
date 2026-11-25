# Roadmap

Living plan for taking Hat from pre-alpha experiment toward something that
could be trusted for real backups. Detail on confidentiality lives in
[`SECURITY.md`](SECURITY.md).

**Status today:** not production-ready. Do not use as the sole copy of
important data.

---

## North star

A backend-agnostic snapshotting backup engine with:

- Deduplicated, encrypted content in external storage
- Crash-safe commit / delete / recover
- Correct GC across shared chunks
- Restores that match source bytes (and, eventually, metadata)
- Verifiable integrity (`check` / FSCK) and honest failure under corruption

---

## Done recently (keep from regressing)

- Modern Rust/Cargo baseline (edition 2021, `rust-version = "1.99"`, current deps)
- **Single local SQLite cache (`cache/index.sqlite3`):** consolidated hashes,
  blobs, families, snapshots, flat `stat_cache`, and local `tree_edges` into one
  `0600` WAL-mode DB with `SQLITE_DBCONFIG_DEFENSIVE` and fail-closed
  `PRAGMA quick_check`; migration drops legacy `gc_metadata`, `key_data`, and
  `key_tree` tables
- **Flat `stat_cache` + ephemeral `StagedTree`:** replaced per-family SQLite
  `key_tree`/`key_data` DBs and `Process` actor wrappers with an O(log N)
  `BTreeMap`-backed in-memory `StagedTree` and a flat `(family_id, rel_path)`
  `stat_cache` storing full stat identity (`dev`, `ino`, `ctime`/`ctime_nsec`,
  `mtime_nsec`, racily-clean guard). Stale per-family entries are pruned on
  publish; deleted-family and swept-hash entries are pruned in `gc`
- **Mark-and-sweep GC with local `tree_edges` cache:** deleted reference-counting
  GC (`src/gc/`) and multi-stage commit/delete tag state machines. `commit`
  flushes all blobs before atomically publishing one `Tag::Done` snapshot row;
  `delete` is O(1) local; `gc`/`gc_pretend` mark liveness from `hashes.childs` +
  `tree_edges` with **0 remote reads** (steady state and post-`recover`), fail
  closed on missing hash IDs, and purge orphan `tree_edges` on startup
- **Chunked `SnapshotList`:** `meta_commit` splits large snapshot histories across
  multiple `SnapshotList` leaves so large repos never exceed `max_blob_size`
- **Checkout & path safety:** path-traversal rejection, safe symlink restore
  (including dangling and non-UTF-8 targets without chmodding targets), raw
  non-UTF-8 filenames end-to-end, `checkout --id <ID>`, and `LeafIterator`
  `Result` error propagation with clean CLI exit codes
- **Fail-fast exclusive repo lock:** `Hat::open_repository` acquires a `0600`
  non-blocking lock on `cache/.hat.lock`
- **Crypto & safety hardening:** migrated to `libsodium-rs` (`#![forbid(unsafe_code)]`),
  `XChaCha20-Poly1305-IETF` (24-byte random nonces, full chunk metadata AAD),
  blob-name-bound BLAKE2b-512 MACs and footer AAD, constant-time MAC/hash checks,
  atomic `0600` 32-byte master key inside `0700` state/cache dirs, fixed-size
  blob padding ([`SECURITY.md`](SECURITY.md)), and root-hash pinning API
  (`Hat::recover_with_expected_root`)
- **Test & benchmark suites (112 tests):** in-memory dataloss suite, on-disk
  `FileBackend` + checkout oracle, SIGKILL crash/reopen (`process_crash.rs`),
  CLI E2E (`cli_e2e.rs`), adversarial corruption/rollback (`adversarial.rs`),
  and Criterion benches (`cargo bench --bench hat_ops`)

---

## Phase 1 — Trust the bits (correctness / durability)

Highest priority before any “safe for disposable data” claim.

| Item | Notes |
|------|--------|
| `FileBackend` fsync / atomic put | **Puts done:** temp write, `sync_all()` (`F_FULLFSYNC` on macOS), rename, dir sync, then `done()`. `list` skips temp and non-hex names. Linux crash cut: `scripts/linux-powerfail.sh`. **Open:** deletes not dir-synced; stale `.tmp-*` files not swept; macOS power-fail not yet tested |
| Broader kill/reopen matrix | Atomic `BeforePublish`, between `commit` & `meta_commit`, and post-`delete` SIGKILL tests done; expand to cold remount & mid-GC kills |
| Power-fail / block-fault lane | Linux: `scripts/linux-powerfail.sh` (privileged Docker, dm-linear, `suspend --noflush`). Proves a returned `store()` is on the block device and an unsynced sibling is not. macOS and real host power cut are still open |
| Recover hardening | `recover` rebuilds `hashes` + `tree_edges` iteratively and supports root pinning (`recover_with_expected_root`). **Open:** read-only recover option; CLI flag for pinned root hash; fewer panics on corrupt CBOR/DB |
| Replace panic-heavy paths | Systematically remove `unwrap`/`expect` on recoverable DB/IO errors |
| Metadata restore completeness | **Done:** non-UTF-8 filenames and symlink targets, regular file modes. **Open:** owners (`uid`/`gid`), timestamps (`mtime`/`atime`), xattrs |
| Symlink / special-file policy | **Done:** symlink round-trip (broken links, non-UTF-8 targets, no target chmod). **Open:** explicit policy/tests for symlink cycles, devices/FIFOs, sparse files |

**Exit:** restore drills on real disk after kill; GC never deletes live shared
data; corrupt/missing blobs never look “healthy.”

---

## Phase 2 — Usable product surface

| Item | Notes |
|------|--------|
| First-class local file backend in CLI | `--backend file --backend-path` works; decide whether file backend should be default or co-equal with `CmdBackend` |
| CmdBackend batching / long-lived helper | Cut process-per-put cost if external helpers stay |
| Stable CLI + docs | `init` / `commit` / `checkout [--id]` / `delete` / `gc [--pretend]` / `resume` / `check` / `recover` / `ls` implemented and documented in `README.md`; polish UX and error messages |
| `--pretend` where it matters | `gc --pretend` done; add `--pretend` to `delete` |
| Progress without log spam | Structured progress, not unbounded prints (`InsertPathHandler` still prints periodically) |
| Upgrade / migration story | Schema migrations exist (`migrations/`), but moving from pre-single-DB `hash_index.sqlite3` requires `recover`; document and formalize versioned upgrades |

**Exit:** a new user can init → backup → kill → recover/resume → checkout → check
without reading the source.

---

## Phase 3 — Confidentiality & key management

See also [`SECURITY.md`](SECURITY.md).

| Item | Notes |
|------|--------|
| Formal threat model | Adversaries: backend observer, stolen laptop, malicious blob store |
| Key wrapping | Passphrase / OS keychain; stop shipping raw `secret-universal-key` as the only story |
| Key rotation / recovery | Document and implement |
| Local metadata stance | `0700`/`0600` fs permissions enforced on `cache/` and single `cache/index.sqlite3` (+ WAL/SHM); treat state dir as secret as the key (or add SQLCipher/encrypted DB later alongside key wrapping) |
| Side channels | Object count / timing; document residual leakage even with fixed-size blobs |
| Keep fixed-size blob padding | Do not remove for perf without a reviewed replacement |

**Exit:** confidentiality claims match documented guarantees; no silent
regressions of size-hiding.

---

## Phase 4 — Verify, scale, operate

| Item | Notes |
|------|--------|
| Stronger FSCK | Beyond `check` (chunk verification): verify `snapshots` → `TreeList` → `tree_edges` → `hashes` graph consistency, with optional repair |
| Signed / published snapshot tips | Authenticity of “this is the backup set” (CLI exposure for `recover_with_expected_root`) |
| Soak / large-tree benches & bounded scan RAM | Grow beyond ~800-file fixtures; consider streaming bottom-up `TreeList` writes if scaling past ~1M files (~1 GB `StagedTree` RAM) |
| Concurrent access policy | **Done:** fail-fast exclusive `0600` lock (`cache/.hat.lock`) in `Hat::open_repository` |
| Blob store I/O isolation | Background thread(s) for backend uploads/downloads |
| Platform battle-testing | Linux/macOS CI matrix; restore on clean machine |

**Exit:** ops can detect silent rot; performance and memory under “many small
files” are measured and acceptable.

---

## Explicit non-goals (near term)

- Competing feature-for-feature with restic/borg/kopia before Phases 1–2
- Perfect traffic-analysis resistance
- Multi-tenant / multi-user ACLs
- Guaranteeing CmdBackend semantics for every user’s shell scripts

---

## How we measure progress

- **Correctness:** `cargo test` (lib unit + `tests/` integration/adversarial/CLI/crash suites)
- **Perf:** `cargo bench --bench hat_ops` (bulk + `filesystem` filter)
- **Security changes:** checklist in [`SECURITY.md`](SECURITY.md)
- Prefer failing loud over silent success when blobs or indexes are inconsistent

---

## Suggested near-term order

1. Finish `FileBackend` durability leftovers (delete dir-sync, `.tmp-*` sweep,
   macOS power-fail proof)
2. Replace remaining `unwrap`/`expect` on recoverable DB/IO paths + expose
   pinned-root / read-only `recover` flags in CLI
3. Metadata restore (`mtime`, owners) + special-file policy tests
4. Key wrapping + threat-model doc
5. Stronger graph-level FSCK + large-tree soak / streaming commit if needed

Update this file when a phase item lands or the bar moves.
