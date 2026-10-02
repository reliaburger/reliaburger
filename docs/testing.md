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
| Property tests | Invariants over generated inputs: scheduling, allocation, parsing | [`proptest`](https://docs.rs/proptest) blocks in `src/` | `make test` |
| Snapshot tests | CLI output, rendered config and TUI frames stay exactly as reviewed | [`insta`](https://insta.rs) snapshots, e.g. [`src/relish/snapshots/`](../src/relish/snapshots) | `make test` |
| Integration suite | Real Bun and Relish processes over HTTP, TLS and the registry | [`tests/suite/`](../tests/suite) | `make test` |
| Doctests | Examples in doc comments compile and run | `///` blocks | `make test-doc` |
| Lint and format | Clippy with warnings as errors, all features and none | [`Makefile`](../Makefile) | `make lint`, `make fmt-check` |
| Coverage floor | Line coverage of the portable suite never drops below 78.65% | `COVERAGE_MIN_LINES` in the [`Makefile`](../Makefile) | `make coverage` |
| Dependency audit | No new RustSec advisory; every exception is dated | [`security.yml`](../.github/workflows/security.yml), [exceptions](qualification/2026-09-18-dependency-exceptions.md) | `make audit` |
| CI scripts | Job selection, ignored-test owners and JUnit keeping, against fixture repositories and the real workflow | [`scripts/ci/test_*.py`](../scripts/ci) | `make test-ci-scripts` |
| Ignored-test owners | Every `#[ignore]` reason names who runs it ([below](#who-runs-an-ignored-test)) | [`ignored_owners.py`](../scripts/ci/ignored_owners.py) | `make check-ignored` |

`make ci` runs the portable set locally, the same way CI does: formatting,
lint, the portable suite, doctests, the CI scripts' tests and the ignored-test
owner check. `make ci-bench` is `make ci` followed by `make bench`. Neither
runs a privileged, cluster or upgrade suite; those have their own targets
below, because they need root, a provisioned host or a lot of wall time.

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
| Multi-node clusters | Leader failover, council self-healing and full-loss recovery, placement, gossip | [`tests/cluster_failover.rs`](../tests/cluster_failover.rs), [`tests/council_self_healing.rs`](../tests/council_self_healing.rs), [`tests/council_disaster_recovery.rs`](../tests/council_disaster_recovery.rs), [`tests/placement.rs`](../tests/placement.rs) | `make test-cluster` |
| Self-upgrade | Rolling binary upgrades and rollbacks with real signed binaries, workloads kept running | [`tests/self_upgrade.rs`](../tests/self_upgrade.rs), [`tests/self_upgrade_cluster.rs`](../tests/self_upgrade_cluster.rs) | `make test-upgrade` |
| Wall-clock acceptance | Timeouts, back-offs and leases that can't be tested with a paused clock | ignored tests in [`tests/integration.rs`](../tests/integration.rs) | `make test-slow` |
| Standard registry clients | `crane` logs in, pushes and pulls through Pickle's TLS listener | [`tests/suite/registry_standard_clients.rs`](../tests/suite/registry_standard_clients.rs) | `make test-standard-clients` (needs `crane`) |
| Benchmarks | Gossip convergence from 5 to 1,000 nodes, plus the data plane on a live cluster (`relish bench`) | [`benches/`](../benches), [`src/testkit/bench/`](../src/testkit/bench) | `make bench`, `make bench-large` |

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

That proves every test *has* an owner, not that the owner still selects it:
`--no-tests=fail` passes as long as a filter matches anything, so renaming a
test out of `test(/runc_/)` would drop it silently. CI closes that gap. Each
nextest suite's JUnit report is kept under its own name
([`keep-junit.sh`](../scripts/ci/keep-junit.sh)) and uploaded as
`junit-<job>` for 14 days, with the command, commit, run and host beside it,
even when the suite fails. The `ignored-test evidence` job then reads every
report, plus the OCI interruption logs, and fails when a test whose owner CI
runs (a target or script named in `ci.yml`) is missing from all of them.

Gates CI can't run are manual, so the evidence check skips them:

| Gate | Needs |
|---|---|
| `make test-apple` | Apple silicon with Apple Container |
| `make test-gpu` | An NVIDIA GPU and `nvidia-smi` |
| `make test-s3` | AWS credentials and `RELIABURGER_TEST_S3_URL=s3://bucket/prefix` |
| `qualify-oci-reboot.sh`, `qualify-discovery-reboot.sh`, `qualify-storage-power-cut.sh` | A disposable VM that can be rebooted or powered off ([below](#pulling-the-plug)) |

### Which jobs a pull request runs

[`select-jobs.sh`](../scripts/ci/select-jobs.sh) diffs a pull request against
its base (with renames split into a deletion and an addition, so moving code
into `docs/` still counts as code). Documentation-only changes skip the Rust
jobs. Pull requests into `main` or into a release's merge-train branch
(`release-*`, one per release, e.g. `release-1-1-4`) run the heavy suites;
pull requests stacked on any other branch skip them unless labelled
`full-ci`. When GitHub retargets a stacked pull request to `main`,
[`ci-retarget.yml`](../.github/workflows/ci-retarget.yml) reruns CI with the
new base, so the heavy suites don't wait for another push. Its fixtures, in
[`test_select_jobs.py`](../scripts/ci/test_select_jobs.py), cover each of
those cases.

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
| Log and metrics exporters, council backups, lease stores | [`tests/power_cut.rs`](../tests/power_cut.rs), [`qualify-storage-power-cut.sh`](../scripts/release/qualify-storage-power-cut.sh) | [2026-09-25](qualification/2026-09-25-v02-power-cut.md) |
| Container and network ownership across a reboot | [`qualify-oci-reboot.sh`](../scripts/release/qualify-oci-reboot.sh), [`qualify-discovery-reboot.sh`](../scripts/release/qualify-discovery-reboot.sh) | same |

The first run of these found two real data-loss bugs, both now fixed.

## Before a release

Nothing ships that hasn't been built once, signed, staged and installed the way
a user would install it. The runbook is [`releasing.md`](releasing.md).

| Gate | What it does | Where |
|---|---|---|
| Staged install | `curl \| sh` against the exact signed candidate, from empty caches, then the homepage tour and a full teardown | [`qualify-staged-install.sh`](../scripts/release/qualify-staged-install.sh), [records](qualification/) |
| Sustained soak, fast tier | About 90 minutes on a 10-minute cycle: every fault kind (killed agents, powered-off VMs, chaos faults, upgrade round trips) and every special (graceful restart, quorum loss, every VM off), with invariants checked every 30 seconds. Run after each round of fixes; it catches what shows up in the first hour | [`qualify-sustained.sh --tier fast`](../scripts/release/qualify-sustained.sh), [plan](plans/2026-09-25-v02-sustained.md); up to about 4 h without upgrade walks on a hosted Linux runner with [`soak.yml`](../.github/workflows/soak.yml) ([runbook](releasing.md#soaking-a-candidate-in-ci)) |
| Sustained soak, final tier | 8 hours on the hourly schedule, once per final candidate, for slow accumulation: bun memory growth, a registry 503 and a memory alert first showed up between 4.8 and 6.2 hours into the 12-hour run of 25 September. Only a clean final-tier run passes V02 | [`qualify-sustained.sh --tier final`](../scripts/release/qualify-sustained.sh), [plan](plans/2026-09-25-v02-sustained.md) |
| Loops | Upgrade, council recovery and lease tests repeated for hours on Linux x86, Linux Arm and macOS | [`v02-loops.yml`](../.github/workflows/v02-loops.yml) |

## When a test flakes

CI runs with `retries = 0`. A test that passes on a re-run still failed once,
so it goes in the [known flakes register](flakes.md) the same
day, with its cause, and stays there until a fix has landed and a loop that used
to fail passes. Most rows turned out to be product bugs, not test bugs.

The harness itself is described in [`design/test-harness.md`](design/test-harness.md),
and the book's [Chapter 15](book/15-ready-for-production.md) tells the story of
building it.
