#!/bin/sh
# a1030: build one binary set inside rb-bench-1030, as tools/build.sh of the
# 2026-10-20 profile record did, with the source and target directories named.
#   build.sh SRC SHA NAME [prof]
#   rel (default): exactly as build.yml (Rust 1.98.0, --locked --release, stripped)
#   prof: the same, plus line tables and frame pointers for perf -g
# The binaries land in $HOME/bin-NAME.
set -e
. "$HOME/.cargo/env"
cd "$1"
export RELIABURGER_GIT_SHA="$2"
name=$3
if [ "${4:-rel}" = prof ]; then
    CARGO_TARGET_DIR="$HOME/t-$name" CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=line-tables-only \
        RUSTFLAGS="-C force-frame-pointers=yes" \
        cargo build --locked --release --features ebpf --bin bun --bin relish 2>&1 | tail -n 3
else
    CARGO_TARGET_DIR="$HOME/t-$name" cargo build --locked --release --features ebpf --bin bun --bin relish 2>&1 | tail -n 3
fi
mkdir -p "$HOME/bin-$name"
cp "$HOME/t-$name/release/bun" "$HOME/t-$name/release/relish" "$HOME/bin-$name/"
sha256sum "$HOME/bin-$name/bun" "$HOME/bin-$name/relish"
