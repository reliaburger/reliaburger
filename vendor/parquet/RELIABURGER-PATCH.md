# Reliaburger's Parquet safety patch

Source: the crates.io `parquet` 54.3.1 source package, licensed under Apache-2.0.
Original archive SHA-256: `bfb15796ac6f56b429fd99e33ba133783ad75b27c36b4b5ce06f1f82cc97754e`.
`LICENSE.txt` and `NOTICE.txt` are retained. The upstream package's development
lockfile and Cargo cache metadata are omitted; the repository's root lockfile
controls the shipped dependency graph.

`RELIABURGER-PATCH.diff` contains every change to upstream source/manifests:

- Use Thrift 0.23.0, which fixes GHSA-2f9f-gq7v-9h6m / CVE-2026-43868.
- Add the UUID dependency and required protocol method for Thrift's new trait.
  Parquet's custom reader refuses this unused wire type explicitly.
- Bound the private slice decoder's integers by their declared bit width,
  including the final byte's payload, before shifting. Unknown-field skipping
  uses those same trait methods.
- Refuse list/set counts larger than the remaining metadata before generated
  code reserves vectors; reject signed count overflow.
- Return an EOF error for a truncated double, avoiding an indexing panic.

The private decoder is independent of the Thrift dependency. Upgrading Thrift
alone does not repair it. `tests/parquet_safety.rs` exercises these boundaries
through the public file reader, alongside normal archive round-trip tests.

This patch retains DataFusion 45 / Arrow 54 and their existing query contract.
Remove it only when a compatible upstream dependency chain passes these
regressions and the metrics, rollup and log restart/query suites. Do not remove
it merely because a newer Parquet no longer depends on the Thrift crate: the
same malformed-integer regression also failed against unmodified Parquet 59.3.

References:
- https://github.com/advisories/GHSA-2f9f-gq7v-9h6m
- https://github.com/apache/thrift/commit/d5152211af61f850ec393604316804096dd4632e
- https://crates.io/crates/parquet/54.3.1
