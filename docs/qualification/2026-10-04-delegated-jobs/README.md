# Delegated jobs: implementation qualification

This records the implementation following the archived 3 October PR #266 review.
Protocol 36 / state 51 requires fresh node data and matching cluster binaries.
The feature and the daily performance claim have separate proof obligations.

## Reproduce the correctness gates

```sh
make ci
make test-cluster
make test-linux
scripts/demo/tour.sh --check
scripts/demo/tour.sh --setup /path/to/development/binaries --jobs
```

Use the provisioned Linux runtime environment for `test-linux`; the real-runc
case `runc_owned_task_arrays_pack_profiles_reuse_slots_and_retire_cancelled_process_trees`
uses a pinned image, two reusable owner slots and small/large requests beside a
reserved app commitment. It checks scratch isolation, retained owner count and
process-tree retirement. It reports warm-image real-container elapsed time;
that short measurement is not a sustained throughput claim.

Portable recovery cases cover torn-tail repair, incremental durability,
node-wide capacity, mixed requests beside app capacity, disk refusal before
chunk acknowledgement, persistent stale-control rejection and winning-grant
selection, including a late stale record in the index. Manifest API tests run
through Raft and check atomic rejection, cancellation, profile identity,
mergeable histograms, empty failure-page cursors and deep indexed lookup.

## Sustained target

100m unique successes/day is 1,157.407/s; use at least 20% headroom, approximately
1,389/s, while the application continues serving. Prepare a representative
multi-profile manifest with enough work to keep the cluster busy. The demo's
small count is for a walkthrough, not a representative daily workload.

```sh
python3 scripts/demo/qualify-jobs.py workload.toml --app-url http://service/healthz --seconds 86400 --output qualification-run
```

Pass `--relish '/path/to/relish --endpoint ... --ca-cert ...'` when the cluster
uses an explicit context. Keep credentials out of the command line; use the
normal local credential configuration. The harness keeps at most four parent
submissions active, measures increases in accepted successes across stable IDs,
checks application responses every observation, saves bounded summary samples
and cancels only the submissions it created on exit. It returns failure for a
shorter run or a throughput shortfall. `throughput_pass` never implies
`qualification_pass`: the latter remains false until the independent evidence
below has been reviewed.

Alongside that run, save hardware/node counts, allocatable resources, runtime
and image warm/cold state, profile requests and limits, attempts/retries, final
attempt histograms, app latency, worker/leader RSS, network and Raft rates, and
ledger + index + runtime/log storage bytes. Inject controlled worker loss,
leader failover, delayed messages and storage faults on the isolated test
cluster, using the correctness gates as the oracle. Prove memory, state,
retained owners and query cost plateau under admission/retention limits. Count
unique accepted successes, not process launches or successful duplicate effects.

FIFO packing can strand capacity behind a large waiter. Apps have shared
reservations but no automatic pre-emption of running jobs; leave rollout
headroom. Cached layers and reusable identities bound metadata per namespace,
but each task still launches a container. Measure that cost before claiming the
cluster can reach the target at a useful price.

## Results

The final portable run exercised 5,450 cases: 5,440 passed, eight snapshot cases
refused to spool below the host's 5% free-disk reserve, and two console-watchdog
cases failed as already tracked in #517. After clearing this task's build cache,
all 18 snapshot-worker cases and the million-record ledger case passed (19/19,
36.266 seconds). The million-record case took 33.914 seconds with the configured
1 MiB result-index cache. Formatting and both Clippy configurations passed.
Earlier transient fixture failures passed unchanged serial retries and are
tracked in [#582](https://github.com/reliaburger/reliaburger/issues/582), with
[evidence](ci-timeouts.md) and an entry in the [flakes register](../../flakes.md).

The matching cluster gate passed 43/43 in 270.251 seconds. The Linux runtime gate
passed 147/147 in 564.882 seconds, including real CPU/memory packing beside an
app commitment, reusable owners, scratch isolation, cancellation and same/cross
namespace policy with retirement after a lost binding. The warm real-container
packing case ran 12 tasks in 15.447 seconds (0.78/s); it demonstrates lifecycle
correctness rather than a useful daily throughput bound.

Two doctests, 52 CI-script tests, the ignored-test owner checker, generated CLI
command check, Go workload tests and executable tour command check passed. The
final homepage/manual command gate passed 14/14 cases.
No 24-hour 100m/day qualification has been run; that claim remains unqualified.


## Live homepage demonstration

The actual homepage Dockerfile was built through the public CLI and Pickle,
then applied with two burger replicas and one podinfo backend. On a 4 vCPU /
8 GiB aarch64 Linux VM, rootful runc and eBPF, the warm-image development debug
run accepted all **1,064 successes**, with **16 retries**, zero terminal failures
and zero not-run tasks. All **547 service probes** returned 200 during execution;
the slowest took **0.621 seconds**. Submission to final indexed query
was **746.855 seconds**, approximately **1.425 accepted successes/s** over
that interval. This includes runtime and query work, rather than quoting the
final chunk's one-second rate burst.

Peak observed agent RSS was **784.8 MiB**. The completed run retained **256
executor identities**, **31,728 ledger bytes** and
**3,178,496 index bytes**. The failed-only page was empty and direct index 42
returned success. These are short-run observations, not a memory/storage plateau
or sustained capacity claim. The results are in [the bounded report](demo-report.json).
The [homepage recording](../../website/assets/jobs.cast) cuts idle gaps to two
seconds and states the actual elapsed time. Its binaries were built from the
implementation working tree; the package version still identifies 0.1.4 until
the maintainer's release process advances it.

Container lifecycle and debug-build costs dominate this demonstration. Do not
extrapolate its hardware cost or scale-out efficiency to the daily target. Use
release binaries and representative workloads for the sustained qualification,
and optimise measured runtime/control/storage bottlenecks before claiming that
100m/day is easy or qualified.


## Process-launch feasibility for the million-job demo

A standalone C probe on the same 4-vCPU / 8-GiB aarch64 Linux VM used four
launcher threads. Every spawned process execs the same small compiled binary,
performs 64 integer mixing rounds dependent on its index and exits. The parent
waits for all exit statuses. It launched **1,000,000 processes in 66.360 seconds**
(**15,069/s**) with zero failures. The earlier 20,000-process sample took 0.865
seconds (**23,110/s**); extrapolating that short sample would have incorrectly
suggested an under-minute result on this VM.

For comparison, executing the same tiny computation in four persistent threads
completed 1,000,000 iterations in 0.088 seconds. That measures an in-process loop,
not independent OS processes, durable task outcomes or the Reliaburger API.
Neither result is a cluster throughput qualification. Both omit resource
admission, isolation, ownership persistence, ledger/index writes, accepted
completion reporting and service probes. The workload is deliberately tiny,
not the burger demo's multi-megabyte hashing task.

The [source](launch-probe.c) and [bounded results](launch-probe.json) make the
probe reproducible on Linux:

```sh
cc -O2 -pthread launch-probe.c -o /tmp/rb-delegated-launch-probe
/tmp/rb-delegated-launch-probe spawn 1000000 4
/tmp/rb-delegated-launch-probe pooled 1000000 4
```

The revised initial demo targets of 500,000 successes in a minute or one million
in two minutes require about 8,333 accepted successes/s, plus headroom for the
app and control/storage work. A warm owned executor that
launches children is the proposed command-job path; an explicit persistent
worker protocol offers further savings with different isolation semantics.
The [demo plan](../../plans/2026-10-04-plan-delegated-jobs.md#high-volume-container-jobs-on-the-landing-page)
records the end-to-end acceptance gate. Executor reuse remains unimplemented.

## 7 October rebase onto main

The new base is `82fa78ee760b74303b4a6a2acf351f59ad67c2b0`, including the
0.1.5 fixes and merged 0.1.6 security train. The rebased feature uses protocol
47 / state 64; main uses 46 / 63. The measurements above describe the earlier
build and do not qualify these rebased binaries.

The integration preserves main's batch preflight/ownership rules, forwards
caller credentials, binds delegated images at admission, applies upstream
allowlists and required cosign checks, and verifies every still-trusted root
during CA rotation. Array and manifest registration preflight the entire shared
ID allocation before changing state. New regression tests reproduced the
image-admission bypass and counter-exhaustion panic before these fixes.

Local validation of the final source changes, before their integration commit:

- `make ci NEXTEST_PROFILE=ci`: formatting, both all-target Clippy configurations,
  6,073 portable tests, two doctests, 317 CI-script tests and ignored-test ownership
  passed. [Portable CI log](rebase-main-20261007-ci.log).
- `make test-cluster NEXTEST_PROFILE=ci`: 44 passed, one skipped.
  [Cluster log](rebase-main-20261007-cluster.log).

Both commands used `CARGO_BUILD_JOBS=2`, `CARGO_INCREMENTAL=0`,
`CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_NET_OFFLINE=true` and an isolated
`CARGO_TARGET_DIR` on the macOS host. Tests retain the repository's line-table
profile. GitHub full CI supplies fresh Linux/runtime and other platform checks
on the published commit; the earlier Linux and demo evidence above remains
historical until those checks complete.
