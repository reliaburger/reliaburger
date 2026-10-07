# Reliaburger v0.1.4 codebase audit — 3 October 2026

**28 issue descriptions: 27 concrete findings and one final hardening issue.** The original audit produced drafts only. On 3 October 2026 the maintainer approved publication and implementation: issues [#528–#555](https://github.com/reliaburger/reliaburger/milestone/11) now belong to milestone 0.1.5; [published-issues.json](published-issues.json) maps drafts to GitHub. Implementation proceeds through the `release-0-1-5-review2` merge train. Each draft includes current code excerpts, pinned source links, the actual/expected behavior, a reproduction or source-verified trigger, a fix direction and acceptance criteria.

Read all drafts together in [ISSUES.md](ISSUES.md), or open the individual descriptions below. P1 means a near-term authorization, data integrity, availability or core deployment defect; P2 means a concrete functional/admission/operational correctness defect. These are proposed priorities, not severity scores or assigned release milestones. Draft 28 is the final cross-cutting implementation step after the accepted individual fixes; it is a follow-up plan rather than another independently reproduced defect.

## Baseline and verification

- Audited checkout: `f4757e7789d3672d21f15ca203031d6604d7e11a` (`v0.1.4-3-gf4757e77`). The three post-release commits modify only qualification/releasing documentation; implementation is identical to `v0.1.4`.
- Compared the full **112 open and closed GitHub issue bodies and their available comments**. The local inventory contains 32 comments. Findings already described by that inventory were excluded, including bugs marked closed. Comparison was made on 3 October 2026; later issue activity can change deduplication.
- Three parallel source-review agents covered runtime/scheduling/storage, security/registry/builds, and networking/observability. The parent reviewed every included finding’s decisive code paths and independently reran the agents’ executable probes. The parent additionally reviewed configuration, CLI, GitOps, upgrade/install/release and qualification paths.
- Seven added temporary observation tests executed against the current source and confirmed CLI/GitOps/configuration behavior. They **assert the current faulty behavior**, so their passing result is evidence of reproduction, not proof of a fix. Five existing targeted registry/signing tests passed and establish baseline behavior/verification semantics.
- Independent parent probe reruns reproduced ingress redirect handling, the 30-second stream cutoff, external TCP DNS refusal, health resurrection, log-flush loss, 33-backend map refusal, shared-prefix metrics overwrite, authenticated batch dispatch, duplicate/stale batch completion, cron overflow in debug/release and invalid autoscale admission.
- Retained reproduction source and outputs are in [evidence/](evidence/README.md). The temporary integration test was removed from `tests/` after execution; no implementation or permanent test suite was changed.
- Linux rootful formatting, privileged eBPF/runc recovery, live 33-node publication, multi-node migration/webhook behavior, CPU exhaustion and a one-hour signing soak were **not** executed on this macOS host. Relevant drafts say when their finding rests on source/path analysis or a narrower pure/router reproduction. No full-suite or privileged-host qualification is claimed.

This is a broad subsystem audit, not a proof that every code path is correct. The list deliberately omits style, speculative defects and new feature requests already captured in completion-plan issues.

## Proposed issues

| Draft | Priority | Suggested title / individual description | Verification |
| --- | --- | --- | --- |
| 01 | P1 | [Distinct managed volume paths can share a loop image and reformat each other’s data](issues/01-volume-backing-image-collision.md) | Validation/path reproduced; Linux formatting path verified |
| 02 | P1 | [Build signer expiry breaks later builds and redeployment of cluster-signed images after one hour](issues/02-build-signature-one-hour-expiry.md) | Full source path; existing clock-injected expiry test |
| 03 | P1 | [Batch submission bypasses token workload scope and Deploy/HostExec grants](issues/03-batch-workload-authorization.md) | Authenticated router dispatch reproduced |
| 04 | P1 | [Manifest publication bypasses repository isolation for globally stored blob references](issues/04-manifest-descriptor-authorization.md) | Full authorization and publication source path |
| 05 | P1 | [Failed Ketchup flush discards rows and advances replay checkpoints past lost data](issues/05-log-flush-data-loss.md) | Failed flush, replay refusal and restart loss reproduced |
| 06 | P1 | [Metrics writers sharing an object-store prefix overwrite each other’s Parquet chunks](issues/06-metrics-object-key-collision.md) | Two production stores using file:// reproduced |
| 07 | P1 | [Cluster apply commits dependent app revisions before run_before migrations complete](issues/07-cluster-migration-gate.md) | Cluster and agent production paths verified; no live council run |
| 08 | P1 | [A previous job’s terminal status can falsely complete a new batch before launch](issues/08-batch-stale-run-completion.md) | Real router and delayed fake agent acknowledgement reproduced |
| 09 | P1 | [Ingress table rebuilds resurrect failed backends and continued probes never exclude them](issues/09-ingress-health-rebuild.md) | Production probe loop and table rebuild reproduced |
| 10 | P1 | [Ingress follows backend redirects and loses the original status, Location and cookies](issues/10-ingress-backend-redirects.md) | Real production proxy with loopback backend reproduced |
| 11 | P1 | [One service with more than 32 cluster backends blocks publication of the whole consumer view](issues/11-consumer-backend-capacity.md) | 33-endpoint map failure reproduced; production publication traced |
| 12 | P1 | [Registry token verification bypasses bounded Argon2 admission and blocks Tokio workers](issues/12-registry-blocking-token-verification.md) | Complete synchronous verification call path |
| 13 | P1 | [images.max_storage does not bound bare blobs or temporary uploads on disk](issues/13-registry-physical-storage-quota.md) | Quota accounting and all normal upload paths verified |
| 14 | P2 | [Large cron steps panic in debug and schedule every minute in release](issues/14-cron-step-overflow.md) | Current library/debug and exact-source/release reproduced |
| 15 | P2 | [Duplicate batch job names silently drop work and leave completion tracking stuck](issues/15-batch-duplicate-identities.md) | Real router dispatch and tracker state reproduced |
| 16 | P2 | [Cluster batches use stale or unlimited fallback capacity and do not retain admission reservations](issues/16-batch-capacity-admission.md) | Capacity translation, fallback and durable tracker source path |
| 17 | P2 | [Ingress aborts healthy SSE and download streams after 30 seconds](issues/17-ingress-stream-timeout.md) | Active 40-second backend stream aborted at 30 seconds |
| 18 | P2 | [External DNS queries fail over TCP, including retries after truncated UDP answers](issues/18-external-dns-tcp.md) | Real UDP/TCP responder and mock upstream reproduced |
| 19 | P1 | [Directory compile silently drops same-named workloads from distinct namespaces](issues/19-compile-namespace-collision.md) | Two-file directory compilation reproduced |
| 20 | P2 | [_defaults.toml silently ignores shared environment, memory and deployment settings](issues/20-compile-ignored-defaults.md) | Documented defaults fixture compiled; fields absent |
| 21 | P2 | [GitOps rejects _defaults.toml and ignores directory-derived namespaces](issues/21-gitops-directory-semantics.md) | Production execute_sync on local Git repository reproduced |
| 22 | P1 | [GitOps reports successful sync while dropping jobs and migration prerequisites](issues/22-gitops-silently-ignored-jobs.md) | Production execute_sync with app and migration job reproduced |
| 23 | P2 | [apply --dry-run calls materially changed workloads unchanged when the image is unchanged](issues/23-dry-run-incomplete-diff.md) | Plan generated with changed replicas, port and env reproduced |
| 24 | P2 | [CLI rejects permission and build manifests that refer to an already-created cluster namespace](issues/24-cli-existing-namespace-validation.md) | Same config refused locally and accepted by validate_against reproduced |
| 25 | P2 | [Directory compile exits successfully and emits an incomplete manifest after workload parse failures](issues/25-compile-partial-success.md) | Good plus malformed file returns successful partial config |
| 26 | P2 | [GitOps webhook returns 202 on followers but discards the trigger before leader sync](issues/26-follower-webhook-lost-trigger.md) | API queue, per-node startup and leader-gated runner traced |
| 27 | P2 | [Autoscaling accepts nonpositive and nonfinite targets, disabling or corrupting scaling decisions](issues/27-autoscale-invalid-targets.md) | Current Config validation accepts all four invalid values |
| 28 | P2 | [Unify behavioral contracts across entry points and add lifecycle regression coverage](issues/28-cross-path-contracts-and-lifecycle-hardening.md) | Cross-cutting follow-up plan based on verified drafts 01–27; implement last |

## Whitepaper and documented-contract findings

| Current claim | Unsupported/broken implementation behavior | Drafts |
| --- | --- | --- |
| Migration jobs complete before dependent apps start (§11, line 652). | Cluster apply publishes app state first; GitOps drops the job entirely. | 07, 22 |
| Directory-mode shared env/memory/deploy defaults and native GitOps trees (line 1050). | Only image defaults are applied; GitOps treats defaults as invalid config and omits path namespaces. | 20, 21 |
| Namespace-qualified configuration / directory namespaces (manual deploy chapter). | Compilation silently retains only one same-named workload across namespaces. | 19 |
| Health-aware ingress load balancing (line 528). | Table replacement resurrects already-failed backends indefinitely. | 09 |
| External names forward to host resolvers (line 562). | TCP external requests and truncated-UDP retries fail. | 18 |
| Webhook-triggered instant deploys (line 679). | Followers acknowledge and consume a local nudge that never reaches the leader. | 26 |
| Shared built-in registry/build trust. | One-hour code-signing leaves make retained built images undeployable and cached signers unusable. | 02 |

These drafts ask for concrete behavior to be repaired or explicitly refused/documented where the capability cannot be provided yet. Existing whitepaper discrepancies already covered by #300 and the feature families below are not relisted.

## Known work excluded / deduplication boundaries

Already tracked: GPU/cached-image placement (#359), rootless and process isolation (#360), upstream trust/digest/cosign and worker key separation (#361), CA operations/grace/recovery (#362), namespace identity/token lifecycle/audiences/audit (#363), future metrics/query/reporting architecture and live-metrics contracts (#364), packet fault qualification (#366), volume retirement/Apple parity (#367), Kubernetes translation gaps (#368), WebSocket/ACME parity (#369), appliance work (#401–407), retained retired runc secrets (#476), recovery/token/backup/catalogue/disk failures (#477–480), VIP withdrawal (#481), wrong-node snapshots (#482), test flakes (#497/#500), consumer/agent-loop timing (#505/#508), renewed TLS leaf pool behavior (#509), and log-pressure council resignation (#510).

Previously tracked/fixed defects were checked rather than copied: snapshot slug collisions (#294), GitOps signed-script/deadline/replay/drift (#295–297/#305), read-permission enforcement (#298), scale-to-zero (#299), log replay checkpoints (#308), build follower routing (#331), stale endpoints (#431), application pending-capacity reservations (#432), daemon self-capacity (#433), changed app placement constraints (#434), stop completion (#435), worker directory diagnostics (#436), and cross-node instance ordinals (#398). Each new draft identifies a related issue where confusion is plausible, especially the separate batch capacity path and shared-prefix object-store data loss.

Deferred or ambiguous candidates were omitted: build submission’s permission-action classification, same-namespace network allowlist wording, helper-only identity grace, broad performance figures, and unused helpers with suspicious naming. The supported process-runtime limits and target throughput figures are already documented or tracked; they are not new short-term findings here.

## Review and handoff

The Markdown under `issues/` is intended for copy/paste. Use its first heading as the GitHub title and the remaining text as the body. Source snippets show the current fault, not a proposed patch. Fixes should begin with failing regression assertions for expected behavior, then run the draft’s acceptance checks on the applicable host/runtime. The maintainer approved all 28 drafts for publication and implementation on 3 October 2026. Use one merge train PR for the approved audit work. All smaller implementation PRs target its integration branch: accepted individual fixes first, then draft 28’s phased consolidation and contract coverage. Review and fully qualify the final integrated result through that train.
