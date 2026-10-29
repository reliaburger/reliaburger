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
| Dependency audit | No new RustSec advisory; every exception is dated | [`security.yml`](../.github/workflows/security.yml), [exceptions](qualification/2026-09-18-dependency-exceptions.md) ([12 Oct](qualification/2026-10-12-dependency-exceptions.md), [21 Oct](qualification/2026-10-21-dependency-exceptions.md), [24 Oct](qualification/2026-10-24-dependency-exceptions.md), [25 Oct](qualification/2026-10-25-dependency-exceptions.md), [26 Oct](qualification/2026-10-26-dependency-exceptions.md) and [28 Oct](qualification/2026-10-28-dependency-exceptions.md) reviews) | `make audit` |
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
| Multi-node clusters | Leader failover, council self-healing and full-loss recovery, placement, gossip, an image surviving the loss of the node it was pushed to (Linux only), and the 0.2.4 exit test: policies through `relish apply`, GitOps and a leader kill, an admin removing a deny-everything policy, and a controller failing a templated pair over | [`tests/cluster_failover.rs`](../tests/cluster_failover.rs), [`tests/council_self_healing.rs`](../tests/council_self_healing.rs), [`tests/council_disaster_recovery.rs`](../tests/council_disaster_recovery.rs), [`tests/placement.rs`](../tests/placement.rs), [`tests/cluster_task_arrays.rs`](../tests/cluster_task_arrays.rs), [`tests/registry_durability.rs`](../tests/registry_durability.rs) | `make test-cluster` |
| Feature demos | [`scripts/demo/rollout.sh`](../scripts/demo/rollout.sh) (roll forward, roll back, a bad version rolled back and marked `RolloutFailed`) and [`scripts/demo/gitops.sh`](../scripts/demo/gitops.sh) (a `file://` repository synced, an app deleted from it, a hand-applied app left alone) run to completion on a one-node cluster each starts through [`lib.sh`](../scripts/demo/lib.sh); a step that doesn't do what it says fails the demo, and each must print the resource readout and summary [`resources.py`](../scripts/demo/resources.py) samples ([`test_demo_resources.py`](../scripts/ci/test_demo_resources.py) covers its parsing and arithmetic). [`test_demo_scripts.py`](../scripts/ci/test_demo_scripts.py) checks on every run that they stop on the first error and swallow nothing | [`tests/demos.rs`](../tests/demos.rs) | `make test-cluster`; by hand `make rollout-demo`, `make gitops-demo` |
| Self-upgrade | Rolling binary upgrades and rollbacks with real signed binaries, workloads kept running | [`tests/self_upgrade.rs`](../tests/self_upgrade.rs), [`tests/self_upgrade_cluster.rs`](../tests/self_upgrade_cluster.rs) | `make test-upgrade` |
| Wall-clock acceptance | Timeouts, back-offs and leases that can't be tested with a paused clock | ignored tests in [`tests/integration.rs`](../tests/integration.rs) | `make test-slow` |
| Standard registry clients | docker, podman, skopeo and crane log in with an API token, verify the cluster CA, push and pull layers back through Pickle's TLS listener; a wrong token, another cluster's CA and an out-of-namespace push are refused | [`tests/suite/registry_standard_clients.rs`](../tests/suite/registry_standard_clients.rs) | `make test-standard-clients` (Linux, with the four clients and `sudo scripts/standard-clients-host.sh --provision`) |
| Benchmarks | Gossip convergence from 5 to 1,000 nodes, one scheduling pass (`plan_pass`) over 1,000 and 10,000 nodes, policy evaluation per app for 10 to 1,000 policies, in-process task-array costs, plus the data plane on a live cluster (`relish bench`) | [`benches/`](../benches), [`src/testkit/bench/`](../src/testkit/bench) | `make bench`, `make bench-large`, `make bench-task-arrays` ([which job](#benchmarks-in-ci)) |

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
| `nightly-qualify.sh` (the power-cut runs and the soak tiers, one after another on a train's tip) | The quiet host with Lima, a staged build of the tip for the soaks, and the signed soak bun for the job final | The quiet host every night, from 0.2.4 ([nightly qualification](releasing.md#nightly-qualification-on-the-train-tip)); `nightly.py freeze-check` compares the candidate with the latest green night. `make test-ci-scripts` tests its plan, record and freeze rule ([`test_nightly.py`](../scripts/release/test_nightly.py)) |
| `qualify-profile.sh` (the release profile: where the time goes, against the previous release) | A quiet Mac with Lima: no build or soak beside it. It creates and deletes a disposable VM, and runs `perf` on each version's never-shipped profiling build from `build.yml` | The maintainer, on every release from 0.2.4, after the tag ([release step 9](releasing.md#staging-a-candidate)). Its rules report what they would flag and don't gate yet. `make test-ci-scripts` tests its parsing, medians, rules and README ([`test_profile.py`](../scripts/release/test_profile.py)) |

### Benchmarks in CI

The `benchmarks` job in [`ci.yml`](../.github/workflows/ci.yml) runs `make
bench` (the gossip benchmarks, then [`benches/scheduler.rs`](../benches/scheduler.rs):
a leader pass placing 150 replicas, and a converged pass, on 1,000 and 10,000
nodes, then [`benches/policy.rs`](../benches/policy.rs): compiling 10, 100 and
1,000 policies, and judging one app under them) and `make bench-large`, and the `task-array benchmarks` job runs `make
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
| Pull request into a merge train (`release-*`, e.g. `release-0-2-4`) | The light set: CI policy, dependency audit, lint and format, minimum Rust, the demo app, portable Linux (with coverage, doctests and the nightly-signing key test) and the optimised contract boundaries | about 25 minutes, set by portable Linux; up to 41 on a slow runner (#1421) |
| The same, labelled `full-ci` | The light set plus privileged Linux, the cluster, acceptance and upgrade suites, and the benchmarks the change touches | about 45 minutes |
| The gate PR (a train into `main`) | The light set on each push; with `full-ci`, the heavy suites too | as above |
| The gate PR labelled `release-gate` | Everything, the three macOS shards included; the evidence gate refuses less | about 45 minutes, plus any wait for macOS runners |
| Any other pull request into `main` | The light set plus the heavy suites and the benchmarks the change touches | about 45 minutes |
| Pull request stacked on any other branch | The light set; the heavy suites only with `full-ci` | about 25 minutes |
| Push to a train or to `main`, merge-queue group, nightly, manual, `build.yml`'s candidate build | Everything | about 45 minutes, plus any wait for macOS runners |

**macOS only where it adds something**
([#1443](https://github.com/reliaburger/reliaburger/issues/1443), maintainer
decision of 28 October 2026). The organisation's plan runs at most five macOS
jobs at once and a full run takes three, so between 25 and 28 October macOS
shards queued a median of 0 minutes but 24 at p90 and 47 at worst. A pull
request gets the three macOS shards only when

- it touches macOS-specific code: `src/grill/apple*`, any Rust file with a
  `target_os = "macos"` cfg before or after the change, the installers
  (`docs/website/install.sh`, `scripts/release/install.sh.in`) or
  `scripts/demo/`; a gate PR's diff holds the whole train, so this rule
  doesn't apply to it;
- it carries the `macos` label (any code PR, Dependabot's included); or
- it's the gate PR labelled `release-gate`.

Dependabot's PRs never get macOS otherwise; the nightly covers them. Every
push to a train or to `main`, the nightly and the candidate build still run
all three shards, so no commit reaches a train or `main` without them.

The light set exists because macOS was the critical path of almost every run:
twelve runs before the change took 45–55 minutes of wall time, of which
runner queueing was mostly under ten, portable macOS 35–49 and acceptance
about 34; every other job finished in about 15. A pull request into a train
only needs to show it compiles, lints and passes the portable suite on Linux,
with the coverage floor (`COVERAGE_MIN_LINES`, which only the Linux job
measures). Nothing is lost, only moved: the push to the train runs every job
on the result, so every commit on a train has full coverage. The gate PR
doesn't run it all again on each push, since that push just did; it runs
everything once it's labelled `release-gate` at code freeze. Label a pull
request `full-ci` when you want the heavy suites on the PR itself, for
example for a change to privileged Linux, cluster or acceptance code. The
evidence gate calls a heavy selection without macOS `train-full` and accepts
it, like the light set, only on a `pull_request` event, and never on a
`release-gate` PR. For macOS-only code the PR runs the shards itself; for
anything else, check it on a Mac with `make test`, or add the `macos` label.

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

Every job that downloads an artefact with `actions/download-artifact` (the
evidence gate fetches every envelope, and the V02 loops fetch their archives)
gives each download 5 minutes and retries a failed one once. A download
normally takes a minute or two, but one stalled for 13 minutes and pushed
`acceptance` past its 40-minute limit
([#977](https://github.com/reliaburger/reliaburger/issues/977)). Now a stall
costs at most 10 minutes. The jobs keep their own limits.
[`test_ci_workflow.py`](../scripts/ci/test_ci_workflow.py) fails any download
step without its own limit or its retry.

That wasn't enough for the cluster and acceptance jobs' nextest archive. At
3.0 GB its download took 33 s to almost 5 minutes in passing runs, so the
limit was a cliff, and a retry started again from the first byte. So the
`build tests` job archives only the test binaries those jobs run
(`ARCHIVED_BINARIES` in
[`workflow_assembly.py`](../scripts/ci/workflow_assembly.py); a test fails
when an archived owner's make filter names a binary the list lacks), and
[`fetch_artifact.py`](../scripts/ci/fetch_artifact.py) fetches it. It lists
the run's artefacts through the API (the jobs, and every workflow that calls
`ci.yml`, grant `actions: read`), resumes a dropped or stalled transfer with
an HTTP `Range` request, abandons an attempt after 60 s without a byte, logs
its progress, checks the zip's digest and stops at 13 minutes, inside the
step's 14.

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

## Fuzzing

Every decoder that reads bytes from another machine has a
[cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html) target
(libFuzzer) ([#1398](https://github.com/reliaburger/reliaburger/issues/1398);
plan: [`plans/2026-10-28-research-kubernetes-release-testing.md`](plans/2026-10-28-research-kubernetes-release-testing.md)):

| Target | Decoder | Bytes from |
|---|---|---|
| `gossip_datagram` | `mustard::message::decode_datagram` | anyone who can reach the gossip UDP port |
| `raft_rpc_frame` | `council::network::decode_and_admit` | a council peer's TCP frame |
| `dns_query` | `onion::dns::decode_query` (and the answers built from it) | any workload |
| `council_backup` | `council::backup::decode_backup` | a backup file an operator hands us |
| `oci_manifest_push` | `pickle::api::parse_pushed_manifest` | a client pushing to Pickle |
| `oci_manifest_upstream` | `grill::oci_pull::parse_manifest` | an upstream registry |
| `join_csr` | `sesame::ca::sign_node_csr` | a joining node |
| `app_config` | `config::Config::parse` | `relish apply` or a GitOps repository |
| `node_config` | `config::node::NodeConfig::parse` | a node's config file |

A target's body lives in [`src/fuzz/mod.rs`](../src/fuzz/mod.rs), behind the
`fuzzing` feature, and [`fuzz/fuzz_targets/`](../fuzz/fuzz_targets) only wraps
it. A body does more than wait for a panic: where the decoder has an encoder,
an accepted input is decoded, re-encoded, decoded again and compared. The
Raft frame and upstream manifest compare JSON values, because key order isn't
meaning; gossip compares postcard bytes. Other checks are specific to the
decoder: the push target checks that Pickle serves a manifest as the media type
the client declared, and the CSR target checks that the certificate names the
node the CA chose, never one the CSR asked for.

The `fuzz/` crate has its own `[workspace]`, so the root crate's `cargo`
commands never build it. Three things run the targets:

| Command | Toolchain | What it does |
|---|---|---|
| `make test` (in `make ci`) | stable | `fuzz::tests`: every fixture seed is accepted and round-trips, every file in `fuzz/corpus/` runs without a panic, 512 mutated seeds run without a panic, and every target has its binary |
| `make fuzz-corpus-check` | stable | Only those tests |
| `make fuzz-smoke` | nightly, cargo-fuzz | Builds every target with sanitizers and runs each for `FUZZ_SECONDS` (default 10). `make fuzz-tools` says how to install what's missing. It builds for the nightly toolchain's host (`FUZZ_TARGET`), because cargo-fuzz otherwise picks the triple it was built for, and CI's prebuilt cargo-fuzz is a musl binary that AddressSanitizer can't use |

[`fuzz.yml`](../.github/workflows/fuzz.yml) runs `make fuzz-smoke` nightly at
300 seconds a target, and for 30 seconds on a pull request that touches a
decoder (or by hand, with any budget). The job summary has each target's last
libFuzzer status line (executions, coverage, corpus size). A crash fails the
job and its input is kept as the `fuzz-crashes` artefact. The workflow pins a
dated nightly (`FUZZ_TOOLCHAIN`) and caches the sanitizer build under one
rust-cache key for nightly and pull request runs; a floating nightly would
change that key every day. Move the pin forward by hand when a newer nightly
is needed. Locally, `FUZZ_TOOLCHAIN` defaults to `nightly`.

A crash is handled like a flake: an issue the same day. Then add the input as
a unit test next to the decoder, watch it fail, and fix it (tests first).
Locally, `cargo +nightly fuzz run <target> fuzz/artifacts/<target>/crash-...`
replays it.

The seed corpus in [`fuzz/corpus/`](../fuzz/corpus) comes from the decoders'
unit-test fixtures and the `examples/` configs
([`src/fuzz/seeds.rs`](../src/fuzz/seeds.rs)). `make fuzz-corpus-seed`
rewrites it; run it after a protocol or state bump, since old gossip and Raft
seeds then stop decoding. libFuzzer writes what it finds to
`fuzz/target/corpus/`, never into the checked-in seeds.

OSS-Fuzz is the follow-up: Google's continuous fuzzing runs Rust projects
through the same cargo-fuzz targets
([guide](https://google.github.io/oss-fuzz/getting-started/new-project-guide/rust-lang/)).
Applying needs a `projects/reliaburger/` directory in the OSS-Fuzz repository
(a `project.yaml`, a Dockerfile and a `build.sh` that runs `cargo fuzz build -O`),
once the targets have run clean nightly for a while.

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

## Hollow nodes

Our biggest real cluster is three VMs. To see what a council does with
hundreds of nodes in front of it, [`tests/hollow_nodes.rs`](../tests/hollow_nodes.rs)
borrows Kubernetes' Kubemark idea ([#1401](https://github.com/reliaburger/reliaburger/issues/1401)):
a real three-member council in one process, and a fleet of hollow nodes in
another. A hollow node ([`tests/support/hollow.rs`](../tests/support/hollow.rs))
runs real gossip, the Raft endpoint, the reporting worker, a real agent and its
placement reconciler. Only its runtime is fake: it "runs" every workload at
once and reports the capacity the harness gives it. The council's size is held
at three, so no hollow node becomes a voter.

| Command | What it does |
|---|---|
| `make test-hollow` | Twenty hollow nodes: they converge, a replica runs on each, a quarter stop and come back. A check that the harness works, not a measurement |
| `scripts/release/qualify-hollow-nodes.sh --sizes 100,500,1000` | One measured run per size, one at a time, each to `hollow-<N>.json`, then a table from [`hollow_summary.py`](../scripts/release/hollow_summary.py) |

A run measures gossip convergence, the council's CPU and memory (it has its
own process, so `ps` can tell them apart), Raft commit latency from a no-op
write each second, how long one apply takes to put a replica on every node,
the latency of twenty small applies, and, when a tenth of the fleet loses all
its traffic at once, how long the council takes to notice and to run the lost
replicas elsewhere. The script won't start a size while the host's load
average is over 40, and the harness stops a run that crosses it. Nothing hollow
is in the library or the release binary: it lives under `tests/`. Records are
in [`qualification/`](qualification/) ([first run](qualification/2026-10-28-hollow-nodes.md)).

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

From 0.2.4 the gate scales with the release
([#1363](https://github.com/reliaburger/reliaburger/issues/1363),
[how](releasing.md#the-release-gate)). A patch release runs the staged
installs, the fast soak tiers and the power-cut and reboot runs; the final
soak is for minor releases and any release that changes durable state, the
wire or the agent loop (`scripts/release/release_tier.py` says which). The
long tiers run every night on the train's tip
(`scripts/release/nightly-qualify.sh`), so at code freeze
`scripts/release/nightly.py freeze-check` only has to show that the latest
green nightly ran the candidate's code. Performance records (benchmarks, the
profile, the day run) are published after the tag. `make test-ci-scripts`
tests the pure parts of all three: the tier rule, the nightly's plan, record
and freeze rule, and `build.yml`'s reuse of a CI run that passed on the
candidate's commit ([`ci_reuse.py`](../scripts/release/ci_reuse.py)).

| Gate | What it does | Where |
|---|---|---|
| Staged install | `curl \| sh` against the exact signed candidate, from empty caches, then the homepage tour and a full teardown | [`qualify-staged-install.sh`](../scripts/release/qualify-staged-install.sh), [records](qualification/) |
| Sustained soak, fast tier | About an hour on a 10-minute cycle: every fault kind (killed agents, powered-off VMs, chaos faults, upgrade round trips) and every special (graceful restart, quorum loss, every VM off), with invariants checked every 30 seconds. Run after each round of fixes; it catches what shows up in the first hour | [`qualify-sustained.sh --tier fast`](../scripts/release/qualify-sustained.sh), [plan](plans/2026-09-25-v02-sustained.md); up to about 4 h without upgrade walks on a hosted Linux runner with [`soak.yml`](../.github/workflows/soak.yml), as a diagnostic that passes no tier ([runbook](releasing.md#soaking-a-candidate-in-ci)) |
| Job qualification, fast tier (0.2.0 exit gate, D18-C) | The same soak with jobs at the #668 target (1,390 accepted a second by default) and a node-level fault every cycle: inter-node delay, a full disk, slow fsync. About an hour compressed; every job mode overlaps every fault kind | [`qualify-jobs-sustained.sh --tier fast`](../scripts/release/qualify-jobs-sustained.sh), [plan](plans/archive/2026-10-10-plan-release-job-soak.md#job-qualification-tiers-686-decision-d18-c) |
| Final soak | 8 hours of faults on the hourly schedule, then a 2¼-hour quiet tail that arms every leak checker on every node ([#1062](https://github.com/reliaburger/reliaburger/issues/1062)), once per final candidate of a release that needs the full gate (a minor release, or one that changes formats or the agent loop): the job soak above on the full schedule, so it checks everything the V02 final does as well as the jobs, and it's the only final soak the gate runs (maintainer decision, 17 October 2026, [#991](https://github.com/reliaburger/reliaburger/issues/991)). It's there for slow accumulation: bun memory growth, a registry 503 and a memory alert first showed up between 4.8 and 6.2 hours into the 12-hour run of 25 September. Only a clean final-tier run whose execution inventory is complete passes: every upgrade walk, fault kind, node fault and special its 8-hour schedule owes, the final job drain and the candidate bytes it verified, counted from what ran rather than from the run's labels ([#1061](https://github.com/reliaburger/reliaburger/issues/1061)). Fault and node choices come from a logged seed that `--seed` repeats, and every leak checker must be armed for 60 minutes on every node, with one unbroken 30-minute window, which the final's quiet tail makes possible ([#1062](https://github.com/reliaburger/reliaburger/issues/1062)). Records the September plan's measurements without gating on them, against targets restated for a soak (maintainer decision, 15 October 2026, [#668](https://github.com/reliaburger/reliaburger/issues/668)). `qualify-sustained.sh --tier final` still runs the V02 final alone, outside the gate | [`qualify-jobs-sustained.sh --tier final`](../scripts/release/qualify-jobs-sustained.sh), [plans](plans/2026-09-25-v02-sustained.md), [runbook](releasing.md#staging-a-candidate) |
| Day run (0.2.3's record, [#1032](https://github.com/reliaburger/reliaburger/issues/1032)) | One real 24-hour day of host jobs on the three-node cluster, each node resized to 4 vCPU / 6 GiB first and host jobs running the scale-out record's `busybox-static`, with no faults, walks or specials, then cancellation and the drain. It fails unless host jobs average the day rate (23,149 a second, 2 billion a day, by default; never below 11,575, a billion), no node takes more than 20% over its slot-weighted fair share (as `relish bench` allows, [#1362](https://github.com/reliaburger/reliaburger/pull/1362)), the last full hour runs within 10% of the first, and every soak check holds for the whole day, with every leak checker armed. The record has each node's total and every hour's rate. A one-off record for 0.2.3, not a gate tier; whether it becomes one is the maintainer's later decision | [`qualify-jobs-sustained.sh --tier day`](../scripts/release/qualify-jobs-sustained.sh), [runbook](releasing.md#staging-a-candidate) |
| Loops | Upgrade, council recovery and lease tests repeated for hours on Linux x86, Linux Arm and macOS | [`v02-loops.yml`](../.github/workflows/v02-loops.yml) |
| Power cut and reboot | A disposable VM forced off mid-write for each storage path (exporters, leases, council backups), and rebooted with containers and eBPF network rules in place, 10 times per lane, or 25 when the release changes storage, locking or ownership (0.2.0, 0.2.1 and 0.2.2 ran 25); every acknowledged write and every owned resource must survive ([above](#pulling-the-plug)) | [`qualify-storage-power-cut.sh`](../scripts/release/qualify-storage-power-cut.sh), [`qualify-oci-reboot.sh`](../scripts/release/qualify-oci-reboot.sh), [`qualify-discovery-reboot.sh`](../scripts/release/qualify-discovery-reboot.sh), [0.2.2 record](qualification/2026-10-25-power-cut-reboot.md) |
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

### Latency SLOs

Everything above gates internal health. A user doesn't feel an agent-loop
turn; they feel how long `relish status` takes, how long an app takes to come
up, and how long a job waits before it starts. So every soak tier (V02 and job
qualification, fast, final and day) also measures five latencies outside fault
windows and puts a "Latency SLOs" table in its record
([#1400](https://github.com/reliaburger/reliaburger/issues/1400)). Kubernetes
does the same with its [published SLOs](https://github.com/kubernetes/community/blob/master/sig-scalability/slos/slos.md)
(API call p99 ≤ 1 s, pod start-up p99 ≤ 5 s); the
[research plan](plans/2026-10-28-research-kubernetes-release-testing.md) says
why we copied the idea and not the tooling.

| SLO (p99) | What's timed | Threshold | Mode |
|---|---|---|---|
| Job task start | A job task's start lag on a pooled executor, from the job campaign's buckets, in runs no fault window overlapped | ≤ 256 ms | Gated in the fast and final tiers |
| API read | `GET /v1/status?cluster=true` and `GET /v1/cluster/nodes` (what `relish status` and `relish nodes` ask) on every node, at every light check, from the host | ≤ 1,000 ms | Record only |
| API write | `relish apply` of the soak workloads, from the call to the Raft commit | ≤ 2,000 ms | Record only |
| App start | Each node's deploy of a new `soak-slo` instance, from its start until the instance passes its health check | ≤ 5,000 ms | Record only |
| Rolling update | From `relish apply` returning until every node runs the new `soak-slo` | ≤ 30,000 ms | Record only |

**Where the gated number comes from.** The job start lag is the one latency
earlier records already kept. Across the ten 0.2.0 to 0.2.2 soak records its
p99 was 2 to 8 ms at the V02 calibration load and 32 to 64 ms at the job
qualification's target load (64 ms once, in the 0.2.1 fast tier; 32 ms in
every final). There are no 0.2.3 soak records yet. The gate sits at 256 ms:
four times the worst value seen, still a quarter of the September plan's
1-second target, and a bucket bound, so the record's number is exact. A day
tier records it without gating, since nothing has measured start lag at the
day rate yet, and a run without a job campaign has nothing to judge. The
campaign keeps two sets of buckets: every drained run's start lag, for the
M8 number in the job report, and only the runs that no fault window
overlapped, for the SLO. A run that was active at any step inside a fault
window counts for M8 alone, so start lag during a leader kill's recovery
can't fail the quiet-cluster SLO
([#1438](https://github.com/reliaburger/reliaburger/issues/1438)). The
earlier records' 32 to 64 ms counted every run, faults included, so they
bound the quiet number from above.

**Record only, from 28 October 2026.** No soak before 0.2.4 timed the API,
an app start or a rolling update, so those four start in record-only mode:
the record shows the value and whether it's over the provisional threshold,
and never fails on it. The provisional thresholds are Kubernetes's for reads
and app start, and a guess with room to spare for writes and rolling updates.
Once the first 0.2.4 fast and final records exist, the maintainer sets each
gate from them, with margin, the same way as the job start.

**How it's measured.** At every light, settle and heavy check outside a fault
window, the harness times the two API reads on each node that is up
(`soak_slo.py api-latency`); the checker drops a node whose host forward is
cut ([#987](https://github.com/reliaburger/reliaburger/issues/987)). Twice a
cycle, outside fault windows, it bumps `soak-slo`'s generation (an idle
BusyBox `httpd` with a health check, one instance per node) and applies the
soak workloads without the job fixtures. The apply is a write; each node's
new entry in `GET /v1/deploys/history/soak-slo` is an app start, timed on
that node's own clock; and the time from the apply returning until every node
has one is the rolling update. Samples go into a bucketed histogram in the
evidence's `slo.json`, so each p99 is the upper bound of its bucket. A failed
call counts with the time it took, and a rollout that didn't finish within
180 s is counted and named in the record. `sustained_check.py render` fails
the tier when a gated SLO is over its threshold or has no samples, and names
the SLO in the record. `soak_slo.measure_api` is the same read probe as a
function, for the hollow-node test
([#1401](https://github.com/reliaburger/reliaburger/issues/1401)) to call.

**The trend.** `python3 scripts/release/soak_slo.py trend docs/qualification`
lists every record's SLO values, oldest first; for a record from before the
table existed it reads the job start lag from the record's `.jobs.json`.

The host reaches each node's API through a Lima port forward, so the API
numbers include the forwarder. They're what `relish` on the same laptop sees,
not what a client next to the node would.

## Checking the history, not just the invariants

The soak's invariants each look at one thing: the writer's sequence has no
gaps, the Redis counter never goes down, every job finishes once. A store can
pass all of them and still be wrong. A read served by a node that hasn't
caught up returns a value the cluster had already overwritten; a lease granted
twice gives two clients the same lock for a moment; a write acknowledged just
before a crash quietly disappears. etcd's
[robustness tests](https://github.com/etcd-io/etcd/blob/main/tests/robustness/README.md)
found exactly these by recording every operation and checking the whole
history against a model, so from 0.2.4 the soak does the same for the council
([#1399](https://github.com/reliaburger/reliaburger/issues/1399), from the
[release testing research](plans/2026-10-28-research-kubernetes-release-testing.md)).

**Linearizable** means every operation looks as if it took effect at one
instant somewhere between the client sending it and getting the answer, and
the answers are the ones those operations would give one at a time in that
order. Two operations that overlap may land either way round. Two that don't
overlap can't swap: once a renew has returned, every read that starts later
must see it. That rule is what invariants miss. A stale read breaks no counter
and loses no data; it is only wrong in the light of what came before it.

What runs, in every tier except the day run:

- **The client.** [`council_history.py`](../scripts/release/council_history.py)
  runs on the host for the whole soak. Four threads take turns holding test
  leases on two namespaces: grant, then renew and read a few times, then
  release, each request to a node picked at random. After every successful
  renew or read it reads the lease straight back through a different node,
  since a lagging node shows itself in the milliseconds after a write. A
  thread whose grant is refused reads the holder's lease instead. That's
  about six requests a second, about half of them council writes, which is
  small beside the job campaign's writes, so it barely touches the
  [latency SLOs](#latency-slos). The `soak-slo` rollouts take no test leases,
  so they never appear in the history. Every operation is a `call` and a `return` event in
  `council-history.jsonl` in the evidence directory, with monotonic and wall
  times. Test leases are what the council lets a client write and read back:
  the lease record is the key-value pair (lease id to expiry and state) and
  the namespace is the lock. There is no general key-value API to record.
- **The model.** A namespace has at most one lease; a grant succeeds only when
  it's free; a renew moves the expiry and a read returns the latest one; a
  release frees the namespace, in two steps (cleanup starts, then the lease
  goes), so a read in between may see it cleaning. The council's reaper may remove a lease once
  its expiry has passed, which the model allows, with 30 seconds' margin for
  the host's and the nodes' clocks.
- **Unknown answers.** A timeout, a dropped connection or a 5xx may or may
  not have taken effect, and may take effect later, after the client gave up.
  The checker allows both: such a write never returns, and it may be put
  anywhere after its call, or nowhere.
- **The check.** [`linearizability.py`](../scripts/release/linearizability.py)
  is a small port of [Porcupine](https://github.com/anishathalye/porcupine):
  a depth-first search over the operations that could come next, with
  memoisation, run on each namespace apart. Once the soak is over, a violation
  records a failure and fails the record. The record says how many operations
  were checked and shows the smallest failing slice: the shortest prefix of
  the history that fails, cut down to the shortest tail that fails from any
  starting state. The search is bounded per namespace, by a budget of states
  and a deadline of its own (600 s); a namespace that runs over is reported as
  *inconclusive* rather than hanging the soak. An inconclusive check fails a
  fast or final tier, since that soak would otherwise pass with no
  linearizability coverage; a custom run only reports it
  ([#1437](https://github.com/reliaburger/reliaburger/issues/1437)).

`make test-ci-scripts` runs the checker's unit tests
([`test_linearizability.py`](../scripts/release/test_linearizability.py)):
hand-made linearizable histories pass, and a stale read, a lost grant, renew
or release, and a lease held by two clients at once each fail with a slice of
two or three operations. [`test_council_history.py`](../scripts/release/test_council_history.py)
runs the client against an in-memory council, honest and with the bug below,
and checks the record. The gated
`cluster_failover::a_lease_history_through_a_leader_kill_is_linearizable`
(`make test-cluster`) runs the client against three in-process nodes while
it kills the leader, and checks the history.

**Showing it can fail.** The bug the issue names is a follower answering a
read from its own copy without confirming leadership: in
`test_lease_get_handler` (`src/bun/api/test_leases.rs`), add `&& false` to
the `confirmed_lease_leader` condition so no node forwards a read. With that
change the gated test above failed three runs out of three (28 October 2026,
about 1,950 operations a run): a renew returned an expiry, and a read through
another node 3 ms later returned the one before it. Without the change it
passed seven runs out of seven. The soak's VMs, with a real network and
fsync between them, lag more than in-process nodes, so the fast tier has a
wider window to catch the same bug.

The same check for job outcomes (exactly one completion per task-array chunk)
is later work.

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
