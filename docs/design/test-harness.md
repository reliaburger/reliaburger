# Test harness

The test count used to answer the wrong question. It mixed portable correctness tests,
tests that returned successfully without running, privileged Linux checks, hour-long scale
simulations and Criterion benchmarks into one reassuring number. A green line could mean
"the behaviour worked" or "this laptop didn't have the prerequisite". Those aren't the
same thing.

This document defines the suites, their contracts and the evidence collected during the
July 2026 audit.

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
| `make test` | Portable unit, component and integration correctness via nextest | Linux and hosted macOS |
| `make test-no-default` | The same portable contract without default features | Linux |
| `make test-doc` | Rust documentation examples | Linux and hosted macOS |
| `make test-slow` | Required wall-clock health and retry acceptance | Linux |
| `make test-linux` | runc, namespaces, eBPF, Btrfs, Buildah and root-only tmpfs | Privileged Linux |
| `make test-cluster` | Gossip, placement, failover, healing, recovery and chaos | Linux, serial resource group |
| `make test-upgrade-node` | Real single-node binary replacement | Linux |
| `make test-upgrade-cluster` | Real rolling cluster replacement | Linux |
| `make test-apple` | Apple Container runtime | Manual Apple-silicon MacBook check |
| `make bench` | Criterion transport and 5–250-node measurements | Pull requests |
| `make bench-large` | Criterion 500- and 1,000-node measurements | Pull requests |
| `make bench-10k` | Deterministic 10,000-node scale acceptance | Pull requests |
| `make coverage` | Combined default and no-default portable line coverage | Pull requests |

Apple Container is the only manual runtime exception. GitHub's hosted macOS runners cannot
provide its nested virtualisation. Everything else runs on a pull request and release tags
must pass the same reusable validation workflow before publication.

## Gating rules

Use `#[cfg(target_os = "linux")]` when code cannot exist on the current target. The compiler
then omits it, so it must be counted as platform-specific rather than ignored.

Use `#[ignore = "requires …"]` for code that compiles but needs root, a provisioned runtime
or deliberate wall-clock time. The matching Make target supplies the prerequisite and runs
ignored tests only. The first assertion in an environment-gated test is a preflight: asking
for the suite without its prerequisite fails instead of manufacturing a pass.

Do not inspect the host inside a test and return successfully. If a portable prerequisite
such as `git` is missing, fail with a useful message. If behaviour depends on whether a tool
is installed, inject capability state or put the two environments in separate suites.

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
concurrently.

## Benchmarks and coverage

Correctness tests use small deterministic inputs and assert exact output. A 100 MB P2P pull
under five seconds was neither portable nor a correctness contract, so the test now checks
parallel multilayer fetching without a wall-clock threshold.

Criterion owns performance measurements. Fast and large gossip benchmarks share one seeded
simulation with the 10k acceptance test, time setup separately from convergence, and fail if
the network never converges. Criterion data is uploaded from CI, but we will not set a
regression threshold until runs are stable on consistent hardware.

`cargo-llvm-cov` combines the portable default and `--no-default-features` runs into LCOV
and HTML artefacts. The measured Linux CI line baseline is 79.65%, so CI starts at 78.65%,
one percentage point lower. Raise it when coverage improves; do not lower it to land
unrelated work.

## Adding a test

Name the externally visible behaviour, make the failure message describe the missing
observation, and choose exactly one suite. Prefer a black-box test when the promise belongs
to `bun` or `relish`. Use snapshots for structured rendering, properties for large algorithm
spaces and Criterion for measurements. A test that can pass without executing its promised
behaviour is worse than no test: it gives us confidence we didn't earn.
