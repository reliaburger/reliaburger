# Dependency exception review, 20 October 2026

This adds one exception. The list in `.cargo/audit.toml` had been empty since
the October dependency refresh ([#1026](https://github.com/reliaburger/reliaburger/issues/1026)),
and the review deadline of **18 November 2026**, which `make audit` enforces,
now covers this one.

On 20 October the dependency audit failed on every branch, with no dependency
change, on an advisory newly added to the RustSec database, for example on
[#1256](https://github.com/reliaburger/reliaburger/pull/1256)
([run 39438028311](https://github.com/reliaburger/reliaburger/actions/runs/39438028311))
and [#1257](https://github.com/reliaburger/reliaburger/pull/1257):

| Advisory and pinned package | Reachability and disposition | Migration work |
| --- | --- | --- |
| [RUSTSEC-2026-0374](https://rustsec.org/advisories/RUSTSEC-2026-0374), basic-toml 0.1.10 | Unmaintained informational advisory, dated 26 September 2026; no patched version. Reached only through `age` 0.12.1 → `i18n-embed-fl` 0.10.1 → `i18n-config` 0.4.8, which are procedural macros: `basic-toml` reads `age`'s translation configuration while `age` compiles, and isn't linked into `bun` or `relish`. No path parses TOML at run time with it; Reliaburger's own TOML goes through the `toml` crate. `age` 0.12.1 and `i18n-config` 0.4.8 are the newest releases, so no upgrade removes it. Retain until the review deadline. | Drop the exception when a new `age` or `i18n-config` moves off `basic-toml`. At the review, check whether `age` can be built without its i18n stack. |

The audit that found it loaded 1,345 advisories and scanned 712 locked
packages. Lockfile SHA-256 on `release-0-2-2`:
`345a87edec818a6fd5dbe89c0a58db3ff0c3fc864ad98eba44c12e68de84f07b`.

## Reproduce the review

```sh
make audit
cargo tree --locked -i basic-toml -e all
cargo info age
cargo info i18n-config
```
