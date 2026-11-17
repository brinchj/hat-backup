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
- Crash-safe commit / delete / resume
- Correct GC across shared chunks
- Restores that match source bytes (and, eventually, metadata)
- Verifiable integrity (`check` / FSCK) and honest failure under corruption

---

## Done recently (keep from regressing)

- Modern Rust/Cargo baseline (edition 2021, current deps)
- In-memory dataloss suite: round-trip, shared-chunk GC, fault-inject resume,
  incomplete-blob recover safety
- On-disk confidence suite: `FileBackend` + checkout oracle, process kill +
  reopen, CLI E2E with file backend, adversarial truncate/missing blob,
  `Hat::check` / `hatbin check`
- Commit durability: flush blob ciphertext **before** SQLite records hash refs
- Relative-root `snapshot_dir` (no absolute `/private/var/...` prefix tax)
- Key-index tag reset between snaps (correct multi-snap cleanup at relative root)
- Criterion benches: bulk MiB + filesystem-shaped trees (`cargo bench --bench hat_ops`)
- Fixed-size blob padding documented as a confidentiality invariant
  ([`SECURITY.md`](SECURITY.md))

---

## Phase 1 — Trust the bits (correctness / durability)

Highest priority before any “safe for disposable data” claim.

| Item | Notes |
|------|--------|
| `FileBackend` fsync / atomic put | Today `flush` is a no-op; define and test durable write contract |
| Broader kill/reopen matrix | Already started; expand stages + cold remount |
| Power-fail / block-fault lane | Firecracker, `dm-flakey`, or equivalent — optional nightly |
| Recover hardening | Read-only recover option; fewer panics; clear errors on missing roots |
| Replace panic-heavy paths | Systematically remove `unwrap`/`expect` on recoverable errors |
| Metadata restore completeness | Modes partially done; owners/times/non-utf8 paths still open |
| Symlink / special-file policy | Document and test (dangling, cycles, devices, sparse) |

**Exit:** restore drills on real disk after kill; GC never deletes live shared
data; corrupt/missing blobs never look “healthy.”

---

## Phase 2 — Usable product surface

| Item | Notes |
|------|--------|
| First-class local file backend in CLI | Default or equal to `CmdBackend` for hermetic/local use |
| CmdBackend batching / long-lived helper | Cut process-per-put cost if external helpers stay |
| Stable CLI + docs | init / commit / checkout / delete / gc / resume / check / recover |
| `--pretend` where it matters | Especially delete/gc (README wishlist) |
| Progress without log spam | Structured progress, not unbounded prints |
| Upgrade / migration story | Schema and on-disk format versioning |

**Exit:** a new user can init → backup → kill → resume → checkout → check
without reading the source.

---

## Phase 3 — Confidentiality & key management

See also [`SECURITY.md`](SECURITY.md).

| Item | Notes |
|------|--------|
| Formal threat model | Adversaries: backend observer, stolen laptop, malicious blob store |
| Key wrapping | Passphrase / OS keychain; stop shipping raw `secret-universal-key` as the only story |
| Key rotation / recovery | Document and implement |
| Local metadata stance | Encrypt indexes **or** explicitly treat state dir as secret as the key |
| Side channels | Object count / timing; document residual leakage even with fixed-size blobs |
| Keep fixed-size blob padding | Do not remove for perf without a reviewed replacement |

**Exit:** confidentiality claims match documented guarantees; no silent
regressions of size-hiding.

---

## Phase 4 — Verify, scale, operate

| Item | Notes |
|------|--------|
| Stronger FSCK | Beyond `check`: snapshot roots, graph consistency, optional repair |
| Signed / published snapshot tips | Authenticity of “this is the backup set” |
| Soak / large-tree benches | Nightly; grow beyond ~800-file fixtures |
| Concurrent access policy | Single-writer lock or documented exclusion |
| Blob store I/O isolation | Background thread(s) for backend (README) |
| Platform battle-testing | Linux/macOS CI matrix; restore on clean machine |

**Exit:** ops can detect silent rot; performance under “many small files” is
measured and acceptable.

---

## Explicit non-goals (near term)

- Competing feature-for-feature with restic/borg/kopia before Phases 1–2
- Perfect traffic-analysis resistance
- Multi-tenant / multi-user ACLs
- Guaranteeing CmdBackend semantics for every user’s shell scripts

---

## How we measure progress

- **Correctness:** `cargo test --lib` + integration tests under `tests/`
- **Perf:** `cargo bench --bench hat_ops` (bulk + `filesystem` filter)
- **Security changes:** checklist in [`SECURITY.md`](SECURITY.md)
- Prefer failing loud over silent success when blobs or indexes are inconsistent

---

## Suggested near-term order

1. Durable `FileBackend` writes + tests  
2. Expand crash/reopen + recover UX  
3. Metadata / non-utf8 restore  
4. CLI defaults + docs for file backend  
5. Key wrapping + threat-model doc  
6. Stronger FSCK + soak  

Update this file when a phase item lands or the bar moves.
