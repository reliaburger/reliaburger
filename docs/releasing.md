# Cutting a release

0.1.0 went through this procedure and was promoted on 29 September 2026: one
build, staged over HTTPS, qualified, then published unchanged. Its [release
closure record](qualification/2026-09-27-v0.1.0-release-closure.md) lists the
thirteen candidates it took and what each soak run found. The
[0.1.0 plan](plans/archive/2026-09-16-v0.1.0-release-plan.md) defined its acceptance
gates (V01–V04 in the [frozen checklist](plans/archive/progress-to-0.1.0.md#acceptance-and-release-gates),
with the ones still open under [known gaps](roadmap.md#known-gaps)); a
later release keeps them unless a dated plan changes them. A green build alone
doesn't qualify the laptop quickstart or its timing.

The commands below use 0.1.0's names (`v0.1.0`, `staging-v0.1.0-…`) as the
worked example. Substitute the version you're releasing.

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
| `reliaburger-guest-ubuntu-24.04-…-aarch64.qcow2` | Ubuntu 24.04 arm64 | Quickstart VM image |
| `reliaburger-guest-ubuntu-24.04-…-x86_64.qcow2` | Ubuntu 24.04 x86_64 | Quickstart VM image |

Pull requests build the guest images only when `build_guest_image.sh`,
`guest-images.json` or the workflow changes.

One more artefact is never shipped: `profiling-build-COMMIT-ATTEMPT` holds
`bun-linux-aarch64` and `relish-linux-aarch64` built again from the same
commit with frame pointers and line tables
(`RUSTFLAGS="-C force-frame-pointers=yes"`,
`CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`, not stripped). The release
profile step uses it to name Bun's frames ([step 9](#staging-a-candidate)),
so the profile never compiles on the bench host. Pushes and manual runs
build it, and pull requests only when the workflow changes. Its name matches
neither the candidate job's `binaries-*` nor `candidate-*`, so it never
reaches `candidate.json`, staging or promotion.

There's no Intel macOS build. 0.2.1 was the last release with
`relish-macos-x86_64` (maintainer decision, 17 October 2026,
[#1018](https://github.com/reliaburger/reliaburger/issues/1018)): nothing ever
qualified it, Intel macOS runners are the scarcest on Actions, and the macOS
CLI only drives Linux VMs anyway. The installer refuses an Intel Mac and
points at a Linux host or at 0.2.1. The x86_64 guest image stays, because
Linux x86_64 hosts use it.

Native runners avoid depending on tools installed outside a cross-build
container. Runner labels follow GitHub's
[hosted runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
The initial Linux build baseline is Ubuntu 22.04. Compatibility with older
systems is not promised. macOS builds are not yet Developer ID signed or
notarised; don't equate an Actions build with clean-host acceptance.

## Merge trains and their CI

Each release collects its changes on a merge-train branch named after the
version (`release-0-2-1`), and one gate PR takes the train into `main`. CI
treats the three kinds of run differently ([which jobs run](testing.md#which-jobs-a-pull-request-runs)):

- **A PR into the train** gets the light set: lint and format, minimum Rust,
  portable Linux with the coverage floor, the demo app and the optimised
  contract boundaries, in 15–20 minutes. A `full-ci` label adds everything
  except the three macOS shards: privileged Linux, cluster, acceptance,
  upgrades and benchmarks. The macOS shards run on the PR only when it
  touches macOS-specific code or carries a `macos` label.
- **The merge queue**, where a train has one, runs every job, macOS shards
  and heavy suites included, on the batch merged onto the train, before any
  of it lands. The queue requires the single check `ignored-test evidence`,
  which waits for every other job and demands every evidence owner.
- **The push to the train** runs everything on the result, macOS included.
- **The gate PR** into `main` gets the light set on each push, because the
  push to the train has just run everything on the same code
  ([#1443](https://github.com/reliaburger/reliaburger/issues/1443)). At code
  freeze, label it `release-gate`: from then on it runs everything, and the
  evidence gate refuses it without every owner, macOS included. Merging it
  pushes to `main`, which runs everything again, and that run is the one the
  candidate build reuses.

So nothing reaches a train, or `main`, without the full set having passed on
that exact commit. macOS runs on every push and on the nightly, on the
release gate PR and on the candidate build, but not on ordinary pull
requests: the organisation's plan runs at most five macOS jobs at once, and
each full run takes three. Dependabot's PRs never get macOS unless labelled;
the nightly on `main` covers them.

## Refreshing dependencies

The first PR on every minor release's train refreshes the dependencies, so
the whole soak gate runs against the new set rather than a lockfile nobody
has looked at since the last advisory
([#1026](https://github.com/reliaburger/reliaburger/issues/1026)). 0.2.1's
gate found why this matters: the soak bricked a Raft store with a redb crash
that redb 4.2.0 had already fixed, while we still locked 2.6.3
([#1024](https://github.com/reliaburger/reliaburger/issues/1024)).

1. Run `make outdated` and save its output for the PR. It lists what `cargo
   update` would move and every crate a major version behind.
2. Run `cargo update`. It stays within the ranges in `Cargo.toml`, so it
   never crosses a major version. Check that `Cargo.lock` has no `git`
   sources.
3. If `time` or `thiserror` moved, copy their new versions into the `=` pins in
   `tests/contracts/cron/Cargo.toml` and run `cargo update --manifest-path
   tests/contracts/cron/Cargo.toml`. CI's optimised cron contract fails
   when the two graphs differ.
4. Run `make audit`, `make ci` and every gated target, and add the `full-ci`
   label to the PR.
5. If one crate's update breaks something, pin it back with `cargo update -p
   <crate> --precise <old version>`, name it in the PR, and open an issue.
6. Pick the majors worth taking this release from the `make outdated` list
   and bump them one crate per PR, security-relevant crates first. A bump
   that changes a wire or durable format bumps `protocol` or `state` in
   `CURRENT` (`src/compatibility.rs`).

Between releases, [Dependabot](../.github/dependabot.yml) opens one grouped
PR a week per ecosystem. Its Cargo PR is lockfile-only, the same `cargo
update` as step 2; it never proposes a major version, which stays a
deliberate PR of its own. It leaves the `dtolnay/rust-toolchain` tags alone,
because those are the compiler pins below. The weekly
[`security.yml`](../.github/workflows/security.yml) run prints `make
outdated` in its job summary.

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

Every refusal leads with the formats it found and the ones this binary needs,
names the binary's version (and commit, when the build recorded one), and
links back here, because `journalctl` cuts long lines at the terminal's width:

```text
incompatible state format: found 63; this binary (reliaburger v0.2.0 (465fdeb)) needs 68. Pre-1.0 builds don't migrate state: …
incompatible cluster formats: found protocol 46, state 63; this binary (reliaburger v0.2.0 (465fdeb)) needs protocol 51, state 68. …
```

Before 1.0 nothing migrates between generations, so there are two ways
forward: run the release that wrote the data (or that the rest of the cluster
runs), or move the data directory aside and recreate the cluster. Both leave
the refused data untouched.

### Compatibility before 1.0.0

Until 1.0.0 every release is a development release, and we don't keep
backwards compatibility (maintainer decision, 28 September 2026). There are no
migrations, no mixed-version clusters across a format change and no feature
gates. Any incompatible wire or durable-state change bumps `protocol` or
`state` in `src/compatibility.rs`; nodes then refuse old peers and old state,
so an upgrade across that bump means starting a fresh cluster. So
`relish upgrade` works only between builds of the same format (maintainer
decision D31 in [#674](https://github.com/reliaburger/reliaburger/issues/674),
10 October 2026). Every release from 0.1.0 to 0.2.2 bumped a format. 0.2.3
is the first to keep its predecessor's pair, so a 0.2.2 cluster could roll
onto it in place
([upgrading from 0.2.2](#upgrading-from-022)), but no rolling upgrade between
two published releases has been qualified yet. Qualifying that walk is a goal,
not a release gate
([#1444](https://github.com/reliaburger/reliaburger/issues/1444)). The pre-1.0 rule for known harness artefacts in
the V02 soak (step 4 of [staging a candidate](#staging-a-candidate))
stays as it is.

Leader-last upgrade ordering does not make an unknown Raft request safe during
elections. Qualify the actual old/new binary pair before advertising it as
supported.

### Versions on main

Once main changes a format after a release, its `Cargo.toml` moves to the next
version with a `-dev` suffix (after 0.1.6's formats changed, main became
`0.2.0-dev` until the 0.2.0 release dropped the suffix; the first format
change after 0.2.0 moves it to the next `-dev`, such as `0.2.1-dev`). A build
from main then can't pass for the release before it: its refusals name the
`-dev` version, its binaries don't collide with the real `bun-v0.2.0` in a
binary store, and `relish setup --quickstart` refuses to download nodes for
it, because no `-dev` release exists and the last published one speaks older
formats. Pass `--development-binaries` (see the
[quickstart](quickstart.md#development-qualification)) to run one.
Drop the suffix in the change that prepares the release, before
`build.yml` builds the candidate, since the build reads the tag from
`Cargo.toml`.

### Upgrading from 0.1.0

0.1.1 can't roll onto a 0.1.0 cluster. The protocol is still 27, but the
state format moved from 44 to 46 (the snapshot layout and the log store's
ingest checkpoint changed), so 0.1.0 → 0.1.1 needs a fresh cluster. Move the
data directories aside (or `relish local destroy --yes` a laptop cluster),
install 0.1.1 and recreate the cluster, then re-apply your apps.

If you try anyway, nothing moves. A 0.1.0 node asks every candidate for
`bun --compatibility` after checking its signatures and before staging it, and
refuses 0.1.1's `{"protocol":27,"state":46}` with a 409. A single node's
`relish upgrade start v0.1.1` fails with that message. In a 0.1.0 or 0.1.1
cluster, `start` records the run, the first node it directs refuses, and the
run pauses with `directive to NODE refused: incompatible binary: …` while
every node keeps running the old release. `relish upgrade abort` ends the
paused run, because no node moved.

From 0.1.2 the leader refuses before it records anything. It fetches the
candidate, checks its signatures and runs `bun --compatibility` on it, and
`start` fails with both format pairs. A 0.1.6 cluster refuses 0.2.0 like this:
`refusing to upgrade to v0.2.0: incompatible binary: found protocol 51, state 68; this cluster (reliaburger v0.1.6 (…)) needs protocol 46, state 63`.
A cluster `relish upgrade rollback vX` is checked the same way: the leader
asks every node which versions its binary store holds (`installed_versions`
in `GET /v1/version`) and refuses a version any node lacks, naming those
nodes, instead of pausing on the first node's
`version vX is not installed in the binary store`.

A laptop cluster refuses the other way. Rerunning setup with a newer
installer finds a saved record that names the older release, so it names
both versions and tells you to run `relish local destroy --yes` (with
`--name` for a cluster not called `laptop`) and set it up again. Before 0.1.2
the message only said the existing cluster's parameters differ.

### Upgrading from 0.1.1

0.1.2 can't roll onto a 0.1.1 cluster either. Pickle's catalogue now records
each image index entry's platform, and image listings carry it, so the
protocol moved from 27 to 28 and the state format from 46 to 47. Recreate the
cluster the same way as above. A 0.1.1 cluster behaves as a 0.1.0 one does
when you try: the first node refuses 0.1.2's `{"protocol":28,"state":47}` and
the run pauses until `relish upgrade abort`.

### Upgrading from 0.1.2

0.1.3 can't roll onto a 0.1.2 cluster. The scheduler now records the apps a
namespace quota keeps unplaced, the volume home a managed-volume app waits
for, and a cluster-wide ordinal for every placement; the cluster-wide views
ask each peer for its own deploy history, events and jobs and stream logs as
`LogFrame` JSON (F07); and gossip, leader hints and Raft carry the recovery
fence's epoch. So the protocol moved from 28 to 33 and the state format from
47 to 49.
Recreate the cluster the same way as [from 0.1.0](#upgrading-from-010): move
the data directories aside (or `relish local destroy --yes` a laptop
cluster), install 0.1.3 and re-apply your apps.

This is the first release where trying anyway is refused up front. 0.1.2's
leader checks the candidate before it records a run (#350), so
`relish upgrade start v0.1.3` on a 0.1.2 cluster fails straight away with
`found protocol 33, state 49; this cluster (reliaburger v0.1.2 (…)) needs protocol 28, state 47`,
and no node moves. There's no paused run to abort.

A council of exactly two voters (a two-node cluster, or a bigger one whose
council hasn't grown past two yet) can't roll an upgrade at all. Each voter
goes down for its swap in turn, and the one left behind is short of the two a
quorum needs. Up to 0.1.2, `start` accepted such a run anyway, and the leader
held it in `UpgradingCouncil` for good: `relish upgrade status` gave no
reason, and `relish upgrade abort` refused because the run wasn't paused. A
run stuck like that carries on once a third node joins and the council
promotes it, since three voters can spare one.

From 0.1.3, `start` and a cluster `relish upgrade rollback` refuse a
two-voter council before recording anything, with
`a council of 2 voters can't upgrade one voter at a time: … Add a node so the council has at least three voters, then start again`.
A run that loses a voter after it started still waits in the council phase,
and the leader now logs `cluster upgrade ID waiting: N of M voters alive` once
when the wait begins.

### Upgrading from 0.1.3

0.1.4 can't roll onto a 0.1.3 cluster. A peer's `/v1/events` answer can now
carry the `token`, `secret` and `identity` audit kinds, which 0.1.3 can't
read, and each node offers its own live events as a feed the others follow.
So the protocol moved from 33 to 34; the state format stays at 49. Recreate
the cluster the same way as [from 0.1.0](#upgrading-from-010): move the data
directories aside (or `relish local destroy --yes` a laptop cluster), install
0.1.4 and re-apply your apps. 0.1.3's leader refuses the upgrade before it
records a run, with
`found protocol 34, state 49; this cluster (reliaburger v0.1.3 (…)) needs protocol 33, state 49`.

### Upgrading from 0.1.4

0.1.5 can't roll onto a 0.1.4 cluster. The leader sweeps expired API tokens
through a new Raft entry, and each peer answers the token list for itself
with scope and last use (F05 I2). The codebase-audit fixes add more to the
council's log and to what nodes exchange: per-blob upload receipts in
Pickle, owned metrics and log archive prefixes, batch attempt identities
and whole-pass admission, run-before migration fences, webhook trigger
receipts, and complete spec fingerprints for `apply --dry-run`. So the
protocol moved from 34 to 40 and the state format from 49 to 58. Recreate
the cluster the same way as [from 0.1.0](#upgrading-from-010): move the data
directories aside (or `relish local destroy --yes` a laptop cluster), install
0.1.5 and re-apply your apps. 0.1.4's leader refuses the upgrade before it records a run, with
`found protocol 40, state 58; this cluster (reliaburger v0.1.4 (…)) needs protocol 34, state 49`.

### Upgrading from 0.1.5

0.1.6 can't roll onto a 0.1.5 cluster. Most of what it adds lives in the
council's state or crosses the wire between nodes:

- apps and jobs store their image bound to a digest
  (`nginx:1.27@sha256:…`), in their specs, deploy history and the
  pull-through cache (F03a);
- a council holds several CAs per role, with a record of every node's leaf,
  and sends the trusted set in joins, signing answers and node snapshots
  (F04 R1, R2);
- rotating an intermediate adds Raft entries for the pending CSR and for each
  node's trust acknowledgement, and a call a node makes to the leader
  (F04 R4);
- a rotated API token keeps its old secret beside the new one for the grace
  period (F05 I3);
- a namespace can have its own secret key, a new field on every namespace
  (F05 I4).

So the protocol moved from 40 to 46 and the state format from 58 to 63.
Recreate the cluster the same way as [from 0.1.0](#upgrading-from-010): move
the data directories aside (or `relish local destroy --yes` a laptop
cluster), install 0.1.6 and re-apply your apps. 0.1.5's leader refuses the
upgrade before it records a run, with
`found protocol 46, state 63; this cluster (reliaburger v0.1.5 (…)) needs protocol 40, state 58`.

Four things behave differently once you're on 0.1.6:

- **Images bind to a digest at apply.** `relish apply` resolves each tag and
  stores `name:tag@sha256:…`, so every node and every restart runs the same
  bytes. Applying again is how you pick up a tag that has moved. An apply
  fails if the registry doesn't answer and the pull-through cache doesn't
  hold the tag
  ([images and volumes](manual/11_images-and-volumes.md)).
- **`deployer` and `read-only` tokens expire after 90 days** unless you pass
  `--ttl-days` or `--no-expiry` to `relish token create`, or change
  `[security.tokens] default_ttl`. Admin tokens still get no default expiry.
  Tokens you re-create after the upgrade pick up the default, so check what
  your CI holds ([security](manual/10_security.md)).
- **Back up the root CA yourself.** Run `relish ca backup` on the node where
  `relish init` ran, as soon as the new cluster is up, and store the file off
  the cluster. `relish ca rotate` asks for it to sign a new intermediate, and
  the root's key never enters the council
  ([security](manual/10_security.md)).
- **Namespace secret keys are opt-in.** Every namespace keeps sharing the
  cluster key until you set `secret_key = true` in its `[namespace.X]`
  section, so values encrypted to the cluster key keep working. Opting in
  re-seals the namespace's apps; re-encrypt the values in your own config
  with `relish secret pubkey --namespace X`, or the next apply puts the
  cluster-sealed ones back.

### Upgrading from 0.1.6

0.2.0 can't roll onto a 0.1.6 cluster. Its jobs live in the council:

- task arrays replicate through Raft, with node ledgers on disk, recovery and
  grant fences, ownership ranges and indexed result pages;
- reusable job definitions, immutable run provenance and schedule
  occurrence claims share one execution store;
- every job names its runtime explicitly instead of having it inferred;
- grant lookahead learns from a recent-duration histogram that travels in
  Raft snapshots.

So the protocol moved from 46 to 51 and the state format from 63 to 68. A
0.1.6 leader refuses `relish upgrade start v0.2.0` before it records a run,
with `found protocol 51, state 68; this cluster (reliaburger v0.1.6 (…)) needs protocol 46, state 63`.
And a sealed council backup from 0.1.6 can't seed a 0.2.0 cluster:
`relish council recover --from` refuses state of another generation. So the
move is a fresh cluster, and nothing the old council held comes across:

- **Secrets.** The new cluster has new keys, so values sealed to the old
  cluster key or a namespace key don't decrypt. Re-encrypt each one with the
  new cluster's `relish secret pubkey` (`--namespace X` for a namespace
  with its own key) before re-applying.
- **API and join tokens.** Create them again with `relish token create` and
  update whatever holds them, such as CI secrets and GitOps webhooks.
- **The CA.** `relish init` makes a new root and intermediates. Clients and
  anything else that trusts the old root must trust the new one, and the new
  root needs its own `relish ca backup`.
- **Job history.** Runs, task arrays, schedules and their results lived in
  Raft. Export anything you need (`relish jobs`, `relish batch results`)
  before you stop the old cluster.
- **Volumes.** Snapshot them with `relish snapshot` and restore them into the
  new cluster; the data directories you move aside still hold them too.

The steps:

1. Export what you want to keep: secrets in your config are already there;
   job results, token holders and volume snapshots are not.
2. Stop bun on every node and move each data directory aside (or run
   `relish local destroy --yes` for a laptop cluster).
3. Install 0.2.0 on every node and create the cluster as you did the first
   time (`relish init`, then join the other nodes).
4. Re-create tokens, re-encrypt secrets to the new keys, restore volume
   snapshots and re-apply your apps and jobs.
5. Back up the new root CA with `relish ca backup`.

`relish upgrade check` from 0.2.0 on reads each release's pair from the
metadata and says when a release needs this instead of a rolling upgrade.

### Upgrading from 0.2.0

0.2.1 can't roll onto a 0.2.0 cluster either. Its fixes change what the
council and the nodes store and send: GitOps ownership and configuration in
Raft, recovery ids on every Raft message, durable registry tags, node-by-node
rollouts, partial placement, gossip replay protection, compacted task results,
new fence files, SHA-256 token hashes and redb 4's file format for the Raft
store (the full list is in `src/compatibility.rs`). So the protocol moved from
51 to 52 and the state format from 68 to 69, and a 0.2.0 leader refuses
`relish upgrade start v0.2.1` before it records a run.

The move is a fresh cluster, as [from 0.1.6](#upgrading-from-016), with the
same steps and the same losses: secrets re-encrypted to the new keys, tokens
created again, a new CA, job history exported first and volumes restored from
snapshots. Tokens have to be reissued anyway, because 0.2.1 stores their
hashes differently.

### Upgrading from 0.2.1

0.2.2 can't roll onto a 0.2.1 cluster either. Gossip, reporting frames and
the Raft log's vote, log ids and encrypted-entry envelope are postcard now,
not bincode. Exec answers carry the command's exit code, jobs carry a
per-command process limit, and faults can name a job's task (the full list is
in `src/compatibility.rs`). So the protocol moved from 52 to 53 and the state
format from 69 to 70, and a 0.2.1 leader refuses `relish upgrade start v0.2.2`
before it records a run.

The move is a fresh cluster, as [from 0.2.0](#upgrading-from-020), with the
same steps and the same losses.

### Upgrading from 0.2.2

0.2.3 keeps 0.2.2's formats: protocol 53 and state format 70. It's the first
release that doesn't bump one, so a 0.2.2 cluster's nodes accept 0.2.3's
binary, and `relish upgrade start v0.2.3` can roll it in place. Nobody has
qualified that published-to-published walk yet
([#1444](https://github.com/reliaburger/reliaburger/issues/1444)). Until a
record says it works, the path we've tested is the one from
[0.2.1](#upgrading-from-021): a fresh cluster.

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

### Nightly signing key

A release train's nightly build ([below](#nightly-qualification-on-the-train-tip))
is unreleased code, so the release key never signs it. It has a key of its
own (maintainer decision, 27 October 2026,
[#1363](https://github.com/reliaburger/reliaburger/issues/1363)):

- The private key is the Actions secret `RELIABURGER_NIGHTLY_KEY`, in the same
  format as the release key's secret. `build.yml` uses it in one step, which
  runs only on a `release-*` branch; the release key's step runs only on
  `main`. A test in `test_build_workflow.py` holds both conditions.
- The public key is `NIGHTLY_SIGNING_KEYS` in `src/upgrade/keys.rs`, next to
  the release keys. Only a binary built with the `nightly-signing` Cargo
  feature trusts it, and that binary trusts no release key. `build.yml`
  turns the feature on for a train's build and never for `main`'s. So a
  released relish refuses a nightly bun or guest image, a released bun never
  upgrades to a nightly, and a nightly cluster never takes a release-signed
  binary for its own.
- `package.py --channel nightly` signs with it. It refuses a key that's in
  the release list, refuses to run at all while `RELIABURGER_RELEASE_KEY` is
  in its environment, and checks the key against the nightly list before
  signing anything, exactly as the release channel does.

The maintainer sets it up once, on a machine with `gh` signed in and a
`relish` binary (any build), keeping the private key off the repository:

```sh
relish dev keygen --out ~/reliaburger-nightly-key
base64 < ~/reliaburger-nightly-key/release.key | gh secret set RELIABURGER_NIGHTLY_KEY
cat ~/reliaburger-nightly-key/release.pub
```

`relish dev keygen` names its files `release.key` and `release.pub` whatever
they're for. The `ed25519:…` line it printed goes into
`NIGHTLY_SIGNING_KEYS`. With an empty list, a train's build fails at signing
with "signing key is not trusted by the binaries it signs". The maintainers
created the key on 27 October 2026, and the 0.2.4 train carries its public
half. Keep an offline backup of the private key
with the release key's. Losing it costs nothing that matters: generate a
new pair, replace the secret and the line, and the next night uses it.

## Metadata and publication

Candidate building and release publication are separate manual operations.
Run:

```sh
gh workflow run build.yml --ref main
```

This builds the native matrix and PDFs at one main commit, and makes sure
that commit passed the whole of source CI. Usually it already has, on the push
to `main`, so the build reuses that run instead of running the suite again
([#1363](https://github.com/reliaburger/reliaburger/issues/1363)): its
`find passing CI` job runs
[`ci_reuse.py`](../scripts/release/ci_reuse.py), which asks the GitHub API
for a `ci.yml` run on exactly this commit, from a push, the merge queue or the
nightly schedule (a pull request's run tests a merge commit and may skip
jobs). The run counts only if it succeeded on its first attempt and every
job in it succeeded, `ignored-test evidence` included. A run that went green
only when someone re-ran it doesn't count: a failure that passes on a re-run
is a flake, retries are 0, and the candidate runs its own suite instead
([#1436](https://github.com/reliaburger/reliaburger/issues/1436)); the script's list of required jobs is kept
equal to `ci.yml`'s own by a test, so a new CI job can't be skipped silently.
Anything else (no such run, a re-run, a failed, cancelled or skipped job,
another commit, an API error) runs the full suite in `validate-release`, as every
build did before 0.2.4. Reusing the run takes the candidate from about 2 hours
to about 50 minutes, the macOS `relish` build's time; most of the hour saved
was spent waiting for hosted macOS runners. The job summary's *Source CI*
line names the run and attempt the candidate rests on.

Only after those checks pass does it collect the two built guest images, sign
all six binaries and both images' digests, and upload `candidate-<commit>-<attempt>` as an Actions artefact.
It creates no Git tag or GitHub release. PR and ordinary main builds never use
the release signing secret, and a manual build of a `release-*` train signs
with the [nightly key](#nightly-signing-key) instead, as a `nightly-*`
artefact.

Download that artefact for qualification. Preserve its run ID, attempt, source
commit and the `candidate.json` SHA-256 alongside the qualification results.
The build prints that digest in the job summary and in the `candidate` job's
log, and uploads `candidate.json` on its own as
`candidate-manifest-<commit>-<attempt>` (kept for one day), so reading the
digest never needs the whole candidate; see
[Staging a candidate](#staging-a-candidate). The record covers every binary, signature, metadata
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
Downloading an Actions artefact alone does not qualify the public quickstart;
the candidate has to be delivered over HTTPS first (see
[Staging a candidate](#staging-a-candidate)).

GitHub documents the [default-branch requirement for manual workflows](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/manually-run-a-workflow)
and the [release asset digest fields](https://docs.github.com/en/rest/releases/releases).

- `metadata.json` selects **Bun** by platform, preserving the existing schema
  and upgrade reader. From 0.2.0 each release entry also carries
  `compatibility`, the `protocol` and `state` pair `package.py` reads from
  `CURRENT` in `src/compatibility.rs`, so `relish upgrade check` and `start`
  can tell a release that needs a fresh cluster from one that can roll.
  `external_signature` is always `null` in published metadata: only the
  operator holds that key, so `relish upgrade start <version>` takes
  `--external-key` (or a countersigned `--sig`) and adds it.
- `cli-metadata.json` uses the same schema to select **Relish**. Keeping the
  documents separate prevents an older agent from interpreting a CLI as an
  upgrade candidate.
- URLs inside each document point to that exact version's GitHub release.
- Bun's default metadata URL is
  `https://github.com/reliaburger/reliaburger/releases/latest/download/metadata.json`.

The website and installer are separate static assets under `docs/website`,
published by `static.yml`. GitHub Pages cannot select a different response for
curl and a browser at `/`; the shell endpoint is `/install.sh`. The bootstrap
installs the version in its `RELIABURGER_VERSION` default (`v0.2.3` today), so
bump that default in the same change that announces a newer release. Both
scripts need a mode, `--laptop` or `--install-only`; with none they explain the
choices and exit 2. For a release from before 0.2.2 the bootstrap drops
`--laptop`, which that release's installer doesn't know.

Before tagging a release, complete the managed-cluster and clean-install
gates. Record timing from an empty cache, the actual artefact digests, host and
guest versions, memory use, and the successful sample workload. Don't publish a
five-minute claim from a source build or a warmed VM.

## The release gate

0.2.3's gate took most of a week from code freeze to ready-to-tag, and only
about two days of that was the gate itself. The rest had three causes: every
benchmark, soak and day run needs a quiet host, so they queued one after
another on one Mac; every failure meant a new candidate, about a day each
time; and the performance records blocked the tag. From 0.2.4 the gate changes
in three ways, besides the candidate build's reuse of a passing CI run
([above](#metadata-and-publication)) (maintainer decision, 26 October 2026,
[#1363](https://github.com/reliaburger/reliaburger/issues/1363)).

### Nightly qualification on the train tip

Every night the quiet host runs the long tiers on the current tip of the
release's train, one after another: the power-cut and reboot runs, the V02
and jobs fast soaks and, for a release that needs the full gate (below), the
job final. A failure then turns up mid-week, on the train, instead of after
the freeze on a candidate.

```sh
scripts/release/nightly-qualify.sh --train release-0-2-4 \
  --base-url https://github.com/reliaburger/reliaburger/releases/download/nightly-v0.2.4-RUN_ID-ATTEMPT \
  --qualified-digest ITS_DIGEST [--soak-bun /path/to/bun-v0.2.4-soak.1]
```

It reads the tip with `git ls-remote`, fetches it, works out the gate the
release needs ([below](#a-gate-scaled-to-the-release)), and runs every tier
from a
detached checkout of that commit, so the tip's own harness runs. Power-cut
and reboot runs build the commit inside their disposable VM; the soaks
install a staged build through `curl | sh`, and the script refuses one whose
`candidate.json` doesn't hash to the digest or names other code. Without a
staged build only the power-cut runs happen and the night isn't green. Every
fast tier runs even after another one fails, so one night shows every
problem; the job final is skipped after any failure. It refuses to start while
`rustc` or `cargo` run, records the host's load around each tier, and writes
`~/reliaburger-nightly/DATE-TRAIN-SHORTSHA/` (`--records` moves it): each
tier's own record, `nightly.json` and a README naming the exact commit and
the verdict. `--plan` (or `--dry-run`) resolves the tip and prints the tiers,
touching nothing.

The staged build is the train's **nightly build**: the tip built by
`build.yml` and signed with the [nightly key](#nightly-signing-key), never the
release key, because a release-signed build of unreleased code is as
dangerous as the soak build [below](#soaking-a-candidate-in-ci) (maintainer
decision, 27 October 2026). Each day, after the last merge to the train:

```sh
gh workflow run build.yml --ref release-0-2-4
# when it finishes, from its summary or log (as in step 1 of staging):
gh workflow run stage.yml --ref main \
  -f channel=nightly -f candidate_run=RUN_ID -f qualified_digest=ITS_DIGEST
```

On a `release-*` branch the build compiles every binary with the
`nightly-signing` feature, so they trust the nightly key and no release key,
signs with `RELIABURGER_NIGHTLY_KEY`, writes `"channel": "nightly"` into
`candidate.json` and uploads it as `nightly-<commit>-<attempt>` (kept for 7
days) instead of `candidate-…`. It reuses the train's CI run exactly as a
candidate build reuses main's. `stage.yml` with `channel=nightly` accepts only
a successful manual build of a `release-X-Y-Z` branch whose record says
nightly, and publishes it as the pre-release `nightly-v0.2.4-RUN_ID-ATTEMPT`,
never marked latest. Nothing can mistake it for a release:

- promotion refuses any tag containing `nightly`, and `candidate.py`
  refuses a nightly record, run or artefact for a release;
- `docs/website/install.sh` refuses a mirror whose last path segment starts
  with `nightly-` unless `RELIABURGER_ACCEPT_NIGHTLY=1`, and
  `nightly-qualify.sh` sets that for its soaks and nothing else does;
- even with that set by hand, a released `relish` or `bun` refuses the
  nightly's signatures, because it doesn't trust the nightly key.

Before any tier, `nightly-qualify.sh` checks the staged build's
`candidate.json` against the digest and the tip, and checks its guest images'
signatures against the key its channel names, so a nightly signed with
anything but the nightly key stops the night before a soak starts. Relish then
checks every bun and guest image it installs against the nightly key compiled
into it. For the full gate's job final, the soak bun is built from the tip
with `--features ebpf,nightly-signing` and signed with the nightly key
(`relish dev sign-binary --key ~/reliaburger-nightly-key/release.key bun-v0.2.4-soak.1`),
because a nightly cluster upgrades only to binaries the nightly key signed.

**Cleanup.** Every nightly staging run ends by deleting all but the newest
seven `nightly-*` pre-releases, with their tags (`candidate.py
prune-nightly --keep 7`). It never touches a release, a draft release of a
version or a `staging-*` candidate. A night that has no staged build of its
tip is red, and the freeze falls back to running the soaks on the
candidate (step 4).

[`nightly-qualify.plist.example`](../scripts/release/nightly-qualify.plist.example)
is a launchd job that starts it at 18:00; the patch gate's tiers take about
3 hours and the full gate's about 14. Nothing installs it: the Mac Studio that
becomes the quiet host arrives in the week of 2 November 2026.

A failed night is triaged that day, like a failed CI run: a product bug gets
an issue and a fix on the train, and anything that passes on a re-run goes in
the [flakes register](flakes.md).

### A gate scaled to the release

Every release runs the **patch gate**: the staged installs (step 3), the V02
and jobs fast tiers (step 4) and the power-cut and reboot runs (step 5). The
**full gate** adds the job final, `qualify-jobs-sustained.sh --tier final`,
8 hours of faults and the quiet tail. A release needs the full gate when it's

- a minor or major release (`vX.Y.0`);
- a release whose `CURRENT` in `src/compatibility.rs` differs from the
  previous release's, so durable state or the wire changed; or
- a release that touches the agent loop (`src/bun/agent.rs` or
  `src/bun/agent/`), whose turn budget and ordering the final's eight hours
  of faults exercise.

[`release_tier.py`](../scripts/release/release_tier.py) applies the rule to
a tag range and prints `patch` or `full` with its reasons:

```sh
scripts/release/release_tier.py --previous v0.2.3 --release v0.2.4 --commit origin/release-0-2-4
```

`nightly-qualify.sh` and `nightly.py freeze-check` apply the same rule
themselves, so neither takes the gate on trust: the release is the version in
the commit's `Cargo.toml`, the previous release the nearest `v*` tag behind
it (`--previous` names another), and the run prints the reasons. `--gate`
can only raise the gate: asking for the patch gate on a release that needs
the full one is refused, so a change that tips a patch release into the full
gate moves its nightlies and its freeze check with it
([#1428](https://github.com/reliaburger/reliaburger/issues/1428)). 0.2.4
needs the full gate: it moves `CURRENT` and the agent loop.

### Code freeze

At freeze the candidate is built on `main` from the gate PR's merge. If the
latest nightly that's green for the release's gate ran the candidate's code,
the soaks and power-cut runs don't run again:

```sh
git fetch origin main
scripts/release/nightly.py freeze-check --commit CANDIDATE_SHA
```

Same code means the same commit, or a commit with the same tree: the gate
PR's merge has a new commit, but with nothing else on `main` its tree is the
train tip's. `freeze-check` fails if the latest green nightly ran other code,
if no nightly is green for that gate, or if a later night on the same code was
red: triage that night, and a newer green night on the code clears it. When
it passes, the gate is the staged
installs (step 3) on the candidate, and the matching nightly's directory is
copied into `docs/qualification/` as the soak and power-cut records. When it
fails, run steps 4 and 5 on the candidate as before.

### Performance records after the tag

Benchmarks, the profile (#1358) and the day run (#1032) don't decide whether a
release works, and they need the quiet host for hours. They're published
after the tag ([step 9](#staging-a-candidate)) instead of blocking it, and a
regression they find goes into the next patch release.

## Staging a candidate

Before promoting a release, publish the exact signed candidate to HTTPS and run
the real `curl … | sh` install against it on every host we advertise.

1. **Build the candidate on main.**

   ```sh
   gh workflow run build.yml --ref main
   ```

   When it finishes, note three things: the run ID (from the URL), the
   candidate commit and the *Qualification manifest SHA-256*. That digest is
   `QUALIFIED_DIGEST` from here on. The run's summary shows all three, and
   from a terminal the log has the digest:

   ```sh
   gh run view RUN_ID --log | grep 'Qualification manifest SHA-256'
   ```

   Or fetch just the manifest (a few kilobytes, not the ~2 GB candidate) and
   hash it; within a day of the build, while its artefact lasts:

   ```sh
   gh run download RUN_ID -n candidate-manifest-COMMIT-ATTEMPT -D manifest
   shasum -a 256 manifest/candidate.json
   ```

   All three come from the build run itself, so they agree. Keep the digest
   with the qualification records; never copy it from a later download or a
   different run. None of this replaces verification: staging and promotion
   still download the full candidate and check every byte against the digest.

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

3. **Qualify it on every host in the matrix**: macOS on Apple silicon,
   Linux x86_64 and Linux arm64. From a checkout of the same `main`:

   ```sh
   scripts/release/qualify-staged-install.sh \
     --base-url https://github.com/reliaburger/reliaburger/releases/download/staging-v0.1.0-RUN_ID-ATTEMPT \
     --qualified-digest QUALIFIED_DIGEST
   ```

   It creates a fresh, short `RELIABURGER_HOME` under `/tmp` and runs
   `curl -fsSL https://reliaburger.com/install.sh | RELIABURGER_RELEASE_BASE_URL=… sh -s -- --laptop --timings`,
   exactly as a user would. Use `--bootstrap docs/website/install.sh` if the
   website doesn't serve the current bootstrap yet (the record notes which one
   ran). It checks `candidate.json` against the digest, `SHA256SUMS` against
   `candidate.json`, that setup used the mirror, and that the installed CLI,
   both Linux binaries and the guest image are candidate assets. Then it
   applies the podinfo tour, runs `status`, `path` and `metrics`, requires
   `relish wtf` to check GitOps on every node, not leave it UNKNOWN (#1286), builds and
   applies the burger demo and requires a whole burger through the ingress, destroys the
   cluster, uninstalls, and writes a Markdown record with the timings. It never
   uses `~/.reliaburger` and fails if that directory's top level or
   `~/.local/bin/relish` changed. Pass `setup --quickstart` options after
   `--`, for example `-- --api-port 29117 --ingress-port 28080 --registry-port 25050`
   when another cluster holds the default ports (the registry forward, 15050,
   clashes too), and `--keep` to leave a failed run for
   debugging (then `relish local destroy --yes` and `relish uninstall --yes`
   with the same `RELIABURGER_HOME`).

   Run it at least twice per host for V04: once with nothing cached (the
   script always starts empty) and again for repeatability.

4. **Soak it (V02)** on Apple silicon, in two tiers
   ([plan](plans/2026-09-25-v02-sustained.md), D3). When
   [`freeze-check`](#code-freeze) passes, the matching nightly has already
   run this step's tiers on the candidate's code: copy its records and go to
   step 6. A patch release runs the fast tiers; the job final is for the
   [full gate](#a-gate-scaled-to-the-release). Build and sign the soak
   bun first (D2). After each round of fixes, run the fast tier: every fault
   kind and every special on the compressed schedule, in about an hour.

   ```sh
   scripts/release/qualify-sustained.sh --tier fast \
     --base-url https://github.com/reliaburger/reliaburger/releases/download/staging-v0.1.0-RUN_ID-ATTEMPT \
     --qualified-digest QUALIFIED_DIGEST --soak-bun /path/to/bun-v0.1.0-soak.1
   ```

   The final tier is 8 hours of faults on the full schedule, then a quiet
   tail of 2 hours 15 minutes (about 10½ hours in all), which catches slow
   accumulation. The
   release gate runs it once, as the job final below
   (`qualify-jobs-sustained.sh --tier final`): that's the same soak with the
   job campaign and a node fault added to each cycle, so it checks
   everything the V02 final does, plus the jobs. All the final soaks fit in
   one slot of about 10 hours that way (maintainer decision, 17 October 2026,
   [#991](https://github.com/reliaburger/reliaburger/issues/991)).
   `qualify-sustained.sh --tier final` still runs the V02 final on its own,
   for example to separate a job problem from a cluster one, but it isn't a
   release step.

   The record's second paragraph states the verdict. A fast run can only say
   "fast tier: clean", and a run without a tier is a diagnostic that says
   nothing about either gate. Only a clean, complete final-tier run, 8 hours
   of faults on the full schedule and its quiet tail, says a gate passes: "the V02 soak gate passes" for
   `qualify-sustained.sh --tier final`, "the release gate's final soak
   passes" only for the job final, whose job controller ran the target load
   and whose node faults all completed. Complete means the record's execution
   inventory has nothing missing
   ([#1061](https://github.com/reliaburger/reliaburger/issues/1061)): every
   upgrade walk the schedule owes (to the soak build, back and settled), every
   fault kind and node fault injected and settled, every special (quorum loss
   with quorum back), every catalogue pulse and offline export, the app
   workloads settled at the baseline, the quiet tail with nothing injected,
   walked or stopped in it, the job campaign drained after it, and the `candidate.json` bytes this run downloaded hashing to
   `--qualified-digest`. The checker counts these from the run's events and
   kept bytes against the 8-hour full schedule, whatever the metadata says, so
   relabelling a shorter run can't promote it. A final tier refuses to start
   without `--soak-bun` or `--qualified-digest`, or with `--home`, since it
   could only end incomplete. Fast and custom runs show their inventory too,
   recorded but not gated.

   Every fault and node choice (which node, how long, after how many
   seconds) comes from one seed, and each is logged as a `choice` event
   ([#1062](https://github.com/reliaburger/reliaburger/issues/1062)). The
   record names the seed; `--seed N` repeats a run's choices in the same
   order, and `--resume` carries on with the run's own seed. The record also
   lists what each fault was seen to do (Bun's pid before and after a kill,
   Lima's status after a power-off, the netem qdisc, free bytes or I/O limits
   a node fault left), and how long each leak checker (`leak-rss`,
   `leak-fd`) could judge each node: a restart, kill or upgrade exec starts
   its warm-up again, and the fault schedule restarts every node's Bun more
   often than `leak-rss`'s hour-long warm-up. So a final ends with a quiet
   tail: 2 hours 15 minutes after the last cycle with no faults, walks or
   specials, while the apps and jobs keep running and the checks carry on.
   That's an hour to arm `leak-rss`, an hour of comparison samples, and 15
   minutes for the sampling: the last cycle's graceful stop comes after its
   heavy sample, so a new Bun is first seen up to five minutes into the tail,
   and each heavy observe delays the next by its own run time. A final fails
   unless both checkers were armed for at least 60 minutes on every node,
   with one unbroken window of at least 30 (maintainer decision, 20 October
   2026, [#1062](https://github.com/reliaburger/reliaburger/issues/1062)).
   Fast and custom runs record their exposure without gating on it. A product fix
   found during the final run means a new candidate, a fresh fast run and
   then a fresh final run. `--resume --evidence DIR` continues an interrupted
   run with its original tier. The [0.1.0 release closure
   record](qualification/2026-09-27-v0.1.0-release-closure.md) shows what these
   runs found for 0.1.0, candidate by candidate, and the PR that fixed each
   failure.

   The [job-soak extension plan](plans/archive/2026-10-10-plan-release-job-soak.md)
   runs fresh-container, shared-container and native host work alongside these
   apps and faults. Accounting, independent effects, heartbeat/progress, exact
   owner inventory, fault coverage and positive cleanup affect the verdict.
   A six-run campaign uses a 2/3/3 per-node command budget, with separate finite
   cron/hooks/publication fixtures. It opts the disposable nodes into mixed
   runtime and allowlists a host BusyBox copy from the pinned container image.
   The verifier binds each private VM address on TCP 8189 with a separate token.
   Drain begins once the last cycle, its special and any quiet tail are over
   (#1338); tier and recovery budgets stay unchanged. Missing support or incomplete coverage fails instead of skipping
   a mode. A local smoke check is not staged fast/final acceptance, and global
   storage bounds remain #668.

   **The 0.2.0 job gate (decision D18-C).** A release that claims sustained
   job throughput also needs the job qualification: #668's list on at least
   three nodes, at 1,158 accepted successes a second or more (the rate of
   100 million a day) plus headroom, with concurrent apps, several resource
   profiles, worker and leader loss, delayed messages and disk faults. The
   final tier runs 8 hours of faults and a 2¼-hour quiet tail on the 3-node
   quickstart cluster, with every fault
   and at the default 1,390 a second, and it is the release gate's only final
   soak ([#991](https://github.com/reliaburger/reliaburger/issues/991),
   above). `qualify-jobs-sustained.sh` runs it through the same soak,
   with the job campaign at the target load and a node-level fault in every
   cycle (inter-node delay, a full disk, slow fsync). Faults stay node-level in
   0.2.0 (D19-B): nothing targets an individual job. While slow fsync caps a
   node's writes at 20 IOPS, that node's agent-loop turns over the 1 s budget
   are recorded as info, not failures: a turn that journals there is slow by
   construction (#972). Every other node's turns, and that node's once the
   window closes, still fail the tier.
   The soak VMs use Lima's default gRPC port forwarder, the one quickstart
   users run; the harness unsets `LIMA_SSH_PORT_FORWARDER` so a caller's
   shell can't switch it. 0.2.0's soaks forwarded over SSH instead, because
   Lima 2.1's gRPC forwarder leaked a guest-agent socket per forwarded
   connection ([lima-vm/lima#5210](https://github.com/lima-vm/lima/issues/5210))
   and at the job tier's API rate the API node's guest agent grew to 1 GiB in
   six hours, until the kernel killed Bun
   ([#982](https://github.com/reliaburger/reliaburger/issues/982)). The
   quickstart pins Lima 2.2.1, which fixes the leak
   ([#983](https://github.com/reliaburger/reliaburger/issues/983)).

   ```sh
   # About an hour, compressed: every fault kind, every node, every special.
   scripts/release/qualify-jobs-sustained.sh --tier fast \
     --base-url https://github.com/reliaburger/reliaburger/releases/download/staging-v0.2.0-RUN_ID-ATTEMPT \
     --qualified-digest QUALIFIED_DIGEST
   # 8 hours of faults on the full schedule, then a 2¼-hour quiet tail, on the
   # final candidate: the one final soak.
   scripts/release/qualify-jobs-sustained.sh --tier final \
     --base-url https://github.com/reliaburger/reliaburger/releases/download/staging-v0.2.0-RUN_ID-ATTEMPT \
     --qualified-digest QUALIFIED_DIGEST --soak-bun /path/to/bun-v0.2.0-soak.1 \
     --record docs/qualification/DATE-job-qualification.md
   ```

   0.2.3 adds a day tier for its headline record
   ([#1032](https://github.com/reliaburger/reliaburger/issues/1032)), outside
   the gate. From 0.2.4 it runs after the tag, with the other performance
   records (step 9, [#1363](https://github.com/reliaburger/reliaburger/issues/1363));
   the command is here because it shares this step's flags: 24 hours of host jobs with no faults, walks or specials, then the
   drain. It needs the bench host to itself for the whole day and about half
   an hour more for setup, the baseline and the drain. Before configuring the
   nodes it resizes each quickstart VM, one at a time, from 2 vCPU / 2 GiB to
   4 vCPU / 6 GiB, the size the 0.2.3 scale-out record measured. Host jobs
   run that record's binary too, Ubuntu's `busybox-static`, downloaded from
   Ubuntu's archive and checked against pinned SHA-256s, instead of the
   image's BusyBox, which costs twice as much per process. The record lists
   both as deviations.

   ```sh
   scripts/release/qualify-jobs-sustained.sh --tier day \
     --base-url https://github.com/reliaburger/reliaburger/releases/download/staging-v0.2.3-RUN_ID-ATTEMPT \
     --qualified-digest QUALIFIED_DIGEST \
     --record docs/qualification/DATE-day-run.md
   ```

   Its `--target-rate` is host-job successes a second: 23,149 (2 billion a
   day) by default, and no lower than 11,575 (a billion). The record fails if
   host jobs average less, if a node takes more than 20% over its fair share,
   if the last full hour runs more than 10% slower than the first, or on any
   soak check; it lists every node's total and every hour's rate.

   `--target-rate` raises the rate above the default 1,390 a second (the
   floor plus 20%); `--plan` prints the schedule without touching anything,
   including the job controller's `job-runs` line: how many runs it expects,
   the most it holds at once and its limit of 256. The controller holds a run
   from admission until an hour after it drains (the candidate's retention),
   and a soak whose load wouldn't fit refuses to start
   ([#989](https://github.com/reliaburger/reliaburger/issues/989)).
   The record fails if any job runtime misses a fault kind, loses or
   double-counts an accepted outcome, or averages less than the target rate.
   Beside it, `DATE-job-qualification.jobs.json` holds the accepted rates,
   per-runtime retry causes and the September plan's measurements: wall time
   of a 1,048,576-task array, p99 start lag, Raft entries a second and run
   state, Bun memory and CPU, executor overhead, task-array storage per
   million tasks and retained logs. Those are recorded in every run and never
   gate it. A soak compares three of them with restated targets
   (maintainer decision, 15 October 2026,
   [#668](https://github.com/reliaburger/reliaburger/issues/668)), since the
   plan's were for one batch on a quiet cluster: task-array storage at most
   64 MiB per million accepted tasks; each Bun process's steady-state RSS
   (the median of its samples once it has been seen for 10 minutes, a process
   being its boot, pid and image) at most 512 MiB on node 1, which serves the
   API, and 384 MiB on the others, with the peak beside it; and Raft entries a
   second for all cluster writes, recorded without a target because the batch
   target counts only task-array writes. The gated memory checks are
   unchanged. Commit both files. Bounded job storage
   ([#678](https://github.com/reliaburger/reliaburger/issues/678)) has to
   pass before the final run: until it does, a full disk stops a worker
   taking grants and fails the run.

   After the tag ([step 9](#staging-a-candidate)), re-record the published
   throughput table on the release with `relish bench --scenario … --report FILE` on the same rig, and
   compare it with the committed reports
   (`relish bench --scenario jobs-host-processes --compare docs/qualification/…/r1-jobs-host-processes.json`);
   `--compare` fails on a drop larger than `--tolerance` (10% by default).

   Either tier also fails on a slow agent loop. Every settle and heavy check
   reads each node's `bun_agent_loop_turn_seconds` histogram from
   `/v1/metrics`, and the checker fails the tier (`agent-loop-turn`) on any
   turn longer than 1 s since the last check (a Bun restart counts the new
   process from zero, and never counts the old one's turns twice), naming
   the node, the branch
   (`command`, `deploy_op`, `health_tick`, …) and how long the worst one
   took. A turn that long holds every caller on the node, the way the stalls
   behind 0.1.0's soak failures did; the budget comes from the [agent-loop
   review](plans/2026-09-30-agent-loop-review.md#decision) (#351). The
   record's Data section lists the turns over budget per node. A failure
   here is a product bug: Bun's log names the turn's command or deploy op
   (`agent loop turn took … ms in …`), and the fix moves that work off the
   loop.

   Until 1.0, a final-tier run whose only failures are known harness
   artefacts counts as passed (maintainer decision, 28 September 2026), if
   each failure matches a documented artefact signature with evidence, the
   run's snapshots are re-checked by replaying the fixed
   `scripts/release/sustained_check.py`, nothing lost data or failed in the
   product, and the record names each failure classified this way and why.
   The known artefacts are `leak-rss` after a self-upgrade exec reused the pid
   (fixed by #271: same pid, a sharp RSS drop at an upgrade walk just before
   the warm sample, fds bounded) and a log-order swap when an instance moved
   nodes under the same name (fixed by #272: one adjacent swap in an
   unlabelled tail, the instance on different nodes in the status snapshots
   either side). For 0.2.0 only, a node cut off from the host by Lima's SSH
   port forwarder was a third (gone with the forwarder, fixed by
   [#983](https://github.com/reliaburger/reliaburger/issues/983)). The job
   controller running out of its 256-run budget in 0.2.0's 12-hour job final
   was a fourth for 0.2.0 only; it's fixed by
   [#989](https://github.com/reliaburger/reliaburger/issues/989) and excuses
   nothing after it. The [plan](plans/2026-09-25-v02-sustained.md#4-pass-and-fail)
   keeps the list. Anything else still needs a new candidate, and from 1.0 on
   only a clean final-tier run passes.

5. **Pull the plug.** Killing a process isn't a power cut: page-cache writes
   survive one and not the other. Run the power-cut and reboot qualifications
   ([what they prove](testing.md#pulling-the-plug)) on the candidate's commit,
   from the host. Every release runs them; a passing
   [`freeze-check`](#code-freeze) means the matching nightly already has:

   ```sh
   scripts/release/qualify-power-cut-reboot.sh --commit CANDIDATE_SHA --iterations 10 \
     --record-dir docs/qualification/DATE-power-cut-reboot
   ```

   Run it from an up-to-date checkout; the lanes themselves run from the
   candidate's. It makes a clean detached checkout of the commit, creates a disposable
   Lima VM `rb-pcut-<short sha>` from
   [`power-cut-vm.yaml`](../scripts/release/power-cut-vm.yaml) (4 vCPUs,
   6 GiB of memory, 6 GiB of swap so the linker isn't killed, and only that
   checkout mounted, read-only), and checks all of that from inside the guest
   before any lane runs. Then it runs the three storage fixtures
   (`qualify-storage-power-cut.sh --fixture exporter|leases|backups`) and
   the OCI and discovery reboot lanes (`qualify-oci-reboot.sh`,
   `qualify-discovery-reboot.sh`) `N` times each, in that order, and stops at
   the first failure. It writes each lane's record into the record directory
   and a summary with each lane's runs and rule-of-three bound next to it, in
   `docs/qualification/DATE-power-cut-reboot.md`, the shape of the
   [16 October record](qualification/2026-10-16-power-cut-reboot.md). Fill in
   anything the summary can't know (other load on the host, why this many
   runs) before committing it. Afterwards it deletes the VM and the checkout;
   `--keep` keeps both, and so does a failure, so the guest evidence survives.
   It never touches the development VM: it refuses `--vm reliaburger-test`,
   a VM that already exists and a dirty or wrong-commit `--worktree`. Ten
   runs per lane take about half an hour on an M-series Mac, plus the first
   boot's provisioning.

   Every iteration must pass. A row marked `PASS (VM start retried)` is the
   known Lima start failure straight after a forced stop, not a product
   failure; any other failure is a release blocker. Ten clean runs only rule out failure rates above 30%, so this catches a
   regression, not a rare race; a release that changes storage, locking or
   ownership (as #613 did) runs 25.

6. **Record it.** Copy each host's record into
   `docs/qualification/DATE-staged-install-HOST.md`, alongside the run ID,
   attempt, commit and digest, and link them from the release's milestone or
   gate issue (V04 is [#288](https://github.com/reliaburger/reliaburger/issues/288)).
   When `freeze-check` passed, copy the matching nightly's directory too and
   name its commit in the release closure record.

7. **Clean up the staging pre-releases.** Delete them before promotion so the
   release page and the release notes' "previous tag" don't pick them up:

   ```sh
   gh release delete staging-v0.1.0-RUN_ID-ATTEMPT --cleanup-tag --yes
   ```

8. **Promote** as described above, with the same run ID and digest. The
   staging tag can't be promoted: it doesn't match `v1.2.3`, `candidate.py`
   refuses it as a version, and `promote.yml` refuses any tag containing
   `staging`.

9. **Publish the performance records**, after the tag
   ([why](#performance-records-after-the-tag)). They don't block the release;
   a regression they find is a bug for the next patch release. Run them on the
   quiet host, one at a time, never beside a nightly.

   **The profile**, from 0.2.4 on (maintainer decision, 26 October 2026,
   [#1358](https://github.com/reliaburger/reliaburger/issues/1358)). The
   soaks check that things work and don't leak; nothing in them checks
   whether a release got slower, or why. The [0.2.3 host-job
   profile](qualification/2026-10-20-host-job-profile/README.md) found a
   GitOps loop burning a core and grants running in lockstep that no test or
   soak had noticed. This step runs that profile on every release and
   compares it with the previous one. The staging pre-release is gone by now,
   so point it at the published release:

   ```sh
   scripts/release/qualify-profile.sh \
     --candidate https://github.com/reliaburger/reliaburger/releases/download/v0.2.4 \
     --qualified-digest QUALIFIED_DIGEST
   ```

   It creates a disposable Lima VM `rb-profile-<random>` with the profile
   record's shape (VZ, 4 vCPUs, 8 GiB, 60 GiB, `template:ubuntu-24.04`),
   and installs two versions in it: the candidate, and the previous release
   (the patch number one lower; `--previous vX.Y.Z` names another). Every
   binary is checked against its `candidate.json`. Rates come from those
   release binaries, unchanged. Attribution comes from each version's
   profiling build: `build.yml` builds `bun` and `relish` for Linux arm64
   again with frame pointers and line tables, as the
   `profiling-build-COMMIT-ATTEMPT` artefact. It's never shipped: the
   candidate job, staging and promotion don't collect it. A version with no
   profiling build (0.2.3 and older) is profiled from its stripped release
   build, which names the kernel, helper and task frames but none of Bun's.

   The scenarios are fixed in the script: raw VM processes (the rig's
   calibration), host jobs at the default cap, shared containers, and an idle
   cluster for 60 s. Fresh containers (`--scenarios …,fresh`) belong to
   the nightly run and the control plane (`control`: `relish bench --quick`
   and API round trips) comes later, so neither runs by default. Rounds
   alternate, A/B then B/A, each on a fresh boot of the VM, three pairs of
   60 s windows. Then one profiling session per version records `perf` on
   host jobs and on the idle cluster. The rig changes between releases (the
   0.2.0 record ran on an M2 Max, the profile on an M1 Max), so the
   comparison never uses a number measured on another day: it measures the
   previous release beside the candidate. The script prints its estimate up
   front, about 70 minutes; if a session would take it past 2 hours it drops
   rounds, never scenarios, and the record says so.

   It needs a quiet host: never run it beside a soak or a build. It refuses
   to start while `rustc` or `cargo` run, and records the host's load and
   compilers every 10 s. It doesn't throw a round away for load, because on a
   Mac every round would go; the alternation takes the noise instead.
   `--plan` prints the sessions and the estimate without touching anything,
   and `--smoke` runs one round of 30 s windows into a scratch directory, to
   check the step itself works.

   The record is `docs/qualification/DATE-vX.Y.Z-profile/`: a generated
   README with each version's rates (the median and range of three rounds,
   plus the steady rate between 15 and 58 s, because accepted counts move
   in whole grant cycles), CPU busy, Bun's
   CPU, start lag, the idle cluster's CPU, wake-ups and RSS, category tables
   from [`profile_categories.py`](../scripts/release/profiling/profile_categories.py),
   flamegraphs (on-CPU, off-CPU and a differential one), `perf sched
   latency`, the grant-cycle probe and the host's load. Its "Compared with
   vPREV" section, from
   [`compare_profile.py`](../scripts/release/profiling/compare_profile.py),
   shows the ratio of medians with a 95% bootstrap interval, each rate
   relative to the same version's raw rate, and category shifts in points.
   Its rules are evidence, not a gate yet: each reports what it *would
   flag*:

   | Would fail | Explain |
   | --- | --- |
   | A scenario more than 10% slower, the interval excluding no change | A category moving more than 5 points |
   | Bun above 2% of one core in the idle cluster | A scenario more than 10% faster: why, and is it a measuring error? |
   | A Bun category above 5% of Bun's CPU that was under 1% | |

   More than 5% of Bun's samples uncategorised asks for new rules in the
   category map instead. Before committing the record, write its answer
   (where the time goes in this release, and what changed) and a sentence
   for every flagged line. A later release turns the rules into a gate, once
   a few records show how noisy the rig is.

   **The benchmarks.** Re-record the published throughput table on the
   release (`relish bench --compare`, [step 4](#staging-a-candidate)), and
   link the release commit's `task-array benchmarks` run from the gate
   issue. That job runs nightly on `main` and fails only on a regression of
   more than 2x against main's last baseline, allowing for the runner's own
   speed ([how](testing.md#benchmarks-in-ci)); its `criterion-task-arrays`
   artefact is the cost evidence for bounded-cost claims such as 0.2.0's
   (#668).

   **The day run**, when the release makes a sustained-rate claim (0.2.3's
   two billion a day, [#1032](https://github.com/reliaburger/reliaburger/issues/1032)):
   24 hours of host jobs on the bench host to itself.

   Commit the records, link them from the release closure record and the
   release log, and open an issue for anything that regressed, on the next
   patch release's milestone.

10. **Update the docs** once the release is public: the release date in the
    [roadmap](roadmap.md) (tick the release) and the homepage
    (`docs/website/index.html`), and the bootstrap's default version if it
    changed. Grep for the previous version to find the other "current
    release" lines (the READMEs, the quickstart, the tour chapter, the
    `VERSION=` lines in [linux-servers.md](linux-servers.md)), and leave
    history alone. Update the five-minute tour to show off the release's new
    features (the homepage's `data-tour` commands, the manual's
    `08_five-minute-tour.md` and `scripts/demo/tour.sh` together), then
    re-record `docs/website/assets/tour.cast` against the published install
    with `scripts/demo/tour.sh --record … --install vX.Y.Z`
    ([how](website/README.md#re-recording-the-tour)), and move the poster time
    in `index.html` if its moment moved. The recording's title and the caption
    under it name the installed version, and `tests/suite/website.rs` fails
    until they match the bootstrap's default. Regenerate the released-CLI
    snapshot with `scripts/release/released_cli.py vX.Y.Z >
    scripts/release/released-cli.tsv` and run `make readme-commands`: commands
    labelled "X.Y.Z, not in" the old release lose their labels, and the same test
    flags any label the docs no longer need
    ([how](website/README.md#commands-the-installed-release-doesnt-have)). The
    website deploys from `main`, and publishes `/demo/*` from the tag the
    bootstrap installs, so merge that change after promotion, not before.

    Refresh the homepage's
    [How a release is tested](https://reliaburger.com/#release-testing)
    section (`#release-testing` in `index.html`) to the new release: its
    latest-release paragraph, its headline numbers with links to this
    release's records, and the soak charts beside them. The charts are copies
    of the app soak's (the V02 fast tier, now that the gate has no V02 final)
    and the job soak's `-charts/` directories (the final's, or the fast
    tier's for a patch release, which runs no final) in
    `docs/website/assets/soak/`, which holds only this release's
    ([how](website/README.md#soak-charts)). `tests/suite/website.rs`
    fails while the section names a version older than the installer's
    default, the release log's newest entry isn't this release, or a chart
    isn't a byte-for-byte copy of this release's record.

    Add the release's paragraph to the top of the
    [release log](release-log.md): the promotion date, how long qualifying it
    took and how many candidates, what each soak found and what fixed it, and
    any known issue it ships with, linking the records from step 6. When the
    profile from step 9 is in, add one line from its answer.

## Soaking a candidate in CI

The V02 sustained soak ([plan](plans/2026-09-25-v02-sustained.md)) also runs
on a hosted Linux runner. Once a candidate is staged:

```sh
gh workflow run soak.yml --ref main \
  -f staging_tag=staging-v0.1.0-RUN_ID-ATTEMPT \
  -f qualified_digest=QUALIFIED_DIGEST \
  -f duration=60m -f schedule=compressed
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
  runs of about four hours. The final tier (about 10½ hours), and the 12-hour lane A run,
  still need a long-running host or a self-hosted runner.
- **No upgrade walks, so it's a diagnostic.** They need a private soak build
  (`0.1.0-soak.1`) signed with the release key (D2), and nothing in CI signs
  with it, so the harness skips the upgrade slots and the record says so.
  Without them no run can complete a tier's execution inventory, so the
  workflow passes `--tier custom` and the record calls the run a diagnostic
  that says nothing about either gate
  ([#1061](https://github.com/reliaburger/reliaburger/issues/1061)). Run the
  tiers on a host that holds the key.
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
The [nightly signing key](#nightly-signing-key) is that separate key for a
train's nightly build, though not for a release candidate's soak, whose
binaries trust only the release key.

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
  sh /tmp/reliaburger-candidate-install.sh --laptop
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
need their own evidence after publication.

## Guest images and bootstrap installer

Each quickstart VM boots from a guest image the release builds itself: the
dated Ubuntu 24.04 cloud image named in `scripts/release/guest-images.json`
with that file's `packages` (runc, uidmap, btrfs-progs, nftables, iptables,
iproute2, buildah) already installed. Without them baked in, every VM spent 15–40 s of
its first boot in `apt-get update` and `install`, against Ubuntu's live mirrors
([measurements](qualification/2026-09-24-guest-image.md)).

Buildah is there so the five-minute tour can `relish build` on the cluster.
With what it pulls in that the stock image lacks (`containers-common`, the CNI
plugins and netavark, `fuse-overlayfs`), it adds about 75 MiB installed,
going by Ubuntu 24.04's package sizes: 47 MiB of that is
`containernetworking-plugins`. Expect the compressed image to grow by roughly
25–40 MiB; record the real figure with the next image build.

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
- **qcow2 with zlib, not zstd or raw.** Lima (2.1.0 to 2.2.1) converts qcow2
  itself, but registers no zstd decompressor for qcow2 clusters, and it
  decompresses a `.zst` or `.xz` file by running the `zstd` or `xz` command,
  which macOS doesn't ship. zlib qcow2 is what Ubuntu itself publishes, so the quickstart
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
this gate. Don't advertise a timing this gate hasn't measured.
