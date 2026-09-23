# Dependency exception review, 18 September 2026

Release gate: V05. Reviewed after `cc7f44e` on PR #167.

`make audit` passes against RustSec database commit
`2b34578f89884736e0fcbd42f7ba8d6b10b4a0ce` (1,251 advisories), with four explicit
exceptions and no unignored findings across 707 locked packages. Lockfile SHA-256:
`1fa2f163d9d3a3caa124792720f91cf146e9cd9d23a8bb6c8978c479a2cc7485`. This does not prove every dependency is free of defects.
CI continues to fetch current advisories on each run.

The next review deadline remains **18 November 2026**. The Makefile fails after
that date. Dependency and feature changes must preserve the assumptions below;
the rkyv assumption is now checked automatically.

| Advisory and pinned package | Reachability and disposition | Migration work |
| --- | --- | --- |
| [RUSTSEC-2024-0370](https://rustsec.org/advisories/RUSTSEC-2024-0370.html), proc-macro-error 1.0.4 | Unmaintained informational advisory. Build-time macro through age 0.10.1 and i18n-embed-fl 0.7.0. Retain for this pinned build chain until the review deadline. | [Age 0.12.1](https://docs.rs/crate/age/latest) uses i18n-embed-fl 0.10, whose [dependencies](https://docs.rs/crate/i18n-embed-fl/0.10.0) include proc-macro-error2. Qualify that crypto API upgrade with persisted secret and envelope interoperability tests. |
| [RUSTSEC-2025-0141](https://rustsec.org/advisories/RUSTSEC-2025-0141.html), bincode 1.3.3 | Unmaintained informational advisory; no patched version. Runtime-reachable in reporting and Council disk metadata/encrypted envelopes. Retain explicitly until the review deadline. | Evaluate postcard, bitcode or wincode and version the wire/disk formats. Reporting checks format headers, bounds decode to the received body and rejects trailing bytes; these controls do not replace a deliberate codec migration. |
| [RUSTSEC-2026-0235](https://rustsec.org/advisories/RUSTSEC-2026-0235.html), rkyv 0.7.46 | Out-of-bounds archive validation vulnerability, fixed in 0.8.17. The locked 0.7 package is on an inactive optional path. Cargo reports no active graph across all root features and targets. Retain only while that remains true, and no later than the review deadline. | Upgrade or remove the parent before enabling its archive feature. `make audit` refuses an active graph or failed graph inspection before applying this exception. |

[RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436.html)
(paste) is **removed**, not renewed. The DataFusion 45 → 55 upgrade drops paste
from the graph, together with the Thrift crate behind GHSA-2f9f-gq7v-9h6m.

[RUSTSEC-2025-0134](https://rustsec.org/advisories/RUSTSEC-2025-0134.html) is
**removed**, not renewed. `cc7f44e` replaces rustls-pemfile with Rustls
`PemObject` and removes the package from Cargo.lock. The audit failed before
migration when that exception was omitted. Afterwards the audit, 12 TLS unit
tests, 17 client tests, seven ingress tests, three operator certificate-reload
tests and strict Linux/macOS Clippy pass.

## Reproduce the review

```sh
make audit
cargo tree --locked --all-features -i proc-macro-error --depth 4
cargo tree --locked --all-features -i paste --depth 2
cargo tree --locked --all-features -i bincode --depth 3
cargo tree --locked --all-features --target all -i rkyv
cargo test --test suite dependency_audit::
```

The audit-gate regression fails before the Makefile change: an active rkyv
fixture still reaches a successful audit. After the change, inactive passes,
active refuses and failed inspection refuses. The fixture fixes the date
before expiry so this test measures the dependency condition rather than the
wall clock. The existing expiry check remains in place.
