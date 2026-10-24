# Dependency exception review, 24 October 2026

This fixes one advisory by changing a dependency version and adds one
exception to the [18 September review](2026-09-18-dependency-exceptions.md).
The exceptions already there are unchanged, and so is their review deadline,
**18 November 2026**. That deadline now covers the new one as well.

On 24 October the dependency audit failed in 0.2.2 candidate 3's build
([run 40102017831](https://github.com/reliaburger/reliaburger/actions/runs/40102017831),
`main` at `c0a02041`). The finding was
[RUSTSEC-2026-0387](https://rustsec.org/advisories/RUSTSEC-2026-0387), which
RustSec published on 23 October.

## Fixed by moving age to 0.11.5

| Advisory | Locked before | Locked now |
| --- | --- | --- |
| [RUSTSEC-2026-0387](https://rustsec.org/advisories/RUSTSEC-2026-0387), crypto-bigint: some operations meant to be constant-time can branch on secret values ([crypto-bigint#1365](https://github.com/RustCrypto/crypto-bigint/pull/1365)) | 0.5.5, through elliptic-curve 0.13, p256 0.13 and hpke 0.12, from age 0.12.1 | None. age 0.11.5 has no P-256 code, so crypto-bigint, elliptic-curve, p256, primeorder and hpke all leave the build |

age 0.12 uses P-256 for its hardware-token ("tag") recipients, and those
dependencies aren't optional. The code was therefore compiled into `bun` and
`relish`, although Reliaburger never calls it: secrets and sealed root-CA
backups use only `age::x25519` keys and `age::scrypt` passphrases. Only
crypto-bigint 0.7.6 and later are patched, and no age release has moved to it,
so 0.12 couldn't be kept without an exception.
[age 0.11.5](https://crates.io/crates/age/0.11.5) came out on the same day as
0.12.1, has every API Reliaburger uses, and seals and opens the same files. The
tests that open backups and secrets sealed by an age 0.10 build still pass.

Moving to 0.11.5 also moves the macros to i18n-embed-fl 0.9.4. In all,
`Cargo.lock` goes from 712 packages to 695. A portable test
(`dependency_audit` in `tests/suite/`) fails if `Cargo.lock` locks a
crypto-bigint older than 0.7.6 again, or if the audit ignores
RUSTSEC-2026-0387.

0.2.1 and earlier aren't affected. They lock age 0.10.1, which has no P-256
code. age 0.12 arrived with the October dependency refresh
([#1026](https://github.com/reliaburger/reliaburger/issues/1026)) on the
0.2.2 line.

## New exception

| Advisory and pinned package | Reachability and disposition | Migration work |
| --- | --- | --- |
| [RUSTSEC-2026-0173](https://rustsec.org/advisories/RUSTSEC-2026-0173), proc-macro-error2 2.0.1 | Unmaintained informational advisory; no patched version. It's reached only through i18n-embed-fl 0.9.4, the procedural macro age 0.11.5 uses to embed its own messages at compile time. It runs only inside the compiler, and no shipped binary contains it. This is the same position as the existing `basic-toml` exception ([21 October](2026-10-21-dependency-exceptions.md)), which comes in through the same macros. Retain until the review deadline. | Drop it once the age line we use stops depending on proc-macro-error2, or once an age release with the patched crypto-bigint lets us return to the newest age. |

`make audit` passes with this exception against RustSec database commit
`21d909e4c8bd4e14c7aea58bf980c881a0053ab1` (1,336 advisories), across 695
locked packages. The lockfile's SHA-256 is
`d5a62bfcf3e681f8ad7f650194c6ab19ad8471d8dca09d3c127a88f595f4ba82`.

## Reproduce the review

```sh
make audit
cargo tree --locked --all-features --target all -i proc-macro-error2
cargo tree --locked --all-features --target all -i crypto-bigint   # nothing to print
cargo nextest run --test suite dependency_audit
```
