#!/bin/sh
# RUSTC_WRAPPER for the fuzz build: coverage only where the decoders run (#1953).
#
# libFuzzer scans every coverage counter after every input. cargo-fuzz puts
# its sanitizer-coverage flags in RUSTFLAGS, which reach every crate, and the
# whole dependency tree came to about 15.3 million counters: most of each
# execution went on counters for tokio, axum, datafusion and the rest, which
# no target reaches. This wrapper drops the coverage pass from every crate
# not named below, so libFuzzer only watches reliaburger and the parsers the
# decoders hand bytes to. AddressSanitizer is a separate flag and stays on
# everywhere. Generic code (a serde derive, serde_json's reader) is compiled
# into the crate that uses it, so most of what a decoder runs is
# reliaburger's own anyway.
#
# Cargo calls it as `rustc-coverage.sh <rustc> <args>...`. The Makefile sets
# RUSTC_WRAPPER for both `cargo fuzz build` and `cargo fuzz run`: a run
# without it would see another rustc and rebuild everything instrumented.
# Cargo doesn't fingerprint this file, so after changing the list, delete
# fuzz/target (or your CARGO_TARGET_DIR) to rebuild the dependencies.
# scripts/ci/test_fuzz_workflow.py checks every name against fuzz/Cargo.lock.

# Crate names as rustc sees them: hyphens become underscores.
COVERAGE_CRATES="
reliaburger
serde serde_core serde_json serde_spanned postcard
toml toml_edit toml_parser toml_datetime winnow
hickory_proto dhcproto
http
x509_parser der_parser asn1_rs rcgen pem
oci_client oci_spec
base64 semver humantime ipnet url idna chrono time uuid
cel regex regex_syntax regex_automata
"

rustc=$1
shift

crate=
previous=
for arg in "$@"; do
    if [ "$previous" = "--crate-name" ]; then
        crate=$arg
    fi
    previous=$arg
done

# `rustc -vV`, `--print` and the like name no crate; the kept crates keep
# every flag.
case " $(echo $COVERAGE_CRATES) " in
    *" $crate "*) exec "$rustc" "$@" ;;
esac
if [ -z "$crate" ]; then
    exec "$rustc" "$@"
fi

# Without the sancov pass, the -Cllvm-args=-sanitizer-coverage-* options
# that come with it have nothing to configure, so they can stay.
count=$#
while [ "$count" -gt 0 ]; do
    arg=$1
    shift
    count=$((count - 1))
    if [ "$arg" = "-Cpasses=sancov-module" ]; then
        continue
    fi
    set -- "$@" "$arg"
done
exec "$rustc" "$@"
