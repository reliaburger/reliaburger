# Dependency exception review, 21 October 2026

This updates three crates and adds one exception. The review deadline of
**18 November 2026**, which `make audit` enforces, now covers it too.

On 20 October the dependency audit failed on every branch, with no dependency
change, on four advisories newly added to the RustSec database. All of them
are about distrusted emSign intermediate certificates. One example is
[#1265](https://github.com/reliaburger/reliaburger/pull/1265)
([run 39487325389](https://github.com/reliaburger/reliaburger/actions/runs/39487325389)).

| Advisory and package | Disposition |
| --- | --- |
| [RUSTSEC-2026-0382](https://rustsec.org/advisories/RUSTSEC-2026-0382), rustls-webpki 0.103.15 | Updated to 0.103.16 in `Cargo.lock`. |
| [RUSTSEC-2026-0383](https://rustsec.org/advisories/RUSTSEC-2026-0383), webpki-root-certs 1.0.9 | Updated to 1.0.10 in `Cargo.lock`. |
| [RUSTSEC-2026-0381](https://rustsec.org/advisories/RUSTSEC-2026-0381), webpki-roots 1.0.9 | Updated to 1.0.10 in `Cargo.lock`. |
| [RUSTSEC-2026-0381](https://rustsec.org/advisories/RUSTSEC-2026-0381), webpki-roots 0.26.11 | Ignored. See below. |

`tokio-tungstenite` 0.30.0, which `relish` uses with `rustls-tls-webpki-roots`,
depends on webpki-roots 0.26. The 0.26 line has no patched release, so the
advisory still names 0.26.11. But 0.26.11 is a semver-compatibility shim: its
only dependency is webpki-roots 1.x, and its `TLS_SERVER_ROOTS` is
`pub use parent::TLS_SERVER_ROOTS`. With 1.x at 1.0.10, the roots that
`relish` trusts are the patched ones.

Ignoring the advisory by ID would also hide it on webpki-roots 1.x, so
`make audit` now fails before running `cargo audit` if `Cargo.lock` holds
webpki-roots 1.0.0 to 1.0.9.

The exception goes when `tokio-tungstenite` depends on webpki-roots 1
directly. 0.30.0 is its newest release.

Lockfile SHA-256 on `release-0-2-2` after the update:
`fb61d7ab04b73fd50fcb42203a501ba5741438958f9a13b2a2be8100828b2241`.

## Reproduce the review

```sh
make audit
cargo tree --locked -i webpki-roots@0.26.11 -e normal
cargo info tokio-tungstenite
```
