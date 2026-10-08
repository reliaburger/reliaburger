# Explicit job runtimes and four-part measurement

This revision replaces the development `isolation` field with
`runtime = "runc" | "shared-runc" | "process"`. Bun's explicit `runc` selection
is container-only; `mixed` and Linux auto-detection can enable both backends.
The incompatible development formats are protocol 50 and state 67. Start a
fresh cluster; the earlier raw reports under `2026-10-08-job-measurements/`
describe protocol 49/state 66 and must not be resubmitted unchanged.

The four measurements keep the raw VM baseline distinct from public job
results: one million raw process exits, 1,000 fresh container jobs, 10,000
shared-container jobs, then 10,000 host jobs. The recorder uses one
monotonic clock and preserves preparation, submission, observation and indexed
verification pauses. It never counts the raw baseline towards accepted job
successes. The matching BusyBox executable has SHA-256
`f19470457088612bc3285404783d9f93533d917e869050aca13a4139b937c0a5`.

The node is a 4-vCPU, 8-GiB Ubuntu 24.04 aarch64 Lima VM with Linux 6.8.0-139
and runc 1.4.0. Image data is warm; shared executors start cold. Container jobs
request 100m CPU and 32 MiB, with a one-core CPU limit, no swap and the default maximum of three attempts.
Retry counts remain visible; the initial one-attempt diagnostic is retained separately.
They ask for concurrency 27. Host jobs use the current default reservation of
one CPU and 64 MiB, so node admission can admit fewer concurrent tasks. Host
jobs do not have enforced CPU/memory limits. The concurrent container service
reserves 500m CPU and 64 MiB.

A separate one-hour runner starts after the four-part demo completes. It
continuously submits bounded host batches with a window of one active batch,
then cancels its own remaining work at the cutoff. It records service availability and latency,
selected Bun RSS and bounded storage scans. An incomplete storage scan cannot
prove a global disk bound. A completed hour is useful evidence even if the
rate misses the target; it does not pass 24-hour qualification.

Reproduce with the recorder and observer commands in
[the manual](../../manual/14_batch-jobs.md). Use matching optimised immutable
binaries, private CLI credentials, a new task-owned data directory and a distinct
subnet identity for the raw baseline's runtime allocator. Keep compilation and
other test traffic stopped while measuring. Process exits omit durable owner
records, admission and accepted task outcomes, so a difference in rates is not
pure scheduling overhead.

The completed recording and full-hour reports will be added after the final
runtime repair and actual runs finish. The initial one-attempt diagnostic is
retained under `one-attempt-diagnostic/`: 998 accepted container successes and
two failures (launcher startup timeout and a missing file during inventory).

The default-three-attempt repeat completed the baseline, 1,000 container jobs
(with one retry) and 10,000 shared-container jobs (no retries). Its million-job
host submission accepted 33,579 successes before the user selected a smaller
completed host tier. The public cancellation classified the remaining 966,421
indexes as not run; no terminal failure or retry was counted in that host tier.
`cancelled-million-diagnostic/` retains the original report, cancellation
boundary and raw samples. Its observer ran for only part of an hour and isn't a
completed one-hour measurement. It also retains 287 inventory timeout/route
warnings, which motivated the read-path repair. Those diagnostics are not the
published completed demo.
