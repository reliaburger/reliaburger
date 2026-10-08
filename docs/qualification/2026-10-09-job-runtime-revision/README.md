# Explicit job runtimes and four-part measurement

This revision replaces the development `isolation` field with
`runtime = "runc" | "shared-runc" | "process"`. Bun's explicit `runc` selection
is container-only; `mixed` and Linux auto-detection can enable both backends.
The incompatible development formats are protocol 50 and state 67. Start a
fresh cluster; the earlier raw reports under `2026-10-08-job-measurements/`
describe protocol 49/state 66 and must not be resubmitted unchanged.

The four measurements keep the raw VM baseline distinct from public job
results: one million raw process exits, 1,000 fresh container jobs, 10,000
shared-container jobs, then one million host jobs. The recorder uses one
monotonic clock and preserves preparation, submission, observation and indexed
verification pauses. It never counts the raw baseline towards accepted job
successes. The matching BusyBox executable has SHA-256
`f19470457088612bc3285404783d9f93533d917e869050aca13a4139b937c0a5`.

The node is a 4-vCPU, 8-GiB Ubuntu 24.04 aarch64 Lima VM with Linux 6.8.0-139
and runc 1.4.0. Image data is warm; shared executors start cold. Container jobs
request 100m CPU and 32 MiB, with a one-core CPU limit, no swap and one attempt.
They ask for concurrency 27. Host jobs use the current default reservation of
one CPU and 64 MiB, so node admission can admit fewer concurrent tasks. Host
jobs do not have enforced CPU/memory limits. The concurrent container service
reserves 500m CPU and 64 MiB.

A separate one-hour read-only observer starts when the public host tier is
running. It excludes successes accepted before its window and leaves the
original submission running. It records service availability and latency,
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

The recording and hour reports will be added here after the actual runs finish.
