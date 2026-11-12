# Dependency exception review, 12 November 2026

This is the review the 18 November deadline asked for. It removes 14 of the 16
exceptions in `.cargo/audit.toml` and keeps two. The next review deadline is
**10 December 2026**, which `make audit` now enforces.

Two things moved upstream since the
[6 November review](2026-11-06-dependency-exceptions.md). The zstd
maintainer published patched releases of the 0.13 line on 1 November, and
RustSec amended the webpki-roots advisory on 21 October so that it no longer
flags the 0.26.11 shim. Neither needed a code change from us: one is a
`Cargo.lock` update, the other needs nothing at all.

## Removed: the 13 zstd advisories

| Advisory | Kind | Locked before | Locked now |
| --- | --- | --- | --- |
| [RUSTSEC-2026-0391](https://rustsec.org/advisories/RUSTSEC-2026-0391), zstd: data from an abandoned stream leaks into the next stream on a reused context | Vulnerability | zstd 0.13.3 | zstd 0.13.4 |
| [RUSTSEC-2026-0395](https://rustsec.org/advisories/RUSTSEC-2026-0395), zstd-safe: use-after-free with `CParameter::StableInBuffer` | Unsound | zstd-safe 7.3.0 | zstd-safe 7.4.0 |
| [RUSTSEC-2026-0396](https://rustsec.org/advisories/RUSTSEC-2026-0396), zstd-safe: `Seekable` reads a NULL or out-of-bounds seek table | Unsound | zstd-safe 7.3.0 | zstd-safe 7.4.0 |
| [RUSTSEC-2026-0397](https://rustsec.org/advisories/RUSTSEC-2026-0397), zstd-safe: `OutBuffer::around_pos` exposes uninitialised memory | Unsound | zstd-safe 7.3.0 | zstd-safe 7.4.0 |
| [RUSTSEC-2026-0398](https://rustsec.org/advisories/RUSTSEC-2026-0398), zstd-safe: `Seekable::decompress` sets a `Vec` length beyond its capacity for offsets past the end | Vulnerability | zstd-safe 7.3.0 | zstd-safe 7.4.0 |
| [RUSTSEC-2026-0399](https://rustsec.org/advisories/RUSTSEC-2026-0399), zstd-safe: use-after-free through `init_using_cdict` and `init_using_ddict` | Unsound | zstd-safe 7.3.0 | zstd-safe 7.4.0 |
| [RUSTSEC-2026-0400](https://rustsec.org/advisories/RUSTSEC-2026-0400), zstd-safe: use-after-free of a dictionary after a one-shot call on a context with an open stream | Unsound | zstd-safe 7.3.0 | zstd-safe 7.4.0 |
| [RUSTSEC-2026-0401](https://rustsec.org/advisories/RUSTSEC-2026-0401), zstd-safe: out-of-bounds read in `train_from_buffer` when sample sizes overflow | Unsound | zstd-safe 7.3.0 | zstd-safe 7.4.0 |
| [RUSTSEC-2026-0402](https://rustsec.org/advisories/RUSTSEC-2026-0402), zstd-safe: heap buffer overflow in `CCtx::try_clone` on a multithreaded context | Unsound | zstd-safe 7.3.0 | zstd-safe 7.4.0 |
| [RUSTSEC-2026-0403](https://rustsec.org/advisories/RUSTSEC-2026-0403), zstd-safe: uninitialised memory exposed when streaming decompression of a legacy frame fails | Vulnerability | zstd-safe 7.3.0 | zstd-safe 7.4.0 |
| [RUSTSEC-2026-0404](https://rustsec.org/advisories/RUSTSEC-2026-0404), zstd-safe: double free in `DCtx::try_clone` | Unsound | zstd-safe 7.3.0 | zstd-safe 7.4.0 |
| [RUSTSEC-2026-0405](https://rustsec.org/advisories/RUSTSEC-2026-0405), zstd-safe: out-of-bounds read with an out-of-range `InBuffer::pos` | Unsound | zstd-safe 7.3.0 | zstd-safe 7.4.0 |
| [RUSTSEC-2026-0406](https://rustsec.org/advisories/RUSTSEC-2026-0406), zstd-safe: use-after-free when changing a multithreaded context with an unfinished frame | Unsound | zstd-safe 7.3.0 | zstd-safe 7.4.0 |

The 26 October exception assumed only `zstd` 0.14.3 and `zstd-safe` 8.2.0
fixed these, so the way out was a DataFusion release on arrow 60. That's no
longer the only way out. [zstd 0.13.4](https://crates.io/crates/zstd/0.13.4)
and [zstd-safe 7.4.0](https://crates.io/crates/zstd-safe/7.4.0) came out on
1 November 2026, and every advisory now lists `^0.13.4` or `^7.4.0` as
patched. Parquet, `arrow-ipc`, `datafusion` and `datafusion-datasource` ask
for `zstd` `^0.13.0`, which 0.13.4 satisfies, so this is one command:

```sh
cargo update -p zstd@0.13.3 --precise 0.13.4
```

It moved those two packages, and one thing it didn't report. Re-resolving
the graph also pointed `tempfile` 3.27.0's `getrandom` edge from 0.4.3 to
0.3.4. Both are in the lockfile, and `tempfile` accepts `>=0.3.0, <0.5`, so
Cargo is free to pick either. We put main's 0.4.3 back by hand, and
`cargo metadata --locked` accepts the result, so the only versions that
change are zstd and zstd-safe. Both still link `zstd-sys`
2.1.2 (libzstd 1.5.7), which `Cargo.lock` holds once, so the compressor
underneath is the same code, and zstd frames are a stable format. Log and
metric Parquet written before the change reads back the same. No `CURRENT`
bump is needed.

The 6 November review came five days after these releases and missed them.
It checked what DataFusion was doing, not whether the 0.13 line had a fix of
its own.

A new test, `lock_file_has_only_patched_zstd_and_audit_needs_no_exception_for_it`
in `tests/suite/dependency_audit.rs`, fails if `Cargo.lock` locks a `zstd`
0.13 older than 0.13.4, a 0.14 older than 0.14.3, a `zstd-safe` 7 older than
7.4.0 or an 8 older than 8.2.0, or if `.cargo/audit.toml` ignores any of the
13 IDs again. The existing
`only_arrow_parquet_and_datafusion_still_lock_zstd_0_13` stays: Reliaburger's
own code keeps using 0.14.

Moving DataFusion to arrow 60 is still worth doing, so `Cargo.lock` holds one
zstd line instead of two, and it stays with
[#1366](https://github.com/reliaburger/reliaburger/issues/1366). This review
doesn't touch DataFusion or arrow. [DataFusion 55.2.0](https://crates.io/crates/datafusion/55.2.0)
is still the newest on crates.io.

## Removed: RUSTSEC-2026-0381, webpki-roots 0.26.11

[RUSTSEC-2026-0381](https://rustsec.org/advisories/RUSTSEC-2026-0381) flags
webpki-roots releases that bundle `emSign ECC Root CA - G3`. When we excused it
on [21 October](2026-10-21-dependency-exceptions.md), its patched range was
`>=1.0.10`, so it flagged the 0.26.11 compatibility shim too, although the shim
only re-exports the roots of the webpki-roots 1.x it resolves to. The same day,
RustSec amended the advisory
([advisory-db commit 8c254009](https://github.com/rustsec/advisory-db/commit/8c254009),
"Clarify webpki-roots 0.26.11 compatibility wrapper status"). It now lists
`= 0.26.11` as unaffected, and says the 1.x parent must be 1.0.10 or newer.

That's exactly the reasoning our exception used. `cargo audit` with
`.cargo/audit.toml` moved aside no longer reports 0381, so the exception has
been excusing nothing. `Cargo.lock` still locks webpki-roots 0.26.11 (for
[tokio-tungstenite](https://crates.io/crates/tokio-tungstenite/0.30.0), whose
newest release, 0.30.0, still asks for `^0.26`) on top of the patched 1.0.10.

`lock_file_has_only_webpki_releases_that_distrust_emsign` already fails if any
locked webpki-roots 1.x, webpki-root-certs or rustls-webpki is older than its
fix. It now also fails if `.cargo/audit.toml` ignores RUSTSEC-2026-0381, -0382
or -0383.

## Kept: the two unmaintained compile-time crates

| Advisory and pinned package | What we checked | Verdict |
| --- | --- | --- |
| [RUSTSEC-2026-0374](https://rustsec.org/advisories/RUSTSEC-2026-0374), basic-toml 0.1.10 | Unmaintained informational advisory with no patched release. [basic-toml 0.1.10](https://crates.io/crates/basic-toml/0.1.10) (March 2025) is still the newest. `cargo tree -i basic-toml -e normal,build` shows it only under [i18n-config 0.4.8](https://crates.io/crates/i18n-config/0.4.8), the newest release, which still requires `basic-toml ^0.1`. i18n-config is a build-time dependency of the `i18n-embed-fl` and `i18n-embed-impl` procedural macros that [age 0.11.5](https://crates.io/crates/age/0.11.5) uses for its own messages. It runs inside the compiler, and no shipped binary contains it. No age release has come out since 14 July 2026, and moving to [age 0.12.1](https://crates.io/crates/age/0.12.1) wouldn't help: it uses the same i18n-config. | Kept until 10 December 2026 ([#1271](https://github.com/reliaburger/reliaburger/issues/1271)). |
| [RUSTSEC-2026-0173](https://rustsec.org/advisories/RUSTSEC-2026-0173), proc-macro-error2 2.0.1 | Unmaintained informational advisory with no patched release. [proc-macro-error2 2.0.1](https://crates.io/crates/proc-macro-error2/2.0.1) (September 2024) is still the newest. `cargo tree -i proc-macro-error2 -e normal,build` shows it only under [i18n-embed-fl 0.9.4](https://crates.io/crates/i18n-embed-fl/0.9.4), a procedural macro, from age 0.11.5. [i18n-embed-fl 0.10.1](https://crates.io/crates/i18n-embed-fl/0.10.1) moved to `proc-macro-error3`, but age 0.11.5 requires `i18n-embed-fl ^0.9`. Only age 0.12 takes 0.10, and age 0.12.1 still requires `p256 ^0.13`, which brings back crypto-bigint 0.5 and [RUSTSEC-2026-0387](https://rustsec.org/advisories/RUSTSEC-2026-0387) (patched only in crypto-bigint `>=0.7.6`). The [25 October review](2026-10-25-dependency-exceptions.md) chose an unmaintained build-time macro over shipping that arithmetic, and nothing has changed that trade. | Kept until 10 December 2026. |

Both are in the position the [20 October](2026-10-20-dependency-exceptions.md)
and [25 October](2026-10-25-dependency-exceptions.md) records describe. They
go when an age release drops the old i18n macros without bringing crypto-bigint
0.5 back.

## The new deadline

The next review is due by **10 December 2026**, four weeks from today. The
`Next review:` line in `.cargo/audit.toml` and the `expiry=` date in the
`audit` target of the `Makefile` both say so, and `make audit` fails after that
date until someone reviews the list again.

`make audit` passes with the two remaining exceptions against RustSec database
commit `fd6f39814c4d3c359fdbe223d0280dec4933f78f` (1,458 advisories), across
741 locked packages. The lockfile's SHA-256 is
`ee099237fe7029bc8b0d8fc16687614f3b806271045764977f19d86a443d3f2f`.

## Reproduce the review

```sh
make audit
mv .cargo/audit.toml /tmp/audit.toml && cargo audit; mv /tmp/audit.toml .cargo/audit.toml
cargo tree -i zstd@0.13.4 -e normal,build --depth 1
cargo tree -i basic-toml -e normal,build
cargo tree -i proc-macro-error2 -e normal,build
cargo nextest run --test suite dependency_audit
```

With the exceptions moved aside, `cargo audit` should report exactly the two
warnings kept above and no vulnerabilities.
