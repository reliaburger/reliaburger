# Plan: verify this cluster in one command

[#1689](https://github.com/reliaburger/reliaburger/issues/1689), on the 0.2.8
train ("agents you can trust"). One read-only command that says whether the
running cluster is exactly what the release says it is.

## The command

`relish agent verify` answers through the agent output contract
(`relish.agent/v1`, exit `0` whenever it answered). `relish verify` prints the
same check for a person, and `-o json` prints its `data`.

Why a human command too: the person who most often asks "is this cluster
running the bytes we released?" is an operator or an auditor, before a
change freeze, after an incident, or in a CI job. They shouldn't need to
parse an agent envelope, and a script wants an exit code that means
something: `0` verified, `1` mismatch, `3` unknown, as `relish wtf` uses
them. Both commands share one collector and one pure verdict, so they can't
disagree.

## What it reads

- Each node's `/v1/version`, through the leader's relay for a council
  (`version`, `platform`, `binary_sha256`), or the node itself for a
  single-node cluster. It's the collector `relish agent upgrade-check`
  already uses, without the `wtf` pass.
- The release metadata for each version the nodes run:
  `https://github.com/reliaburger/reliaburger/releases/download/<version>/metadata.json`
  (bun), and `cli-metadata.json` for the version of `relish` running the
  check. `--release-url` points at a mirror.
- The release artefacts themselves (`bun-<platform>`), fetched from the URL
  the metadata names, or read from `--artefacts DIR`. The release key signs
  the bytes, not their hash, so checking the signature needs the bytes.
- This `relish`'s own executable.

## What it checks

For every live node:

1. it answered, with a version, a platform and the SHA-256 of its binary;
2. the release metadata has that version and a `bun` build for that platform;
3. the node's SHA-256 equals the published one;
4. the published artefact hashes to that SHA-256, and its `embedded_signature`
   verifies against an Ed25519 release key compiled into `relish`
   (`src/upgrade/keys.rs`, with the same `verify_binary` the upgrade path
   uses).

Across the cluster: every live node runs the same version. For `relish`
itself: its bytes hash to the `cli-metadata.json` entry for its version and
platform, and the signature over those bytes verifies.

The metadata isn't signed: it travels over TLS. That's fine, because the
check never trusts it on its own. A node's hash must equal a hash whose
bytes carry the release signature, so a lying metadata host can make the
check fail, never pass.

## The answer

`verdict` is one of:

- `verified`: every node answered, every check passed;
- `mismatch`: something isn't the published release: `hash_mismatch` (drift),
  `bad_signature`, `artefact_mismatch`, `version_skew`, `no_artefact`,
  `unpublished_version`, `relish_mismatch`;
- `unknown`: a piece of evidence is missing: `metadata_unavailable`,
  `cluster_unreachable`, `node_unanswered`, `hash_unreported`,
  `platform_unknown`, `signature_unchecked`, `relish_unchecked`.

A mismatch outranks missing evidence; missing evidence never reads as
verified. `reasons` lists each finding with its code, kind, node and one
line. `nodes` gives each node's version, platform, reported and published
SHA-256, and whether the hash and the signature checked out; `relish` does
the same for the CLI; `trusted_keys` names the keys the signatures were
checked against; `metadata` names where each document came from.

## Offline and air-gapped clusters

`--metadata FILE` reads the bun metadata from a file and turns the network
off: nothing is fetched. `--artefacts DIR` supplies the `bun-<platform>`
binaries for the signature check, and `--cli-metadata FILE` the `relish`
metadata. Without the bytes the hashes are still compared, and the answer
is `unknown` with `signature_unchecked`, naming the flag to pass. When the
metadata can't be fetched, the reason says so and the next step is the
`gh release download` line that fetches the files for `--metadata` and
`--artefacts`.

## Out of scope

- A node reports its own hash. A compromised node can lie; proving what a
  machine runs needs measured boot or remote attestation, which this isn't.
- `relish` checks itself, but a tampered `relish` could lie about that too.
  Check `relish` first with `gh attestation verify` (#1583) if that matters.
- GitHub build attestations (`gh attestation verify`) need `gh` and the
  network; this command stays within what `relish` carries.
- Workload images, guest images and configuration drift.
- `relish mcp` doesn't offer it: like `upgrade-check`, it reaches past the
  cluster API to the internet and reads local files.

## Tests

- Unit tests for the verdict over hand-built evidence: a verified cluster,
  a node whose hash differs, a version the metadata doesn't have, a bad
  signature and a tampered artefact (with throwaway keys), an unreachable
  node, a dead node, version skew, no metadata, and the offline flags.
- `insta` snapshots of the envelope for each verdict, and of the human
  output.
- A `tests/suite` test runs the `relish` binary against a stub API and a
  metadata file, with no network.
- The gym: a `release-drift` scenario whose release metadata names other
  bytes than the cluster runs; the baseline must call `relish agent verify`
  and name the fault `binary_drift`.
