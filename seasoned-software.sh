#!/bin/bash

set -euo pipefail

export CFLAGS=""
export CXXFLAGS=""

# Print environment for debuggin.
env

# Switch to nightly Rust as needed by Cargo-fuzz.
rustup override set nightly


# Install libsodium.
./travis-install-libsodium.sh


if [ -z ${PKG_CONFIG_PATH+x} ]; then
  PKG_CONFIG_PATH=""
fi
export PKG_CONFIG_PATH=$HOME/libsodium/lib/pkgconfig:$PKG_CONFIG_PATH

if [ -z ${LD_LIBRARY_PATH+x} ]; then
  LD_LIBRARY_PATH=""
fi
export LD_LIBRARY_PATH=$HOME/libsodium/lib:$LD_LIBRARY_PATH


# Build and register all tests known to Cargo-fuzz.
for t in $(cargo fuzz list|sed 's@\x1b[^m]*m@@g'); do
	echo "Building test: $t"
	cargo fuzz run $t --release -- -help=1
	exe="$(find fuzz/target -iname $t -executable)"

	echo "Registering: $exe"
	upload-binary "$exe"
done
