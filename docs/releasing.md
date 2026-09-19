# Cutting a release

The [0.1.0 plan](plans/2026-09-16-v0.1.0-release-plan.md) defines the acceptance
gates. A green build alone doesn't qualify the laptop quickstart or its timing.
No public 0.1.0 release has been published by this work.

## What the workflow builds

`.github/workflows/build.yml` builds the following native artefacts on pull
requests, main and version tags:

| File | Build host | Use |
| --- | --- | --- |
| `bun-linux-x86_64` | Ubuntu 22.04 x86_64 | Linux agent, embedded eBPF |
| `bun-linux-aarch64` | Ubuntu 22.04 arm64 | Linux agent, embedded eBPF |
| `relish-linux-x86_64` | Ubuntu 22.04 x86_64 | Linux CLI |
| `relish-linux-aarch64` | Ubuntu 22.04 arm64 | Linux CLI |
| `relish-macos-aarch64` | macOS 15 Apple silicon | Laptop CLI |
| `relish-macos-x86_64` | macOS 15 Intel | CLI build; cold-install qualification still required |

Native runners avoid depending on tools installed outside a cross-build
container. Runner labels follow GitHub's
[hosted runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
The initial Linux build baseline is Ubuntu 22.04. Compatibility with older
systems is not promised. macOS builds are not yet Developer ID signed or
notarised; don't equate an Actions build with clean-host acceptance.

## Compiler baseline

The source minimum is Rust 1.97 (`Cargo.toml`); CI checks the locked dependency
graph and runs both feature configurations on 1.97.0. Native release jobs pin
Rust 1.98.0 and build with `--locked`. To change this policy, update the manifest,
workflow pins and installation docs together, then rerun minimum-compiler,
native-build and upgrade qualification. A compiler change produces a new
candidate and new checksums; never replace an existing release's binaries.
The compiler pin does not promise identical bytes across different linkers or
host operating systems.

## Cluster compatibility

0.1.0 requires fresh clusters. Preserve pre-release development data separately;
startup and recovery refuse it rather than attempting an implicit migration.
Do not copy a format stamp onto old data to bypass this check.

Run `bun --compatibility` to read the binary's current `protocol` and `state`
generations as JSON without starting a node. `GET /v1/version` includes the same
contract. Different product versions may
roll or roll back only when both generations match exactly. The agent verifies
the signed executable and checks this contract before staging it. Joins and
cluster transports also enforce compatibility; absent evidence is a refusal.

For a future incompatible wire or state change, bump the relevant generation
and design migration separately. Leader-last upgrade ordering does not make an
unknown Raft request safe during elections. Qualify the actual old/new binary
pair before advertising it as supported.

## Signing identity

Configure the Actions secret `RELIABURGER_RELEASE_KEY` with the base64 encoding
of the existing Ed25519 PKCS#8 DER private key whose public key is listed in
`src/upgrade/keys.rs`. Never commit that private key. This workflow does not
rotate the project's identity or generate a replacement when the secret is
missing.

The 0.1.0 signing identity was established on 17 September 2026 because the
pre-release development private key was unavailable. Fresh 0.1.0 installations
trust the new public key in `src/upgrade/keys.rs`; old development binaries are
not an upgrade source. Keep an encrypted offline backup of the private key.
Replacing a key after a supported release requires an overlap release trusting
both identities, not an unannounced replacement.

The packaging script derives the public key and checks it against the compiled
trust list before signing. A missing key, wrong key, incomplete matrix or failed
signing operation stops publication. Unit tests use fresh temporary keys and
verify that a modified binary no longer passes signature verification.

Each binary gets a schema-1 `.sig` envelope compatible with the existing
upgrade verifier. `SHA256SUMS` supports download checks; it doesn't replace
signature verification. Public release signatures establish project provenance.
Operators using the dual-signature upgrade policy still need to approve binaries
with their configured external key.

## Metadata and publication

The tag must equal `v` plus the version in `Cargo.toml`. After source CI, native
builds, packaging tests and PDF generation pass, the tag workflow signs all six
binaries and attaches them, their envelopes, checksums, metadata and PDFs to a
GitHub release.

- `metadata.json` selects **Bun** by platform, preserving the existing schema
  and upgrade reader.
- `cli-metadata.json` uses the same schema to select **Relish**. Keeping the
  documents separate prevents an older agent from interpreting a CLI as an
  upgrade candidate.
- URLs inside each document point to that exact version's GitHub release.
- Bun's default metadata URL is
  `https://github.com/reliaburger/reliaburger/releases/latest/download/metadata.json`.

The website and installer are separate static assets under `docs/website`,
published by `static.yml`. GitHub Pages cannot select a different response for
curl and a browser at `/`; the planned shell endpoint is `/install.sh`.

Before tagging 0.1.0, complete the managed-cluster and clean-install gates in the
release plan. Record timing from an empty cache, the actual artefact digests,
host and guest versions, memory use, and the successful sample workload. Don't
publish a five-minute claim from a source build or a warmed VM.

## Guest images and bootstrap installer

The release job mirrors the two dated Ubuntu images in
`scripts/release/guest-images.json`, verifying SHA-256 before publication. The
CLI embeds that same manifest and verifies its downloaded image. Update the
manifest deliberately when changing the guest baseline; don't introduce an
unpinned `current` fallback.

Packaging generates `install.sh` with the exact native CLI checksums. The
static `docs/website/install.sh` fetches that versioned installer over HTTPS.
The CLI then verifies the project signatures on its Linux binaries. HTTPS is
the initial bootstrap trust boundary; the shell script doesn't claim to verify
its own signature. Both scripts finish downloads before executing them and
leave an existing CLI untouched on a checksum failure.

See [quickstart.md](quickstart.md) for the managed workflow. Before publishing,
run it from the signed candidate with empty caches, including three-node and
single-node runs, interruption/resume, stop/start and destroy. A run using
`--development-binaries` is useful integration evidence but doesn't replace
this gate. The installer and five-minute promise remain pending until it passes.
