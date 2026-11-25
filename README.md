Hat Backup System
=================

Disclaimer: This is not an official Google product.

Warning: This software is pre-alpha and under active development. Do not use it
as the sole copy of important data. See [`SECURITY.md`](SECURITY.md) for current
confidentiality guarantees and known gaps, and [`ROADMAP.md`](ROADMAP.md) for
what remains before a stable release.

Project
-------
Hat is a backend-agnostic, snapshotting backup system with content-addressed
chunk deduplication, authenticated encryption, and crash-tolerant recovery.

Why Hat (Design Niche)
----------------------
Most open-source backup tools (such as Restic, Borg, and Kopia) expose
repository directory layouts (`snapshots/`, `index/`, `data/`), store
variable-sized pack files, and rely on symmetric keys held on the backup host.
Hat is built around a narrower storage contract and threat model:

- **Opaque, fixed-size backend layout:** External storage holds only flat,
  randomly named blobs padded with random bytes to exact `max_blob_size` and
  authenticated against their own names—hiding repository structure, snapshot
  boundaries, and per-blob fill levels *(disable Zstd via `--compression none`
  when strict volume hiding is preferred; see [`SECURITY.md`](SECURITY.md))*.
- **Minimal dumb-storage contract:** Backends require only four flat operations
  (`put`, `get`, `delete`, `list`) with no server daemon, directory hierarchy,
  or byte-range reads. Mark-and-sweep `gc` operates with zero remote reads.
- **Asymmetric write-only backup architecture:** Blob access keys and footers
  are sealed with Curve25519 public-key sealed boxes (`Keeper`), and `commit`,
  `meta_commit`, `delete`, and `gc` never require decryption keys. Unattended
  backup hosts can commit, deduplicate, and prune without being able to decrypt
  historical backups *(key split exists in `Keeper`; CLI write-only key
  management is tracked in [`ROADMAP.md`](ROADMAP.md))*.
- **Zero-FUSE interactive browsing:** Built-in `hat browse` terminal UI provides
  in-place snapshot time-travel (`[`/`]`), recursive search (`f`), and
  collision-safe multi-file checkout without installing FUSE.

Key design properties implemented today:

- **Encrypted, fixed-size blobs:** File streams are split with keyed FastCDC
  content-defined chunking (16 KiB min / 64 KiB avg / 256 KiB max), hashed
  uncompressed, compressed with Zstd by default (falling back to raw if
  incompressible; configurable via `--compression zstd[:1-19]|none`), and
  sealed with `XChaCha20-Poly1305-IETF` (24-byte random nonces, metadata-bound
  AAD) via `libsodium-rs` (`#![forbid(unsafe_code)]`), packed into
  random-padded `max_blob_size` blobs with keyed BLAKE2b-512 authentication
  tags covering both blob name and ciphertext.
- **Single local SQLite cache (`cache/index.sqlite3`):** All local state
  (`hashes`, `blobs`, `family`, `snapshots`, flat per-family `stat_cache`, and
  local `tree_edges`) lives in one `0600` WAL-mode SQLite file protected by a
  fail-fast `0600` lock (`cache/.hat.lock`).
- **Incremental scanning:** Filesystem walks stage into an ephemeral in-memory
  `StagedTree` and skip re-reading/re-hashing unchanged files via `stat_cache`
  (keyed by `(family_id, rel_path)` with `dev`/`ino`/sub-second `mtime`/`ctime`
  identity and a racily-clean guard). Stale rows are pruned per family on commit
  and for deleted families during `gc`.
- **Atomic commits & zero-remote-read mark-and-sweep GC:** Blobs are flushed to
  the backend before SQLite publishes a completed snapshot row. Snapshot deletion
  is O(1) local, and mark-and-sweep `gc` computes liveness locally from
  `hashes.childs` + `tree_edges` (0 remote reads in steady state and after
  `recover`), failing closed if index invariants are violated.
- **Full local cache recovery:** `recover` rebuilds the local SQLite index and
  `tree_edges` cache from backend blobs and the repository key alone.

Roadmap & Security
------------------
- [`ROADMAP.md`](ROADMAP.md) — current status, completed milestones, and phased plan
- [`SECURITY.md`](SECURITY.md) — cryptographic design, size-hiding invariants, and known gaps

Building from source
--------------------
Required system libraries and toolchain:

- Rust toolchain (`rust-version = "1.99"`, edition 2021)
- `libsodium`
- `libsqlite3`
- `libfuse` (optional, only when building with `--features fuse`)

Build:

```sh
cargo build --release
```

Usage (`hat`)
-------------
The CLI supports a local directory backend (`--backend file --backend-path <DIR>`)
and an external command backend (`--backend cmd`). Backend settings passed to
`hat init` are saved in `<state-dir>/config.json` (`0600`) so subsequent commands
only need `--state-dir` or `HAT_STATE_DIR`.

```sh
# Initialize a new repository state directory (0700 dir, 0600 key & config)
hat init /path/to/hat-state --backend file --backend-path /path/to/blobs
export HAT_STATE_DIR=/path/to/hat-state

# Commit a directory snapshot into family "docs"
hat commit docs /path/to/source-dir

# List families, snapshots, or directory contents (defaults to `/`; `-l` long, `-R` recursive)
hat ls -l
hat ls -l docs
hat ls -lR docs/latest

# Interactively browse snapshots, recursively find files (`f`), mark (`Space`), and checkout (`c`) into cwd
hat browse
hat browse docs/latest --dest ./restore-dir

# Checkout latest snapshot, a specific snapshot (by composite name, ID, or date prefix), or a subpath
hat checkout docs /path/to/restore-dir
hat checkout docs/1 /path/to/restore-dir
hat checkout docs/2026-11-25/subdir /path/to/restore-dir

# Verify every indexed hash chunk against the backend
hat check

# Delete a snapshot and garbage-collect unreferenced blobs (`-n`/`--dry-run` to preview)
hat delete docs/1 --dry-run
hat delete docs/1
hat gc --dry-run
hat gc

# Rebuild local cache/index.sqlite3 from backend blobs (requires secret-universal-key)
hat recover

# Clean up incomplete local state and republish snapshot metadata
hat resume
```

Global options (`--state-dir`, `--backend`, `--backend-path`, `--compression`,
`-q/--quiet`, `-v/--verbose`, `--json`) can be passed before or after any
subcommand. Precedence is CLI flags > environment variables (`HAT_STATE_DIR`,
`HAT_BACKEND`, `HAT_BACKEND_PATH`, `HAT_COMPRESSION`) > `<state-dir>/config.json`.

Testing & Benchmarks
--------------------
```sh
cargo test
cargo bench --bench hat_ops
```

License and copyright
---------------------
See the files `LICENSE` and `AUTHORS.txt`.

Contributions
-------------
We gladly accept contributions/fixes/improvements via pull requests, as long as
the author has signed the Google Contributor License:

- https://developers.google.com/open-source/cla/individual
- https://developers.google.com/open-source/cla/corporate

Authors
-------
See `AUTHORS.txt`.

This project is inspired by a previous version of the system written in Haskell:
https://github.com/mortenbp/hindsight
