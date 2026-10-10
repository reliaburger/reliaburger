# Equal-window job demonstrations and hourly saturation runs

The landing page will explain fresh containers, shared containers and trusted host
commands, with raw processes on the same VM as a measured reference ceiling.
Each recorded stage has a 60-second measurement window. Count only successful
exits or durable accepted successes observed within the deadline; preparation,
cancellation and positive cleanup remain visible outside the window. Daily
projections are the minute count times 1,440, explicitly labelled extrapolations.

Keep the pinned BusyBox executable, concurrency 27 and public CPU/memory profile
identical. On this four-vCPU VM, 500m is reserved for the node and 500m for the
concurrent application. The initial 100m job request plus 10m helper overhead
admitted 27 slots in the 3,000m job budget; the wider review below lowers the
common request to 25m while retaining the one-core limit. Keep one active submission to cap total requested concurrency at 27. Large
submissions avoid rollover gaps; renew immediately when one completes. Fresh
containers use single-job receipt chunks so a minute can show accepted progress;
fast paths use 1,000-job chunks to amortise receipts. Disclose that difference. Measure short pilots and
report the observed saturation; raw processes omit limits and durability.

Tests precede timing/counting and presentation changes. Preserve failed runs and
freeze binary hashes before measuring. Re-record all four stages, then run each
for one continuous hour, sequentially, without concurrent builds or tests.
Probe the same application, observe original process identities, CPU, memory and
filesystem capacity, and retain timestamped samples. Archive only positively
retired owned fixtures to make local disk space. Never stop unrelated workloads.

The requested soak is one hour per scenario, replacing a 24-hour run for this
review. It checks saturation health and growth, not fault recovery or global
historical metadata bounds. Document findings in the book/manual/whitepaper and
qualification record; update the homepage, PR and #640, run portable CI and the
matching Linux measurement gate, and return the PR to ready after all checks pass.

The concurrency review expands the curve to 1, 4, 8, 16, 27, 32, 48 and 64.
The initial 100m request could admit only 27 helper-inclusive slots, so compare
all three public modes with a common 25m request and unchanged one-core limit /
32 MiB memory. Reusable pools have a separate 32-slot limit; record that admitted
bound and sampled activity rather than calling a configured 64-slot cap 64
running commands. For each point, submit once with cold executors and observe
60 seconds, then measure another 60 seconds on that same submission with initial
accepted counters subtracted. Cancel and positively drain before the next point.
Repeat close candidates and retain the full curve, then select a common cap for
the recorded comparison and document any scenario-specific optimum separately.
Preserve the earlier raw hour as historical evidence. Repeat it if the selected
cap, executable or raw supervisor changes. No competing builds or tests during timed windows.

Add a short rig note next to the landing-page results: a local Lima VM on an
Apple M2 Max host (12 physical cores, 32 GiB host RAM), allocating 4 vCPU and
8 GiB to Ubuntu 24.04 aarch64, Linux 6.8 and runc 1.4. The VM's allocated cores,
reservations, command, executor warmth and receipt policy define the comparison;
readers should measure their own hardware and representative workload.

The completed curve selects 27 as a common comparison point, not a universal
optimum. Native host throughput reaches the same second-minute 5,333.3/s plateau
at 8, 16, 27 and 32; 48/64 are worse. Shared second-minute throughput at 27 won
all three runs, while 8–16 often improved its cold minute. Fresh containers are
near their observed plateau from 16 upwards. Preserve those trade-offs and re-record at common 27 and
25m-1000m / 32 MiB. Raw CPU profile inputs are ignored, so their nominal request
isn't an enforced limit or admission setting. Run public hours at common 27 with
the new matched positive request. The rig note and curve belong beside the results.

Package the four timed scenarios in `relish bench`, using `--scenario
jobs-containers`, `jobs-shared-containers`, `jobs-host-processes` and
`jobs-vm-baseline`. Default to 60 seconds, concurrency 27 and the measured
25m-1000m / 32 MiB profile. Use the configured endpoint and credentials for
public jobs, the existing manifest API and bounded summaries; cancel only the
benchmark's own submission and positively await drain on completion, error or
Ctrl-C. Preserve the ordinary benchmark suite when no scenario is selected.
Do not require Python, a source checkout, a Bun binary path or fixture paths.
Host execution retains Bun's explicit binary allowlist; the raw baseline runs
on the Linux machine where Relish executes. Document these prerequisites and
report the executable/image boundary rather than silently claiming unlike
binaries or different machines form a matched comparison.

Test the packaged commands against the real node and re-record the landing-page
demo with those commands. The packaged raw supervisor differs from the earlier
Python driver, so repeat all four hours using the frozen Relish build. The soak
evidence remains attached to its original binary hashes and harness; any changed
measurement or runtime implementation requires new measurements. Remove the
superseded 1,064-job development preview, place the new numbered section
immediately after the five-minute tour, keep its player visible and fold the
results, use cases and rig details into a closed disclosure.

## Implementation and review evidence

The packaged command and publication observer are implemented and tested. The
clean measurement source is commit `006aca5f9157a3b9432752470d507ca6ecb7e0d5`;
Bun retains its earlier frozen runtime bytes. A fresh recording of the four
default commands completed 993,718 raw exits, 213 container jobs, 58,000 shared
container jobs and 294,000 native host jobs within their respective 60-second
windows. No failures, retries or application probe failures occurred. All public
submissions positively drained after cancellation.

All four packaged hours completed sequentially with builds, tests and preview
playback stopped: 55,407,709 raw exits, 14,199 fresh-container jobs, 11,799,000
shared-container jobs and 19,145,000 host jobs. There were no terminal failures;
fresh containers recovered four retries. All 14,361 application probes succeeded.
Every public owner positively drained, and the original fixture's runtime,
network and exact kernel owner retired after measurement. The landing-page
recording and the book/manual/whitepaper now use these measured boundaries.

The [qualification record](../../qualification/2026-10-09-timed-job-scenarios/README.md)
retains the minute recording, concurrency sweep, full hours, storage growth,
incomplete scans and earlier interrupted diagnostics. Remaining 24-hour and
global cost/cardinality qualification belongs to #668. A completed hour does not
resolve that issue. Final portable CI, the Linux benchmark gate, publication contracts and browser
verification passed. The implementation and requested measurement work are complete.
