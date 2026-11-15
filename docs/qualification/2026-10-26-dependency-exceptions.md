# Dependency exception review, 26 October 2026

This adds 13 exceptions to the
[25 October review](2026-10-25-dependency-exceptions.md), all against the zstd
0.13 line. The other exceptions are unchanged, and so is the review deadline,
**18 November 2026**, which now covers these as well.

On 26 October RustSec published 13 advisories against `zstd` 0.13.3 and
`zstd-safe` 7.3.0. The dependency audit then failed in 0.2.3's first candidate
build
([run 40344141576](https://github.com/reliaburger/reliaburger/actions/runs/40344141576),
`main` at `3e8322eb`). 0.2.3 was at the start of its gate, so the maintainer
decided to ship it with a time-boxed exception rather than wait for an
upstream release: the exposure is low, and a dependency move would mean a new
candidate after re-testing. The fix is
[#1366](https://github.com/reliaburger/reliaburger/issues/1366).

## Why there is no upgrade yet

`zstd` 0.14.3 and `zstd-safe` 8.2.0 fix all 13, and `Cargo.lock` already holds
them through `compression-codecs`. The old pair is still required by
`parquet` and `arrow-ipc` 59.3.1, `datafusion` and `datafusion-datasource`
55.2.0, and `include-flate-compress` 0.3.4. Arrow moved to zstd 0.14 in
[arrow-ipc 60.0.0](https://crates.io/crates/arrow-ipc/60.0.0), but
[DataFusion 55.2.0](https://crates.io/crates/datafusion/55.2.0), the newest
release, requires arrow `^59.2`. Arrow's 59.3.2 patch release, published the
same morning, still requires zstd `^0.13.0`.

## Where Reliaburger reaches zstd

Reliaburger compresses its own logs and metrics as zstd Parquet, and
decompresses only Parquet it wrote: the node's local log and metric files, and
the cluster's own object-store bucket when a query reads archived data
(`src/ketchup/log_store.rs`, `src/mayo/store.rs`). It never decompresses zstd
from a request, an image layer or a peer's message. Anyone able to plant a
crafted file there already has write access to the cluster's bucket or the
node's data directory.

## New exceptions

| Advisory | Kind | Reachability and disposition |
| --- | --- | --- |
| [RUSTSEC-2026-0391](https://rustsec.org/advisories/RUSTSEC-2026-0391), zstd 0.13.3: data from an abandoned stream leaks into the next stream on a reused context | Vulnerability | Parquet's codec reuses contexts while writing our own pages. The worst case mixes one of our own logs or metrics into another of our own files; no data crosses a trust boundary. Retain until #1366. |
| [RUSTSEC-2026-0398](https://rustsec.org/advisories/RUSTSEC-2026-0398), zstd-safe 7.3.0: `Seekable::decompress` sets a `Vec` length beyond its capacity | Vulnerability | Only in the seekable format, which neither Reliaburger nor Parquet uses. Retain until #1366. |
| [RUSTSEC-2026-0403](https://rustsec.org/advisories/RUSTSEC-2026-0403), zstd-safe 7.3.0: uninitialised memory exposed when streaming decompression of a legacy frame fails | Vulnerability | Needs a crafted legacy frame in one of our own Parquet files, which means write access to the node's data directory or the cluster's bucket. The bulk API is unaffected. Retain until #1366. |
| [RUSTSEC-2026-0395](https://rustsec.org/advisories/RUSTSEC-2026-0395), [0396](https://rustsec.org/advisories/RUSTSEC-2026-0396), [0397](https://rustsec.org/advisories/RUSTSEC-2026-0397), [0399](https://rustsec.org/advisories/RUSTSEC-2026-0399), [0400](https://rustsec.org/advisories/RUSTSEC-2026-0400), [0401](https://rustsec.org/advisories/RUSTSEC-2026-0401), [0402](https://rustsec.org/advisories/RUSTSEC-2026-0402), [0404](https://rustsec.org/advisories/RUSTSEC-2026-0404), [0405](https://rustsec.org/advisories/RUSTSEC-2026-0405), [0406](https://rustsec.org/advisories/RUSTSEC-2026-0406), zstd-safe 7.3.0 | Unsound (informational) | Unsound safe APIs: stable input buffers, the seekable format, dictionaries, dictionary training, multithreaded contexts and context cloning. These are reached only through the same dependency path, on the same files. Retain until #1366. |

0.2.2 and 0.2.1 are affected in the same way: both lock zstd 0.13.3, with
zstd-safe 7.3.0 and 7.2.4 respectively.

`make audit` passes with these exceptions against RustSec database commit
`a78282a1414d6c28cc228c4eae3570451affb0dc`, across 695 locked packages. The
lockfile's SHA-256 is
`e21a2d1742e79d5e86b75a990785f1a112f74aac4953dd051c0aa770aee34da0`.

## Reproduce the review

```sh
make audit
cargo tree -i zstd@0.13.3 -e normal --depth 1
```
