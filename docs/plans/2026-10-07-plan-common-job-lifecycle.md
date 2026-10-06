# Plan: one job execution lifecycle

Date: 7 October 2026. Issue: #638. Release: 0.2.0.
Base: main after #266 (56ede9d9).

## Contract

A job definition describes reusable work. A run fixes a definition revision,
trigger identity and execution policy. Indexed tasks and attempts belong to that
run. A singleton has count one and uses the same placement, CPU/memory admission,
owned runtime, worker ledger, retry, cancellation and accepted-result machinery
as an array. Mixed manifests continue to group resource profiles.

Manual submission, cron and deployment hooks create runs. They do not select
separate execution engines. Keep the existing image digest/trust checks,
namespace and host-execution authorisation, script policy, secret decryption and
output behaviour. Preparation must finish before committing runnable work;
plaintext secrets must not enter Raft or the worker outcome ledger.

## Durable definitions and triggers

Extend the replicated task-array store with bounded named definitions and run
provenance. Fix the complete template and task policy in each admitted run, so
updating a definition cannot change an existing attempt. A run request has an
idempotent trigger identity: a manual operation, a deployment operation/job pair,
or a definition revision and UTC schedule minute. Refuse changed content under
an existing identity. Persist schedule occurrence claims and run creation in
one state-machine transaction, with no crash gap between them.

Cron evaluates on the leader. Default to forbidding overlapping runs and
skipping missed minutes; state the policies in the API, manifest and manual.
A skipped matching occurrence advances the durable cursor too, so a new leader
cannot later launch that occurrence after the prior run finishes. Bound any
catch-up policy and retain occurrence fencing independently of result pruning.
Clock rollback cannot replay a claimed minute. A definition update retains its
schedule cursor and never rewrites in-flight runs.

Ordinary jobs and migration hooks must retain conservative unknown-outcome
behaviour. Explicit retry policy determines whether a lost owner's unaccepted
work may run again. Bulk tasks retain their documented at-least-once policy;
side-effecting singleton/hook work requires acknowledged replay when execution
cannot be established. A successful launch is not a successful migration.

Standalone operation must use the same deterministic store backed by private,
fsync'd durable storage. Publication failure fences admission; startup reloads
and reconciles before dispatch. A production standalone service must never use
the volatile test-harness store. Incompatible wire/state changes bump both
compatibility generations and require a fresh cluster.

## Wiring order

1. Write state-machine regressions for count-one defaults, immutable revisions,
   duplicate triggers, cron leader handover, rollback, skipped overlap, pruning,
   malformed/oversized state and refused atomic transactions. Implement the
   common definition/run model through the existing task-array store.
2. Preserve runtime contracts: authorised scripts, namespace-key decryption,
   host isolation and supported resource limits, bounded result/output access,
   uncertain ownership and retirement. Extend worker admission and preparation
   without storing decrypted values in replicated definitions.
3. Route normal job submission and array submission through common admission.
   Commit durable cron registrations and fire runs through the leader loop.
   Persist standalone writes before acknowledgements or runtime mutation.
4. Run migration hooks through common runs, await accepted successful outcomes,
   then publish dependent applications. Retain prerequisite claims on ambiguous
   outcomes and cancellation; settle them only from exact run provenance.
   Remove obsolete node-local submission, retry and cron paths only when their
   full contracts are covered by the new path.
5. Align CLI/API/dashboard status, cancellation, rerun and selected output with
   run IDs. Update README, docs index, manual, whitepaper and book chapters 8
   and 12 together. Link #638–#641 in the roadmap with accurate scope.
6. Run portable CI plus cluster, Linux runtime, upgrade and relevant acceptance
   gates. Cover singleton/array parity, cron handover, hooks, encrypted env,
   scripts, worker loss/restart, storage failure, cancellation and serving apps.
   Open the fresh PR as a draft during implementation; close #638 only once its
   complete acceptance criteria are implemented, validated and merged.

## Boundaries

Reusable executors (#639), throughput qualification and the high-volume demo
(#640), and resident model workers (#641) are separate follow-ups. This change
unifies semantics; it does not establish 100m accepted successes/day. GPU
placement remains in #359; distributed training and dependency pipelines are
subsequent work.

## Progress

Fresh branch created from main after #266. The first implementation step adds
bounded replicated definitions, immutable run provenance, atomic manual/hook
trigger admission and cron occurrence transactions to the task-array store.
State-machine preflight allocates no IDs for replays or deliberately skipped
occurrences. Model and Raft regressions cover these behaviours.

Portable CI and the multi-node cluster gate pass for this step. Public admission,
leader cron firing, hooks, runtime contract preservation, unknown-outcome policy
enforcement and standalone persistence remain pending. Before wiring cron, define
its registration-time baseline and test clock rollback before the first firing.
The draft PR remains open for the remaining steps; this model alone does not
complete #638.
