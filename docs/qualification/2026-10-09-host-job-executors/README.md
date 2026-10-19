# Native host executors and current job throughput

The tree keeps this record's READMEs, reports, matrices, manifests, binary
hashes and build boundaries. The raw samples, logs and casts (about 11 MB) were
removed to keep the repository small; they're still in the repository history
at [006aca5f](https://github.com/reliaburger/reliaburger/tree/006aca5f9157a3b9432752470d507ca6ecb7e0d5/docs/qualification/2026-10-09-host-job-executors).

The frozen current implementation is `ed87f20b5f73b754fce0427261a03c14705d1cfe`.
[Binaries and hashes](current-build/build-boundary.json), [node boundary](current-build/measurement-boundary.json)
and [experiment order](current-build/sequence-boundary.json) identify the runs.
This is a four-vCPU, 8-GiB Ubuntu 24.04 aarch64 VM on Linux 6.8.0-139.
The digest-pinned BusyBox executable has SHA-256
`f19470457088612bc3285404783d9f93533d917e869050aca13a4139b937c0a5`.
No builds or CI tests ran beside the measurements. A real container application
served requests throughout. Existing unrelated VM processes were not stopped.

## Matched execution contracts

The [62-case matrix](current-build/matched-repaired/matrix.json) uses concurrency
1, 3 and 27, matching counts within each comparison, cold and short-warmup cases,
and short, sleeping, CPU and output commands. Its eight paths distinguish raw
exits, resource-limited raw exits, owned host/shared/fresh execution and the
corresponding worker-ledger paths. Each report records the command, executable
hash, resource enforcement and durability boundary. Public dispatch additionally
includes live namespace supervision, submission, Raft acceptance and reporting.
None of these differences is pure scheduling overhead.

All limited paths use the same 100m CPU / 32 MiB profile and one-core hard limit.
All limited paths disable swap. Native/shared tasks and the limited raw
comparator additionally enforce a 256-process cap; fresh containers retain their
existing OCI PID policy. These single-process commands do not exercise that
difference. The frozen direct reports' generic `resource_enforcement` string
overstated fresh-container PID enforcement. The report label is corrected in
this PR; the original reports remain unchanged and this paragraph is their
erratum. Execution behaviour and measured rates are unchanged. Native/shared helpers reserve another 10m / 8 MiB.
Direct owned and worker paths share a 3000m / 4-GiB execution budget. Raw exits
omit resource admission and ownership; raw limited exits set the same limits
before exec, but use a large Rust parent and a pre-exec cgroup move. Native
helpers use a small parent and clone into the task cgroup at birth. Their
resource-limited raw comparison is therefore not a universal process floor.

The [fully warmed matrix](current-build/matched-steady/matrix.json) runs 100,000
warmup commands, then measures an identical 10,000 commands at concurrency 27.
Every owned path has **zero additional executor startups during measurement**.
Warmup/image preparation is recorded separately and excluded from its clock.

| Path | Verified successes/s | Completion contract |
|---|---:|---|
| Raw processes | 16,192.9 | Exit statuses |
| Resource-limited raw processes | 3,906.2 | Limits before exec; exit statuses |
| Native host | 11,553.2 | Owned executor, fresh command, positive cleanup |
| Shared runc | 8,251.3 | Owned container, fresh command, positive cleanup |
| Native host + worker ledger | 9,309.3 | Durable indexed outcomes and verified chunk receipts |
| Shared runc + worker ledger | 7,239.2 | Durable indexed outcomes and verified chunk receipts |

Serial warmed comparisons use the same 1,000 commands and four-command warmup:
host 1,023.5/s, shared 1,179.5/s; worker-ledger host 869.7/s, shared 950.6/s.
The earlier tenfold host deficit does not persist under matching reservations
and native ownership amortisation. The remaining serial difference is visible.
At serial host execution the measured command mean is 789.3 µs and cleanup
130.5 µs; shared execution records 662.8 µs and 120.1 µs. Compute interval means
from cumulative sample/count deltas. Lifetime maxima cannot be subtracted.

## Public acceptance and the recording

The [public matrix](current-build/public-matched-v2/matrix.json) matches host and
shared counts at concurrency 1, 3 and 27, matches all three runtimes for a smaller
fresh-container experiment, and runs two different native CPU/memory profiles
beside the service. All ten cases succeeded without accepted retries.

Separate initially cold public pilots use 100,000 identical commands, concurrency
27 and matching profiles: [native](current-build/native-pilot-100000/report.json)
completed in 23.43s (4,267.2/s), [shared](current-build/shared-pilot-100000/report.json)
in 77.80s (1,285.3/s). Both have zero failures/retries. Public rates remain below
the fully warmed worker rates. Cold startup, conservative mean-based lookahead,
live namespace supervision and the public control path remain part of this
contract; these reports do not assign the whole gap to any one cause.

The [complete four-part recording](https://github.com/reliaburger/reliaburger/tree/006aca5f9157a3b9432752470d507ca6ecb7e0d5/docs/qualification/2026-10-09-host-job-executors/current-build/three-tiers/jobs.cast) retains
one monotonic clock, actual pauses, indexed first/middle/last checks, rates,
backlog, retries and service probes. [Its report](current-build/three-tiers/report.json)
is copied byte-for-byte to the landing page, as is the cast.

| Path | Completed | Elapsed | Whole rate | Accepted retries |
|---|---:|---:|---:|---:|
| Raw VM baseline | 1,000,000 exit statuses | 62.67s | 15,957.2/s | Not applicable |
| Fresh runc | 1,000 accepted successes | 249.62s | 4.0/s | 0 |
| Shared runc | 10,000 accepted successes | 22.73s | 439.9/s | 0 |
| Native process | 500,000 accepted successes | 90.44s | 5,528.7/s | 0 |

All 511,000 public successes are distinct accepted identities. The raw million
is not counted as public successes. Each public tier requests concurrency 27 and
the same CPU/memory profile; admission still applies. The normal three-attempt
policy is retained, with no accepted retries or terminal failures. Demo counts
differ deliberately; use the matched matrices for equal-count comparisons.
Images are warm, contexts initially cold. Chapter/speed controls change playback
only. Worker activity is a snapshot; unavailable activity remains unknown and
chunk receipts cause bursts in recent rates. The total recording is 427.80s.

## Retained diagnostics

[The first native build](prior-dispatch-policy/build-boundary.json) is
`c4fc14746d93b347ceccdf4221dd0096936d3ecc`. Its matched/steady/public results are
retained rather than relabelled as current-policy evidence. The original public
native pilot completed 100,000 in 100.84s (991.6/s), despite 9,294.7/s direct
worker completion. Two 1,000-task grants and separate acceptance/delivery ticks
limited supply. The new planner uses existing verified duration buckets for
bounded lookahead, with learned depth capped at sixteen chunks (the original two-slot-round
floor still applies to tiny chunks); actual execution still
requires concurrency and CPU/memory admission. More queued ownership increases
the reconciliation/replay window after loss, with unchanged attempt fences.

The original v1 public harness invocation rejected an invalid warmth argument
before submission; its log is retained under `prior-dispatch-policy/public-matched`.
The first v2 direct matrix omitted the original isolated hostname wrapper. Its
runc setup conflicted with the live node identity: all 1,000 attempts failed in
startup, with zero command samples. Its [four-case record](current-build/matched/matrix.json)
is retained. Restoring the wrapper produced the separate successful 62-case
`matched-repaired` matrix; no failed run was overwritten.

[Check records](checks/boundary.json) retain failing planner, volume and native
CI-manifest regressions, portable CI, strict Linux lint and cluster evidence.
The first cluster gate failed a hard-coded-leader setup write and placement
wait. An unchanged isolated retry retained old Raft state beneath a mounted
identity tmpfs: the harness ignores fixed-directory deletion errors. Separate
fresh fixture roots passed the isolated case and all 46 cluster tests. The
failed receipts and the limitation remain recorded in `docs/flakes.md`.
Native per-task limits, OOM, detached descendants, cancellation, dropped-caller
quarantine and original-owner recovery are covered by real Linux gates. The
trusted CI manifest names both native cases explicitly; protocol mocks alone
are not runtime qualification.

## Reproduce

Build the recorded commit with `cargo build --release --features ebpf --bin bun
--bin relish --example job-throughput` on the isolated rootful Linux VM. Pin the
same image digest found in the reports. Configure a rootful mixed Bun node,
allowlist the matching host BusyBox executable, retain the declared budget and
profiles, and run a real HTTP service beside it. Supply your own authenticated
endpoint, CA and token through the normal CLI environment; private fixture
credentials are deliberately not published.

For each direct path run `target/release/examples/job-throughput --path PATH
--root FRESH_DIRECTORY --bun target/release/bun --image PINNED_IMAGE --count 10000
--concurrency 27 --warmup-count 100000 --workload true --service-url SERVICE_URL`.
Use the paths/commands from the matrices for other counts and workloads. Direct
runc runners need an isolated node network identity when sharing this VM with a
Bun node: the original harness uses a mount namespace with a private
`/etc/hostname`, leaving the service network namespace intact.

Record with `python3 scripts/demo/record-job-tiers.py --process-count 500000
--binaries FROZEN_BINARIES --relish FROZEN_BINARIES/relish --image PINNED_IMAGE
--host-binary ABSOLUTE_ALLOWLISTED_BUSYBOX --service-url SERVICE_URL --output
FRESH_OUTPUT --observe-pid ORIGINAL_BUN_PID --observe-dir NODE_DATA`.
Then run `python3 scripts/demo/qualify-jobs.py FRESH_OUTPUT/process.toml
--seconds 3600 --window 1 --relish FROZEN_BINARIES/relish --app-url SERVICE_URL
--observe-pid ORIGINAL_BUN_PID --observe-dir NODE_DATA --output FRESH_HOUR_OUTPUT`.
Finished measurements need the original service PID/start time/generation
check and positive retirement of the runner's own remaining resources.

## Continuous hour and qualification boundary

The [completed continuous run](current-build/one-hour/report.json) measured
**18,103,000 unique accepted successes in 3600.00s
(5,028.6/s)**, with zero terminal failures or accepted
retries. It kept one 500,000-command submission active at a time, concurrency 27
and the same 100m / one-core / 32-MiB profile. Thirty-six submissions completed;
accepted outcomes from the next submission also count before the cutoff. The
runner cancelled only its remaining submission, with no cleanup error. Outcomes
accepted after the cutoff are not added to the measured total.

All 3,490 application probes succeeded. Service p95 latency was
1.32 ms and its maximum 12.82 ms.
[Original-service proof](current-build/service-after-hour.json) confirms the
original Bun and application PID/start-time identities and durable application
generation. [Final retirement](current-build/node-retirement.json) proves all
284 runc intents retired, 28 host owners retired or never launched, zero held
addresses, original processes gone and the exact kernel owner positively
`Retired`. No unrelated VM process was signalled or node journal deleted.
[Aggregate inventory diagnostics](current-build/inventory-diagnostics.json)
contain zero timeout lines across the whole fixture lifetime, including this
hour; the private raw node log is not published.

The 117 process observations all retained the original Bun identity. Sampled
RSS started at 193.6 MiB and ended at
229.7 MiB; its largest sampled RSS was
247.7 MiB. The last sample's process-lifetime
high-water mark was 258.1 MiB, not a separate
aggregate node measurement. Every storage scan was incomplete. Allocated bytes
observed in the capped traversal went from
81.0 to 806.5 MiB;
these are not whole-directory totals. Filesystem available space went from
2258.2 to 1524.6 MiB,
with no concurrent builds/tests. These observations don't prove global storage
or historical cardinality bounds.

A completed hour is distinct from 24-hour qualification. Neither the demo nor any direct throughput rate qualifies
100m/day. The remaining #640 work includes sustained headroom, multiple profiles
and applications, worker/leader loss, delayed messages, disk faults and global
metadata/collection bounds. Fixed live pool limits do not bound history across
new namespaces, profiles and credentials. Incomplete capped scans cannot prove
global disk bounds. Resident model inference and coordinated GPU training are
separate contracts; this `true` workload does not measure them.
