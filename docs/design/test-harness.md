# Test harness

The test count used to answer the wrong question. It mixed portable correctness tests,
tests that returned successfully without running, privileged Linux checks, hour-long scale
simulations and Criterion benchmarks into one reassuring number. A green line could mean
"the behaviour worked" or "this laptop didn't have the prerequisite". Those aren't the
same thing.

This document retains the July 2026 audit history and defines the current suite,
ownership and evidence contracts. Historical counts and timings remain historical.

## What the audit found

The suite has broad behavioural coverage. It exercises the agent and HTTP API, real child
processes, Raft membership, gossip, reporting, ingress, image distribution, upgrades and
failure paths. The problem was the harness around those tests:

- environment-gated tests returned from the test body and appeared as passes;
- a Linux CI step reran the complete suite serially merely to reach a handful of root-only
  tests;
- duplicate scheduler tests, field-access checks and demonstration programs inflated the
  total without protecting behaviour;
- several names promised health, status, rollback, log following or chaos effects that the
  body did not actually observe;
- correctness tests contained host-speed thresholds, while scale tests duplicated the
  benchmark simulation and reported non-convergence as a sentinel value;
- fixed sleeps made the suite slow and created genuine races. CI caught a three-node test
  seeing two members and a reporting test receiving no reports after 14 seconds;
- GitHub's cleanup found orphan `sleep` processes after a failed cluster run;
- image-pull checks depended on a mutable public image and registry availability.

The original audit measured the warm default suite at about 124 seconds. On the refreshed
`main`, a clean local Cargo run executed 2,510 tests in 230.34 seconds. After the split,
three warm nextest runs executed 2,537 tests and skipped 36 explicitly categorised tests in
39.76, 38.86 and 38.64 seconds. The 38.86-second median is 69% below the audited warm
baseline. Counts vary by target because Rust does not compile Linux-only tests on macOS.
We therefore report executed, ignored, platform-specific and benchmark tests separately.

The CI evidence that prompted the audit remains useful:

- the portable step took 3m54s and the serial Linux-gated step took 10m38s in
  [run 29289232685](https://github.com/reliaburger/reliaburger/actions/runs/29289232685);
- gossip exposed only two of three members in
  [run 29211573782](https://github.com/reliaburger/reliaburger/actions/runs/29211573782);
- TCP reporting waited 14 seconds and received nothing in
  [run 29192001786](https://github.com/reliaburger/reliaburger/actions/runs/29192001786).

## Suite taxonomy

Every command fails when its filter selects no tests. Retries stay disabled. A flake should
be visible until we remove its race.

| Command | Contract | Automation |
|---|---|---|
| `make lint` | Clippy for every target, with all features and with none, so the build without default features stays healthy | Linux and hosted macOS |
| `make test` | Portable unit, component and integration correctness via nextest | Linux and hosted macOS |
| `make test-doc` | Rust documentation examples | Linux and hosted macOS |
| `make test-slow` | Required wall-clock health and retry acceptance | Linux; PRs into `main`, `main` and nightly |
| `make test-linux` | runc, namespaces, eBPF, Btrfs, Buildah and root-only tmpfs | Privileged Linux; PRs into `main`, `main` and nightly |
| `make test-cluster` | Gossip, placement, failover, healing, recovery and chaos | Linux, serial resource group; PRs into `main`, `main` and nightly |
| `make test-upgrade-node` | Real single-node binary replacement | Linux; PRs into `main`, `main` and nightly |
| `make test-upgrade-cluster` | Real rolling cluster replacement | Linux; PRs into `main`, `main` and nightly |
| `make test-rootless-runc` | Rootless runc networking and port adoption as a non-root user | Privileged Linux; PRs into `main`, `main` and nightly |
| `make test-standard-clients` | docker, podman, skopeo and crane against Pickle's TLS listener, verifying the cluster CA | Linux, the runner image's docker, podman and skopeo plus a pinned `crane`, on a host prepared by `scripts/standard-clients-host.sh`; PRs into `main`, `main` and nightly |
| `make test-apple` | Deferred Apple adapter | Manual Apple-silicon development check, outside 0.1.0 |
| `make test-gpu`, `make test-s3` | Real NVIDIA hardware; a real S3 bucket | Manual |
| `make test-ci-scripts`, `make check-ignored` | The CI scripts' fixtures; every `#[ignore]` names its owner | Every pull request ("CI policy") and `make ci` |
| `make ci` | `fmt-check`, `lint`, `test`, `test-doc`, `test-ci-scripts`, `check-ignored` | Local; CI runs the same pieces as separate jobs |
| `make ci-bench` | `make ci`, then `make bench` | Local |
| `make bench` | Criterion transport and 5–250-node measurements | `main`, nightly, and PRs touching gossip |
| `make bench-large` | Criterion 500- and 1,000-node measurements | `main`, nightly, and PRs touching gossip |
| `make fuzz-corpus-check` | Every fuzz target body over its seeds and checked-in corpus, on stable (also part of `make test`) | Local; `make test` everywhere |
| `make fuzz-smoke` | Every cargo-fuzz target for `FUZZ_SECONDS` each (nightly Rust and cargo-fuzz) | `fuzz.yml`: 300 s a target nightly, 30 s on PRs touching a decoder |
| `make coverage` | The portable suite, run once under line coverage; this is the Linux test gate too | Linux |
| `make audit` | Current RustSec database, with new vulnerabilities and maintenance warnings denied | Pull requests, release gate and weekly schedule |
| `tests/examples.rs` (part of `make test`) | Every checked-in workload config parses, validates and plans through Relish without deployment | Linux and hosted macOS |

Direct Apple Container is disabled for 0.1.0. Its manual development tests remain
separate because hosted macOS runners cannot provide its nested virtualisation.
The supported runtime gates run on pull requests, and release tags must pass the
same reusable validation workflow before publication. Managed macOS laptop
acceptance uses Linux VMs (V04 in the roadmap's [known gaps](../roadmap.md#known-gaps)).

Pull requests stacked on another branch skip the acceptance suites (wall-clock,
cluster, upgrade and privileged Linux) unless labelled `full-ci`; they run once the
PR targets `main`, and `ci-retarget.yml` reruns CI as soon as GitHub retargets one.
Pull requests into a release's merge-train branch (`release-*`) run the light set:
lint, minimum Rust, portable Linux with coverage, the demo app and the optimised
contract boundaries. Labelled `full-ci`, they run every heavy owner but not the
macOS shards (the `train-full` selection). The merge queue on the train, the push to it and the gate PR
into `main` run everything, including the three macOS shards
([which jobs run](../testing.md#which-jobs-a-pull-request-runs)).
Documentation-only pull requests skip the Rust jobs entirely, except for the manual
and the snippets `documentation_first_run` checks. `scripts/ci/test_select_jobs.py`
holds a fixture for each of these rules.

CI retains each owner's discovery, completion, receipt and logs in a separate
platform/gate envelope. It still keeps JUnit on failure for diagnosis. The final
`ignored-test evidence` job requires successful current-owner completion of every
applicable exact identity; presence in a union of reports is insufficient. See
[who runs an ignored test](../testing.md#who-runs-an-ignored-test) and the
[generated finite matrix](../testing/qualification-matrix.md).

### Tests no CI job runs

These need hardware, credentials or a reboot that hosted runners can't provide. They
are `#[ignore]`d with a reason naming the target or script that runs them:

| Test | Needs | How to run |
|---|---|---|
| `grill::apple::tests::*` | Apple silicon with Apple Container | `make test-apple` |
| `bun::gpu::tests::nvidia_detector_finds_hardware` | An NVIDIA GPU and `nvidia-smi` | `make test-gpu` |
| `ketchup::export::tests::export_to_real_s3_manual` | AWS credentials and a bucket | `RELIABURGER_TEST_S3_URL=s3://bucket/prefix make test-s3` |
| `owned_runc::actual_host_reboot_*`, `oci_crash::actual_bun_kernel_discovery_host_reboot` | A Linux VM that can be rebooted mid-test | `scripts/release/qualify-oci-reboot.sh`, `scripts/release/qualify-discovery-reboot.sh` |
| `power_cut::actual_power_cut_*` | A disposable Linux VM whose power can be cut while workers write | `scripts/release/qualify-storage-power-cut.sh --vm VM --fixture exporter\|leases\|backups --iterations N` |
| All of the above, for a release candidate | Lima on the host; it creates and deletes the VM | `scripts/release/qualify-power-cut-reboot.sh --commit SHA --iterations N --record-dir DIR` |

`.github/workflows/v02-loops.yml` is the other long-running lane. It runs only when
someone pushes the `v02-loops` branch or dispatches it by hand, and loops the upgrade,
council, lease-recovery and exporter-lock suites for hours on Linux x86-64, Linux
arm64 and macOS, then reports each lane's runs, failures and rule-of-three bound.
`scripts/release/loop_summary.py` gives every test a status (pass, fail, skip or
incomplete) and checks the combined record against
`scripts/release/v02-loop-lanes.json`: a lane or named test with no results, a
test with fewer runs than its loop's iterations, a skipped-only result or a record
from another commit fails the summary, and its exit status matches its verdict.

A gated test must be `#[ignore]`d, and once selected it must fail when its
prerequisite is missing: assert on the variable, don't return early. An early
return reports a pass without testing anything, and `--run-ignored=only` on the
wrong host would count it. The exceptions are subprocess fixtures (their
`#[ignore]` reason says so), which return when their parent test didn't start
them, because a gated binary selected whole also selects its fixtures.

The dependency audit refreshes its database on every run. It denies new
vulnerabilities, unsoundness, yanked packages and unmaintained-package notices.
The repository-local exception list names only reviewed findings whose affected
API is unreachable or whose upstream has no compatible fix. Those exceptions
aren't permanent configuration: `make audit` rejects all of them after 18
August 2026, forcing a new reachability and upgrade review. GitHub Dependabot
continues to track ecosystem advisories that RustSec doesn't carry.

## Gating rules

Use `#[cfg(target_os = "linux")]` when code cannot exist on the current target. For an
optional Linux dependency, gate its owner with both facts:

```rust
#[cfg(all(feature = "ebpf", target_os = "linux"))]
mod maps;
```

The `all` predicate means both conditions must hold. This matters when macOS runs
`--all-features`: enabling the portable Cargo feature does not somehow install Linux or make
Aya available. The compiler omits the module, so it must be counted as platform-specific
rather than ignored.

Use `#[ignore = "requires …; run with make <gate>"]` for code that compiles but needs root,
a provisioned runtime or deliberate wall-clock time. The matching Make target supplies the
prerequisite and runs ignored tests only, and the reason must name it: `make check-ignored`
fails on a reason with no owner. The first assertion in an environment-gated test is a preflight: asking
for the suite without its prerequisite fails instead of manufacturing a pass.

Do not inspect the host inside a test and return successfully. If a portable prerequisite
such as `git` is missing, fail with a useful message. If behaviour depends on whether a tool
is installed, inject capability state or put the two environments in separate suites.

## Public images: pinned, warmed once, served locally

The privileged Linux suites run real public images (BusyBox, Alpine, Redis, nginx,
podinfo). A release candidate once failed because one ECR read stalled mid-test, so no
timed test pulls from the internet any more:

- `src/testkit/pinned_images.rs` names every image by digest, and
  `tests/fixtures/pinned-images.txt` lists the same references for tooling. A portable
  unit test fails if the two drift or if `examples/kubernetes/podinfo.yaml` uses an image
  outside the list.
- `make test-images` runs `scripts/test-images/mirror.py warm`, which fetches each image's
  index, this host's platform manifest, config and layers once, with retries, into a
  digest-checked content-addressed cache (`TEST_IMAGE_CACHE`, default
  `~/.cache/reliaburger/test-images`).
- `make test-linux` wraps nextest in `mirror.py run`: it warms (a no-op on a verified
  cache), serves the cache as a read-only OCI registry on `127.0.0.1:5099` and sets
  `RELIABURGER_TEST_IMAGE_MIRROR`. Tests turn that into `[images] mirrors` for every
  pinned registry (`local_test_mirrors()`): Bun nodes get it in their config, image stores
  in runc unit tests get it directly, and `relish test` stages its fixture through the
  node's reported mirrors.
- `pinned_images_serve_verified_from_the_local_test_mirror` reads every image from the
  mirror alone, so a gap in the warm list fails in one place, not mid-suite.
- CI caches the warmed directory keyed on the list, warms in its own step, and passes
  `TEST_IMAGE_CACHE` to `make test-linux`. A registry outage then fails "Warm the pinned
  test images", clearly labelled, instead of a random acceptance test.

The mirror is untrusted by design: Bun verifies the full digest chain whichever registry
answers, and falls back to the upstream when a mirror fails. Adding an image means adding
its digest to both lists and running `make test-images`. The OCI interruption
qualification (`scripts/release/qualify-oci-interruptions.sh`) needs neither: its
fixtures use local root filesystems inside a network namespace with no uplink.

## Deterministic asynchronous tests

The preferred order is:

1. a channel, `Notify`, watch receiver, semaphore or barrier that exposes the event;
2. Tokio's paused clock or an injected instant for pure time-driven logic;
3. a bounded 20 ms predicate loop where the production API exposes no event.

A fixed sleep proves only that the machine happened to be fast enough. The rewritten mock
runtime blocks creates on a semaphore, the proxy tests gate requests with notifications,
reporting waits for its watch receiver to change, and Raft tests poll replicated state rather
than guessing how long replication needs. Genuine deadlines remain in `make test-slow`.

Harnesses own their spawned tasks and cancellation tokens. The main ProcessGrill harness
waits for shutdown in `Drop`, then aborts only after a bounded grace period. TCP reporting
does the same for listeners, workers and aggregators. Tests bind port `0`, use temporary
directories and keep every resource scoped so worktrees and nextest processes can run
concurrently. A listener that must come back on the same port after a restart or an
upgrade exec is the exception: it leases its port instead (see the cluster fixtures below).

## A new executable's first launch

On macOS the first `exec` of a newly written executable waits for `syspolicyd` to assess
it. Terminals with the Developer Tools permission are exempt, so a test can pass every
time on a developer's laptop and still time out on a CI runner or in an agent's shell. The
wait happens in the child after `posix_spawn` has returned, so it lands inside any deadline
the test put around the child, and assessments queue host-wide: a freshly linked debug
`relish` (about 490 MB) took 4 to 8 seconds alone and over 15 seconds while two other fresh
binaries were being assessed ([#573](https://github.com/reliaburger/reliaburger/issues/573)).
A small fresh script waits in the same queue.

A fixture that times a subprocess therefore pays the first launch outside its deadline.
`tests/support/first_launch.rs` runs each executable once, all at the same time, with a
45-second bound that names the phase if the queue never drains, and remembers which paths
it has launched in this test process. The behavioural deadline then measures only the
program it was written for.

Fake tools are the bigger source. nextest runs every test in its own process, so a script
each test writes for itself is a new file every time, and six lifecycle tests starting
together each queued a fresh `limactl` behind the others
([#582](https://github.com/reliaburger/reliaburger/issues/582)). Fixtures install scripts
with `install_script(path, body)` instead of `std::fs::write`. It keeps one read-only copy
of each distinct body, named by its SHA-256, in a directory only the current user can
write (`reliaburger-fixture-scripts-<uid>` under the system temporary directory), launches
it once with `--reliaburger-first-launch` (a line the helper adds, so the body doesn't
run), and hard-links it to `path`. A hard link shares the already-assessed file; a copy is
a new file that gets assessed again. Per-test state goes beside the link, and the script
finds it through `${0%/*}`, the directory of the name it was started as, so one body
serves every test. Library unit tests reach the same file through
`#[cfg(all(test, unix))] #[path = "../tests/support/first_launch.rs"] mod first_launch;`
in `src/lib.rs`.

That one shared launch still queues behind whatever else the host is assessing, including
another worktree's freshly built binaries: a two-line script waited up to 32 s behind one
fresh 800 MB test binary, and 75 to 94 s behind two at load average 50 to 180. A fake that
the code under test runs through a configurable program path can avoid the launch
altogether. The quickstart lifecycle fake is a plain script run as `/bin/sh <script> <args>`
via the test-only `Lima::shell_script`
([#762](https://github.com/reliaburger/reliaburger/issues/762)); `/bin/sh` is a system
binary and never assessed, and the same script started that way took 0.02 s. The runner's
fake `limactl` does the same, and the provisioning tests define their stub `apt-get`,
`systemctl`, `dpkg-query` and `install` as Bash functions ahead of the script instead of
files on `PATH`. Prefer this
shape for a new fixture whose caller can take an interpreter; use `install_script` when the
caller needs a single executable path.

The `[[bin]]` targets (`bun`, `relish`, `testapp`) get the same treatment twice. `make test`
depends on `make warm-executables`, which builds them with `cargo test --no-run`, as the
suite will, and runs each of them once before the suite
([#722](https://github.com/reliaburger/reliaburger/issues/722),
[#846](https://github.com/reliaburger/reliaburger/issues/846)). That doesn't cover a filtered
`cargo nextest run` straight after a build, or an assessment the host has to repeat, so a
test that starts one of them under a deadline also pays its first launch before the clock
starts ([#1134](https://github.com/reliaburger/reliaburger/issues/1134),
[#1149](https://github.com/reliaburger/reliaburger/issues/1149)). The shared fixtures do it
for their callers: `bun_process::first_launch_binaries()` launches all three together, and
`BunProcess::spawn_runtime`, `wait_for_relish` and `wait_for_relish_output` call it first.
A test that spawns `bun` or `relish` itself, or hands `bun` to `ProcessControl` as an owner
(15 s to answer), calls it, or `first_launch` directly, before its own deadline. A
candidate binary under the upgrade compatibility query is always a fresh private copy, so no
warm-up applies there; `QUERY_DEADLINE` (60 s) stays a hang guard
([#1078](https://github.com/reliaburger/reliaburger/issues/1078)).

## Benchmarks and coverage

Correctness tests use small deterministic inputs and assert exact output. A 100 MB P2P pull
under five seconds was neither portable nor a correctness contract, so the test now checks
parallel multilayer fetching without a wall-clock threshold.

Criterion owns performance measurements. Fast and large gossip benchmarks share one seeded
simulation, time setup separately from convergence, and fail if the network never converges.
The 10k acceptance exercises a single real node's 10,000-member production state through
bounded protocol messages and dissemination batches. Simulating every membership table on
one runner needs 100 million records and confuses distributed scale with single-process
capacity. Criterion data is uploaded from CI, but we will not set a regression threshold
until runs are stable on consistent hardware.

`cargo-llvm-cov` runs the portable suite once, instrumented, and writes LCOV and HTML
artefacts. On Linux that run is also the test gate and the portable JUnit source, so the suite isn't
compiled and run a second time uninstrumented. The measured Linux CI line baseline is 79.65%, so CI starts at 78.65%,
one percentage point lower. Raise it when coverage improves; do not lower it to land
unrelated work.

## Finite inventory and authority

`tests/contracts/manifest.json` names concrete contracts and exact
`(gate, binary, full test name)` assignments. Duplicate assignments are refused.
Case IDs stay stable across source refreshes. The source emitter records current
tracked regular files, test bodies, source attributes and hashes; all discovery
and execution flags remain false until the real producer supplies evidence.
Host and feature conditions must be reconciled with actual Cargo/nextest lists.
An absent Linux-only case on macOS isn't a successful skip.

The original ignored bindings retain their reason-derived Make/script owners.
A case already assigned by an audit family is reused with both references;
the strengthened defaults are references to existing cases too. Twenty exact
legacy OCI aliases are the entire reviewed bridge between original Linux owner
claims and the interruption driver. The direct held-capacity OCI control has its
own script declaration. No prefix matching or generic alias can widen the bridge.
The seventeen original manual/subprocess classifications remain outside CI
inventory, with the original policy visible in the generated matrix.

`workflow_assembly.py` defines eleven literal owners and derives light, portable,
train-full or full selection from trusted current workflow inputs; only a pull request may
qualify with less than full. The macOS owner runs as three hash-partition
shards, and aggregation proves their lists cover every test exactly once. `workflow_adapter.py` preserves
those commands and their complete selections. Ordinary nextest producers retain
discovery and JUnit under one selector/environment plan. `completion.py` retains
libtest discovery, selected-case completion and the actual child exit for owned
OCI binaries. Required cases must be selected and pass; missing, skipped,
incomplete, wrong-owner or failed-command evidence is refused. Manual owner
context cannot qualify CI.

The archive builder publishes current successful archive/runtime digests before
archived jobs execute. Aggregation requires those builder outputs, successful
selected jobs and each producer's sealed output digest. Commit, run ID, attempt,
host, owner command, inputs and payload bytes all belong to that context. Commit,
run ID and host must equal the aggregator's; the attempt may be earlier, because
re-running only the failed jobs keeps the passed jobs' attempt-N outputs, and all
owners of one job must share an attempt. An archived owner takes the builder's
attempt from the digest-bound archive runtime, never later than its own. Do not
reconstruct authority from artifact filenames or let a late report choose which
owners should have run. Always retain whole-command failure as failure, even if
all finite required assertions passed before the command failed elsewhere.

The Linux coverage owner intercepts the five operations of the unchanged
`make coverage` recipe. It binds discovery to the actual instrumented nextest
run, captures compiler/Cargo/nextest/LLVM identity and checks the unchanged line
floor after report generation. The current schema and guards check the owning
parent as well as its child. A fresh ordinary JUnit file or synthetic receipt
cannot stand in for this run.

### Sensitivity and lifetime controls

A behaviour-breaking variant must fail the intended oracle. Keep its precise
source delta and actual selected execution alongside the baseline and repaired
receipts. A build error or missing preflight proves only setup failure. Static
source inspection and Python model fixtures stay useful, but report them under
that scope rather than as Rust or runtime execution.

Writer controls use explicit entry/release notifications or owned operation
faults, rather than guessing when a sleep has crossed a publication boundary.
Cancellation drops a caller's future; it does not necessarily stop a blocking
writer or undo a remote PUT. The store must keep pending rows, immutable bytes,
publication identity and the writer guard until acknowledgement is complete.
Queries must retain those rows once while publication/listing races and retries
are unresolved. Local controls also revisit owned directory entries after an
uncertain earlier attempt, including a fresh store, before acknowledging work.

Ketchup's restart controls recover the frozen capture identity and checkpoint
from the same publication as its rows. Replacing a path must not associate old
read offsets with a newly opened file. These checks cover the specified
append-only capture/restart contract. They don't promise arbitrary file rotation
or preservation of unflushed memory after process loss. Temporary I/O faults and
controlled object stores cannot establish power-cut or cloud durability.

A fixture timeout with no established phase stays unknown. Record the exact
failure and one separately labelled diagnostic without retries or increased
production deadlines. Subprocess readiness/capture controls should name the
bounded phase, observe the owned child and distinguish an empty readable log
from a log read/decode error. Never include secrets to improve a diagnostic.

## Adding a test

Name the externally visible behaviour, make the failure message describe the missing
observation, and choose exactly one suite. Prefer a black-box test when the promise belongs
to `bun` or `relish`. Use snapshots for structured rendering, properties for large algorithm
spaces and Criterion for measurements. A test that can pass without executing its promised
behaviour is worse than no test: it gives us confidence we didn't earn.

A new portable integration test goes in `tests/suite/` as a module: add the file and a
`mod` line in `tests/suite/main.rs`. Every file directly under `tests/` is a separate crate
that links the whole library, so each one costs a link step and hundreds of megabytes of
debug executable. Merging 45 small files into `suite` took the test binaries from 77 to
33 and their size from 9.1 GB to 4.0 GB. Its tests are named `<module>::<test>`, so filter them with
`test(/^module::/)`, not `binary(module)`. Add a new top-level binary only when the tests
need gating by a Makefile target, their own nextest group or override (selected with
`binary(...)`), or process isolation that nextest doesn't already give, such as a fixture
that re-executes the test binary by name. Shared helpers under `tests/support/` are
included once at the suite root and reached through `crate::`.

Multi-node tests start their clusters through `tests/support/cluster.rs`: the fully wired
`bun --cluster` node, the in-memory five-node council, `local`, `wait_until` and the leader
waits all live there, so include it rather than copying another start-up helper.

A cluster test shares nothing with another test or with an earlier run. `make test-cluster`
runs its binaries side by side, and #721 found two ways that went wrong in
`cluster_failover`: a fixed port block that `placement` also used (node `fo3`'s reporting
port was `wk1`'s Raft port, so whichever started second failed with "Address already in
use"), and a fixed data directory that a killed root run left with a `reliaburger-identity`
tmpfs still mounted, which `start_wired_node` failed to remove and then reused. So:

- lease each node's port block with `reserve_port_block_lease` from
  `tests/support/bun_process.rs` and hold it until the test ends;
- give each test its own temporary `data_root`; `start_wired_node` clears a node's directory
  with `clear_fixture_dir`, which unmounts anything beneath it on Linux and fails the test
  rather than starting on what it couldn't remove;
- ask the cluster which node leads before writing, reading the leader's aggregated reports
  or killing it. Leadership can move under load, and a write to a follower returns
  `ForwardToLeader`.

`placement`, `cluster_task_arrays` and `council_disaster_recovery` lease their blocks the
same way (#744), and so does `cluster_gossip` (#840). Before that, three
`cluster_task_arrays` tests shared one fixed block and two more shared another, so two runs
of the binary at once (two worktrees, say) failed with "Address already in use" on the
second run's gossip bind. `cluster_gossip` was the last `make test-cluster` suite on fixed
ports: two nextest processes running it at once lost 4 of their 16 tests that way. A new
multi-node test leases its ports like these. The two leader-loss tests in
`owned_task_arrays` (`make test-linux`) were the last on fixed bases (24510 and 24610);
they lease their three nodes' ports the same way and keep the nodes' data under their own
fixture root (#852). A port that has to stay fixed keeps out of the leased 27000–30000
range.

The same rule covers any port a process gives up and binds again: a crash fixture's
restart, and every self-upgrade or rollback, because `exec` closes Bun's sockets and the
new binary binds the configured ports once it starts. Never choose such a port with
`bind("127.0.0.1:0")` and release it. That port comes from the ephemeral range, and in the
gap any other test's `bind(0)`, outgoing `connect()` or slirp forward can take it. That was
one of #734's two causes: `oci_crash`'s upgrade fixture released its API port, a parallel
`owned_rootless` test could draw the same number for a slirp forward, and the exec'd Bun
exited with "Address already in use". Nothing restarts Bun in that fixture, so the walk
reported only "owned runtime never settled on v0.1.0" after 45 seconds. (The other cause
was in the product: adoption after an exec refused while the old process's last `runc`
command was still finishing; see chapter 15.) The upgrade
fixtures in `oci_crash`, `self_upgrade` and `self_upgrade_cluster`, and the slirp host
ports in `owned_rootless`, now lease their ports for the whole test. The settle wait also
fails at once with the node log when the Bun it watches exits.

Cleanup has to find everything a test started, including processes whose command lines
don't name the test root. `oci_crash`'s `RootCleanup` killed only those that did, so a
failed rootless run leaked its `slirp4netns` helper: the helper names only its generation's
`/tmp/rbr-<uid>-<generation>/` directory. The leaked helper also kept every descriptor it
inherited, including the lock the shared Linux test VM queues its users on, and it held
that lock for an hour. `RootCleanup` now reads each node's runtime intents for those
directories and kills whatever names them too.

Cleanup also removes the root, mounts and all. A rootful Bun mounts a `reliaburger-identity`
tmpfs for each workload beneath its volumes directory, and the mounts outlive Bun. Until
#780 `RootCleanup` never removed the root, so every rootful run left its identity tmpfs
mounted under `/tmp`. It now finishes with `clear_fixture_dir` from
[`tests/support/fixture_dir.rs`](../../tests/support/fixture_dir.rs), the helper the
cluster fixtures use, which unmounts everything beneath the root deepest first and then
removes it. A root it can't remove fails a passing test; while a failed test unwinds, the
error goes to stderr, because a second panic would abort the test binary.

The owned-runc capacity harness in `tests/owned_runc.rs` had the same leak in a quieter
form (#852). It handed its agent a volumes `TempDir` and dropped it once the agent task
ended, and dropping a `TempDir` ignores a failed removal, so a mounted identity tmpfs
stayed behind without a word. The harness now owns the directory for its whole life; its
`Drop` stops the agent and API first, then calls `clear_fixture_dir` with the same
panic-or-print rule.

Cleanup must also stop at what the test owns. The scan reads every process on the host, and
on a CI runner that includes the runner. Until #800 it killed the whole process group of any
match outside the test's own group. From #758 on, a match could be any process with a file
open under the root. A process the test didn't start, sitting in someone else's group, would have taken
that group down with it. A runner killed that way loses the job and every log it had.
That is how the privileged Linux job's OCI step failed five times on 11 October, though no
run proved which process matched. The rule now
lives in one place, [`tests/support/root_processes.rs`](../../tests/support/root_processes.rs),
shared by `oci_crash`, `kubernetes_demo` and `autoscale_memory`:

- A process counts as the test's if its command line names one of the test's unique paths,
  or if it descends from the test process. Holding a file under the root isn't enough.
- A group is killed only when one of the test's processes leads it, as Bun's detached owners
  do. Any other process of the test's is killed alone.
- Anything else is left running, and the cleanup logs that it spared it.

`root_teardown` in the portable suite proves it on Linux. A sleeper started through
`setsid -f` (its own group, outside the test's tree) holds a file under the root. Next to it
runs an owner that leads its own group. Teardown must kill the owner and its child and leave
the sleeper running.
