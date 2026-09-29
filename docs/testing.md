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
| Property tests | Invariants over generated inputs: scheduling, allocation, parsing | [`proptest`](https://docs.rs/proptest) blocks in `src/` | `make test` |
| Snapshot tests | CLI output, rendered config and TUI frames stay exactly as reviewed | [`insta`](https://insta.rs) snapshots, e.g. [`src/relish/snapshots/`](../src/relish/snapshots) | `make test` |
| Integration suite | Real Bun and Relish processes over HTTP, TLS and the registry | [`tests/suite/`](../tests/suite) | `make test` |
| Doctests | Examples in doc comments compile and run | `///` blocks | `make test-doc` |
| Lint and format | Clippy with warnings as errors, all features and none | [`Makefile`](../Makefile) | `make lint`, `make fmt-check` |
| Coverage floor | Line coverage of the portable suite never drops below 78.65% | `COVERAGE_MIN_LINES` in the [`Makefile`](../Makefile) | `make coverage` |
| Dependency audit | No new RustSec advisory; every exception is dated | [`security.yml`](../.github/workflows/security.yml), [exceptions](qualification/2026-09-18-dependency-exceptions.md) | `make audit` |

`make ci` runs the portable set locally, the same way CI does.

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
| Benchmarks | Gossip convergence from 5 to 1,000 nodes, plus the data plane on a live cluster (`relish bench`) | [`benches/`](../benches), [`src/testkit/bench/`](../src/testkit/bench) | `make bench`, `make bench-large` |

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
so it goes in the [known flakes register](progress.md#known-flakes) the same
day, with its cause, and stays there until a fix has landed and a loop that used
to fail passes. Most rows turned out to be product bugs, not test bugs.

The harness itself is described in [`design/test-harness.md`](design/test-harness.md),
and the book's [Chapter 15](book/15-ready-for-production.md) tells the story of
building it.
