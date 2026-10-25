# Dependency exception review, 24 October 2026

This adds one exception to `.cargo/audit.toml`. It falls under the review deadline `make audit` already enforces, **18 November 2026**.

On 24 October the dependency audit failed on every branch on an advisory newly added to the RustSec database, with no dependency change. The first failure was [#1317](https://github.com/reliaburger/reliaburger/pull/1317) ([run 40061977863](https://github.com/reliaburger/reliaburger/actions/runs/40061977863)).

| Advisory and pinned package | Reachability and disposition | Migration work |
| --- | --- | --- |
| [RUSTSEC-2026-0387](https://rustsec.org/advisories/RUSTSEC-2026-0387), crypto-bigint 0.5.5 | See below. Retain until the review deadline. | Drop the exception when `age` moves to `hpke` or `p256` releases built on crypto-bigint 0.7.6 or later. At the review, check whether `age` can be built without its HPKE recipients. |

**The advisory.** Several operations meant to be constant time have timing variability. The advisory is dated 23 October 2026, and the fix is crypto-bigint 0.7.6 or later.

**How crypto-bigint gets in.** It's reached only through this chain:

`age` 0.12.1 → `hpke` 0.12.0 / `age-core` 0.12.0 → `p256` 0.13.2 → `elliptic-curve` 0.13.8 → `crypto-bigint` 0.5.5

`elliptic-curve` 0.13 requires crypto-bigint 0.5, and `age` 0.12.1 is the newest release, so no upgrade removes it.

**Why Reliaburger never runs the affected code.**

- `age` uses HPKE and P-256 for recipient types Reliaburger doesn't use.
- Reliaburger encrypts and decrypts with `age::x25519` identities (`src/sesame/secret.rs`) and `age::scrypt` passphrases (`src/sesame/root_backup.rs`). Neither touches P-256 or crypto-bigint.
- Image signatures are verified with `ring`'s `ECDSA_P256_SHA256_ASN1` (`src/pickle/cosign.rs`), not the `p256` crate.

So the affected code is compiled in, but no Reliaburger path calls it with secret data.

**The tripwire.** `tests/suite/dependency_audit.rs` (`crypto_bigint_is_reached_only_through_age`) fails if `Cargo.toml` ever depends directly on `crypto-bigint`, `elliptic-curve`, `p256`, `hpke` or `ecdsa`. That keeps the reachability claim true.

**The audit.** It scanned 712 locked packages. Lockfile SHA-256 on `main`: `6b16ce22571f545ba61a9bfb4debd65980e9e14cddcf4be0de000a2f31f2daf3`.

## Reproduce the review

```sh
make audit
cargo tree --locked -i crypto-bigint -e all
cargo info age
grep -rnE 'age::(x25519|scrypt|plugin|ssh)' src
grep -n 'ECDSA_P256' src/pickle/cosign.rs
```
