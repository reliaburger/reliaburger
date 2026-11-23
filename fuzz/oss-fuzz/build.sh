#!/bin/bash -eu
# Draft of projects/reliaburger/build.sh for the OSS-Fuzz repository (#1414).
#
# Builds every cargo-fuzz target in fuzz/ and copies each binary, with its
# checked-in seeds as <target>_seed_corpus.zip, to $OUT. `cargo fuzz list`
# names the targets, so a new one needs no change here.

cd "$SRC/reliaburger"

# cargo-fuzz's default of one codegen unit compiles reliaburger as a single
# sanitized LLVM module, which took 31 GB in one rustc (#1839). The Makefile
# and fuzz.yml build with 64 for the same reason.
cargo fuzz build -O --codegen-units 64

release="fuzz/target/x86_64-unknown-linux-gnu/release"
for target in $(cargo fuzz list); do
    cp "$release/$target" "$OUT/"
    if [ -d "fuzz/corpus/$target" ]; then
        (cd "fuzz/corpus/$target" && zip -q -r "$OUT/${target}_seed_corpus.zip" .)
    fi
done
