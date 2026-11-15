# Dependency exception review, 6 November 2026

This narrows the 13 zstd 0.13 exceptions added on
[26 October](2026-10-26-dependency-exceptions.md). It doesn't remove any yet.
The other exceptions are unchanged, and so is the review deadline,
**18 November 2026**.

## What moved

| Crate | Before | After | Why |
| --- | --- | --- | --- |
| `zstd` (Reliaburger's own dependency and build dependency) | 0.13.3 | 0.14.3 | The source bundle (#1570) is packed by `build.rs` and read by `src/relish/source_bundle.rs`. It used 0.13 only to share Arrow's copy. |
| `include-flate`, `include-flate-codegen`, `include-flate-compress` (through `rust-embed`'s `compression` feature) | 0.3.4 | 0.3.5 | [include-flate-compress 0.3.5](https://crates.io/crates/include-flate-compress/0.3.5), published 27 October, requires `zstd` `^0.14.0`. A `Cargo.lock` update only. |

After this, `cargo tree -i zstd@0.13.3 -e normal,build --depth 1` names only
`arrow-ipc` 59.3.2, `parquet` 59.3.2, `datafusion` 55.2.0 and
`datafusion-datasource` 55.2.0.
`only_arrow_parquet_and_datafusion_still_lock_zstd_0_13` in
`tests/suite/dependency_audit.rs` fails if any other package depends on
`zstd` 0.13 or `zstd-safe` 7 again, or if Reliaburger stops depending on 0.14.

## Nothing durable changed

Both zstd lines link the same C library, `zstd-sys` 2.1.2 (libzstd 1.5.7),
which `Cargo.lock` holds once. The source bundle is compressed by the same
code at the same levels (3 for a dev build, 19 for a release build), and zstd
frames are a stable format either way. Log and metric Parquet still goes
through Parquet's own zstd 0.13 codec, which this change doesn't touch.
No `CURRENT` bump is needed.

## Why the rest is still excused

Parquet and `arrow-ipc` 59.3.2, the newest 59 release, require `zstd`
`^0.13.0`, which Cargo won't satisfy with 0.14. Arrow 60 moved to 0.14, but
[DataFusion 55.2.0](https://crates.io/crates/datafusion/55.2.0), still the
newest on crates.io, requires arrow `^59.2`. Reliaburger writes its logs as
zstd Parquet (`src/ketchup/log_store.rs`), so turning Parquet's zstd feature
off isn't an option.

DataFusion 56.0.0 moves to arrow and parquet 60.1.0 and `zstd` 0.14. Its
[`branch-56`](https://github.com/apache/datafusion/tree/branch-56) was cut, and
the version and changelog PR
([apache/datafusion#27098](https://github.com/apache/datafusion/pull/27098))
opened on 4 November. The release vote and crates.io publish are still open
items in the release tracker
([apache/datafusion#24461](https://github.com/apache/datafusion/issues/24461)).
We won't build from a git branch to retire an audit entry. Its
[upgrade guide](https://github.com/apache/datafusion/blob/branch-56/docs/source/library-user-guide/upgrading/56.0.0.md)
suggests a small move for us: MSRV 1.95 (ours is 1.97), object_store 0.14
(which we already use), arrow's new `Metadata` type for schema metadata, and
`TableProvider::scan_with_args` gaining a `skip` that defaults to `scan`.
Reliaburger has one `TableProvider` (`src/mayo/scan.rs`).

The exposure is as the 26 October record describes: only Parquet that
Reliaburger wrote itself is decompressed through zstd 0.13.

## Reproduce the review

```sh
make audit
cargo tree -i zstd@0.13.3 -e normal,build --depth 1
cargo nextest run --test suite dependency_audit
```
