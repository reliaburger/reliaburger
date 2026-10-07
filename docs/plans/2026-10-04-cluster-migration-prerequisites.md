# Cluster migration and job ownership

Issue: [#534](https://github.com/reliaburger/reliaburger/issues/534).

Cluster apply must keep a new app revision out of schedulable desired state until every `run_before` migration has positively exited zero and that outcome is durably recorded. Node-local records alone cannot fence a second leader or a changed submission. The implementation combines local deploy-operation ownership with a bounded replicated claim.

## Submission and publication

1. Validate the complete manifest. A migration references `app.<name>` in the same apply and effective namespace; scheduled migrations are refused. Preserve intrinsic job validation for checkpoint/recovery, which does not have the original app manifest.
2. The real agent actor reserves every target and performs image/signature admission for all app, init and job images before any migration runs. A registered recurring job cannot be silently replaced with a migration. Preparation and metadata acknowledgments share bounded queue/reply budgets.
3. Capture the current leadership term, then commit `PrerequisiteBegin` with the original prepared manifest. A claim owns every app and job identity before publication. Limits are 64 active claims, 1,024 resources per manifest and an 8 MiB encoded claim inventory. Admission and snapshot decoding reject malformed, duplicate and overlapping active identities. Ownership bounds are checked before cloning and staging desired-state writes. Known desired-state refusals are then checked on a staged copy before authorizing side effects. Namespace quota retains its existing scheduler enforcement.
4. Run nonscheduled prerequisite jobs under the reserved local operation. Require a positive zero exit and durable success checkpoint. A positively observed, durably recorded failure may release this claim in the original leadership/recovery generation. Unknown exits, canceled replies, persistence uncertainty and leadership changes retain ownership and never imply success or automatic replay.
5. `PrerequisiteCommit` rechecks current namespaces, the original term and recovery epoch, then applies every desired write to a staged state and publishes that state atomically. A refusal preserves both old desired state and the claim. Cancellation is checked after migration settlement and again immediately before proposing publication. Cancellation after a Raft proposal has been submitted cannot reverse its eventual commit. Successful migration jobs are removed from the ordinary dispatch manifest.
6. If ordinary jobs remain, keep their replicated identities reserved after app publication. Dispatch through the original worker, capture exact namespace/name/spec/generation receipts, and release only on trusted positive terminal settlement in the same leadership/recovery generation. Unknown or missing receipts, changed generations, unfinished retries and stopped-but-still-owned runtimes retain the fence. Startup completion is not execution completion.

Each Begin, Commit, known-failure settlement and terminal ordinary-job settlement write has a five-second proposal deadline. Timeout bounds the caller without proving that the write failed to commit; no unknown admission dispatches work or implies ownership release. A terminal settlement watcher ends on an uncertain write instead of hanging indefinitely.

Job-only cluster apply uses the same leader ownership path. Leased node-local work retains its established path. `AppSpec`, stop/delete and subsequent app-only/job-only manifests cannot bypass a held identity; batch registration uses the same held-job namespace and exact physical-execution guards. Independent batch display labels matching only a held app remain admissible.

## Deliberate boundaries

Cluster apply refuses manifests containing recurring schedules before preparing work or writing claims. A recurring registration has no finite terminal execution receipt and cannot be retired by an empty one-shot receipt. Standalone recurring registration remains supported. This boundary is covered by real HTTP and actor tests.

An old-term or recovered claim remains held because a replacement leader cannot prove what the original runtime did. There is no automatic claim expiry, automatic retry or inference of runtime absence from a missing local record. Administrator recovery is separately proposed and is not part of this draft unless explicitly approved and qualified.

## Expected-behavior verification

Tests first established the cluster desired-write ordering defect, same-identity bypasses after handover, stale worker publication after recovery, negative-exit persistence, failed migration retry, signature admission before migration side effects and ordinary-tail release errors. Real blocked Process migrations cover success, failure, follower forwarding and leadership transfer. Cold restart preserves a failed generation and accepts an explicitly corrected generation without repeating the failed side effect. Exact receipt tests cover pending, exhausted failure, uncertainty, missing records, changed generations, stopped ownership and recurring schedule refusal. Snapshot controls reject missing fields, duplicate map keys, malformed claims and overlapping distinct operations.

Keep baseline compilation/setup failures separate from behavior reds. A tentative quota-refusal test was removed because it contradicted established scheduler behavior; no quota semantics were changed. The registered-cron migration control must use the real `PreparePrerequisites` actor boundary rather than a test-only worker helper.

Publish one child PR after inheriting the integrated #535 and #543 fixes. Adapt the staged Raft calls to their actual apply-entry position and use the next actual wire/state generations. Run focused tests, both Clippy configurations, `make ci` and cluster checks, then qualify every remote check on the exact published head. Preserve all earlier children and test receipts.

## Additional parent-review controls, 4 October

Two early-bound controls compiled and failed against the original admission order: the oversized-resource manifest visited 1,024 actual staged namespace writes, and the oversized encoded claim visited one. Moving the existing inventory validation ahead of staging makes both refuse with zero such visits.

A real ProcessGrill/HTTP cancellation control compiled and failed: cancellation was acknowledged while the migration was blocked, but its subsequent zero exit still published the `new` app instead of retaining `old`. The fixed actor and pre-commit boundaries preserve the exact uncommitted claim and report an error. These three controls passed together after the minimal fixes.

Four actual Council hang-hook controls compiled and failed at the six-second observation guard for Begin, Commit, known-failure settlement and terminal ordinary-job settlement. The finite actor fixtures distinguish command acknowledgements from real execution evidence; the terminal watcher test owns and joins its actual task rather than asserting a startup SSE error. Five-second write deadlines passed all four controls; those and the two early-bound controls passed together. Timeout is an uncertain proposal outcome, not a proof of absence.

Three genuine node-policy preparation reds established denied host execution and unsupported ProcessGrill CPU limits on both ordinary tails and migrations. The integrated preparation path now calls the same read-only Supervisor admission introduced by #535, before local or replicated ownership. All 49 focused migration, policy, cancellation and settlement controls passed on the inherited #535 source.

The first full portable run selected 5,598 cases: 5,595 passed and three existing authority fixtures failed because they did not answer the new preparation command. Their replacement trace preserves all denial checks and requires actual prepare/dispatch commands, a finite startup acknowledgement, the exact canonical receipt and removal of the same real Council claim. All six authority cases then passed. The corrected full run passed 5,597 of 5,598; the unchanged Apple CLI fixture lacked its PID marker after 2.198 seconds. Its one diagnostic passed in 2.052 seconds, with cause unknown. Both full results and separate JUnit files are retained; neither is called a complete local CI pass. Both lint configurations, two doctests, 52 CI script controls, ignored-owner reasons and 42 real multi-node cluster cases passed. Final inherited #543 and remote qualification remain pending.

## Capacity integration and additional parent review

The actual parent is #584's merge `ca3da70ea68ae6b42c94983d1284da86e9686edd` (protocol 38/state 54). This change uses protocol 39/state 55. Both staged prerequisite transactions call `apply_request_at` with the original apply-entry position, preserving the parent's guarded scheduling and batch writes.

Nine real lifecycle, Raft, planner and HTTP controls first produced eight expected-behaviour reds and one real Process positive control. Narrowing the logical fence to actual held jobs retained an independent held-app display-label positive control; eleven cases produced nine baseline reds and two passes. A second parent review covered a same-spelled app/job in different namespaces: two controls passed and the independent batch-label control failed. Its minimal effective-namespace guard then passed all fourteen capacity/identity cases. No owner-node field or administrator recovery API was introduced.

Held migration and ordinary-job claims also protect placement capacity. These
jobs run on the receiving leader; the claim records no authoritative worker
assignment. Both app and batch planners therefore reserve the complete held
CPU/memory request on every candidate, without crediting a guessed replica or
an unrelated report. Before app publication this includes all jobs; afterwards
it includes only the ordinary tail. This can over-reserve capacity, including
capacity already reported locally. It prevents new placements from spending
uncertain commitments; it does not add initial job capacity or quota admission.
Positive terminal settlement releases the corresponding held request.

The complete combined source passed 68 independently enumerated focused cases in 5.126 seconds and full offline `make ci`: both Clippy configurations, all 5,640 selected portable tests without retries, two doctests, 52 script controls and ignored-owner checks. The full portable JUnit is retained at `/tmp/reliaburger-review2-534-held-capacity-evidence/combined-full-ci-junit.xml`, SHA-256 `e90eb6eb98673d1d86eadc8b90f12ec9dfa4b9343437e90a5c21f1aafd8d1239`. Earlier failed and diagnostic receipts above remain evidence; this later complete pass does not erase them.

The managed publication worktree inherits the actual qualified parent. All compiled sources match that portable snapshot except `tests/cluster_failover.rs`, which retains exactly the parent's two guarded fixture corrections. A separate full cluster snapshot overlays that exact file; its result and publication-worktree CI must be recorded before committing. Source reconciliation and the retained pre-inheritance stash are recorded in `/tmp/reliaburger-review2-534-actual-parent-integration/`.

The actual publication worktree subsequently passed complete `make ci`: all 5,640 portable tests in 190.433s, both Clippy matrices, two doctests, 52 scripts and ignored-owner checks. Independent cluster qualification passed all 42 cases in 344.139s. The managed-source comparison matched all 710 code/test/build inputs and both guarded fixture corrections; changed embedded manual prose was separately covered by this actual CI run. Actual full portable JUnit SHA-256 is `d21a0d30043509bd2ce98044945ee671fa4cc937afa17ed54b41d24c43208479`; cluster JUnit SHA-256 is `3bfd1ac704e100b73311073da80650625d7c64c519ef8ab8c1b45946ba0ffa11`. Logs and receipts remain in the two paths above. Exact-head remote qualification remains required before merge into the train.
