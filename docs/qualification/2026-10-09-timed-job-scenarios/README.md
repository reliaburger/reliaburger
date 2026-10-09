# Equal-minute job demonstrations and hourly saturation runs

The repaired recording measures four genuine 60-second windows on one four-vCPU,
8 GiB Ubuntu 24.04 aarch64 Lima VM (Linux 6.8.0-139, runc 1.4), running locally
on an Apple M2 Max host with 12 physical cores and 32 GiB RAM. The VM receives
four cores, not all twelve. Other unrelated VM processes were retained; no
competing builds or benchmarks ran during measurement windows. Raw processes and
public jobs execute the same pinned BusyBox `true` binary. Warm images, cold
executor processes, concurrency cap 27, normal maximum three attempts.

| Scenario | Completed in 60 seconds | Rate | Extrapolated runs/day |
|---|---:|---:|---:|
| Raw VM processes | 876,671 | 14,611.2/s | 1.3B |
| Fresh runc | 196 | 3.3/s | 282.2k |
| Shared runc | 23,000 | 383.3/s | 33.1M |
| Native host | 246,000 | 4,100.0/s | 354.2M |

Public windows accepted **269,196 unique successes**, with no terminal failures,
accepted retries or application probe failures. Unused queued work was cancelled,
then verified drained without earning post-cutoff credit. Idle owned executor
journals positively retired before switching modes. The cast preserves actual
preparation and cleanup pauses outside the equal measurement windows.

The baseline counts raw successful exits, omitting admission, limits, ownership
journals and task ledgers. It is a measured reference for this VM/executable,
not a universal physical limit. Six 15-second raw pilots plateau around 16–54
concurrent commands; the repaired pilot's highest observed rate was at 27. The selected cold minute
was slower than the earlier minute; both reports remain retained, without choosing
the larger count for publication.
One active public submission caps each scenario at 27. Node CPU reservations
are 500m for Bun and 500m for the live container application, leaving 3,000m;
27 helper-inclusive 35m task reservations fit comfortably. Public requests are 25m CPU /
32 MiB, with a one-core CPU limit, zero swap and helper overhead of 10m / 8 MiB
for native/shared contexts. Fresh OCI retains its existing PID policy;
native/shared enforce their 256-process command cap. The raw path has no limits.

Receipt chunks differ deliberately: one job for slow fresh containers, 1,000
for fast paths. Chunk size controls reporting/amortisation, not resource bin
packing. The fresh count is capped at 60,000 queued tasks to respect the 65,536-
chunk limit; fast modes queue 16,000,000 tasks. Only one submission runs at once;
a finished submission renews immediately. The minute counts are conservatively
sampled accepted results received by the cutoff; no later summary or cancellation
result is credited. Runs/day is count × 1,440, not an observed daily total.

## Frozen build and reproduction

[Build boundary](current-build/build/build-boundary.json) contains exact binary
and source hashes. Production Bun/relish binaries are identical to the initial
frozen build; the repaired raw example corrects time-window count metadata, and
the Python harness corrects receipt granularity/concurrency/positive drain.
Runtime implementation and protocol 50 / state 67 remain unchanged. Both original
production provenance and current measurement source hashes are retained.

- `bun`: `9ec5ec8ab663e251812ce26584ba05eafb021b825546bcb3b8c95f6f3f198863`
- `relish`: `ca99c4e0a7079d930770417d1a9da396306b3a883ba3f5e58b06f8448bb4d19e`
- `job-throughput`: `1731b4fd7ced304c0b3174c23dcfb3a4f9aea2b272af385c03b0deed46bf0d79`

BusyBox executable SHA-256:
`f19470457088612bc3285404783d9f93533d917e869050aca13a4139b937c0a5`.
Image index:
`public.ecr.aws/docker/library/busybox@sha256:9532d8c39891ca2ecde4d30d7710e01fb739c87a8b9299685c63704296b16028`.

See the [manual commands](../../manual/14_batch-jobs.md#measured-development-results)
and [plan](../../plans/2026-10-09-plan-timed-job-scenarios.md). Run the recorder
with `--seconds 60`, matching immutable binaries and private CLI credentials.
The raw driver needs a private mount namespace/hostname so its image preparation
doesn't collide with the concurrent Bun's node subnet identity. The public
manifests and complete reports are under
[current-build/sixty-second-windows](current-build/sixty-second-windows/).
Copied website cast/report bytes match these raw files.

## Concurrency review

The wider public curve uses one submission, a 25m CPU request, a one-core limit
and 32 MiB memory for every runtime. Each point has two successive 60-second
windows: cold executors after submission, then a second window on the same
submission with initial accepted counters subtracted. The image stays warm.
Cancellation/drain and context retirement happen after both windows, without
adding late outcomes. The lower request admits values above the old 27-slot
budget; it doesn't change the per-command CPU limit. Reusable pools still cap
contexts at 32. Configured caps above that can add waiting attempts, not contexts.

| Configured cap | Fresh cold / second (jobs/s) | Shared cold / second (jobs/s) | Host cold / second (jobs/s) |
|---:|---:|---:|---:|
| 1 | 0.6 / 0.7 | 366.7 / 400.0 | 466.7 / 500.0 |
| 4 | 2.2 / 2.3 | 550.0 / 500.0 | 1,900.0 / 2,000.0 |
| 8 | 3.2 / 3.4 | 1,433.3 / 1,550.0 | 4,833.3 / 5,333.3 |
| 16 | 3.6 / 3.9 | 1,716.7 / 1,883.3 | 4,733.3 / 5,333.3 |
| 27 | 3.6 / 4.0 | 1,000.0 / 3,883.3 | 4,916.7 / 5,333.3 |
| 32 | 3.7 / 4.1 | 350.0 / 500.0 | 4,866.7 / 5,333.3 |
| 48 | 3.6 / 4.2 | 366.7 / 466.7 | 3,016.7 / 3,666.7 |
| 64 | 3.6 / 4.0 | 400.0 / 416.7 | 3,450.0 / 2,933.3 |

The shared candidates were repeated in ascending and descending order. Across
all three runs, second-window rates were:

| Cap | Minimum | Median | Maximum |
|---:|---:|---:|---:|
| 8 | 1,550.0/s | 1,833.3/s | 1,933.3/s |
| 16 | 1,650.0/s | 1,883.3/s | 1,950.0/s |
| 27 | 2,466.7/s | 3,150.0/s | 3,883.3/s |
| 32 | 466.7/s | 500.0/s | 1,700.0/s |

These are second-minute results, not a claim that all startup effects have
vanished. Cold shared throughput at 8–16 was often better than at 27. Duration
histograms at higher caps contain more slow preparation/queueing attempts; the
grant estimator includes those durations, and one-second idle expiry can require
new contexts across control gaps. This makes a short shared run sensitive to
startup and receipt cadence. Host throughput's exact 5,333.3/s plateau also shows
that concurrency alone won't close the gap to raw spawning: the bounded learned
grant window and control/acceptance path is the next profiling target; the bounded learned grant
window is a plausible contributor, not a proven sole cause.

We keep 27 for the common recording and hourly profile because it had the best
repeated second-minute shared throughput, reached the host plateau, and was near
the fresh/raw plateaus. For this command, 8 is the smallest host setting that
reaches the measured public maximum. Forty-eight and 64 don't improve pooled
execution. This is a workload-specific choice, not a new global default. Use
representative commands and resource requests when tuning another rig.

All 64 public windows completed without terminal failures, accepted retries or
application failures, and each submission positively drained before the next
point. Sampled active-command maxima aren't continuous occupancy measurements;
short commands can disappear between node syncs, and missing values stay `null`.
Raw evidence lives under `current-build/concurrency-curve` and
`current-build/concurrency-recheck-1` / `concurrency-recheck-2`.

## Hourly sequence

Each scenario runs sequentially for one hour at full admitted speed, using the
same executable, CPU/memory profile and concurrency cap. The same original
application is probed throughout; samples retain aggregate VM CPU ticks, original
Bun/application identities, selected RSS/HWM and filesystem/storage observations.
Builds and tests remain stopped during measurements. Saturation health is separate
from the daily-rate gate. Capped scans and one profile don't prove global metadata
cardinality or fault recovery. Final hourly totals and positive retirement proofs
will be recorded here after all four runs finish.

## Retained initial policy and interrupted run

[initial-window-policy](initial-window-policy/) preserves the first recording,
its source/binary hashes and interrupted raw soak. Its 1,000-job fresh chunks
published zero accepted successes within a minute despite real command execution.
Two active submissions also made small concurrency pilots incomparable as total
caps. The first recording's old health flag accepted an empty window; tests now
reject this. It is diagnostic evidence, not the published demo or a completed
hour. We stopped only the original raw benchmark, verified it was reaped, then
positively retired the original fixture's runtime/network/kernel owners before
starting the repaired node. Unrelated VM workloads were retained.

[Earlier fixed-volume evidence](../2026-10-09-host-job-executors/README.md) keeps the
matched direct/warm/CPU/sleep/output matrices, recovery gates, earlier failures
and completed native-only hour. Private credentials, node configuration, full
Bun logs and runtime internals remain outside published evidence.

## Checkpoint status

The raw hour completed at concurrency 27: **55,423,302 verified successful
exits in 3,600 seconds**, with zero failures. Its report and original-service
identity proof are retained under `current-build/hours/raw` and
`raw-hour-reuse-boundary.json`. The nominal raw CPU request has no admission
or enforcement effect; reuse retains the same VM, executable and concurrency.

The fresh-container hour was interrupted for the user's commit/push checkpoint
so required CI could run separately from timing. It is **not** a completed hour.
It recorded four accepted retries and no terminal failures before interruption.
The owned submission positively drained, job contexts retired, and the original
Bun/application identities and HTTP service remained healthy. See
`current-build/interrupted-fresh-hour-checkpoint.json` and
`current-build/hours/runc-interrupted-checkpoint`. The three public hours restart
after checks. Final four-scenario soak results and the recording using the new
packaged `relish bench` commands remain pending in this checkpoint.
