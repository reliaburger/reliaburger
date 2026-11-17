# Dependency exception review, 17 November 2026

On 17 November RustSec published three "unmaintained" advisories against the
date and time crates of the [chronotope](https://github.com/chronotope)
project, after its maintainer wound the project down
([chronotope/chrono#1768](https://github.com/chronotope/chrono/issues/1768)).
Our `make audit` denies warnings, so from about 18:00 GMT the dependency
advisory audit failed on every pull request, and with it the required
"ignored-test evidence" check on the 0.3.0 train and on `main`.

This review adds three exceptions. It removes none. The two kept on
[12 November](2026-11-12-dependency-exceptions.md) (basic-toml and
proc-macro-error2) are unchanged, and so is the review deadline,
**10 December 2026**, which `make audit` already enforces.

## Added: chrono, chrono-tz and humantime

| Advisory and locked package | Where it comes from | Verdict |
| --- | --- | --- |
| [RUSTSEC-2026-0543](https://rustsec.org/advisories/RUSTSEC-2026-0543), [chrono 0.4.45](https://crates.io/crates/chrono/0.4.45) | Unmaintained informational advisory with no patched release; 0.4.45 (June 2026) is the newest. Not our dependency. `cargo tree -i chrono -e normal,build --depth 1` shows arrow 59.3.2 (`arrow-arith`, `-array`, `-cast`, `-csv`, `-json`), parquet 59.3.2, DataFusion 55.2.0 (eight crates), chrono-tz, object_store 0.13.2 and 0.14.2, oci-client 0.18.0, openraft 0.9.25 and openraft-macros, prometheus-parse 0.2.5, rmcp 3.9.0 and schemars 1.2.3. | Kept until 10 December 2026 ([#2036](https://github.com/reliaburger/reliaburger/issues/2036)). |
| [RUSTSEC-2026-0555](https://rustsec.org/advisories/RUSTSEC-2026-0555), [chrono-tz 0.10.4](https://crates.io/crates/chrono-tz/0.10.4) | Unmaintained, no patched release; 0.10.4 is the newest. Only arrow-array 59.3.2 and datafusion-functions 55.2.0 use it. It's an optional feature in both, but all 26 DataFusion 55.2.0 crates ask for arrow with `features = ["prettyprint", "chrono-tz"]`, so turning DataFusion's default features off wouldn't remove it. | Kept until 10 December 2026 (#2036). |
| [RUSTSEC-2026-0542](https://rustsec.org/advisories/RUSTSEC-2026-0542), [humantime 2.4.0](https://crates.io/crates/humantime/2.4.0) | Unmaintained, no patched release; 2.4.0 (July 2026) is the newest. Only object_store 0.13.2 and 0.14.2 use it, and [object_store 0.14.2](https://crates.io/crates/object_store/0.14.2), the newest, requires `humantime ^2.1` unconditionally. The advisory says humantime wraps chrono, but 2.4.0 has no normal dependencies (chrono is a dev-dependency), so nothing of chrono's comes in through it. It's flagged because it lives in the same [chronotope](https://github.com/chronotope/humantime) organisation. | Kept until 10 December 2026 (#2036). |

Reliaburger's own code never names any of the three. It keeps time with
`std::time`, and the policy expression crate, `cel`, is already built without
its chrono feature (#1117).

### No upgrade removes them

We checked the newest releases on crates.io on 17 November:

- [DataFusion 55.3.0](https://crates.io/crates/datafusion/55.3.0), published
  that day, still requires `chrono ^0.4.45` and arrow `^59.2.0`.
- Arrow 60.1.0, which the DataFusion 56 move in
  [#1366](https://github.com/reliaburger/reliaburger/issues/1366) brings in,
  still requires chrono (`arrow-array`, `arrow-cast`, `parquet`), and
  [arrow-array 60.1.0](https://crates.io/crates/arrow-array/60.1.0) keeps the
  optional chrono-tz. #1366 is still worth doing, for its own reasons, but it
  won't retire these exceptions.
- [object_store 0.14.2](https://crates.io/crates/object_store/0.14.2) is the
  newest and requires both chrono and humantime.
- [openraft 0.10.0](https://crates.io/crates/openraft/0.10.0),
  [oci-client 0.18.0](https://crates.io/crates/oci-client/0.18.0),
  [rmcp 3.11.0](https://crates.io/crates/rmcp/3.11.0) and
  [prometheus-parse 0.2.5](https://crates.io/crates/prometheus-parse/0.2.5)
  all require chrono. schemars 1.2.3 has it as an optional feature, but
  chrono stays locked through the others either way.

So there's no version to move to, and replacing chrono would mean replacing
or forking a dozen other projects' crates. That isn't a change to make the
night before a release candidate. The crates still build and work: an
unmaintained advisory says no one will fix the next bug, not that a bug is
known.

The exposure is small for a time library. chrono and chrono-tz parse and
format the timestamps in Parquet files and SQL expressions that Reliaburger
writes and queries itself. humantime parses duration strings in
object_store's configuration, which comes from the operator's node config.

### The new test

`only_dependencies_lock_the_unmaintained_time_crates_and_audit_excuses_only_those_locked`
in `tests/suite/dependency_audit.rs` fails if the `reliaburger` package in
`Cargo.lock` depends on humantime, chrono or chrono-tz directly, so new code
can't start using them under cover of the exception. It also fails if
`.cargo/audit.toml` still ignores one of the three advisories after its crate
has left `Cargo.lock`, so each exception goes as soon as it excuses nothing.

## What happens next

[#2036](https://github.com/reliaburger/reliaburger/issues/2036) (milestone
0.3.1) tracks the removal. Each review checks whether the crates above have
moved off chrono and humantime (the chrono maintainer recommends
[jiff](https://crates.io/crates/jiff)) and takes those releases.
prometheus-parse 0.2.5 hasn't had a release since December 2023, so #2036
also asks whether we can parse the Prometheus exposition format without it.

The RustSec database commit for this review,
[`233155b4`](https://github.com/rustsec/advisory-db/commit/233155b4025b854c68a4b199c0e32d7fd53291a2),
also published advisories against hickory-proto, hickory-net, zip, libz-sys,
actix-http and chrono-tz-build. Only hickory-proto is in our lockfile, at
0.26.3, and the hickory-proto advisories list `>= 0.26.0-beta.1` as
unaffected. `cargo audit` reports none of them.

`make audit` passes with the five exceptions against RustSec database commit
`233155b4025b854c68a4b199c0e32d7fd53291a2` (1,527 advisories). On the
`release-1-3-0` train that's 750 locked packages, with lockfile SHA-256
`19087bbdd5a1a25a58ed1c87710b0cca6216bef0b17f8908e0f13a649907e069`; on `main`
it's 741, with `a9d37f57db24647db095e7e97b9e4628aeaf6b17d3f6212e6c0b573be728efab`.
This review changes neither lockfile.

## Reproduce the review

```sh
make audit
mv .cargo/audit.toml /tmp/audit.toml && cargo audit; mv /tmp/audit.toml .cargo/audit.toml
cargo tree -i chrono -e normal,build --depth 1
cargo tree -i chrono-tz -e features
cargo tree -i humantime -e normal,build --depth 1
cargo nextest run --test suite dependency_audit
```

With the exceptions moved aside, `cargo audit` should report exactly five
warnings: the three above and the two kept on 12 November.
