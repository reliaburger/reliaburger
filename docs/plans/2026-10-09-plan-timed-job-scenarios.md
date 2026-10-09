# Equal-window job demonstrations and hourly saturation runs

The landing page will explain fresh containers, shared containers and trusted host
commands, with raw processes on the same VM as a measured reference ceiling.
Each recorded stage has a 60-second measurement window. Count only successful
exits or durable accepted successes observed within the deadline; preparation,
cancellation and positive cleanup remain visible outside the window. Daily
projections are the minute count times 1,440, explicitly labelled extrapolations.

Keep the pinned BusyBox executable, concurrency 27 and public CPU/memory profile
identical. On this four-vCPU VM, 500m is reserved for the node and 500m for the
concurrent application; 27 helper-inclusive 110m reservations fill the 3,000m
job budget. Keep one active submission to cap total requested concurrency at 27. Large
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
Preserve the already-running raw hour as a 27-concurrency reference; redo it if
the selected cap differs. No competing builds or tests during timed windows.

Add a short rig note next to the landing-page results: a local Lima VM on an
Apple M2 Max host (12 physical cores, 32 GiB host RAM), allocating 4 vCPU and
8 GiB to Ubuntu 24.04 aarch64, Linux 6.8 and runc 1.4. The VM's allocated cores,
reservations, command, executor warmth and receipt policy define the comparison;
readers should measure their own hardware and representative workload.

The completed curve selects 27 as a common comparison point, not a universal
optimum. Native host throughput reaches the same second-minute 5,333.3/s plateau
at 8, 16, 27 and 32; 48/64 are worse. Shared second-minute throughput at 27 won
all three runs, while 8–16 often improved its cold minute. Fresh containers are
near their observed plateau from 16 upwards. Preserve those trade-offs, confirm
the raw 16/27 plateau with equal-minute windows, then re-record at common 27 and
25m-1000m / 32 MiB. Reuse the completed raw hour only at its unchanged 27 cap and
executable; raw CPU profile inputs are ignored, so their nominal request isn't
an enforced limit or admission setting. Run public hours at common 27 with the
new matched positive request. The rig note and curve belong beside the results.

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

After the current frozen runtime soaks finish, test the packaged commands
against the real node and re-record the landing-page demo with those commands.
The soak evidence remains attached to its original runtime binary hashes and
harness; any changed runtime implementation would require new soaks. Remove the
superseded 1,064-job development preview, place the new numbered section
immediately after the five-minute tour, keep its player visible and fold the
results, use cases and rig details into a closed disclosure.
