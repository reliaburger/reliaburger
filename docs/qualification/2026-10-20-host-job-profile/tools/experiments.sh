#!/bin/sh
# a1029: build two throwaway experiment binaries (never committed) in rb-bench-1029.
#   e1: MAX_LEARNED_GRANT_DEPTH 16 -> 64
#   e2: e1, and the GitOps loop no longer wakes on every Raft metrics change
# Same compiler and flags as the stripped build.yml-style set.
set -e
. "$HOME/.cargo/env"
mkdir -p "$HOME/bin-rel"
cp "$HOME/t-rel/release/bun" "$HOME/t-rel/release/relish" "$HOME/bin-rel/"
for e in e1 e2; do
    rm -rf "$HOME/src-$e"
    cp -a "$HOME/src" "$HOME/src-$e"
    sed -i 's/^pub const MAX_LEARNED_GRANT_DEPTH: u64 = 16;/pub const MAX_LEARNED_GRANT_DEPTH: u64 = 64;/' \
        "$HOME/src-$e/src/meat/task_array_state.rs"
    grep -n '^pub const MAX_LEARNED_GRANT_DEPTH' "$HOME/src-$e/src/meat/task_array_state.rs"
done
sed -i '/change = leadership.changed() => { if change.is_err() { break } true },/d' "$HOME/src-e2/src/lettuce/runner.rs"
sed -i 's/let mut leadership = council.metrics();/let _leadership = council.metrics();/' "$HOME/src-e2/src/lettuce/runner.rs"
grep -c 'leadership.changed' "$HOME/src-e2/src/lettuce/runner.rs" || true
export RELIABURGER_GIT_SHA=f68755d9
for e in e1 e2; do
    cd "$HOME/src-$e"
    CARGO_TARGET_DIR="$HOME/t-rel" cargo build --locked --release --features ebpf --bin bun --bin relish 2>&1 | tail -n 2
    mkdir -p "$HOME/bin-$e"
    cp "$HOME/t-rel/release/bun" "$HOME/t-rel/release/relish" "$HOME/bin-$e/"
done
sha256sum "$HOME"/bin-*/bun
