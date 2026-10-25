#!/bin/sh
# a1029: build the two binary sets inside rb-bench-1029.
#   rel:  exactly as build.yml (Rust 1.98.0, --locked --release, stripped)
#   prof: same, plus symbols (line tables) and frame pointers for perf -g
set -e
. "$HOME/.cargo/env"
cd "$HOME/src"
export RELIABURGER_GIT_SHA="$1"
CARGO_TARGET_DIR="$HOME/t-rel" cargo build --locked --release --features ebpf --bin bun --bin relish 2>&1 | tail -n 3
CARGO_TARGET_DIR="$HOME/t-prof" CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=line-tables-only \
    RUSTFLAGS="-C force-frame-pointers=yes" \
    cargo build --locked --release --features ebpf --bin bun --bin relish 2>&1 | tail -n 3
ls -la "$HOME/t-rel/release/bun" "$HOME/t-rel/release/relish" "$HOME/t-prof/release/bun" "$HOME/t-prof/release/relish"
