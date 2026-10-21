# Dependency exception review, 21 October 2026

This adds two exceptions to the [18 September review](2026-09-18-dependency-exceptions.md)
and fixes three advisories by upgrading. The exceptions already there are
unchanged, and so is their review deadline of **18 November 2026**, which now
covers these two as well.

On 21 October the dependency audit on the 0.2.1 records PR
([#1268](https://github.com/reliaburger/reliaburger/pull/1268),
[run 39536327843](https://github.com/reliaburger/reliaburger/actions/runs/39536327843))
failed on four findings that RustSec published on 19 and 20 October.

## Fixed by upgrading

| Advisory | Locked before | Locked now |
| --- | --- | --- |
| [RUSTSEC-2026-0382](https://rustsec.org/advisories/RUSTSEC-2026-0382), rustls-webpki: certificate verification may accept distrusted emSign intermediates | 0.103.15 | 0.103.16 |
| [RUSTSEC-2026-0381](https://rustsec.org/advisories/RUSTSEC-2026-0381), webpki-roots: bundles `emSign ECC Root CA - G3` | 1.0.6, re-exported by the 0.26.11 shim | 1.0.10, re-exported by the same shim ([exception below](#new-exceptions)) |
| [RUSTSEC-2026-0383](https://rustsec.org/advisories/RUSTSEC-2026-0383), webpki-root-certs: the same root | 1.0.9 | 1.0.10 |

The private keys of those intermediates are public. Reliaburger's outbound
HTTPS trusts the bundled Mozilla roots: reqwest, oci-distribution image pulls,
object storage and tokio-tungstenite. Cluster mTLS uses the cluster's own CA
and isn't affected. 0.2.0 and 0.2.1 ship the old versions, a known issue the
maintainer accepted for 0.2.1 on 21 October; 0.2.2 is the first release built
with the fix ([#1269](https://github.com/reliaburger/reliaburger/issues/1269)).
A portable test (`dependency_audit` in `tests/suite/`) fails if `Cargo.lock`
locks an unpatched copy of any of the three again.

## New exceptions

| Advisory and pinned package | Reachability and disposition | Migration work |
| --- | --- | --- |
| [RUSTSEC-2026-0381](https://rustsec.org/advisories/RUSTSEC-2026-0381), webpki-roots 0.26.11 only | The patched range is `>=1.0.10`, so RustSec flags every 0.26 release. But 0.26.11 is a compatibility shim with one item, `pub use parent::TLS_SERVER_ROOTS`, where `parent` is webpki-roots 1.x, and `Cargo.lock` locks 1.x at the patched 1.0.10. The roots the shim serves therefore exclude `emSign ECC Root CA - G3`. Only [tokio-tungstenite 0.28](https://crates.io/crates/tokio-tungstenite/0.28.0) pulls it, for `relish`'s `wss://` fallback, and every release up to 0.30.0 still requires `webpki-roots ^0.26`. The ID covers 1.x too, so `dependency_audit` fails if any locked 1.x copy is older than 1.0.10. Retain until the review deadline. | Drop it once tokio-tungstenite depends on webpki-roots 1.x, or once `relish` always passes its own rustls connector and the `rustls-tls-webpki-roots` feature goes ([#1269](https://github.com/reliaburger/reliaburger/issues/1269)). |
| [RUSTSEC-2026-0374](https://rustsec.org/advisories/RUSTSEC-2026-0374), basic-toml 0.1.10 | Unmaintained informational advisory; no patched version. Reached only through [i18n-config 0.4.8](https://crates.io/crates/i18n-config/0.4.8), the newest release, which `age`'s proc-macro crates use to read `i18n.toml` while compiling. It runs only inside the compiler; no shipped binary contains it. On the 0.2.2 line, which already has `age` 0.12.1 from the October refresh (#1026), this exception came a day earlier, in the [20 October review](2026-10-20-dependency-exceptions.md). Retain until the review deadline. | Move off it through an `age` or `i18n-config` release that doesn't depend on basic-toml, then drop the exception ([#1271](https://github.com/reliaburger/reliaburger/issues/1271)). |

`make audit` passes with these exceptions against RustSec database commit
`e6f94f3760d5e6c54c1b05f204002263bc025524` (1332 advisories), across 711 locked packages, on `main`'s lockfile
(SHA-256 `2f3b04eaa19395f7d77b794d29239cfea4a9323c0467279c9d7584edae9411c5`).

The 0.2.2 train made the same three upgrades the day before, in
[#1272](https://github.com/reliaburger/reliaburger/pull/1272), on top of the
October dependency refresh, and its audit passed in CI. Its lockfile after
`main` merged in: SHA-256
`fb61d7ab04b73fd50fcb42203a501ba5741438958f9a13b2a2be8100828b2241`.

## Reproduce the review

```sh
make audit
cargo tree --locked --all-features --target all -i basic-toml
cargo tree --locked --all-features --target all -i webpki-roots@1.0.10
```
