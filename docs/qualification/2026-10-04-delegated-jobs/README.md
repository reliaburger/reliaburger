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
Earlier transient fixture failures passed unchanged serial retries. A separate
issue draft is retained locally pending publication approval; existing failures
are recorded in the [flakes register](../../flakes.md).

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
