#!/bin/bash

set -euo pipefail

export CFLAGS=""
export CXXFLAGS=""

env

clang --version


curl https://sh.rustup.rs -sSf | sh -s -- -y
export PATH="/home/user/.cargo/bin:${PATH}"


rustup override set nightly
cargo install cargo-fuzz


export CUSTOM_LIBFUZZER_PATH="$(clang -print-file-name=libclang_rt.fuzzer-x86_64.a)"


./travis-install-libsodium.sh


if [ -z ${PKG_CONFIG_PATH+x} ]; then
  PKG_CONFIG_PATH=""
fi
export PKG_CONFIG_PATH=$HOME/libsodium/lib/pkgconfig:$PKG_CONFIG_PATH

if [ -z ${LD_LIBRARY_PATH+x} ]; then
  LD_LIBRARY_PATH=""
fi
export LD_LIBRARY_PATH=$HOME/libsodium/lib:$LD_LIBRARY_PATH


for t in $(cargo fuzz list|sed 's@\x1b[^m]*m@@g'); do
	echo "Building test: $t"
	cargo fuzz run $t --release -- -help=1
	exe="$(find fuzz/target -iname $t -executable)"

	echo "Registering: $exe"
	register-binary "$t" "$exe"
done
