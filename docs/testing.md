# How Reliaburger is tested

An orchestrator is only as good as its worst day, so most of our tests are
about bad days: killed agents, cut power, lost quorum, half-written files. Here
is every layer, from the fastest to the slowest, with where it lives and how to
run it.

## Every commit

| Layer | What it proves | Where | Run |
|---|---|---|---|
| Unit tests | Pure logic, state machines, parsers, every failure branch | `#[cfg(test)]` modules across [`src/`](../src) | `make test` |
| 10,000-member gossip | One node's membership table and dissemination at 10,000 members, in about a second | [`tests/gossip_10k.rs`](../tests/gossip_10k.rs) | `make test` |
| Mayo memory | Metrics reads (the periodic alert and rollup cycle, unbounded and old-window queries, the council rollup store and the object-store backend) need the same heap whether a node holds one hour of history or four, and the periodic ones leave nothing behind (a counting allocator in its own binary) | [`tests/mayo_memory.rs`](../tests/mayo_memory.rs), [record](qualification/2026-09-29-mayo-memory.md) | `make test` |
| Upgrade and connection memory | Preparing an upgrade, the leader's candidate check and a rollback need the same heap for a 64 MiB binary as for a small one, and an idle API connection doesn't keep a copy of its largest answer (counting allocators, one binary each) | [`tests/upgrade_memory.rs`](../tests/upgrade_memory.rs), [`tests/connection_memory.rs`](../tests/connection_memory.rs) | `make test` |
| Property tests | Invariants over generated inputs: scheduling, allocation, parsing | [`proptest`](https://docs.rs/proptest) blocks in `src/` | `make test` |
| Snapshot tests | CLI output, rendered config and TUI frames stay exactly as reviewed | [`insta`](https://insta.rs) snapshots, e.g. [`src/relish/snapshots/`](../src/relish/snapshots) | `make test` |
| Integration suite | Real Bun and Relish processes over HTTP, TLS and the registry | [`tests/suite/`](../tests/suite) | `make test` |
| Doctests | Examples in doc comments compile and run | `///` blocks | `make test-doc` |
| Doc snippets | Every `relish` command and TOML block a reader copies from the manual, the READMEs and the whitepaper parses with the real code ([below](#doc-snippets)) | [`tests/suite/doc_snippets.rs`](../tests/suite/doc_snippets.rs) | `make test` |
| Lint and format | Clippy with warnings as errors, all features and none; the macOS job runs the all-features pass only, because that one holds the macOS-only code | [`Makefile`](../Makefile) | `make lint`, `make lint-macos`, `make fmt-check` |
| Coverage floor | Line coverage of the portable suite never drops below 78.65% | `COVERAGE_MIN_LINES` in the [`Makefile`](../Makefile) | `make coverage` |
| Dependency audit | No new RustSec advisory; every exception is dated | [`security.yml`](../.github/workflows/security.yml), [exceptions](qualification/2026-09-18-dependency-exceptions.md) ([12 Oct](qualification/2026-10-12-dependency-exceptions.md) and [21 Oct](qualification/2026-10-21-dependency-exceptions.md) additions) | `make audit` |
| Dependency lag | Nothing: a report, never a failure. It lists what `cargo update` would move and every crate a major version behind, for the refresh at the start of each minor release ([`releasing.md`](releasing.md#refreshing-dependencies)); [Dependabot](../.github/dependabot.yml) opens the weekly compatible refresh | [`security.yml`](../.github/workflows/security.yml) prints it in the weekly job summary | `make outdated` |
| CI scripts | Job selection, ignored-test owners, JUnit keeping, the benchmark baseline and the flakes register, against fixture repositories and the real workflow | [`scripts/ci/test_*.py`](../scripts/ci) | `make test-ci-scripts` |
| Flakes register | Every open row of [`flakes.md`](flakes.md) links an issue that is still open | [`flakes_register.py`](../scripts/ci/flakes_register.py), run by [`flakes.yml`](../.github/workflows/flakes.yml) nightly and on pull requests that edit the register | `python3 scripts/ci/flakes_register.py check` (needs `gh`) |
| Ignored-test owners | Every `#[ignore]` reason names who runs it ([below](#who-runs-an-ignored-test)) | [`ignored_owners.py`](../scripts/ci/ignored_owners.py) | `make check-ignored` |

`make ci` runs the portable set locally, the same way CI does: formatting,
lint, the portable suite, doctests, the CI scripts' tests and the ignored-test
owner check. `make ci-bench` is `make ci` followed by `make bench`. Neither
runs a privileged, cluster or upgrade suite; those have their own targets
below, because they need root, a provisioned host or a lot of wall time.

`make test` first runs `make warm-executables`: it builds the suite with
`cargo test --no-run`, which builds the `[[bin]]` targets (`bun`, `relish`,
`testapp`) as the tests use them, and, on macOS, runs each binary once with
`--help` ([`warm_executables.py`](../scripts/ci/warm_executables.py)). It
used to run `cargo build --bins`, which resolves features without the
dev-dependencies (`proc-macro2`'s `span-locations` is one), so nextest's own
build then compiled the library and the binaries a second time and replaced
the files the step had just warmed
([#846](https://github.com/reliaburger/reliaburger/issues/846)). macOS
assesses the first exec of every new executable before it runs, one file at a
time across the host, and outside a terminal with the Developer Tools
permission (a CI runner, a launchd service, a sandboxed agent) that costs
about a second for a release `bun` and several seconds for a debug build.
Without the warm-up, a test that launches a freshly built binary under a
deadline waits in that queue. nextest already runs each test binary once to
list its tests, so those are warm by then. Tests that write their own new
executables, such as the upgrade probe's private copy of a candidate, still
pay for one assessment each ([#722](https://github.com/reliaburger/reliaburger/issues/722)).
To reproduce the CI context on a Mac, start the command from a process that
disclaims responsibility (`posix_spawn` with
`responsibility_spawnattrs_setdisclaim`); the measurements are in the book's
[Chapter 14](book/14-changing-the-tyres.md#the-deadline-was-timing-macos-not-the-candidate).

The same rule holds for every target that builds before its tests. `make
test-linux` needs `target/debug/bun` with the `ebpf` feature
(`RELIABURGER_BUN_BINARY` names it), so it first runs `cargo test --no-run
--features ebpf`, the build nextest's `--features ebpf` will ask for. It used
to run `cargo build --features ebpf --bin bun`, so on a fresh target nextest
compiled the library, every proc macro and `bun` a second time, and replaced
the `bun` the step had built. On a Mac the two builds took 710 s; now they take 327 s
([#904](https://github.com/reliaburger/reliaburger/issues/904)).
A pre-build is `cargo test --no-run` with exactly the build flags of the test
run that follows it (`--features`, `--release` and the like); selection flags
such as `-E` don't matter, since nextest builds every test target anyway.
[`test_make_prebuilds.py`](../scripts/ci/test_make_prebuilds.py) reads each
recipe, with its prerequisites, and fails on a `cargo build` before a test
run or on a pre-build whose features differ. The other gated targets build
nothing before nextest: in CI the cluster and acceptance suites run from the
`build tests` job's nextest archive, and the `qualify-*.sh` scripts already
build with `cargo test --no-run`.

### Doc snippets

The manual ships inside `relish`, so a wrong snippet ships in the binary.
[`doc_snippets.rs`](../tests/suite/doc_snippets.rs) reads
`docs/manual/*.md`, `README.md`, `docs/README.md` and `docs/whitepaper.md`
and checks three things:

- **Shell blocks** (`sh`, `bash`, `shell`, `console`): every `relish`
  invocation parses with the real CLI under `RELISH_PARSE_ONLY=1`. It finds
  them as the shell would: after a `$ ` prompt, across `\` continuations,
  behind `sudo`, `VAR=…` or `limactl shell VM`, inside `$(…)`, and as
  `target/debug/relish` or `cargo run --bin relish --`. Comments and
  redirections are dropped, and a `<PLACEHOLDER>` becomes `1`. In a
  `console` block only prompt lines count.
- **TOML blocks**: each top-level table goes to the type that reads it.
  `[app.*]`, `[job.*]`, `[namespace.*]`, `[permission.*]` and `[build.*]`
  go through `Config::parse` and `validate()`; `[[step]]` files through the
  chaos scenario loader; `[[cohort]]` files through the batch manifest type;
  everything else through `NodeConfig::parse`, which rejects a misspelt
  table by name. A block mixing workload and node tables is split and both
  halves are checked.
- **Inline spans** in prose (`` `relish …` ``): the leading words must name a
  real subcommand, and every flag must be one that subcommand takes. Missing
  arguments are fine; prose rarely spells out a whole command.

A snippet that can't pass says why, on the line before its fence:

| Marker | Meaning | Checked? |
|---|---|---|
| `<!-- snippet: fragment -->` | Not complete enough to parse alone. Prefer completing it (add the `[app.NAME]` and `image` lines a sub-table needs) | No |
| `<!-- snippet: planned #N -->` | Shows a feature issue N will build | No |
| `<!-- snippet: pending #N -->` | Shows CLI or config issue N is adding | Yes, and it must still fail: when issue N lands the test asks for the marker to go |

An inline span for a command that doesn't exist yet is either followed by
` *(planned)*` in the text or listed in `INLINE_PENDING` with its issue, and
it too must keep failing. `planned` and `pending` must name an issue in the
test's `OPEN_ISSUES` list (CI has no GitHub token to ask); when an issue
closes, drop it from that list and the markers naming it fail. An unknown
marker, or a marker that isn't right before a fence, fails the test rather
than skipping anything. Run the marker test with `--no-capture` to see every
opt-out.

## The agent loop's turn budget

Each node's agent runs one loop that owns all of the node's state, one turn at
a time. A turn that waits on something slow holds every caller, and that one
shape caused most of the 0.1.0 and 0.1.1 soak failures
([review](plans/2026-09-30-agent-loop-review.md), #351). Four things keep
turns short:

| Piece | What it does | Where |
|---|---|---|
| Turn meter | Times every turn by branch, exports `bun_agent_loop_turn_seconds{branch}` through Mayo, logs any turn over 250 ms with the command or deploy op it ran. Tests read the worst turn, including one still running | [`src/bun/loop_meter.rs`](../src/bun/loop_meter.rs) |
| Starvation harness | One scenario per inline await from the review: a `MockGrill` whose calls can each be slowed (`set_call_delay`), a council whose writes hang, a log client that never reads, and test-only stalls for awaits no mock stands behind (the disk, `nft`). Each scenario queues a status command during the slow work and fails unless it's answered, and the worst turn ends, within 1 s | [`src/bun/agent/tests/loop_harness.rs`](../src/bun/agent/tests/loop_harness.rs) |
| Inline-await rule | Parses the agent's source with `syn`, walks every method a turn can reach from `run_loop`, and fails on an await with neither the turn's deadline (`tokio::time::timeout_at(deadline, ..)`) nor a `// LOOP-INLINE: <why>` comment. A fixed `tokio::time::timeout` needs the tag too: 1 s is already the whole budget (#418) | [`src/bun/agent/tests/loop_rule.rs`](../src/bun/agent/tests/loop_rule.rs) |
| Soak gate | The V02 checker reads each node's turn histogram at every settle and heavy check and fails the tier on any turn over 1 s | [`scripts/release/sustained_check.py`](../scripts/release/sustained_check.py) (`turn_findings`) |

The first three run in `make test`; the gate's own tests run with the other
release-script tests (`python3 -m unittest discover -s scripts/release -p
'test_*.py'`). Every harness scenario runs; none is ignored any more. Egress
DNS re-resolution and the execution fence have no scenario because they need
a loaded eBPF program; their off-loop halves have unit tests
(`bun::agent::egress_resolution`, `bun::agent::off_loop_work`). A new inline
await either gets a deadline (`timeout_at` on the turn's shared runtime
budget, for work a later turn can retry), moves into a task, or gets a tag a
reviewer can argue with. A scenario for the new await belongs in the harness,
not behind an `#[ignore]`.

## Real systems, gated

These need root, Linux, several processes or minutes of wall time, so they're
ignored by default and run by their own targets and CI jobs
([`ci.yml`](../.github/workflows/ci.yml)).

| Layer | What it proves | Where | Run |
|---|---|---|---|
| Linux runtime | runc, user namespaces, eBPF service discovery, nftables, Btrfs, rootless runc | [`tests/owned_runc.rs`](../tests/owned_runc.rs), [`tests/ebpf.rs`](../tests/ebpf.rs), [`tests/owned_network.rs`](../tests/owned_network.rs), [`tests/owned_rootless.rs`](../tests/owned_rootless.rs) | `make test-linux` |
| Crash recovery | Bun killed at every awkward moment: before adoption, mid-rollout, with owners alive | [`tests/oci_crash.rs`](../tests/oci_crash.rs), [`tests/job_recovery.rs`](../tests/job_recovery.rs), [`tests/registry_recovery.rs`](../tests/registry_recovery.rs), [`qualify-oci-interruptions.sh`](../scripts/release/qualify-oci-interruptions.sh) | `make test-linux` |
| Multi-node clusters | Leader failover, council self-healing and full-loss recovery, placement, gossip, an image surviving the loss of the node it was pushed to (Linux only) | [`tests/cluster_failover.rs`](../tests/cluster_failover.rs), [`tests/council_self_healing.rs`](../tests/council_self_healing.rs), [`tests/council_disaster_recovery.rs`](../tests/council_disaster_recovery.rs), [`tests/placement.rs`](../tests/placement.rs), [`tests/cluster_task_arrays.rs`](../tests/cluster_task_arrays.rs), [`tests/registry_durability.rs`](../tests/registry_durability.rs) | `make test-cluster` |
| Feature demos | [`scripts/demo/rollout.sh`](../scripts/demo/rollout.sh) (roll forward, roll back, a bad version rolled back and marked `RolloutFailed`) and [`scripts/demo/gitops.sh`](../scripts/demo/gitops.sh) (a `file://` repository synced, an app deleted from it, a hand-applied app left alone) run to completion on a one-node cluster each starts through [`lib.sh`](../scripts/demo/lib.sh); a step that doesn't do what it says fails the demo, and each must print the resource readout and summary [`resources.py`](../scripts/demo/resources.py) samples ([`test_demo_resources.py`](../scripts/ci/test_demo_resources.py) covers its parsing and arithmetic). [`test_demo_scripts.py`](../scripts/ci/test_demo_scripts.py) checks on every run that they stop on the first error and swallow nothing | [`tests/demos.rs`](../tests/demos.rs) | `make test-cluster`; by hand `make rollout-demo`, `make gitops-demo` |
| Self-upgrade | Rolling binary upgrades and rollbacks with real signed binaries, workloads kept running | [`tests/self_upgrade.rs`](../tests/self_upgrade.rs), [`tests/self_upgrade_cluster.rs`](../tests/self_upgrade_cluster.rs) | `make test-upgrade` |
| Wall-clock acceptance | Timeouts, back-offs and leases that can't be tested with a paused clock | ignored tests in [`tests/integration.rs`](../tests/integration.rs) | `make test-slow` |
| Standard registry clients | docker, podman, skopeo and crane log in with an API token, verify the cluster CA, push and pull layers back through Pickle's TLS listener; a wrong token, another cluster's CA and an out-of-namespace push are refused | [`tests/suite/registry_standard_clients.rs`](../tests/suite/registry_standard_clients.rs) | `make test-standard-clients` (Linux, with the four clients and `sudo scripts/standard-clients-host.sh --provision`) |
| Benchmarks | Gossip convergence from 5 to 1,000 nodes, one scheduling pass (`plan_pass`) over 1,000 and 10,000 nodes, in-process task-array costs, plus the data plane on a live cluster (`relish bench`) | [`benches/`](../benches), [`src/testkit/bench/`](../src/testkit/bench) | `make bench`, `make bench-large`, `make bench-task-arrays` ([which job](#benchmarks-in-ci)) |

A test that runs a host command names a path every Unix host has. macOS has
`/bin/sh`, `/bin/echo` and `/bin/sleep`, but keeps `true` and `false` only in
`/usr/bin`, so a job naming `/bin/false` is refused there as a missing binary
([#934](https://github.com/reliaburger/reliaburger/issues/934)). For a command
that fails, use `/bin/sh -c "exit 1"`.

The eBPF tests load the object embedded in the library under test, as Bun
does, and never search the target directory. Worktrees can share one
`CARGO_TARGET_DIR` without testing each other's programs.
`tests_load_the_object_this_build_compiled` checks this without a kernel.

### Who runs an ignored test

Every `#[ignore = "..."]` reason names its owner, and `make check-ignored`
(part of `make ci`, and the "CI policy" job) fails when one doesn't, or when
the owner doesn't exist. An owner is one of:

| Owner | Example reason | Checked |
|---|---|---|
| A Make gate: a target whose recipe runs `--run-ignored` | `"requires root and runc; run with make test-linux"` | the target exists and runs ignored tests |
| A script | `"run through scripts/release/qualify-oci-interruptions.sh"` | the file exists |
| A parent test | `"subprocess fixture for owned Runc caller death"` | something else in the same file names the fixture |
| A tracking issue, for deferred work | `"stage 3 of #351"` | the reason carries `#<number>` |

That proves every test *has* an owner, not that the owner still selects it.
`--no-tests=fail` passes as long as a filter matches anything. Renaming one
case out of `test(/runc_/)` can leave a green gate that no longer checks it.
The finite inventory binds an exact Cargo binary, full test name and owner
gate for each required case. Its [generated matrix](testing/qualification-matrix.md)
retains the individual issue and original owner references, including overlaps.

CI retains discovery, completion, command status and current execution context
together under each owner. JUnit is still kept for diagnosis, including failed
runs. The `ignored-test evidence` job accepts a case only through its successful
current owner envelope. A matching line from another report, an old run or a
failed whole command cannot supply that evidence. The exact twenty reviewed
legacy OCI mappings retain their original `make test-linux` declarations and
receive evidence through the owned interruption driver; no other case gets an
alias by resemblance.

The image-backed delegated-job cases live in `tests/owned_task_arrays.rs` and
run through `make test-linux`, where the warmed digest-pinned mirror remains
reachable. The shared-runc leader-loss case refuses to start without it: its
cold executors read the image manifest dozens of times, which trips the public
registry's anonymous rate limit (#786). Cases that must interrupt a real
cold pull put `tests/support/slow_registry.rs` in front of that mirror: a
loopback registry that forwards manifests at full speed and paces every blob
body to 64 KiB a second, so the pinned workload image takes about half a
minute to pull and a test can cancel it, or let a deadline expire, mid-layer
(#740). Two of them start three in-process nodes (gossip, Raft, the API
and the leader loop, as `bun --cluster` wires them) with real shared-runc or
cgroup host executors, and stop the leader's control plane while it runs its
own share of an array: no accepted outcome may be lost or counted twice, and
only the chunks the lost leader held may run again. `make test-cluster` runs
the same leader loss with fake runners at a hundred thousand tasks
(`tests/cluster_task_arrays.rs`). They do not run in the OCI interruption driver's isolated network
namespace. Their exact identities and the worker-loss cluster case are recorded
in the finite inventory. A repository check verifies complete ignored-test
bindings and current fingerprints for the retained OCI aliases.

Gates CI can't run are manual, so the evidence check skips them. No workflow
runs these; the maintainer runs each one on the release candidate, and the
record goes in [`qualification/`](qualification):

| Gate | Needs | Who runs it, and when |
|---|---|---|
| `make test-apple` | Apple silicon with Apple Container | The maintainer, before any release that touches the Apple Container runtime (`src/grill/apple.rs`) |
| `make test-gpu` | An NVIDIA GPU and `nvidia-smi` | The maintainer, before any release that touches GPU detection (`src/bun/gpu.rs`) |
| `make test-s3` | AWS credentials and `RELIABURGER_TEST_S3_URL=s3://bucket/prefix` | The maintainer, before any release that touches log export, council backups or the S3 object-store client (`src/ketchup/export.rs`, `src/council/backup.rs`, `src/object_storage.rs`). It exports logs to the bucket and round-trips a council backup through it (put, list, prune, restore), with credentials and region from the standard `AWS_*` variables |
| `qualify-power-cut-reboot.sh`, which runs `qualify-oci-reboot.sh`, `qualify-discovery-reboot.sh` and `qualify-storage-power-cut.sh` | A disposable VM, which it creates and deletes that can be rebooted or powered off ([below](#pulling-the-plug)) | The maintainer, on every release candidate ([release gate](releasing.md#staging-a-candidate)) |

### Benchmarks in CI

The `benchmarks` job in [`ci.yml`](../.github/workflows/ci.yml) runs `make
bench` (the gossip benchmarks, then [`benches/scheduler.rs`](../benches/scheduler.rs):
a leader pass placing 150 replicas, and a converged pass, on 1,000 and 10,000
nodes) and `make bench-large`, and the `task-array benchmarks` job runs `make
bench-task-arrays`. Both run nightly on `main` and on pushes to it. A pull
request runs the first when it touches the gossip protocol or its benchmarks,
and the second when it touches what `benches/task_arrays.rs` measures: the
task-array leader and node pool, the executor, range sets, the benchmark
itself or `bench_baseline.py` ([`select-jobs.sh`](../scripts/ci/select-jobs.sh)). A change to
`criterion`'s version, in `Cargo.toml` or only in `Cargo.lock`, runs both,
because both parse its output and use its filter syntax
([#1151](https://github.com/reliaburger/reliaburger/issues/1151)). A pull request into a
merge train without the `full-ci` label runs neither: there both wait for the merge queue and the push. The gossip numbers
gate nothing. The task-array job compares each benchmark's mean with the
baseline the last `main` run saved
([`bench_baseline.py`](../scripts/ci/bench_baseline.py)). Hosted runners
differ by tens of per cent from run to run, and on 12 October 2026 one was
uniformly 1.4–2.1× slower (#857), so the job allows for the runner it got. It
divides each benchmark's ratio by the median ratio of the other benchmarks in
the same run (counted as at least 1, so a fast runner never tightens the
bound), and fails when a benchmark is more than twice as slow as the rest of
its run. It also fails when the run's median ratio is above 2, since a slow
runner and a regression across most of the suite look the same from inside
one run. A disk-bound benchmark can still cross the limit on its own when the
runner's disk stalls during it (`storage/ledger_and_index_64k` did twice on 13
October, #936), so a benchmark over the limit gets one more chance: the step
re-runs just those benchmarks (`--remeasure`, which appends an anchored
Criterion filter naming them to `cargo bench --bench task_arrays --`), compares
again, and fails a benchmark only when it's over the limit on both
measurements. The median check isn't re-measured.

That didn't save `storage/ledger_and_index_64k` (#1038). Most of its time
goes on more than 30 fsyncs, so it measures the runner's disk, and a slow disk
can stay slow for the whole job: in the 0.2.1 merge queue for PR #992 it was
3.20× the rest of its run and 2.93× on the re-measure. In the eight runs from
12 to 18 October where it alone was over the limit, its ten samples ranged from about 15 ms to as much as 134 ms
within one benchmark, where a healthy runner keeps them within a few per cent,
and no other benchmark in those runs was above 1.11× the rest. So the compare
step names it with `--report-only`: it's measured and shown, marked "report
only", but never fails the run, is never re-measured, and isn't part of the
median any other benchmark is measured against. Its twin,
`storage/ledger_and_index_64k_tmpfs`, does the same writes in a directory on
`/dev/shm`, where an fsync costs nothing, and is gated like everything else
once `main` has recorded a baseline for it (until then it's listed as new).
That still catches a ledger or index change that makes encoding, writing or
indexing the records slower. What it can't catch is a change that adds
fsyncs, since those cost nothing on tmpfs; that shows up only in the on-disk
benchmark's reported ratio. The tmpfs twin runs on Linux only: on macOS
`make bench-task-arrays` skips it with a note. A normalising disk reference in
the same job was the other option, and the samples ruled it out: the disk's
speed changes several-fold within one benchmark's few seconds, so a reference
measured before or after it sees a different disk.

The job summary shows each benchmark's ratio, its ratio against the rest and
the runner factor (the median), plus both measurements of any benchmark it
re-measured. The means of the 12 October runs are a fixture in
[`fixtures/task-array-runs-2026-10-12.json`](../scripts/ci/fixtures/task-array-runs-2026-10-12.json):
`make test-ci-scripts` checks that they pass and that a benchmark pushed to
2.1× the rest of any of them fails. The eight disk-stall runs are a second
fixture,
[`fixtures/task-array-disk-stalls-2026-10.json`](../scripts/ci/fixtures/task-array-disk-stalls-2026-10.json):
each fails with every benchmark gated, passes with the on-disk ledger report
only, and still fails when any gated benchmark, the tmpfs twin included, is
pushed to 2.1× the rest. The gate matches benchmarks by name, so
a benchmark whose workload changes gets a new name (as
`state/json_encode_1m_x64` did, #872): under its old name it would be
compared with `main`'s baseline for the old workload, and a new name starts
fresh. The job's Criterion output and baseline are kept as the
`criterion-task-arrays` artefact for 90 days, the cost trend #668 reads.
`test_every_benchmark_target_is_run_by_a_workflow` fails if a `bench` target
in the `Makefile` isn't run by any workflow.

### Which jobs a pull request runs

[`select-jobs.sh`](../scripts/ci/select-jobs.sh) diffs a pull request against
its base (with renames split into a deletion and an addition, so moving code
into `docs/` still counts as code). Documentation-only changes skip the Rust
jobs. What else runs depends on where the change is going:

| Run | Jobs | Typical wall time |
|---|---|---|
| Pull request into a merge train (`release-*`, e.g. `release-0-2-1`) | The light set: CI policy, dependency audit, lint and format, minimum Rust, the demo app, portable Linux (with coverage and doctests) and the optimised contract boundaries | 15–20 minutes |
| The same, labelled `full-ci` | Everything the change touches except the three macOS shards: the light set plus privileged Linux, the cluster, acceptance and upgrade suites, and the benchmarks | about 45 minutes |
| Any pull request whose head is a train (the gate PR into `main`); any pull request into `main` | Everything the change touches: the light set plus three macOS shards, privileged Linux, the cluster, acceptance and upgrade suites, and the benchmarks | about 45 minutes, plus any wait for macOS runners |
| Merge-queue group on a train (`merge_group`), push to a train or to `main`, nightly, manual | Everything | about 45 minutes, plus any wait for macOS runners |
| Pull request stacked on any other branch | The light set plus macOS; the heavy suites only with `full-ci` | 40 minutes |

The light set exists because macOS was the critical path of almost every run:
twelve runs before the change took 45–55 minutes of wall time, of which
runner queueing was mostly under ten, portable macOS 35–49 and acceptance
about 34; every other job finished in about 15. A pull request into a train
only needs to show it compiles, lints and passes the portable suite on Linux,
with the coverage floor (`COVERAGE_MIN_LINES`, which only the Linux job
measures). Nothing is lost, only moved: the merge queue on each `release-*`
branch runs every job on the batch merged onto the train before any of it
lands, and the push to the train runs everything again on the result. So
every commit on a train, and the gate PR into `main`, has full coverage. Label
a pull request `full-ci` when you want the heavy suites on the PR itself, for
example for a change to privileged Linux, cluster or acceptance code. On a
pull request into a train the label leaves out the three macOS shards, which
each run took from a pool that queued for over an hour; the merge queue still
runs them before the change lands. The evidence gate calls this selection
`train-full` and accepts it only on a `pull_request` event. For macOS-only
code, check it on a Mac with `make test` before the queue does.

The merge queue requires one check, `ignored-test evidence`. It waits for
every other job, fails if any of them failed or was cancelled, and then runs
the evidence aggregation below, so a skipped job can only count when select
jobs left it out. On a merge-queue group, a push or a manual run the
aggregation demands every owner; only a pull request may qualify with fewer.
Evidence there binds to the queue's temporary group commit
(`github.event.merge_group.head_sha`), the commit it checked out.

A newer push to the same pull request, or to the same train, cancels the
older run; pushes to `main`, merge-queue groups and nightly runs each keep
their own. Closing (or merging) a pull request cancels its run too: the
`closed` event starts a run in the same group whose only job is a no-op, and
every other job skips it
([#1054](https://github.com/reliaburger/reliaburger/issues/1054)). When GitHub retargets a stacked pull request,
[`ci-retarget.yml`](../.github/workflows/ci-retarget.yml) reruns CI with the
new base, so the new base's set applies without another push. The fixtures in
[`test_select_jobs.py`](../scripts/ci/test_select_jobs.py) cover each case.

Every job that downloads an artefact (the cluster and acceptance jobs fetch
the nextest archive, the evidence gate fetches every envelope, and the V02
loops do both) gives each download 5 minutes and retries a failed one once.
A download normally takes a minute or two, but one stalled for 13 minutes and
pushed `acceptance` past its 40-minute limit
([#977](https://github.com/reliaburger/reliaburger/issues/977)). Now a stall
costs at most 10 minutes, and the second attempt usually recovers it. The
jobs keep their own limits.
[`test_ci_workflow.py`](../scripts/ci/test_ci_workflow.py) fails any download
step without its own limit or its retry.

### Portable macOS in three shards

The portable suite on macOS runs as three parallel jobs, `portable macOS
(1/3)` to `(3/3)`, each running `make test` with `--partition hash:K/3`
(`SHARDS` in [`workflow_assembly.py`](../scripts/ci/workflow_assembly.py)).
A hash partition keeps each test in the same shard whatever else is added, so
a failure in shard 2 reproduces locally with
`cargo nextest run --partition hash:2/3`. Each shard compiles the suite
itself, and compiling is most of a macOS job: on the first sharded run each
shard spent about 38 minutes compiling and under 3 testing (7 unsharded), so
a shard took 42–44 minutes. That included a second build of the library and
binaries, 8–10 minutes of it, until `warm-executables` switched to
`cargo test --no-run`
([#846](https://github.com/reliaburger/reliaburger/issues/846)). Across nine
runs before that change, shard jobs took 23.6–46.1 minutes, with the slowest
shard in a run at a median of about 41.6. On the first full run after it,
shards took 31.0–35.8 minutes, the "Portable tests" step 29.1–34.7 of that,
still a few minutes longer than acceptance (26.5–32.6). Building once and sharing
a nextest archive would take the compile off two of the three runners, but not
off the critical path. One shard runs `make lint-macos` and another the
doctests. A shard re-run on its own keeps its own attempt; the evidence gate
treats each shard as its own job when it checks attempts.

Each shard seals its own envelope (`contract-darwin-portable-darwin-K`) and
sets only its own output. A nextest list under a partition still names every
test, marking the other shards' tests as `mismatch` with reason `partition`,
so the evidence gate merges the three: they must list the same tests, run
disjoint parts of them, run all of them between them, and together complete
every required case. A missing, failed or swapped shard fails the gate.
[`test_shard_partition.py`](../scripts/ci/test_shard_partition.py) checks that
merge against what the pinned nextest really prints, on a throwaway crate.
Coverage is unaffected: only the Linux portable job measures it.

## Finite contracts and current evidence

[`tests/contracts/manifest.json`](../tests/contracts/manifest.json) is the finite
selection contract for the audit fixes and their final lifecycle controls.
[`ignored-bindings.json`](../tests/contracts/ignored-bindings.json) preserves the
exact original ignored-test identities. The generated
[matrix](testing/qualification-matrix.md) lists concrete cases once and records
their applicable gates; it isn't a cross product of every test and every host.
Reading the source can't prove that a particular host/feature combination
compiled or ran a case. Actual discovery
must confirm the literal binary and full name before completion can qualify it.

The workflow has eleven owners: Linux portable coverage, macOS portable tests,
optimised cron boundaries, rootless Linux, privileged Linux storage/runtime,
owned OCI interruptions, cluster, slow acceptance, node upgrade, cluster upgrade
and standard registry clients. They keep their full existing selections and
prerequisites. The finite cases are required observations within those commands,
not permission to run a reduced replacement suite. The independent cron crate
imports the actual production parser; its two required boundary oracles don't
replace the owner's other imported tests.

`workflow_adapter.py produce` observes the assigned owner and seals its discovery,
completion and input hashes. `aggregate` derives the required owners from the
current changes-job outputs, verifies the workflow's waited successful job
results and matches their trusted output digests. It rejects a missing owner, an unfinished or skipped
required case, a failed whole command, or mismatched commit, run, attempt, host,
selector or artifact. Archived suites also bind their executable/archive bytes
to the successful current builder. Keep the failure artifacts; they explain a
failure but cannot qualify another gate.

The cluster and acceptance jobs extract the downloaded archive once each
([#1050](https://github.com/reliaburger/reliaburger/issues/1050)).
`workflow_adapter.py extract` checks the archive's digest against the build
job's output, runs `cargo nextest list --archive-file tests.tar.zst
--extract-to . --list-type binaries-only`, and writes
`target/archive-extraction.json`: the archive digest, the command and the
digests of `target/nextest/binaries-metadata.json` and `cargo-metadata.json`.
Its step output carries the receipt's digest, and every owner in the job takes
it as `--archive-extraction`. Each owner's `list` and `run` then use
`--binaries-metadata`, `--cargo-metadata`, `--target-dir-remap target` and
`--workspace-remap .`, and check the two metadata files before discovery. The
extracted binaries aren't rehashed: they never leave the job. `aggregate`
refuses an archived owner whose selectors open the archive again, whose
metadata differs from its job's receipt, or whose receipt names another
archive or another nextest. A re-run job extracts again for its own attempt,
even when the archive came from an earlier one.

"Re-run failed jobs" works without a new commit. GitHub gives the re-run jobs
attempt N+1 and carries over the outputs and artifacts of the jobs that passed
in attempt N. So each seal keeps the attempt that produced it, and `aggregate`
accepts any attempt from 1 up to its own, provided the commit, run and host are
exactly its own. Every owner of one job must share one attempt. An archived
suite re-run on its own takes the builder's attempt from the archive runtime
file, whose digest the build job published. The trusted job output still picks
the one seal that counts, and GitHub reports the latest attempt of each job, so
a seal from another commit or run, a later attempt or an earlier attempt that a
re-run replaced can't qualify.

Linux coverage wraps the existing `make coverage` once, including its clean,
instrumented nextest run, LCOV/HTML reports and unchanged line floor. Discovery
uses that run's instrumented environment, selectors and target directory.
A separate ordinary test run doesn't validate the coverage run. The owner also
checks actual compiler/tool bytes and the runtime parent/child context, so a
successful child receipt cannot erase a later failed report or floor check.
Manual OCI runs have an observed operator and fresh manual session. They can
help diagnosis, but their receipts cannot satisfy CI authority.

The original seventeen non-CI source classifications keep their manual or
subprocess-parent policy in [the generated policy](testing/manual-owner-policy.json).
Apple Container requires a suitable macOS host; portable fake-CLI tests don't
prove its real runtime. GPU, S3 and physical reboot/power-cut checks retain their
own prerequisites. Three Smoker resource controls use temporary files to check
cgroup-setting I/O and restoration. They don't establish real node pressure;
the actual node-pressure/OCI owners provide their separate evidence.

## Controls that show the test can fail

Keep tests-first and sensitivity records beside the successful regression.
For a sensitivity control, make one explicit behaviour-changing variant that
breaks the contract, then run the exact selected regression with the same
prerequisites. Record the changed bytes, selector, observed failure and repaired
result. A compiler error, absent test or missing runtime prerequisite is setup
failure, not proof that the assertion detects the defect. Synthetic Python
fixtures can establish an evidence guard's refusal rules; they can't establish
Rust, coverage, OCI or real-node outcomes.

Publication controls pause the real writer at a named boundary, cancel the
caller or inject a specific I/O failure, and then observe queries, retry and
reopen. A pending batch belongs to the store and its writer, not the request
future. Capture identity must travel from the same opened file that supplied
the bytes into the immutable checkpoint metadata. Directory-entry confirmation
and immutable publication identity remain part of acknowledgement. These
controlled local/object-store cases don't establish power-loss survival or a
real cloud's durability guarantees; those have separate owners.

## Inside a live cluster

Relish carries its own test runner, so you can check a real cluster (yours)
rather than trust ours. `relish test` runs a catalogue of live cases (service
discovery, ingress, volumes, secrets, jobs, identity, deployments and more)
inside temporary, leased namespaces that clean themselves up.
`relish test --chaos` runs a separate suite that kills the leader, kills a
worker, partitions a minority and exhausts a node, then checks the cluster
heals.

- Catalogue: [`src/testkit/cases/`](../src/testkit/cases)
- Chaos scenarios: [`src/testkit/chaos/`](../src/testkit/chaos)
- Leases and safety rails: [`src/testkit/lease.rs`](../src/testkit/lease.rs), [`src/testkit/safety.rs`](../src/testkit/safety.rs)
- Proven against a real cluster: [`tests/relish_test_catalogue.rs`](../tests/relish_test_catalogue.rs)

## Pulling the plug

Killing a process isn't a power cut: page-cache writes survive one and not the
other. These fixtures hard-power a disposable VM off mid-write, boot it, and
check that everything acknowledged is still there.

| Fixture | Driver | Record |
|---|---|---|
| Log and metrics exporters, council backups, lease stores | [`tests/power_cut.rs`](../tests/power_cut.rs), [`qualify-storage-power-cut.sh`](../scripts/release/qualify-storage-power-cut.sh) | [2026-10-16](qualification/2026-10-16-power-cut-reboot.md) (first run: [2026-09-25](qualification/2026-09-25-v02-power-cut.md)) |
| Container and network ownership across a reboot | [`qualify-oci-reboot.sh`](../scripts/release/qualify-oci-reboot.sh), [`qualify-discovery-reboot.sh`](../scripts/release/qualify-discovery-reboot.sh) | same |

The first run of these found two real data-loss bugs, both now fixed. They need
a VM that can lose power, so no workflow runs them; they are step 5 of the
[release gate](releasing.md#staging-a-candidate) instead. One command runs all
five lanes on a fresh VM and records them:

```sh
scripts/release/qualify-power-cut-reboot.sh --commit SHA --iterations 10 \
  --record-dir docs/qualification/DATE-power-cut-reboot
```

[`qualify-power-cut-reboot.sh`](../scripts/release/qualify-power-cut-reboot.sh)
makes a clean checkout of `SHA`, creates `rb-pcut-<short sha>` from
[`power-cut-vm.yaml`](../scripts/release/power-cut-vm.yaml) with only that
checkout mounted, read-only, checks the swap and the mount from inside the
guest, runs each lane `N` times, stops at the first failure, writes the records
and a rule-of-three summary, and deletes the VM. It never touches
`reliaburger-test`. [`test_power_cut_reboot.py`](../scripts/release/test_power_cut_reboot.py)
(in `make test-ci-scripts`) checks its decisions against a fake `limactl`.

## Before a release

Nothing ships that hasn't been built once, signed, staged and installed the way
a user would install it, then soaked under faults for hours. The runbook is
[`releasing.md`](releasing.md); the homepage's
[How a release is tested](https://reliaburger.com/#release-testing) is the
short version, and the [release log](release-log.md) is the track record: what
each release's soaks found, and how many candidates it took.

In order: `build.yml` builds and signs every binary and guest image at one
commit, `stage.yml` publishes them as a private pre-release, the gates below
run against those exact files, and `promote.yml` copies them, byte for byte,
into the release once the maintainer tags the commit. Nothing is rebuilt
after the first step.

| Gate | What it does | Where |
|---|---|---|
| Staged install | `curl \| sh` against the exact signed candidate, from empty caches, then the homepage tour and a full teardown | [`qualify-staged-install.sh`](../scripts/release/qualify-staged-install.sh), [records](qualification/) |
| Sustained soak, fast tier | About an hour on a 10-minute cycle: every fault kind (killed agents, powered-off VMs, chaos faults, upgrade round trips) and every special (graceful restart, quorum loss, every VM off), with invariants checked every 30 seconds. Run after each round of fixes; it catches what shows up in the first hour | [`qualify-sustained.sh --tier fast`](../scripts/release/qualify-sustained.sh), [plan](plans/2026-09-25-v02-sustained.md); up to about 4 h without upgrade walks on a hosted Linux runner with [`soak.yml`](../.github/workflows/soak.yml), as a diagnostic that passes no tier ([runbook](releasing.md#soaking-a-candidate-in-ci)) |
| Job qualification, fast tier (0.2.0 exit gate, D18-C) | The same soak with jobs at the #668 target (1,390 accepted a second by default) and a node-level fault every cycle: inter-node delay, a full disk, slow fsync. About an hour compressed; every job mode overlaps every fault kind | [`qualify-jobs-sustained.sh --tier fast`](../scripts/release/qualify-jobs-sustained.sh), [plan](plans/archive/2026-10-10-plan-release-job-soak.md#job-qualification-tiers-686-decision-d18-c) |
| Final soak | 8 hours of faults on the hourly schedule, then a 2-hour quiet tail that arms every leak checker on every node ([#1062](https://github.com/reliaburger/reliaburger/issues/1062)), once per final candidate: the job soak above on the full schedule, so it checks everything the V02 final does as well as the jobs, and it's the only final soak the gate runs (maintainer decision, 17 October 2026, [#991](https://github.com/reliaburger/reliaburger/issues/991)). It's there for slow accumulation: bun memory growth, a registry 503 and a memory alert first showed up between 4.8 and 6.2 hours into the 12-hour run of 25 September. Only a clean final-tier run whose execution inventory is complete passes: every upgrade walk, fault kind, node fault and special its 8-hour schedule owes, the final job drain and the candidate bytes it verified, counted from what ran rather than from the run's labels ([#1061](https://github.com/reliaburger/reliaburger/issues/1061)). Fault and node choices come from a logged seed that `--seed` repeats, and every leak checker must be armed for 60 minutes on every node, with one unbroken 30-minute window, which the final's quiet tail makes possible ([#1062](https://github.com/reliaburger/reliaburger/issues/1062)). Records the September plan's measurements without gating on them, against targets restated for a soak (maintainer decision, 15 October 2026, [#668](https://github.com/reliaburger/reliaburger/issues/668)). `qualify-sustained.sh --tier final` still runs the V02 final alone, outside the gate | [`qualify-jobs-sustained.sh --tier final`](../scripts/release/qualify-jobs-sustained.sh), [plans](plans/2026-09-25-v02-sustained.md), [runbook](releasing.md#staging-a-candidate) |
| Loops | Upgrade, council recovery and lease tests repeated for hours on Linux x86, Linux Arm and macOS | [`v02-loops.yml`](../.github/workflows/v02-loops.yml) |
| Power cut and reboot | A disposable VM forced off mid-write for each storage path (exporters, leases, council backups), and rebooted with containers and eBPF network rules in place, 10 times per lane, or 25 when the release changes storage, locking or ownership (0.2.0 ran 25); every acknowledged write and every owned resource must survive ([above](#pulling-the-plug)) | [`qualify-storage-power-cut.sh`](../scripts/release/qualify-storage-power-cut.sh), [`qualify-oci-reboot.sh`](../scripts/release/qualify-oci-reboot.sh), [`qualify-discovery-reboot.sh`](../scripts/release/qualify-discovery-reboot.sh), [0.2.0 record](qualification/2026-10-16-power-cut-reboot.md) |
| Task-array benchmarks | Nightly on `main`, failing on a regression of more than 2x against the last baseline | [Benchmarks in CI](#benchmarks-in-ci) |


**What fails a release.** One occurrence of any of these, in any soak: a lost
write or a counter going backwards; a certificate close to expiry or a
rotation that stops; no leader for more than 30 seconds, or quorum not back
within two minutes; a fault that doesn't settle by its deadline; containers,
namespaces, eBPF pins, file descriptors or memory leaking after settling; an
upgrade that ends on the wrong version; a Bun panic or a restart no fault
explains; an agent loop turn over a second; or, with jobs, a run that executes
twice, goes missing or never drains. The full list is the soak plan's
[pass and fail](plans/2026-09-25-v02-sustained.md#4-pass-and-fail). Each
failure gets a root cause; a product fix means a new candidate and every soak
again. Before 1.0, a final run whose only failures match a documented harness
artefact still passes, with the evidence in its record.

**What the soaks don't prove.** They run three small VMs on one laptop, so
they say nothing about large clusters, real networks between machines or
failing hardware. A VM forced off is the closest they come to a power cut. The
fault schedule isn't production traffic, and zero failures in 25 runs rules out
failure rates above about 12% (the rule of three), not rare races.

Every soak record also gets charts of each node's Bun memory, open fds and CPU,
and of accepted jobs a second when jobs ran, with the faults marked along the
time axis. `sustained_check.py render` draws them beside the record, and
`sustained_check.py render-charts EVIDENCE DIRECTORY` redraws them from a kept
evidence directory ([plan](plans/archive/2026-10-17-soak-charts.md)). The
homepage shows the released version's final soak charts
([how](website/README.md#soak-charts)).

## When a test flakes

CI runs with `retries = 0`. A test that passes on a re-run still failed once,
so it goes in the [known flakes register](flakes.md) the same
day, with its observed failure, command and retained evidence. Write the cause
as unknown until a control establishes it. A diagnostic pass does not replace
the failed full run. The row stays until a fix has landed and a loop that used
to fail passes. Most rows turned out to be product bugs, not test bugs.

The harness itself is described in [`design/test-harness.md`](design/test-harness.md),
and the book's [Chapter 15](book/15-ready-for-production.md) tells the story of
building it.

### Jobs in the release soak

`make test-ci-scripts` also runs the release-controller regression tests. They
cover lost admission replies, resumption with the same request ID, monotonic
accounting, unknown-owner fences, independent effects, stale observations,
exact orphan detection and failure propagation into the final V02 verdict.
For the job qualification tiers they also parse `--plan` output (an hour-long
fast tier whose first four cycles carry every fault kind and every special,
node faults on every node, an 8-hour final whose cycles, quiet tail and run
budget fit together),
the target-load bodies and rate gate, retry-cause accounting (the harness's
cause list must match the fields of the product's `RetryCauses`, and an
unknown cause is refused by name), the retained-run limit (an 8- or 12-hour final
fits, a schedule that wouldn't is refused before it starts, a drained run
frees its slot an hour later, a full budget still polls and drains its runs,
and a run the candidate has pruned counts as drained only if the controller
saw it drain), CPU sampling
across restarts and the `.jobs.json` report beside the record. The
host-forward probes (#987) are tested against local servers and a fake
`limactl`: a port refused on the host while the guest serves it is one
`host-forward` failure with no settle timeouts, a forward that hangs up the
way Lima's gRPC forwarder does is caught too, a node down inside its guest is
left to the other checks, host-agent logs are kept before Lima truncates
them, and the controller's per-node API fallbacks reach the record. The
guest memory report (#985) runs against stub `ps`, `systemctl` and
`journalctl` and a fake `/proc`: it keeps meminfo, the top RSS, the guest
agent's RSS and fds and only the OOM report from the kernel log, and each
rise in unexplained Bun restarts is a failure of its own.

On rootful Linux with runc, BusyBox and cgroup v2, run the actual mixed-runtime
fixtures with:

```sh
sudo python3 scripts/release/job_soak_linux.py --bun /absolute/path/to/bun
```

This creates a separate data directory and a private mount/network identity,
uses only its own Bun PID, and checks real executions, limits, deadlines,
cancellation and retirement. It leaves compact diagnostics under the printed
`/var/tmp/rb-release-job-smoke-*` path. For repeat checks, `--image-cache
/var/tmp/rb-release-job-smoke-previous/images` copies a stopped fixture's image
content and image catalogue into the new fixture to avoid registry rate
limits. It preserves shifted rootfs ownership and the catalogue's format stamp,
so incompatible candidates still refuse it. It never reuses the
job state or owner journals. The small smoke run is not release
qualification. The [mixed job soak plan](plans/archive/2026-10-10-plan-release-job-soak.md)
describes the job gates included in the unchanged staged fast/final tiers.
