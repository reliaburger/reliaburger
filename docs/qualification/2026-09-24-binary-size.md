# Release binary size, 24 September 2026

Why the release profile is `strip = true` plus `codegen-units = 1`, and what
it saves a cold quickstart.

## Where the bytes were

The staged 0.1.0 candidate (`staging-v0.1.0-35972219250-3`) shipped
`relish-macos-aarch64` at 177.8 MB and `relish-linux-aarch64` at 201.8 MB.
The guess was debug info. It wasn't: since Rust 1.77, a release build with
`debug = 0` already defaults to `strip = "debuginfo"`, and
`strip --strip-debug` on the Linux binary removed 0.1 MB. The weight was:

| Part (relish, from the release assets) | macOS aarch64 | Linux aarch64 |
| --- | ---: | ---: |
| Machine code (`__text` / `.text`) | 96.9 MB | 99.0 MB |
| Unwind tables and exception data | 21.9 MB | 22.7 MB |
| Read-only data (includes the embedded `src/` for `relish source`) | 12.9 MB | 13.4 MB |
| Symbol table (`__LINKEDIT` / `.symtab` + `.strtab`) | 46.0 MB | 60.6 MB |
| Relocations and other data | 0.1 MB | 6.1 MB |
| **File** | **177.8 MB** | **201.8 MB** |

So `strip = "debuginfo"` would have changed nothing. The symbol table was a
quarter of the file, and the machine code over half.

## Options measured

Local builds of `cargo build --locked --release --bin relish --bin bun` on an
Apple M-series Mac (12 cores), each in its own empty target directory, then
rebuilt after `touch src/lib.rs` ("warm", which is what CI does with its
dependency cache). Sizes are exact and reproducible. Build times are
indicative only: other agents' builds and tests shared the machine, with load
averages from 4 to over 200, and one stress loop reniced some `bun` builds.
The load at each run is recorded next to it.

| Profile | relish | bun | gzip relish | `__text` | Cold | Warm | Load |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Default (`strip = "debuginfo"`) | 177.7 MB | 198.5 MB | 54.1 MB | 96.9 MB | 18m36s | 5m48s | 25→183 |
| `strip = true` | 134.7 MB | 149.1 MB | 48.4 MB | 96.9 MB | 16m09s | 2m11s | 8→28 |
| `strip = true`, `lto = "thin"` | 138.6 MB | 154.3 MB | 49.7 MB | 101.5 MB | 9m20s | 6m42s | 157→76 |
| **`strip = true`, `codegen-units = 1`** | **90.5 MB** | **97.9 MB** | **35.2 MB** | **65.5 MB** | **9m34s** | **6m53s** | **17→4** |
| `strip = true`, `lto = "fat"`, `codegen-units = 1` | 87.2 MB | 94.7 MB | 35.6 MB | 66.5 MB | 24m12s | 22m50s | 51→6 |

What the rows say:

- **Stripping symbols** removes 43 MB (24%) from relish and 49 MB from bun,
  for no build time at all.
- **Thin LTO made the code bigger.** It inlines more across crates without
  deduplicating, so `__text` grew by 4.6 MB.
- **One codegen unit per crate** cut the machine code by a third (96.9 →
  65.5 MB). With sixteen units, LLVM optimises each piece separately and keeps
  a private copy of every generic instantiation and inlined helper each piece
  uses; with one it sees the whole crate. The cost is parallelism inside a
  crate: the warm rebuild, which is mostly the big `reliaburger` crate and the
  two binaries, took about three times longer than strip-only at similar load.
- **Fat LTO** saved only another 3 MB, and its single-threaded whole-program
  link made even a warm rebuild take 23 minutes.

## Backtraces

A throwaway crate that indexes out of bounds, built both ways:

- `strip = "debuginfo"`: the panic prints `panicked at src/main.rs:2:5` and
  the message; `RUST_BACKTRACE=1` lists `panicky::outer`, `panicky::main`.
- `strip = true`: the same panic line and message; the backtrace has no
  frames.

Release builds never had file and line numbers in backtraces (that needs
debug info), so the panic location, which Rust compiles into the panic call
itself, is still the most useful line in a bug report. Keeping function names
would cost about 40 MB per binary per download. Nothing at runtime reads
symbols or debug info: `relish source` embeds `src/` as data with
`rust-embed`, and the eBPF objects are `include_bytes`. To get function names in a
backtrace, build from source with `CARGO_PROFILE_RELEASE_STRIP=none`
or use a debug build.

## Linux

Built once with the chosen profile in the `reliaburger-test` Lima VM (Ubuntu
24.04 arm64, 4 vCPUs, `CARGO_BUILD_JOBS=2`, Rust 1.97.0), exactly as CI builds
it: `cargo build --locked --release --features ebpf --bin bun --bin relish`.
Cold build: 16m39s.

| aarch64 Linux | Candidate asset | `strip = true`, `codegen-units = 1` | gzip |
| --- | ---: | ---: | ---: |
| relish | 201.8 MB | 94.6 MB (-53%) | 36.9 MB |
| bun | 230.4 MB | 104.9 MB (-54%) | 39.7 MB |

`.text` fell from 99.0 MB to 66.5 MB, the same third as on macOS.

## What a cold quickstart saves

A cold `relish setup --quickstart` on Apple silicon downloads the macOS CLI
(through the installer), the Linux aarch64 relish and bun for the VMs, the
guest image and Lima:

| Download | Before | After |
| --- | ---: | ---: |
| `relish-macos-aarch64` | 177.8 MB | 90.5 MB |
| `relish-linux-aarch64` | 201.8 MB | 94.6 MB |
| `bun-linux-aarch64` | 230.4 MB | 104.9 MB |
| Guest image | 633.6 MB | 633.6 MB |
| Lima 2.1.0 | 37.2 MB | 37.2 MB |
| **Total** | **1,280.8 MB** | **960.8 MB** |

That's 320 MB less (the binaries shrink by 52%, the whole cold download by a
quarter): 51 seconds on a steady 50 Mbit/s link, and about three and a half
minutes at the 1.5 MiB/s the failing qualification runs actually got. x86_64
hosts should save about the same; their binaries were 5 to 7% larger.

## CI build time

The candidate workflow (`build.yml`, run 35972219250) spent 4m45s (Linux
x86_64), 5m24s (Linux arm64), 6m10s (macOS arm64) and 10m46s (macOS Intel)
in its "Build locked release binaries" step, with a warm dependency cache.
Expect those to grow two to three times with one codegen unit, and the first
build after this change to be cold, because the cache key includes
`Cargo.toml`. The binary builds aren't the workflow's critical path: in that
run they finished by 08:08 while `validate-release` ran until 09:10. Benchmarks
use the `bench` profile, which inherits `release`, so `make bench` builds
slow down by the same few minutes.
