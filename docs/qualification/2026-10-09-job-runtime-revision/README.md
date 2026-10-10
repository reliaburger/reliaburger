# Explicit job runtimes and four-part measurement (superseded)

This revision replaced the development `isolation` field with
`runtime = "runc" | "shared-runc" | "process"` (protocol 50/state 67). Its
measurements predate native host executors: host jobs still started one owned
process per job, and its four tiers ran different job counts (one million raw
exits, 1,000 fresh, 10,000 shared and 10,000 host jobs), so the rates weren't
comparable with each other. The [timed job scenarios](../2026-10-09-timed-job-scenarios/README.md)
replace them.

One result from this generation is still worth keeping. Before native
executors, a continuous one-hour host run on the same 4 vCPU / 8 GiB Ubuntu
24.04 VM accepted 182,000 unique successes (50.6/s) with no terminal failures
or retries, while a concurrent service passed all 3,490 probes. That is the
baseline the native executors improved on. Selected Bun RSS peaked at
229.8 MiB; the bounded storage scans were incomplete, so the run can't prove a
global storage bound.

The raw reports, samples, casts, diagnostics and regression logs were removed
from the tree to keep the repository small. They remain in git history at
[006aca5f](https://github.com/reliaburger/reliaburger/tree/006aca5f9157a3b9432752470d507ca6ecb7e0d5/docs/qualification/2026-10-09-job-runtime-revision).
