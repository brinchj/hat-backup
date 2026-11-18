# Linux image for the FileBackend power-loss test.
# Device-mapper, loop, and ext4 tools are required. libsodium is linked
# through pkg-config so the build does not compile libsodium from source.
FROM rust:1-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config \
        libsodium-dev \
        libsqlite3-dev \
        dmsetup \
        e2fsprogs \
        util-linux \
        mount \
    && rm -rf /var/lib/apt/lists/*

ENV SODIUM_USE_PKG_CONFIG=1
ENV PATH="/usr/local/sbin:/usr/sbin:/sbin:${PATH}"
