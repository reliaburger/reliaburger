# Dependency exception review, 12 October 2026

This adds one exception to the [18 September review](2026-09-18-dependency-exceptions.md);
the three exceptions there are unchanged, and so is their review deadline of
**18 November 2026**, which now covers this one too.

On 12 October the weekly `make audit` failed on `main`
([run 38245599366](https://github.com/reliaburger/reliaburger/actions/runs/38245599366)),
and so did the dependency audit on the 0.2.0 release PR
([#819](https://github.com/reliaburger/reliaburger/pull/819)), on a new finding:

| Advisory and pinned package | Reachability and disposition | Migration work |
| --- | --- | --- |
| [RUSTSEC-2024-0449](https://rustsec.org/advisories/RUSTSEC-2024-0449), unsafe-libyaml 0.2.11 | Unmaintained informational advisory; no patched version. Reached only through [serde_yaml 0.9.34](https://docs.rs/crate/serde_yaml/0.9.34+deprecated), itself deprecated, and only from `relish`: the `bun` node agent and every server path never parse or write YAML. Most uses write YAML output (`--output yaml`, bench reports). The one parsing path is the Kubernetes import (`src/relish/k8s_import.rs`), which reads manifests the operator names on their own machine, through `relish import` or a Kubernetes manifest given to a manifest command. Retain until the review deadline. | Move `relish` to a maintained YAML crate that doesn't pull unsafe-libyaml, checked against the advisory database before adopting it, and keep the import's document-splitting and strict-mode tests passing ([#818](https://github.com/reliaburger/reliaburger/issues/818), 0.2.1). |

`make audit` passes with this exception against RustSec database commit
`25a288f68f2d5ebe658b243a334110c7217a2569` (1,299 advisories), across 712 locked
packages. Lockfile SHA-256:
`c9ec266ea196417df3057cb55fc790ef7bdd5a6e134add1bb773364984d788d8`.

## Reproduce the review

```sh
make audit
cargo tree --locked -i unsafe-libyaml
git grep -ln serde_yaml -- src
```
