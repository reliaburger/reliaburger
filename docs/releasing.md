# Cutting a release

The [0.1.0 plan](plans/2026-09-16-v0.1.0-release-plan.md) defines the acceptance
gates. A green build alone doesn't qualify the laptop quickstart or its timing.
No public 0.1.0 release has been published by this work.

## What the workflow builds

`.github/workflows/build.yml` builds the following native artefacts on pull
requests, main and manual candidate builds:

| File | Build host | Use |
| --- | --- | --- |
| `bun-linux-x86_64` | Ubuntu 22.04 x86_64 | Linux agent, embedded eBPF |
| `bun-linux-aarch64` | Ubuntu 22.04 arm64 | Linux agent, embedded eBPF |
| `relish-linux-x86_64` | Ubuntu 22.04 x86_64 | Linux CLI |
| `relish-linux-aarch64` | Ubuntu 22.04 arm64 | Linux CLI |
| `relish-macos-aarch64` | macOS 15 Apple silicon | Laptop CLI |
| `relish-macos-x86_64` | macOS 15 Intel | CLI build; cold-install qualification still required |
| `reliaburger-guest-ubuntu-24.04-…-aarch64.qcow2` | Ubuntu 24.04 arm64 | Quickstart VM image |
| `reliaburger-guest-ubuntu-24.04-…-x86_64.qcow2` | Ubuntu 24.04 x86_64 | Quickstart VM image |

Pull requests build the guest images only when `build_guest_image.sh`,
`guest-images.json` or the workflow changes.

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

## Release profile and binary size

`[profile.release]` sets `strip = true` and `codegen-units = 1`. Release builds
never carried debug info (Cargo drops it by default); the 0.1.0 candidate's
binaries were big because of the symbol table and because sixteen codegen units
per crate each kept their own copies of generic code. The two settings halve
every binary:

| Asset | Staged candidate | Now |
| --- | ---: | ---: |
| `relish-macos-aarch64` | 177.8 MB | 90.5 MB |
| `relish-linux-aarch64` | 201.8 MB | 94.6 MB |
| `bun-linux-aarch64` | 230.4 MB | 104.9 MB |

A cold quickstart on Apple silicon downloads 320 MB less (961 MB instead of
1,281 MB, most of what's left being the guest image). The price is a
rebuild of the `reliaburger` crate that's two to three times slower, because
that crate no longer compiles in parallel pieces; expect the "Build locked
release binaries" step to grow from 5–11 minutes to roughly 12–30, which is
still off the candidate workflow's critical path. Fat LTO saved only 3 MB more
and took 23 minutes even warm, and thin LTO made the code bigger, so neither
is on.

Panics still print their message and `file:line`, but `RUST_BACKTRACE=1`
shows no function names in a released binary. Ask for the panic line in bug
reports; for function names, build from source with
`CARGO_PROFILE_RELEASE_STRIP=none`. Nothing reads symbols at runtime. The
measurements, including build times and the backtrace check, are in
[qualification/2026-09-24-binary-size.md](qualification/2026-09-24-binary-size.md).

## Cluster compatibility

0.1.0 requires fresh clusters. Preserve pre-release development data separately;
startup and recovery refuse it rather than attempting an implicit migration.
Do not copy a format stamp onto old data to bypass this check. Bun also refuses
legacy workload-ID aliases and inconsistent ownership records without touching
their runtimes or deleting their records. Canonical identity must agree with the
recorded app, namespace and replica; startup does not rename surviving owners.

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
`src/upgrade/keys.rs`, for example
`base64 < release-key.der | gh secret set RELIABURGER_RELEASE_KEY`. The packaging
script also accepts the base64 of a PEM private key or of the bare 32-byte seed,
and converts either to DER; anything else fails with a message saying what it
found, never the key itself. It then checks the derived public key against
`src/upgrade/keys.rs`, so a wrong key fails before anything is signed. Never commit that private key. This workflow does not
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

Candidate building and release publication are separate manual operations.
After this workflow is on `main`, run:

```sh
gh workflow run build.yml --ref main
```

This reruns source CI and builds the native matrix and PDFs at one main commit.
Only after those checks pass does it collect the two built guest images, sign
all six binaries and both images' digests, and upload `candidate-<commit>-<attempt>` as an Actions artefact.
It creates no Git tag or GitHub release. PR and ordinary main builds never use
the release signing secret.

Download that artefact for qualification. Preserve its run ID, attempt, source
commit and the `candidate.json` SHA-256 printed in the job summary alongside
the qualification results. The record covers every binary, signature, metadata
file, installer, PDF and guest image. Artefacts expire after 90 days; archive the
qualified files, and don't expect promotion to rebuild an expired candidate.
Rerunning the candidate run requires recording and qualifying its new attempt.

Once all release gates pass, an operator creates the version tag at that exact
candidate commit. The tag must equal `v` plus `Cargo.toml`'s version. Pushing a
tag does not rebuild or publish anything. Promotion is explicit:

```sh
gh workflow run promote.yml --ref main \
  -f tag=v0.1.0 -f candidate_run=RUN_ID -f qualified_digest=RECORDED_SHA256
```

The promotion workflow runs `main`'s own scripts and reads the tag only as data
(its `Cargo.toml` version and commit), so nothing in the tagged tree executes
with the release token. It checks the tagged source, successful manual main build,
repository, version and recorded manifest digest. It downloads the preserved
candidate and requires an exact file inventory and matching lengths/hashes.
It then uploads those unchanged files to a new draft, checks GitHub's stored
asset digests and publishes. It neither compiles nor signs. An existing release
refuses replacement; a failed upload/check leaves a draft for investigation,
not an automatically repaired or overwritten release.

The digest input must come from the qualification record. Copying a fresh digest
from unqualified downloads defeats the gate. Workflow verification establishes
identity and byte preservation; it cannot establish that somebody actually ran
the cold-install and recovery tests. Those remain operator acceptance criteria.
The complete hosted candidate, staging and promotion paths have not yet been
exercised. Actual pre-publication mirror delivery (see
[Staging a candidate](#staging-a-candidate)) remains part of V03; downloading
an Actions artefact alone does not qualify the public quickstart.

GitHub documents the [default-branch requirement for manual workflows](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/manually-run-a-workflow)
and the [release asset digest fields](https://docs.github.com/en/rest/releases/releases).
This PR must land before those manual workflows can run.

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

## Staging a candidate

Before promoting 0.1.0, publish the exact signed candidate to HTTPS and run
the real `curl … | sh` install against it on every host we advertise. The
workflows below need to be on `main`; the first real staging run happens once
this lands.

1. **Build the candidate on main.**

   ```sh
   gh workflow run build.yml --ref main
   ```

   When it finishes, open the run's summary and note three things: the run ID
   (from the URL), the candidate commit and the *Qualification manifest
   SHA-256*. That digest is `QUALIFIED_DIGEST` from here on. Keep it with the
   qualification records; never copy it from a later download.

2. **Stage it.**

   ```sh
   gh workflow run stage.yml --ref main \
     -f candidate_run=RUN_ID -f qualified_digest=QUALIFIED_DIGEST
   ```

   `stage.yml` runs `main`'s own `candidate.py`: it requires the run to be a
   successful manual build of `main`, downloads `candidate-<commit>-<attempt>`,
   and checks the exact inventory and every byte against the digest, the same
   code promotion runs. Then it uploads the unchanged files as a draft
   pre-release, checks GitHub's stored digests for every asset, publishes it as
   a pre-release (never `--latest`) and checks the tag points at the candidate
   commit. The job summary prints the staged base URL:

   ```
   https://github.com/reliaburger/reliaburger/releases/download/staging-v0.1.0-RUN_ID-ATTEMPT
   ```

   Re-running it for the same candidate is safe: a published staging
   pre-release is only re-verified, an unfinished draft is deleted and staged
   again, and one whose bytes differ stops the run without changing anything.
   A rebuilt candidate (a new attempt) gets its own tag.

3. **Qualify it on every host in the matrix**: Apple silicon, Intel macOS,
   Linux x86_64 and Linux arm64. From a checkout of the same `main`:

   ```sh
   scripts/release/qualify-staged-install.sh \
     --base-url https://github.com/reliaburger/reliaburger/releases/download/staging-v0.1.0-RUN_ID-ATTEMPT \
     --qualified-digest QUALIFIED_DIGEST
   ```

   It creates a fresh, short `RELIABURGER_HOME` under `/tmp` and runs
   `curl -fsSL https://reliaburger.com/install.sh | RELIABURGER_RELEASE_BASE_URL=… sh -s -- --timings`,
   exactly as a user would. Use `--bootstrap docs/website/install.sh` if the
   website doesn't serve the current bootstrap yet (the record notes which one
   ran). It checks `candidate.json` against the digest, `SHA256SUMS` against
   `candidate.json`, that setup used the mirror, and that the installed CLI,
   both Linux binaries and the guest image are candidate assets. Then it
   applies the podinfo tour, runs `status`, `path` and `metrics`, destroys the
   cluster, uninstalls, and writes a Markdown record with the timings. It never
   uses `~/.reliaburger` and fails if that directory's top level or
   `~/.local/bin/relish` changed. Pass `setup --quickstart` options after
   `--`, for example `-- --api-port 29117 --ingress-port 28080` when another
   cluster holds the default ports, and `--keep` to leave a failed run for
   debugging (then `relish local destroy --yes` and `relish uninstall --yes`
   with the same `RELIABURGER_HOME`).

   Run it at least twice per host for V04: once with nothing cached (the
   script always starts empty) and again for repeatability.

4. **Soak it (V02)** on Apple silicon, in two tiers
   ([plan](plans/2026-09-25-v02-sustained.md), D3). Build and sign the soak
   bun first (D2). After each round of fixes, run the fast tier: every fault
   kind and every special on the compressed schedule, in about 90 minutes.

   ```sh
   scripts/release/qualify-sustained.sh --tier fast \
     --base-url https://github.com/reliaburger/reliaburger/releases/download/staging-v0.1.0-RUN_ID-ATTEMPT \
     --qualified-digest QUALIFIED_DIGEST --soak-bun /path/to/bun-v0.1.0-soak.1
   ```

   Once the fast tier is clean on the final candidate, run the final tier
   once: 8 hours on the full schedule, which catches slow accumulation.

   ```sh
   scripts/release/qualify-sustained.sh --tier final \
     --base-url https://github.com/reliaburger/reliaburger/releases/download/staging-v0.1.0-RUN_ID-ATTEMPT \
     --qualified-digest QUALIFIED_DIGEST --soak-bun /path/to/bun-v0.1.0-soak.1 \
     --record docs/qualification/DATE-sustained-v02.md
   ```

   The record's second paragraph states the verdict. A fast run can only say
   "fast tier: clean"; only a clean final-tier run, 8 hours on the full
   schedule with the digest checked, says the V02 gate passes. A product fix
   found during the final run means a new candidate, a fresh fast run and
   then a fresh final run. `--resume --evidence DIR` continues an interrupted
   run with its original tier.

5. **Record it.** Copy each host's record into
   `docs/qualification/DATE-staged-install-HOST.md`, alongside the run ID,
   attempt, commit and digest. Gates V03 and V04 in
   [progress.md](progress.md) point at these records.

6. **Clean up the staging pre-releases.** Delete them before promotion so the
   release page and the release notes' "previous tag" don't pick them up:

   ```sh
   gh release delete staging-v0.1.0-RUN_ID-ATTEMPT --cleanup-tag --yes
   ```

7. **Promote** as described above, with the same run ID and digest. The
   staging tag can't be promoted: it doesn't match `v1.2.3`, `candidate.py`
   refuses it as a version, and `promote.yml` refuses any tag containing
   `staging`.

## Soaking a candidate in CI

The V02 sustained soak ([plan](plans/2026-09-25-v02-sustained.md)) also runs
on a hosted Linux runner. Once a candidate is staged:

```sh
gh workflow run soak.yml --ref main \
  -f staging_tag=staging-v0.1.0-RUN_ID-ATTEMPT \
  -f qualified_digest=QUALIFIED_DIGEST \
  -f duration=90m -f schedule=compressed
```

`staging_tag` also takes the staged base URL. The job runs on `ubuntu-24.04`
(x86_64, 4 vCPUs, 16 GiB), opens `/dev/kvm` to the runner user with the udev
rule GitHub documents for the Android emulator, installs QEMU (the Linux
quickstart leaves QEMU to the user) and runs `qualify-sustained.sh` with the
inputs. The harness bootstraps the three-node quickstart cluster with
`curl | sh` from the staging pre-release, soaks it and tears it down. The
cluster's three VMs (2 vCPUs and 2 GiB each) overcommit the runner's four
cores but fit in memory, so no VM sizing changes. It isn't slow: in the
first run `curl | sh` to a ready cluster took 84 s, the whole setup to the
first fault under 4 minutes, and faults settled in 20–90 s.

The record lands in the job summary. The artefacts, kept for 30 days, are
`soak-record-*` (the Markdown record), `soak-evidence-*` (the whole evidence
directory, less the cluster token and the soak CA's keys) and, when the job
fails, `soak-failures-*` (the failure bundles, the bootstrap log and any Lima
logs). A record that says FAIL fails the job; that's the soak doing its job,
not the workflow breaking. Triage the failure rows as the plan describes.

What it covers, and what it doesn't:

- **Hosted jobs stop at 6 hours.** The workflow refuses a duration that
  wouldn't fit next to about 45 minutes of setup, one cycle that starts just
  before the end and teardown: up to 285 minutes compressed, 235 minutes on
  the full schedule. That covers the compressed (fast) tier and full-schedule
  runs of about four hours. The 8-hour final tier, and the 12-hour lane A run,
  still need a long-running host or a self-hosted runner.
- **No upgrade walks.** They need a private soak build (`0.1.0-soak.1`) signed
  with the release key (D2), and nothing in CI signs with it, so the harness
  skips the upgrade slots and the record says so. Run those on a host that
  holds the key.
- **One platform.** Linux x86_64 with Lima's QEMU driver. The macOS VZ path is
  lane A's host.

**Follow-up: a signed soak build in CI.** Upgrade slots on Actions would need
a job that builds bun at the candidate commit with version `0.1.0-soak.1` and
signs it with `relish dev sign-binary --key`. That means the release signing
key as an Actions secret, reachable from a workflow anyone with write access
can dispatch, on a runner that also runs third-party actions. A binary signed
that way is a genuine release-signed bun below 0.1.0: if it leaked from an
artefact or a cache, any cluster that trusts the release key would accept it
as a downgrade target. Doing it safely needs its own environment with
required reviewers, the key used in one step and never written to disk,
signing inside the soak job so the build never leaves the runner or becomes
an artefact, and ideally a separate soak key that only soak clusters trust. None of that is in place, so CI doesn't try.

## Qualifying a candidate from another host

`stage.yml` is the usual way to get the candidate onto HTTPS. Any other HTTPS
directory works the same way. Download the candidate artefact and verify it
against the separately recorded source/run identity and manifest digest before
staging it:

```sh
python3 scripts/release/candidate.py verify --directory candidate \
  --version v0.1.0 --repository reliaburger/reliaburger \
  --commit FULL_COMMIT_SHA --run-id RUN_ID --run-attempt ATTEMPT \
  --qualified-digest RECORDED_SHA256
```

Serve those unchanged files from one HTTPS directory. The directory must contain
the entire inventory, including both guest images and both metadata documents.
Do not rewrite URLs inside the metadata or regenerate the installer. To exercise
the candidate installer from empty caches:

```sh
curl --fail --location --proto '=https' --proto-redir '=https' \
  https://YOUR_HOST/candidate/install.sh -o /tmp/reliaburger-candidate-install.sh
RELIABURGER_RELEASE_BASE_URL=https://YOUR_HOST/candidate \
  sh /tmp/reliaburger-candidate-install.sh
```

The static bootstrap accepts the same environment variable and fetches the
candidate's installer first. The generated installer pins the CLI's checksum
and passes `--release-mirror` to managed setup. If you installed the candidate
CLI separately, run `relish setup --quickstart --release-mirror https://YOUR_HOST/candidate`.
Repeat the same command to resume an interrupted setup.

Only that version's Reliaburger release URLs are redirected to the directory.
The pinned Lima tooling URLs stay unchanged. HTTPS, bounded requests, guest-image
signatures and embedded binary signatures remain enforced. Credentials in URLs,
query strings and fragments are refused. This cannot be combined with
`--development-binaries`. As with the default bootstrap, HTTPS authenticates the
selected installer host; the independently retained candidate digest establishes
which complete file set is under qualification.

Record the mirror URL and all downloaded hashes with the cold-run measurements.
A staged run qualifies those signed bytes; final public URL/Pages checks still
need their own evidence after publication. No candidate has been staged yet:
`stage.yml` first runs once it is on `main`.

## Guest images and bootstrap installer

Each quickstart VM boots from a guest image the release builds itself: the
dated Ubuntu 24.04 cloud image named in `scripts/release/guest-images.json`
with that file's `packages` (runc, uidmap, btrfs-progs, nftables, iptables,
iproute2) already installed. Without them baked in, every VM spent 15–40 s of
its first boot in `apt-get update` and `install`, against Ubuntu's live mirrors
([measurements](qualification/2026-09-24-guest-image.md)).

`scripts/release/build_guest_image.sh` builds one image, for the host's own
architecture:

```sh
sudo apt-get install -y qemu-utils
sudo scripts/release/build_guest_image.sh --output guest > guest-record.json
```

It downloads the upstream image (or takes `--source FILE`) and refuses it unless
its SHA-256 matches the pin. It converts it to a sparse raw file, loop-mounts
the root and boot partitions, and runs `apt-get install` in a chroot, with
package indexes and downloads on a tmpfs so they never reach the image and with
service starts blocked. Then it seals the image: an empty `/etc/machine-id`,
`cloud-init clean`, no SSH host keys, random seed, logs or temporary files, so
every VM still generates its own identity at first boot. `fstrim` returns freed
blocks to the sparse file, and `qemu-img convert -c` writes a zlib-compressed
qcow2. The installed package versions go into `/usr/share/reliaburger/guest-image.json`
inside the image and into the JSON record the script prints.

Why these choices:

- **Native builds, chroot, no emulation.** The package scripts run in the
  chroot, so the build host's CPU must match. GitHub's `ubuntu-24.04-arm`
  runner builds aarch64 and `ubuntu-24.04` builds x86_64, at native speed and
  without `/dev/kvm`, which libguestfs (`virt-customize`) would want, and
  without the `qemu-user-static` a cross-architecture chroot would need.
- **qcow2 with zlib, not zstd or raw.** Lima 2.1.0 converts qcow2 itself, but
  registers no zstd decompressor for qcow2 clusters, and it decompresses a
  `.zst` or `.xz` file by running the `zstd` or `xz` command, which macOS
  doesn't ship. zlib qcow2 is what Ubuntu itself publishes, so the quickstart
  treats the built image exactly like the stock one.
- **Live archive, not a snapshot.** `snapshot.ubuntu.com` doesn't serve
  `ubuntu-ports` (arm64) anonymously, so the build installs the current
  packages and records their versions.

A rebuild never produces the same bytes: file times and the ext4 journal
differ. So the CLI can't have the image's digest compiled in, the way it had
the upstream image's. Instead `package.py` signs a statement per architecture
(version, architecture, asset name, image SHA-256 and upstream SHA-256) with
the release key and publishes `guest-image-metadata.json`. The CLI downloads
that, requires a valid signature from an embedded release key, the pinned asset
name and the pinned upstream digest, and only then downloads the image and
checks its SHA-256. `candidate.py` checks that the metadata names both images
with their actual digests and the pinned source, and records every byte in
`candidate.json` as usual.

Update `guest-images.json` deliberately when changing the guest baseline (new
upstream image date or package list); don't introduce an unpinned `current`
fallback. Development runs (`--development-binaries`) have no release to take a
built image from, so they boot the pinned upstream image and install the
packages at first boot. The VM's provisioning script skips `apt` whenever every
package is already present, so both paths use the same script.

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
