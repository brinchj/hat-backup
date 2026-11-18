#!/usr/bin/env bash
# Run the Linux FileBackend power-loss test.
#
# On macOS this builds docker/powerfail.Dockerfile and runs it privileged.
# Inside Linux it is the test itself: FileBackend unit tests (sync_all path)
# plus the ignored device-mapper crash test.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)

if [[ "${1:-}" != "--inside" ]]; then
    docker build -t hat-powerfail:local -f "$ROOT/docker/powerfail.Dockerfile" "$ROOT/docker"
    exec docker run --rm --privileged \
        -v "$ROOT":/src \
        -w /src \
        -e CARGO_TARGET_DIR=/tmp/hat-target \
        -e SODIUM_USE_PKG_CONFIG=1 \
        hat-powerfail:local \
        bash /src/scripts/linux-powerfail.sh --inside
fi

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "linux-powerfail: --inside must run on Linux" >&2
    exit 1
fi
if [[ "$(id -u)" != "0" ]]; then
    echo "linux-powerfail: needs root for loop, device-mapper, and mount" >&2
    exit 1
fi

modprobe loop || true
modprobe dm-mod || true
command -v dmsetup >/dev/null
command -v losetup >/dev/null
command -v mkfs.ext4 >/dev/null
rustc --version

# Linux sync_all path: the unit tests call FileBackend::store.
cargo test --lib backend::file::tests -- --nocapture
# Crash cut: returned store() is on the block device; an unsynced write is not.
cargo test --test powerfail_linux -- --ignored --nocapture
