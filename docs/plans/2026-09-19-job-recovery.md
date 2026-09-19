# Durable job attempts and explicit recovery

Status: implemented and locally verified on PR #167; C34 remains open.

Before this change, a surviving job adopted with zero retries and the app default of
unlimited restarts. A missing exit status also followed the non-zero-exit retry
path. Both violated the approved rule: an uncertain execution requires an
explicit rerun.

Before creating each job attempt, Bun now durably records its identity,
specification, run generation and consumed retry budget. Observed exits and
operator stop intent also survive replacement. Adoption restores the same
budget and continues observing the existing process; missing execution evidence
becomes a durable Unknown outcome, never an automatic new attempt. A failed or
uncertain checkpoint write fences subsequent mutations until startup reloads it.

`relish apply jobs.toml --rerun-jobs` explicitly authorises rerunning unknown
jobs on the selected node. The manifest must contain only non-scheduled jobs;
normal apply and GitOps do not gain this authority. Existing runtime owners must
be stopped and their artifacts retired before claiming the next run. Unknown
outcomes appear in job and instance status. Lease retirement may forget the job
ledger only after confirmed resource cleanup.

Cron registration and occurrence claiming retain their separately agreed skip
policy. A future firing cannot overwrite an unresolved live owner. Recovery of
resources created before their first adoption record and complete process-tree
identity remain separate C34 requirements; this change must not claim to close
those gaps.

Verification starts with failing regressions for budget reset and unknown-exit
retries. Cover pre-launch persistence refusal, crash/reload before first runtime
record, observed success/failure, repeated replacement, explicit rerun and stop,
namespace isolation, corrupted checkpoints, HTTP scope checks and the actual
CLI flag. Run the full Bun/library suites and strict Clippy on macOS/Linux,
then requalify actual replacement and cluster upgrades for the new state format.

## Verification

The retry-reset and unknown-outcome regressions fail before implementation.
Additional failing-first tests cover unrelated app retirement during a job-store
failure and persistence of absence before deleting runtime evidence. Malformed,
duplicate, oversized and symlinked checkpoints refuse recovery before mutation.
The full library suites pass 3,362 macOS / 3,416 Linux tests (five / 19 explicit
gates), and strict all-target/all-feature Clippy passes on both platforms.
Actual Bun/Relish suites, both compatibility checks and all eleven CLI tests
pass. The physical Bun SIGKILL/explicit-CLI-rerun test passes on both platforms;
its final fixture discovers Bun's bound ephemeral API port rather than racing a
released reservation. Actual signed exec/adoption passes on macOS (19.45s).
The Linux node/cluster upgrade matrix is still running; final hosted and release
candidate qualification are separate gates.

The checkpoint is schema 1 and advances durable state to generation 10;
protocol 8 and test-lease schema 4 remain unchanged. Pre-release clusters must
be recreated. This does not close runtime discovery before the first adoption
record, process-tree retirement, registry/volume leases or final V01–V04 gates.
