# Codebase completion plan

Reviewed 17 September 2026 against `fc11d253cf081d8eb9ef8f41389c2f98a0000545`,
the tip of [PR #165](https://github.com/reliaburger/reliaburger/pull/165).
This analysis is stacked on that release branch. It does not implement the fixes
below or claim that 0.1.0 is ready to publish.

The implementation is much further along than its open checkboxes suggested.
Registry streaming, managed laptop clusters, the installer, diagnostics, TUI
repairs and several error-propagation fixes are done. The remaining work is
substantial, though: archive exports can lose older generations, several safety
and recovery paths have ownership gaps, and a full catalogue still contains
unconditional unknown results. Green source CI does not close those gaps.

This is the current completion backlog. [progress.md](../progress.md) owns its
checkboxes; this document supplies evidence, ordering and acceptance criteria.
The [release plan](2026-09-16-v0.1.0-release-plan.md) still owns packaging and the
supported 0.1.0 contract. Dated reviews remain historical evidence, not competing
lists to implement blindly.

## Scope and confidence

The review reconciled all 43 existing plan/review documents with the roadmap,
progress ledger, component designs and current implementation. Source inspection
followed configuration and CLI entry points into runtime ownership, persistence,
networking, security, observability and test verdicts. Searches for TODOs, stubs,
panics and swallowed errors guided those reads; a textual match alone was not
classified as a defect. This is a broad engineering audit, not a formal proof or
a claim that every line and every failure interleaving was exercised.

Seven defect observations were reproduced locally: three export failures, one
CLI overflow, certificate lifetime truncation, a certificate-input panic and
premature port exhaustion. Other C items are source-confirmed gaps or unresolved
behavioural requirements from earlier reviews; their proposed regressions still
need to be written and run. H items include investigation and optional refactoring;
F items are missing capabilities, not evidence of failures in supported behaviour.
[Audit evidence and reproducible probes](../qualification/2026-09-17-code-audit.md)
record those distinctions and the hosted checks inspected.

There are **87 tracked work packages**: 58 correctness/contract items, 12
engineering follow-ups, 12 capability families and five acceptance/release gates.
The correctness items include 29 P1 priorities; these are engineering priorities, not
security severity ratings. A capability family can require several commits.

## What is already done

The progress reconciliation closes stale implementation boxes for Pickle streaming,
TUI log switching/viewport sizing, pooled health probes, storage/bootstrap/GitOps
error handling, the named startup/client/testapp panic fixes, and the Phase 15
command infrastructure. It splits mixed boxes so these completions do not hide
unfinished port allocation, certificate arithmetic, archive handling or fixtures.
Native Apple mounts/settings and dashboard replica reporting are implemented.

All applicable hosted checks on the base tip passed, including portable Linux
and macOS, privileged/rootless Linux, cluster and upgrade suites, coverage,
benchmarks, packaging, four native builds and PDF generation. Tag-gated release
publication and validation were skipped. This is fresh evidence for source CI;
it is not evidence that a public signed candidate was installed successfully.

The 241.75-second laptop run remains useful development evidence. It excludes
initial signed CLI/guest binary downloads and is one M2 Max run. Repeated cold
candidate installs, the complete real catalogue and sustained recovery remain open.

## Coverage map

| Area reviewed | Implemented baseline | Remaining work |
|---|---|---|
| Agent and runtimes | Lifecycle, health, restart, adoption, rootful/rootless runc, process and Apple backends | C05, C08, C18, C38–C40; F02, F10 |
| Consensus, gossip, scheduler | Durable Raft, replicated desired state, cordons, admission and reporting | C06–C07, C21, C37; F01, F06 |
| Networking and ingress | VIP routing, userspace DNS, cross-node ingress, HTTP/WebSocket forwarding | C15, C33, C43; F08–F09 |
| PKI and identity | Authenticated control plane, workload rotation, signatures and configured trust domains | C13–C16, C23; F03–F05 |
| Registry and storage | Authenticated OCI flow, bounded streamed pushes, managed volumes | C01–C05, C19–C22; F10 |
| Metrics and logs | Collection, export, SQL queries, rollups, alerts and cluster views | C01–C04, C19–C22, C42; F06–F07 |
| Deployments and upgrade | Rolling deployment records, adoption, coordinator quorum/rejoin checks | C05, C07, C37–C38; V02 |
| CLI, TUI and dashboard | Diagnostics, catalogue commands, managed setup and cluster status | C17, C23–C33, C39, C41; F07, F11 |
| Chaos and test ownership | Server grants, app/namespace leases, guarded cases | C06–C12, C29–C36; V01 |
| Build, docs and distribution | Installer, signed-release tooling, static website and hosted build matrix | H01–H12; V03–V05 |

## Implementation order and release boundary

Work in the waves below. Fix supported behaviour before adding a new roadmap
capability; within a wave, use the earliest owning roadmap phase first. This is a
hardening pass over already implemented phases, not permission to skip future
phase dependencies. The feature families remain explicit backlog rather than
quietly becoming prerequisites for the laptop release.

1. **Protect persisted data.** C01–C05 first; settle archive object identity and
   destination-scoped acknowledgements before checkpoint compaction or pruning.
2. **Close safety and recovery gaps.** C06–C15 and C37. Mixed-version policy must
   precede new replicated fields; readiness signalling must precede destructive
   qualification. Certificate lifetime precision precedes renewal acceptance.
3. **Fix runtime and data correctness.** C16–C22, C39–C40, C42–C43. Reporting chunks
   depend on the protocol policy; checkpoint bounds depend on wave 1.
4. **Make diagnostics and acceptance verdicts trustworthy.** C23–C36, C38, C41.
   Exact lease ownership and fixtures precede the complete catalogue; benchmark
   verdict fixes precede using comparisons as release evidence.
5. **Finish engineering follow-ups.** H01–H12. Optional module/parser changes can
   be deliberately deferred with a recorded reason; they are not release blockers.
6. **Implement declared missing capabilities in roadmap order.** F01–F12. Each
   family needs a separate supported contract. Future Windows/federation/TPM work
   does not block a scoped Linux/macOS 0.1.0.
7. **Qualify and publish the same bytes.** V01–V05. Qualification can run alongside
   fixes, but final evidence must identify the exact candidate and configuration.

P1 items affecting the advertised runtime must be fixed or the affected behaviour
must be explicitly removed from release scope with an enforced refusal. A label
in a document is insufficient for a data-loss or unsafe-operation path. P2 items
also need a release disposition: fixed, enforced unsupported boundary, or a
specific documented limitation and follow-up. Do not treat optional H refactors
or all F families as mandatory 0.1.0 work. Keep the original release gate open
until those dispositions, H12 dependency triage and V01–V04 evidence exist.

For each fix, write a regression that demonstrates the intended behaviour, make
it pass, and commit that fix separately. Do not combine independent fixes merely
because they touch one file. Each commit includes its relevant book explanation;
chapter numbers below identify where to explain the design, Rust syntax first
introduced and the test. Larger F families must be split into separate feature
commits. Only then check off the corresponding progress item, with commit and
verification evidence. Observation probes in the audit record are not replacement
regressions: they intentionally demonstrate today's incorrect behaviour.

## Correctness and behavioural contracts

### C01 — Preserve every exported log generation

**Priority:** P1. **Wave:** 1. **Book chapters:** 05, 06.

Evidence: `src/ketchup/export.rs:export_logs`.

The checkpoint hashes contents, but the remote object key is only node/filename; a reused filename overwrites the older archive.

**Completion test:** Export two different generations under the same filename; both remain independently queryable after restart.

### C02 — Scope export acknowledgements to their destination

**Priority:** P1. **Wave:** 1. **Book chapters:** 06.

Evidence: `src/ketchup/export.rs:ExportCheckpoint`.

The shared checkpoint contains no destination or node-prefix identity. Exporting to B after A reports no new files even when B is empty; pruning also consumes this evidence.

**Completion test:** Export A then B, change the node prefix, restart, and exercise disk-pressure pruning; only acknowledgements for the configured destination permit deletion.

### C03 — Make export checkpoints durable and serialised

**Priority:** P1. **Wave:** 1. **Book chapters:** 06.

Evidence: `src/ketchup/export.rs:ExportCheckpoint::save`; `src/bin/bun.rs:checkpoint.save`.

Checkpoint writes truncate in place, independent exporters can race, and the agent still discards checkpoint-save errors even though the offline CLI now reports them.

**Completion test:** Concurrent/manual/periodic exports and injected write/rename/fsync failures preserve the last valid checkpoint and expose partial success without premature pruning.

### C04 — Report non-transient export read failures

**Priority:** P1. **Wave:** 1. **Book chapters:** 06.

Evidence: `src/ketchup/export.rs:export_logs`.

Directory iteration and file-read errors are discarded. A directory named logs.parquet produces a successful no-new-files result.

**Completion test:** Permission errors, malformed entries and I/O failures return contextual errors; only an explicitly handled concurrent NotFound can be skipped.

### C05 — Persist new instances during rolling deployments

**Priority:** P1. **Wave:** 1. **Book chapters:** 01, 07, 14.

Evidence: `src/bun/agent.rs:RegisterRollingInstance`; `persist_instance_record`.

Registration calls persistence before the instance is in the supervisor; persistence returns early, leaving mid-rollout workloads without adoption records.

**Completion test:** Kill and restart Bun between each prepare/start/health/publish step; adopt or clean up every new instance without duplicating workloads, ports or mounts.

### C06 — Reserve node-fault capacity across the cluster

**Priority:** P1. **Wave:** 2. **Book chapters:** 08, 15.

Evidence: `src/bun/api.rs:check_node_fault_cluster_safety`.

Safety counts unavailable voters from current observations but does not reserve in-flight kills. Concurrent requests can pass the same quorum check.

**Completion test:** Concurrent kills via different API nodes and leader changes never exceed the permitted unavailable-voter budget; reservations expire or reverse reliably.

**Completed on PR #167:** One durable node-experiment slot serialises kills, drains, pressure and legacy council partitions. Grants bind the exact normalised request to a target process and increasing sequence. Expiry triggers a target-side fence; capacity is released only after confirmed reversal, including pressure-helper absence. Snapshot/leader-term and actor fencing regressions are implemented. Protocol 4/state 3 refuse earlier development clusters. Membership changes wait behind a committed barrier until reversal. Live concurrent kills and leader failover pass in 27.93s, including exactly one closed transport gate, inherited ownership and expiry recovery. Legacy quorum refusal, manual recovery and privileged pressure cleanup pass too; a fresh pressure controller detects surviving helpers before acknowledging cleanup. The full Linux library checkpoint passes 3,236 tests, followed by 152 final gossip tests and strict all-target/all-feature Clippy. Separate gossip fixes repair both seedless bootstrap isolation and partial membership loss.

### C07 — Define and enforce mixed-version compatibility

**Priority:** P1. **Wave:** 2. **Book chapters:** 02, 14.

Evidence: `src/bun/api.rs:test lease creation`; `src/council/types.rs`; `src/cluster/orchestrate.rs:build_cluster_cache`.

Lease writes have no peer-schema negotiation/upgrade barrier, while old nodes lacking readiness are fenced. Historical D1/D2 recommendations were not implemented.

**Completion test:** A supported old/new pair upgrades with safe scheduling and decodable logs/snapshots; unsupported peers and new writes are refused before replication. Never infer healthy readiness from missing evidence.

**Implementation:** the approved 0.1.0 policy requires fresh clusters, with exact
protocol/state generation equality for later rolling upgrades. Generation 2 is
explicit in cluster traffic, joins, durable state, snapshots and backups. A
verified candidate must answer a bounded `--compatibility` query before staging;
rollback checks retained binaries too. Development state is preserved and refused.
The real Linux suite exercises different product versions with equal formats,
including rollback and pause/resume after a deliberately broken replacement.


### C08 — Publish readiness after resources are acquired

**Priority:** P1. **Wave:** 2. **Book chapters:** 01, 15.

Evidence: `src/bun/readiness.rs:spawn_owned`; `spawn_reconstructible`.

Both helpers mark Ready before polling the owner task or invoking its factory. A not-yet-bound or failing task can briefly advertise readiness.

**Completion test:** An owner that never signals stays Starting; bind failure never becomes Ready; restart generations cannot publish stale readiness.

### C09 — Bound lease cleanup lock acquisition

**Priority:** P1. **Wave:** 2. **Book chapters:** 15.

Evidence: `src/testkit/lease.rs:begin_cleanup_operation`.

Cleanup awaits an operation lock without a bound; one held lease can stall a sequential reaper before it reaches other expired leases.

**Completion test:** Hold lease A's operation guard indefinitely while B expires; B is reclaimed promptly and A remains retryable, with a Busy/deferred outcome.

### C10 — Record fault ownership before injection can be cancelled

**Priority:** P1. **Wave:** 2. **Book chapters:** 08, 15.

Evidence: `src/testkit/chaos/mod.rs:ChaosGuard::inject_fault`; `inject_partition`.

A successful injection response is tracked only after another await. Cancellation in that window can leave an effect absent from the cleanup ledger.

**Completion test:** Cancel before send, after server acceptance and before tracking; exact-id cleanup or an explicit unknown pending operation remains, never a clean report.

### C11 — Persist standalone leases through directory-sync failures

**Priority:** P1. **Wave:** 2. **Book chapters:** 04, 15.

Evidence: `src/testkit/lease.rs:persist_leases`.

The file is synced and renamed, but its parent directory is not synced. The fixed .tmp path also deserves the same private atomic-write discipline as other state.

**Completion test:** Crash/failure injection at every write boundary preserves acknowledged lease ownership; private unique temporary files cannot clobber another writer.

### C12 — Sweep pressure leftovers even after disabling pressure

**Priority:** P1. **Wave:** 2. **Book chapters:** 08, 15.

Evidence: `src/smoker/node_pressure.rs:prepare_controller`.

The disabled-limits return happens before stale-cgroup cleanup. Disabling the feature after a crash can retain its old resources.

**Completion test:** Create an owned stale helper/cgroup, restart with pressure disabled, and prove cleanup without advertising injection capability or touching unrelated cgroups.

### C13 — Issue certificates with exact validity timestamps

**Priority:** P1. **Wave:** 2. **Book chapters:** 04.

Evidence: `src/sesame/ca.rs:issue_end_entity_cert`; `node_leaf_params`; `system_time_to_date_components`.

Issuance rounds validity to calendar days; a requested 60-second leaf had a zero-second validity interval. Hand-written date conversion also mishandles century boundaries.

**Completion test:** Inject time; verify second-level lifetimes across midnight, leap days and 2100. Use a single captured instant and checked timestamp arithmetic. Workload identity already uses precise timestamps and must retain that behaviour.

### C14 — Renew and hot-reload every served certificate class

**Functional fixes completed on PR #167. Sustained qualification remains V02.**

**Priority:** P1. **Wave:** 2. **Book chapters:** 03, 04, 15.

Evidence: `src/wrapper/tls.rs:IngressCertResolver::key_for`; `src/bin/bun.rs:security refresh`.

**Original finding:** The ingress resolver returned cached keys without expiry checks; node/API leaves were loaded at process start and reported restart_required. Workload rotation did not cover these consumers.

**Completion test:** Advance time through renewal/expiry and replace configured cert files while serving TLS; prove new connections get valid leaves, old connections drain safely and diagnostics name unsupported consumers.

**Issuer-chain prerequisite completed:** The resolver now sends the original
root-signed intermediate with every ingress leaf. Root-only real TLS trust fails
with `UnknownIssuer` before the fix and succeeds after it (0.02s); ten ingress
TLS tests and strict Linux 1.97/macOS 1.98 Clippy pass. This does not close renewal.
Cached-leaf renewal now uses the X.509 validity midpoint, rejects expired cache
entries and limits issued leaves to the original issuer certificate's validity.
Operator files now reload as validated pairs, retaining the last usable pair
during incomplete or invalid replacements. Real Wrapper acceptance verifies
new full handshakes use the replacement and existing HTTP connections survive
(3.26s); expired and future-dated initial files are refused.
Ingress session resumption is disabled: a reconnect must consult the current
resolver and validate its certificate, matching the node/API policy. Real TLS
1.2/1.3 reconnections pass for static and dynamic resolvers (0.02s), with renewal,
file reload and strict Linux/macOS Clippy still passing.
API/registry/ingress connections now enforce the one-hour lifetime policy,
starting HTTP draining 30 seconds before the deadline. The deadline follows the
I/O stream through WebSocket upgrades. Three failing-first timer tests, actual
upgraded ingress retirement and reconnect (0.12s), and Bun API in-flight draining
(0.05s) pass. Raft and reporting already use short deadline-bounded RPC exchanges.
Node identity renewal remains, split into separately reviewable changes:

- Identity persistence is implemented: one private snapshot is the source of
  truth, with validated key/chain/node/serial binding and signed validity dates.
  Failed export replacement preserves the previous snapshot; missing/corrupt
  snapshots never fall back to PEM exports, and partial initial installation is
  refused. Seventeen persistence tests, 29 managed-bootstrap tests, the full 3,255-test
  Linux library checkpoint (19 privileged gates), 11 security and two API TLS
  integrations, and strict Linux/macOS Clippy pass.
- The shared live credential handle is implemented. It preserves node/CA
  identity, serialises replacements, persists before publishing, refuses stale
  serials and completes publication after caller cancellation. Five real TLS
  tests pass (6.15s), including required/optional authentication, bound/unbound
  peer verification, failure recovery and expired-client refusal; the deliberate
  post-persistence cancellation test passes (0.04s). Strict Linux/macOS Clippy
  passes. CA rotation remains separate; expired clients must never become
  omitted certificates on optional-mTLS API connections.
- Bun now shares the live handle across API/registry listeners, Raft/reporting
  clients and servers, internal HTTPS clients and diagnostics. Seven live tests
  pass (6.54s), including reused HTTPS clients and current diagnostic metadata.
  The real three-node test replaces every identity, revokes all old leaves,
  then proves Raft replication and fresh reports still work (15.18s).
- Node issuance now caps leaf expiry at the real Node CA expiry and refuses
  expired or future issuers. Both failing-first regressions pass (0.01s), checking
  local issuance, direct CSR signing and reconstruction from stored CA DER, with
  274 Sesame tests (one privileged gate), seven live identity tests and strict
  Linux/macOS Clippy. Sidecar dates cannot extend the signed issuer window.
- The authenticated renewal endpoint is implemented. It requires the service
  principal and the actual TLS peer leaf, validates the CSR's node identity,
  checks current validity and leaf/issuer revocation with quorum-backed reads,
  and allocates distinct serials through Raft. Followers refuse rather than
  forward another node's identity. Seven renewal tests pass (0.24s), the actual
  Bun listener proves TLS peer attribution and draining (0.05s), and strict
  Linux/macOS Clippy passes.
- The automatic worker renews at the signed lifetime midpoint, persists before
  publishing and reports live health. It resolves the leader on every attempt,
  refuses redirects, bounds response size/time and retries failures after five
  seconds. Twelve endpoint/worker regressions pass (12.13s), including failed
  persistence, oversized responses, cancellation and stopped-owner diagnostics.
  The real three-node request interrupted by leader failure succeeds on the
  new leader and reloads from disk (20.40s). Actual Bun startup automatically
  renews a due leaf, serves HTTPS and reuses it after restart (3.64s). All 23 Bun
  tests, seven live TLS tests and strict Linux/macOS Clippy pass.
- Hosted CI caught the missing renewal-route matrix entry and middleware-layer
  handling in the syntax-tree audit. Both are repaired; all eight route/scope
  audit tests pass (0.19s), including refusal of unknown wrappers.
- Expired offline identities still require authorised re-enrolment, and CA
  rotation remains F04. Sustained expiry/storage/upgrade qualification is V02;
  these functional tests do not mark that separate release gate complete.

### C15 — Make ingress serials unique across nodes and restarts

**Priority:** P2. **Wave:** 2. **Book chapters:** 04.

Evidence: `src/wrapper/tls.rs:INGRESS_SERIAL`.

The atomic counter starts at the same fixed value in every process. The earlier fix guarantees only process-local uniqueness, not per-issuer uniqueness.

**Completion test:** Mint under one CA from separate processes and after restart; no repeated serials. Specify revocation ownership and persistence rather than claiming the current counter solves it.

**Completed on PR #167.** Each ingress issuance obtains an independent positive
20-byte serial with 158 random bits from the operating system; randomness failure
refuses issuance. This gives probabilistic cross-process uniqueness without a
counter that can reset or a consensus call in the synchronous handshake. The
certificate carries its identity and the resolver owns its cache. Individual
ingress revocation remains unsupported; the 64-bit node CRL API does not own or
truncate these serials. Four real processes sharing one CA reproduce the old
collision and verify signed leaves after the repair. Ten ingress TLS tests,
265 Sesame tests and strict Linux Clippy pass.

### C16 — Reject invalid certificate inputs without panicking

**Priority:** P2. **Wave:** 3. **Book chapters:** 04.

Evidence: `src/sesame/ca.rs:issue_end_entity_cert`; `generate_intermediate_ca`.

The Result-returning helper unwraps DNS SAN conversion; a non-ASCII SAN panics. An invalid Root intermediate role also reaches unreachable!.

**Completion test:** Public issuance returns typed errors for invalid SAN/role/lifetime input. Confirm admission boundaries separately; this audit does not claim remote handshake exploitability.

### C17 — Reject overflowing CLI durations

**Priority:** P2. **Wave:** 3. **Book chapters:** 09.

Evidence: `src/relish/commands.rs:parse_since`.

amount * multiplier is unchecked. The debug CLI exits 101 for --since 18446744073709551615d; release arithmetic can wrap.

**Completion test:** Boundary and overflow inputs in debug/release return ordinary invalid-flag errors; valid epoch and duration inputs preserve semantics.

**Fault CLI follow-up (19 September):** The separate fault/test duration parser
also multiplied minutes/hours without checking and narrowed `u128` nanoseconds
with a truncating cast. Both boundary regressions fail before the fix.
`checked_mul` and `u64::try_from` preserve accepted units and reject overflow
before any request. All 19 fault-command tests pass on macOS/Linux, with strict
all-target/all-feature Clippy on both. This closes the additional numeric bug;
H06's library/grammar evaluation remains separate.

### C18 — Find a free port before reporting exhaustion

**Priority:** P2. **Wave:** 3. **Book chapters:** 01.

Evidence: `src/grill/port.rs:PortAllocator::allocate`.

After 1,000 random probes the allocator reports Exhausted even when a free port remains; all 32 audit attempts failed with one free port.

**Completion test:** Nearly full pools always allocate the final free port; genuinely full, empty and inverted ranges refuse. Preserve concurrent uniqueness with a bounded deterministic fallback.

### C19 — Count only successful metric-file pruning

**Priority:** P2. **Wave:** 3. **Book chapters:** 06.

Evidence: `src/mayo/store.rs:prune`; `src/mayo/rollup_store.rs:prune`.

Both increment deleted after ignoring remove_file failure, overstating successful cleanup under disk pressure.

**Completion test:** Inject permission/I/O errors; counts match actual removals and failures reach the caller without losing surviving data.

### C20 — Deduplicate rollup ownership at query merge

**Priority:** P1. **Wave:** 3. **Book chapters:** 06, 11.

Evidence: `src/bun/api.rs:cluster metrics queries`; `src/mayo/rollup_store.rs`.

Per-aggregator node/minute dedup does not remove overlap when two aggregators retain the same minute after reassignment.

**Completion test:** Reassign a node with overlapping history and query both aggregators; each node/minute/series contributes exactly once across restart and retry.

### C21 — Bound and chunk reporting payloads

**Priority:** P2. **Wave:** 3. **Book chapters:** 02, 11.

Evidence: `src/reporting/transport.rs:MAX_REPORT_SIZE`; `src/config/node.rs:ReportingTreeSection`.

A single rollup over 1 MiB cannot be sent; max_events_per_report is explicitly reserved and unenforced.

**Completion test:** Oversized event/metric batches deliver within bounded memory using versioned chunks and idempotent assembly, or fail admission explicitly; no silent event loss.

**Completed (18 September):** Use explicit refusal for 0.1.0. Preflight serialised size before allocating a payload; reject over 1 MiB or 100 events, and refuse custom event-limit configuration. Bound both connection tasks and queued reports to 16; shutdown aborts and joins stalled peers. Protocol 3 adds volatile queue-admission receipts, with missing receipts and capacity exhaustion returned to senders. State workers log failures and bound snapshot admission/response waits; normal rollup failures request the existing five-minute backfill and name older gaps explicitly. Event production and automatic chunking remain F06. Three before/after regressions prove former false success and stalled shutdown. All 3,217 Linux library tests pass (19 privileged gates), strict all-target/all-feature Clippy passes, and TCP/TLS (3), reporting-tree (1), startup compatibility (2) and security (11) integration tests pass. State stays generation 2.

### C22 — Bound export checkpoint growth safely

**Priority:** P2. **Wave:** 3. **Book chapters:** 06.

Evidence: `src/ketchup/export.rs:ExportCheckpoint`.

Every exported content ID remains forever, increasing load/save work and memory as source retention removes old files.

**Completion test:** Long retention/export/restart simulation bounds checkpoint size while preserving immutable archive generations and correct destination-specific pruning proofs.

### C23 — Distinguish expired certificates from renewal warnings

**Priority:** P2. **Wave:** 4. **Book chapters:** 15.

Evidence: `src/relish/wtf/diagnose.rs:certificate checks`.

Expired leaves are warnings, and automatic_rotation can suppress time-expiry evidence; the OK message incorrectly promises 14 days for healthy short-lived leaves.

**Completion test:** Expired means critical regardless of stale rotation labels; healthy rotating short-lived leaves are OK with accurate wording; near-expiry and unknown consumers remain distinct.

### C24 — Keep unknown council membership out of quorum arithmetic

**Priority:** P2. **Wave:** 4. **Book chapters:** 15.

Evidence: `src/relish/wtf/collect.rs:collect_council_evidence`.

An unavailable council endpoint can become a degraded zero-member observation, which downstream code can interpret as observed quorum loss.

**Completion test:** Missing membership produces unknown; an observed failed majority produces critical. Cover empty, stale and partially reachable observations.

### C25 — Identify diagnostic filesystems by identity

**Priority:** P2. **Wave:** 4. **Book chapters:** 15.

Evidence: `src/relish/wtf/collect.rs:coalesce_disk_filesystems`.

Grouping by node/used/total merges distinct equally sized filesystems and can split one filesystem as readings change.

**Completion test:** Use a stable device/mount identity; two identical-size disks remain separate and multiple storage domains on one filesystem coalesce.

### C26 — Match diagnostic instance and VIP evidence exactly

**Priority:** P2. **Wave:** 4. **Book chapters:** 15.

Evidence: `src/bun/diagnostics.rs:collect restart evidence`; `src/bun/agent.rs:trace validation`.

Substring matching can confuse api-1 with api-10 or a VIP with a longer textual address.

**Completion test:** Adversarial prefixes and IPv4/IPv6 forms cannot satisfy another instance/address's evidence; parse structured probe output where possible.

### C27 — Preserve watch-mode failures and exit outcomes

**Priority:** P2. **Wave:** 4. **Book chapters:** 15.

Evidence: `src/relish/wtf_cmd.rs:run_watch`.

A collection error ends watch immediately; Ctrl-C returns Clean regardless of the last observed report.

**Completion test:** Fail one collection then recover; watch continues, displays unknown evidence and returns the last meaningful outcome on interruption.

### C28 — Make TUI status reflect the cluster

**Priority:** P2. **Wave:** 4. **Book chapters:** 13.

Evidence: `src/relish/tui/data.rs:HttpDataProvider::status`.

The provider still calls the node-local status endpoint while the CLI and dashboard now use cluster status. A remote replica can disappear from the TUI.

**Completion test:** A three-node remote-only replica appears with node identity and namespace; partial collection is visible. Define local-only logs/history views explicitly.

### C29 — Fail benchmark comparisons when required metrics vanish

**Priority:** P1. **Wave:** 4. **Book chapters:** 15.

Evidence: `src/relish/bench_cmd.rs:run`; `render_human`; `src/testkit/bench/compare.rs`.

Missing metric lists exist in the result but neither drive the non-zero exit nor appear in the human verdict.

**Completion test:** A disappeared baseline metric produces a named regression and Problems unless the comparison is explicitly informational; newly added metrics are shown separately.

### C30 — Replace unconditional unknown catalogue cases with real evidence

**Priority:** P1. **Wave:** 4. **Book chapters:** 15.

Evidence: `src/testkit/cases/secrets_config.rs`; `image_registry.rs`; `workload_identity.rs`; `src/testkit/report.rs`.

The audit found four cases returning Unknown unconditionally. Secret/config and workload-identity observations are now implemented; runnable registry deployment remains. Development and full profiles share a failed_any verdict, so missing required evidence still rejects acceptance.

**Completion test:** Implement runnable registry, secret/config and workload-identity fixtures in separate commits. Full profiles require their observations; development reports supported skips/warnings honestly and never converts missing evidence to success.

**Secret/config fixtures completed (18 September):** A scoped read-only API
returns only the active public age recipient and generation. The harness fetches
it over its authenticated client, seals independent ciphertexts, deploys a leased
container and reads the actual owning node's environment, including Unicode and
newline preservation and an unchanged plaintext value. The config-file probe
also selects the owning node. Two API regressions fail against the missing
endpoint; the actual catalogue initially returns Unknown. The public-key tests,
eight route audits, 117 testkit tests and strict Linux/macOS Clippy pass. The real
TLS/runc catalogue now passes all three cases with confirmed cleanup (65.36s),
after the separately committed C34 retirement/observation repairs. Its ignored
`runc_` acceptance test is included by the existing privileged CI target.
**Workload identity fixture completed (18 September):** The real TLS/runc
regression initially returns Unknown. The case now deploys a leased container,
reads only its public certificate bundle on the owning node and validates the
chain, current validity and client-auth usage against the caller’s configured
CA anchors. Its sole SAN must match the exact cluster/namespace/app SPIFFE URI.
The three-case group passes; a second run trusting only the Node CA rejects the
Workload-CA-issued leaf, with confirmed cleanup in both runs (51.73s total). All 117 testkit tests
(0.62s) and strict Linux/macOS Clippy pass.
No private key/token is read, and a mounted CA never becomes a trust anchor.
A missing configured CA remains conditional Unknown. Runnable registry
deployment remains open and depends on C34 registry ownership.

The broad library checkpoint was not green: 3,276 passed, one upgrade probe
failed with Linux ETXTBSY, and 19 privileged gates were ignored. This execution
race was subsequently repaired with bounded ETXTBSY-only retries under V02. Cluster API port-zero propagation is repaired separately under H03; the runc fixture uses the existing fixed-port retry harness.

### C31 — Give concurrent test runs distinct namespaces

**Priority:** P2. **Wave:** 4. **Book chapters:** 15.

Evidence: `src/relish/test_cmd.rs:generate_run_id`.

Run IDs are seconds truncated to 32 bits; simultaneous invocations generate identical namespace prefixes.

**Completion test:** Many invocations in one clock tick produce distinct valid IDs; fixed namespaces retain explicit collision/refusal semantics without cross-run cleanup.

### C32 — Use typed case outcomes and validated runner configuration

**Priority:** P2. **Wave:** 4. **Book chapters:** 15.

Evidence: `src/testkit/registry.rs:UNKNOWN_MARKER`; `src/testkit/runner.rs`; `src/testkit/bench/runner.rs`.

Runtime outcomes depend on a magic string prefix and public runner paths rely on expect for non-zero timeouts.

**Completion test:** A typed error/outcome cannot misclassify workload text as a skip/unknown; invalid direct RunConfig callers receive errors and output/exit contracts stay compatible.

### C33 — Discover actual registry and ingress test endpoints

**Priority:** P2. **Wave:** 4. **Book chapters:** 15.

Evidence: `src/testkit/context.rs:registry_base`; `src/testkit/cases/ingress.rs:wait_for_proxy`.

Registry host extraction splits at the first colon and hardcodes 5050; ingress now parses URLs correctly but still assumes native port 80.

**Completion test:** Bracketed IPv6, configured ports, HTTPS and host-forwarded managed clusters reach the declared service. Workload requests never inherit control-plane credentials.

**Completed on PR #167:** Bun reports actual bound service origins, including ephemeral ports. Managed contexts substitute only explicit host forwards, with a configurable authenticated registry forward. Ingress probes preserve workload Host/SNI through forwards using a separate CA-aware client with normal hostname verification and no API credentials. The 3,241-test library checkpoint, 13 final endpoint tests, 29 managed-cluster tests, real ephemeral Bun listener probes, managed-status integration and strict all-target/all-feature Clippy pass. A focused run also caught and removed a test-only dependency on another TLS test installing the process crypto provider.

### C34 — Define lease ownership for the remaining test resources

**Priority:** P2. **Wave:** 4. **Book chapters:** 15.

Evidence: `src/testkit/lease.rs:LeasedResource`; `src/testkit/bench/suites.rs`.

Only apps and namespaces are leased. Jobs, images, tokens, mounts and node effects need their own authority/cleanup contracts; cold-cache image benchmarks cannot safely evict arbitrary images.

**Completion test:** Extend one resource family per commit with durable ownership, renewal bounds, leader/client-death cleanup and refusal to delete pre-existing resources.

**Managed-storage family (19 September):** Durable per-application journals claim
managed volumes and generated configuration before provisioning. An explicit
lease retirement waits for runtime exit, then safely unmounts/removes only that
storage before acknowledging the placement. Ordinary Stop/rebalance preserves
data; host sources remain outside ownership. Corrupt, unowned, overlapping and
symlinked paths refuse. Disposable test snapshots are unsupported. Full library
checkpoints, final volume/lifecycle regressions, strict Clippy and binary
compatibility pass on macOS/Linux. Four privileged Linux cases cover real ext4,
Btrfs, busy/unknown mounts and physical owner death. Three-node expiry tests
preserve former-placement data until cleanup and retain a failed owner's lease
until repair. Protocol 8/state 12; lease schema 4. See the
[implementation and qualification record](2026-09-19-managed-test-storage.md).
Registry leases, atomic process identity and the pre-adoption runtime crash
window remain open.

**Ordinary-job recovery decision:** The user selected explicit rerun for a batch
job whose outcome is unknown after Bun crashes. Recovery must persist/report
that uncertainty rather than automatically repeat the execution. The durable
intent and retry-budget implementation is recorded in the
[job recovery plan](2026-09-19-job-recovery.md); the separately agreed cron skip
policy remains unchanged.

**Token family completed on PR #167:** Atomic Raft issuance binds a non-admin
credential to the exact authenticated lease owner, its namespace and expiry.
Reserved test names cannot be created without a lease; duplicate and already
owned names refuse without returning an uncommitted secret. Cleanup fences
against the credential fingerprint and survives snapshots and leader failure.
Renewal never extends existing token expiry. The catalogue preserves explicit
CA/host-forward settings and proves namespace refusal with a read-only probe.
Protocol 5/state 4 and lease schema 2 require fresh development clusters.
Both ownership API regressions fail before implementation. Five final API tests
(2.36s), 74 Raft state-machine tests, 13 local lease tests, actual HTTPS Bun/Relish
catalogue execution and cleanup (12.33s), and three-node leader-failure cleanup
(17.60s) pass. Two compatibility checks (0.01s), all 17 client tests (0.17s), and strict
Linux/macOS all-target/all-feature Clippy also pass. Hosted CI then exposed
three old-format literals in compatible join/upgrade fixtures (ten failures).
The fixtures now derive their declaration from CURRENT; explicit incompatible
fixtures remain. The complete Linux library checkpoint passes 3,263 tests with
19 explicit privileged gates (39.69s), with strict Linux/macOS Clippy.

**Job cleanup prerequisite completed:** Stopping a registered cron job now
retires its exact namespace's schedule even before its first instance exists.
The running-agent command regression fails before the fix; all 104 agent tests
pass afterwards (12.57s), with strict Linux/macOS Clippy. In-flight job-worker
fencing and durable job ownership are still open.

**Admission prerequisite completed:** Lease-bearing manifests no longer fall
back to ordinary apply when they contain only jobs, builds or permissions.
Unsupported/empty manifests refuse before HTTP; the API also refuses unleased
jobs in reserved test namespaces. Both regressions fail before the repair;
14 context tests and 48 lease-filtered tests pass (one explicit gate), with
strict Linux/macOS Clippy. This closes the bypass, not durable job support.

**Job retry prerequisite (18 September):** Both a successful retry and an
explicitly stopped retry were selected again merely because their historical
restart count was non-zero. Two running-agent regressions reproduce this.
Track pending retry intent separately; retain the historical counter and clear
intent on success, stop, accepted restart or exhausted budget. Both regressions
pass after the fix (5.03s), along with 44 supervisor tests, 104 agent tests
(12.28s) and strict Linux/macOS Clippy.

**Cron worker fencing prerequisite (18 September):** The running-agent
regression reproduces an untracked cron worker during blocked runtime creation.
Cron and ordinary apply now share operation admission and completion tracking.
User/cleanup stops refuse before mutation while a worker owns their target;
the node-local HTTP API returns 409 with the operation ID. Internal egress-loss
emergency stops retain their fail-closed path. The regression now also checks
HTTP conflict/success and overlapping-deploy refusal. Three lifecycle tests,
104 agent tests, 114 API tests and strict Linux/macOS Clippy pass.

**Schedule removal prerequisite (18 September):** Reapplying a job without a
schedule now removes its prior cron registration in that namespace. Actual
cron firings bypass registration and retain their recurring schedule. The
command-channel regression fails before the fix; all 105 agent tests (12.50s),
three lifecycle tests (5.03s) and strict Linux Clippy pass.

**Stop acknowledgement prerequisite (18 September):** Local stop now requires
observed exit even after SIGKILL, bounds signal/observation calls and preserves
adoption records and enforcement on uncertainty. Three HTTP regressions fail
before the fix; a fourth covers stalled kill. All seven lifecycle tests
(12.21s), 105 agent tests (11.79s), 48 lease-filtered tests (one explicit gate)
and strict Linux Clippy pass.

**Runtime observation prerequisite (18 September):** Four public ProcessGrill
regressions fail before the fix: stop/kill/drop signal a PID whose recorded
identity no longer matches, and a failed child-status read reports Stopped.
Explicit signals now refuse an unverified adoptee, drop leaves it alone,
forced waits are bounded and signal/wait errors propagate. Process/runc status
reads and the shared adopted-process poller retain inspection errors; rootless
shutdown propagates them. A direct runc status regression preserves ownership
when the launcher cannot be observed. All 265 Linux runtime tests pass (13
explicit gates, 7.57s), plus seven lifecycle tests (12.03s), 106 agent tests
(11.73s), native process tests and strict Linux/macOS Clippy. This is not an
atomic kernel-backed process-identity guarantee or complete process-tree/runc
resource cleanup; those remain open with the crash window below.

**Normal retirement prerequisite (18 September):** Local lease cleanup and
removed cluster placements now use an internal Retire command. It shares the
busy-worker fence and confirmed-exit path, then releases instance inventory,
host ports and cached specs. Ordinary Stop retains completion history. The
running-agent lease regression fails before the fix; all 106 agent tests
(12.44s) and strict Linux Clippy pass. This does not close the crash window below.

**Observation prerequisite (18 September):** After lease release, a temporary
status failure no longer ends cleanup confirmation early. Observations retry
inside the original deadline; only an empty namespace confirms absence, and
persistent failure remains Unknown. The HTTP recovery regression fails before
the fix; recovery/deadline cases, all 117 testkit tests (0.54s) and strict Linux
Clippy pass.

**Runc startup and signalling prerequisite (18 September):** Stop and force-stop
now check the CLI outcome, preserve the foreground launcher and require its
observed exit after a force signal. Three controlled regressions fail first.
Real adoption qualification also caught rootful start returning before OCI
creation; startup now waits for a running PID or a completed batch launcher.
The real fixture covers immediate kill and both zero/non-zero short jobs.
Nine ordinary tests, seven rootful acceptance cases (26.35s), non-root
port adoption (3.85s, with the CI AppArmor prerequisite) and strict
Linux/macOS Clippy pass.

**Retryable runtime cleanup (18 September):** Runc cleanup now propagates OCI,
launcher, rootfs and network failures. It confirms OCI absence and launcher exit,
requires a normal unmount, and removes forwarding and the namespace before
retiring the address. Failed or cancelled cleanup retains ownership; status and
exit-code reads cannot publish Stopped before cleanup succeeds. Rootless helper
retirement borrows the tracked handle until success. Repeated adopted-process
observations preserve an already recorded exit code. Two deletion regressions,
an adopted exit-code regression and a real busy-overlay regression fail first;
controlled cancellation/retry, 271 ordinary runtime tests (13 explicit gates,
7.64s), seven rootful cases (26.49s), non-root port recovery (3.70s), the real
TLS/runc secret/config catalogue (three confirmed cleanups, 62.76s), and strict
Linux/macOS Clippy pass.

**Placement ownership prerequisite (18 September):** A real HTTP/agent-channel
regression finds no persisted owner when Deploy reaches the agent. The reconciler
now syncs Pending ownership before queueing deployment, records Applied only after
terminal success, and keeps either state until Retire succeeds. Restart inventory
checks invalidate convergence without deleting ownership. Unreadable, malformed,
duplicate or unsupported checkpoints refuse; private atomic replacement syncs
both file and directory. Checkpoint schema 2 requires state generation 5; protocol
5 and lease schema 2 are unchanged. The restart fixture aborts the reconciler,
withdraws the assignment, drops a cleanup reply, and requires eventual retirement;
it also injects a persistence failure before deployment. All 61 cluster unit
tests pass (4.15s), followed by the strengthened refusal/restart regression
(6.17s), five compatibility tests, all nine real multi-node placement cases
(135.38s), the full Linux library checkpoint (3,299 passed, 19 explicit gates,
54.89s), and strict Linux/macOS Clippy.

**Compatibility fixture follow-up:** Hosted CI at `806b1f6` caught the actual
binary query test still asserting state 4. It now decodes `Compatibility` and
compares with `CURRENT`. Both actual-binary query/development-state refusal
tests pass (0.04s); the independent refusal test remains unchanged. Runtime,
cluster, upgrade and release-build jobs passed at that commit, but portable
CI and Build & Release both pass at the repaired `d7897fc` head.

**Additional cleanup evidence to establish:** Atomic process identity,
complete process-tree retirement and physical crash recovery remain open.
**Startup ownership refusal (18 September):** Adoption now propagates runtime
errors and enforces a ten-second deadline before the API serves. The complete
record inventory is read on a blocking worker; unreadable/corrupt records,
unsupported schemas, filename mismatches, symlinks and non-regular inputs
refuse instead of implying absence. Runtime mismatches and host-port conflicts
also refuse. Confirmed dead-owner cleanup removes identity material before its
record and retains the record on failure. Both original record/identity-loss
regressions fail first. All 435 Bun tests (one explicit gate, 34.34s), 12 record
tests and 11 ordinary actual-binary first-run cases (17.60s; two privileged cases
remain gated) pass, including corrupt-state refusal and job recovery after Bun
is killed. Strict Linux/macOS Clippy passes. The stale-record fixture now has an isolated volume path after strict
error handling exposed its accidental access to the system volume directory.
Process/runc adoption now uses checked process observation and refuses invalid
PID selectors before any syscall. The invalid-record regression fails first;
runc also proves it never invokes cleanup on that error. All 277 Linux runtime
tests (13 explicit gates, 7.26s), 22 native ProcessGrill tests (0.22s), real
rootful runc adoption/rootfs retirement (6.81s), and strict Linux/macOS Clippy
pass.

**Apple inspection refusal:** A failing-first CLI fixture reproduces daemon
failure being reported as absence. Inspection now requires a successful,
identity-matched response; only an empty successful inventory means NotFound.
Nonterminal created/paused containers refuse adoption, as do command and parse
errors. Inspection has a ten-second deadline and cancellation kills its CLI
child. Eight native unit tests pass (10.01s), including deadline/reaping, and
real Apple Container adoption plus removal passes (1.89s). Strict native and
Linux Clippy pass; the Apple module is macOS-only, so Linux runs no Apple tests.

**Restart ownership prerequisite:** The retry driver discarded kill errors and
recreated an instance without observing exit. It now shares explicit stop's
bounded force-kill/exit path. Failed, ineffective, unreadable or stalled cleanup
keeps the instance Pending with its port, record and in-memory retry count;
a later tick retries without creating a replacement first. All four regressions
fail before implementation and pass with subsequent recovery. All 440 Linux
Bun tests (one explicit gate, 34.83s), 441 native Bun tests (one gate, 20.68s),
and strict Linux/macOS Clippy pass.

**Restart startup recovery (19 September):** Create/start failures now retain
ownership in Stopping until cleanup is observed. Then the shared retry path
honours backoff and spends the next bounded attempt. Apps crashing during
backoff retain retry eligibility; explicit stop also cancels Pending attempts.
Four original startup regressions and the Pending-stop regression fail before
implementation. All 452 Linux Bun tests (37.77s), 453 native Bun tests (21.15s;
one explicit gate each), 33 lifecycle transition tests and strict Linux/macOS
Clippy pass. A real process fixture removes/restores an executable and checks
recovery plus another crash during backoff. The initial sandboxed native run
could not bind test listeners; validation passed with loopback access enabled.
Durable retry budgets across Bun replacement remain separate work.

**Durable cron recovery (19 September):** Registrations, explicit retirement and
last-fired UTC minutes now use a private, atomically replaced checkpoint. Bun
loads the complete validated schedule inventory before serving. Each firing is
claimed durably before launch; it waits for any existing deployment operation
first. Failed or cancelled writes fence scheduling and retain every possible
owner until startup reload. Recovered schedules reserve their workload names.
The agreed 0.1.0 policy skips missed or uncertain firings: no catch-up or
exactly-once guarantee. The pre-launch crash window can skip an occurrence.
Three original regressions fail before implementation; a later test also
reproduces the registration-worker race. All 446 Linux Bun tests (36.07s) and
447 native Bun tests (21.25s; one explicit gate each), the real Bun SIGKILL
registration/retirement test (0.27s), both actual-binary compatibility tests
(0.01s), and strict Linux/macOS Clippy pass. Durable state advances to generation
7; protocol 6 and lease schema 3 remain unchanged.

**Ordinary-job recovery completed (19 September):** The
[job recovery record](2026-09-19-job-recovery.md) documents durable pre-launch
intent, bounded retry restoration, observed outcomes, explicit rerun authority
and stop/retirement ordering. Full library suites pass 3,362 macOS/3,416 Linux
tests, with strict Clippy, binary/CLI/compatibility checks and actual Bun crash
recovery on both. The signed macOS replacement test passes in 19.45s. Durable
state is generation 10, with protocol 8 and lease schema 4. All three Linux
cluster upgrade/revert/rollback cases pass (176.05s); all six node cases pass
across the original run and the separately repaired isolated-node fixture.
That fixture reported an occupied registry address before upgrade: keep its
fixed socket reservations together and let Pickle bind port zero. Its actual
isolated replacement/revert case passes on macOS/Linux (36.92s/26.22s).
Final hosted and candidate qualification remain separate acceptance work.
Discovery of resources created before their initial runtime adoption record,
and cluster-lease completion before every former placement confirms retirement,
still need implementation and qualification. Controlled reconciler interruption
is not physical crash qualification.

**Reconciliation deadline prerequisite (18 September):** Two failing-first
regressions reproduce an unfinished HTTP body stalling future placement polls
and a withheld retirement reply blocking later owners. Placement headers and
body now share one ten-second timeout; retirement queueing and acknowledgement
also share ten seconds. Unknown outcomes keep their ownership record while the
loop continues. Routing/deployment queue sends are bounded, and shutdown can
interrupt read-only observation while durable intent remains. All 63 cluster
unit tests (10.32s), real three-node stop/non-resurrection (13.14s), and strict
Linux/macOS Clippy pass.

**Node-local job ownership (18 September):** Typed node-job leases reserve a
server-generated 128-bit namespace, persist job ownership before deployment,
and keep cleanup behind the deployment guard. Cluster nodes persist and reap
their own job leases alongside Raft's application leases. Job IDs never forward
to a leader or resolve on another node; Raft and application scopes refuse
job records and reserved job namespaces. The running-agent regression fails
before implementation. Five scope/routing/restart regressions pass (5.97s),
119 testkit tests pass (1.23s), and the actual Bun-kill recovery plus three-case
TLS/CLI catalogue pass together (14.00s), including confirmed cleanup.
Protocol/state advance to 6/6 and the lease schema to 3. The full Linux library checkpoint passes 3,306 tests (19 explicit gates, 63.99s), and both actual-binary compatibility tests pass (0.01s). Strict Linux/macOS all-target/all-feature Clippy passes. This retains node-local
job scheduling; physical-crash, initial-record and full process-tree gates remain
separate.

**Hosted macOS follow-up:** CI at `c4ec142` passed the three job assertions but
failed cron cleanup with EPERM. The same real catalogue failure reproduces on
macOS (17.74s), and signalling twelve exited, unreaped child-owned groups
returns EPERM in all twelve cases. A non-reaping `waitid` regression reproduces
EPERM through ProcessGrill itself (0.07s). Stop and kill now collect an already
completed child's result before deciding whether to signal, preserving both
zero and non-zero exits. Live-process observation/signal errors still propagate.
Full process-tree ownership remains a separate gate. Both hosted CI and Build
& Release pass at `50fed01`, including the repaired macOS path. The release build at
`c4ec142` passed. After the repair, 21 native ProcessGrill tests pass (0.24s),
the actual macOS catalogue reports three passes with confirmed cleanup (17.79s),
and 274 Linux runtime tests (13 explicit gates, 7.58s), seven job lifecycle tests
(12.03s), both real Linux job catalogue/recovery cases (13.32s) and strict
Linux/macOS Clippy pass. Hosted qualification of the repair remains separate.

**Job completion evidence (18 September):** The catalogue previously counted a
stopped job with no exit code as success. Its HTTP regression fails first; the
probe now requires an observed zero exit, rejects both missing and non-zero
values, and retains the original deadline. All 120 testkit tests pass (1.52s),
and the actual TLS/CLI job catalogue passes with confirmed cleanup (8.65s).
Strict Linux/macOS Clippy passes.

**Job log publication (18 September):** The log probe no longer assumes that
stdout is queryable in the same instant as process exit. It polls inside the
original case deadline, including the HTTP request, and requires the expected
line. The corrected delayed-publication fixture fails first; a never-published
line still fails. All 122 testkit tests pass (1.76s), and the actual TLS/CLI job
catalogue passes with confirmed cleanup (7.48s). Strict Linux/macOS Clippy passes.

**Repository metadata prerequisite (19 September):** Identical image bytes
pushed into two repositories used to collapse into one digest-keyed metadata
row. A failing-first regression reproduces the lost owner. The catalogue now
keeps a row per repository/digest while blobs and holder locations remain
content-addressed. Deleting a tag updates only its repository. Moving a tag
preserves the old digest for pulls which already verified it; an existing
integrity test caught an initial implementation that removed that evidence.
Content signatures apply to every copy and survive later copies and reload.
Peer pulls and scheduling select the repository for pinned references too.
All 235 Pickle, 82 Raft state-machine and 29 scheduler tests pass on both
platforms. The 17 registry cluster, five integrity and two upload integration
tests pass on macOS/Linux, including new actual HTTP and snapshot recovery
cases. Both compatibility checks and strict Clippy pass. Durable state advances
to generation 11; protocol 8 and lease schema 4 remain unchanged. This is a
prerequisite, not completion of repository/upload lease cleanup.

**Catalogue transaction prerequisite (20 September):** Failed local persistence
now refuses a manifest push instead of acknowledging it. Publication and GC hold
one owned write guard through durable persistence and physical deletion, with
fresh blob checks before commit. Five transaction regressions, all 240 Pickle
and 24 Linux registry integration tests pass; native full-library/registry cases
and strict Clippy on both platforms pass. See the
[registry retirement plan](2026-09-19-registry-retirement.md) for remaining work.

**Authoritative forwarding prerequisite (20 September):** Clustered registry
writers use a bounded, non-redirecting service/node-TLS proposal path to the
advertised leader, including fresh workers and followers. The leader validates
current peer authority against a quorum read; Raft refuses retired holders at
application time. Four real TLS integration cases pass on macOS/Linux (10.02s),
including three-node election, old-leader refusal, lost quorum and request-body
limits. All 248 Pickle, 83 state-machine and eight route-audit tests, existing
registry/renewal integrations and strict Clippy pass. No schema changed. The
[registry retirement plan](2026-09-19-registry-retirement.md) records details.

Remaining resource contracts, each in its own commit: registry uploads/repositories
and process identity/pre-adoption recovery. Managed volume/mount cleanup is
implemented and qualified as recorded above. Node effects already use C06 reservations
and C10 exact-fault receipts. Image distribution currently reports uncontrolled
cache state and never evicts arbitrary images; any future cold-cache mode needs
exclusive ownership before eviction.


**Hosted qualification repair (18 September):** Portable Linux no-default CI
at `e095a13` failed because the bootstrap test released a candidate API port
before Bun acquired it. Bun now prints the actual bound API address. Both the
bootstrap and service-endpoint probes request port zero and discover that
address without a release/rebind gap. The updated bootstrap regression fails
against the old reporting line. Five bootstrap tests (0.62s), the endpoint test
(0.06s) and strict Linux Clippy pass.

**Peer API addressing (19 September):** Membership now exposes each node's
resolved API socket address. Testkit and diagnostic/trace clients preserve the
entry client's authentication and CA while targeting that address; missing,
unspecified, zero-port or malformed evidence fails instead of guessing a port
or silently omitting a member. Upgrade plans honour explicit overrides and
validate membership endpoints before uploading. The original HTTP regression
fails first. Both API endpoint tests, 126 testkit tests and 332 Relish tests pass
on macOS/Linux. Actual authenticated three-node placement verifies distinct
ports and successful peer reads (19.96s native, 18.21s Linux). Strict Clippy
passes on both platforms. Protocol/state remain 6/7; the HTTP field is additive.

**Upload cleanup prerequisite (19 September):** Expired or failed upload
sessions now enter an explicit Retiring state. They refuse new writers and
remain owned until temporary-file deletion and directory sync succeed. The
reaper skips active writers, retries failed deletions and continues through
other expired uploads. Finalisation fences writes before its blocking commit;
API/body/quota failures retain cleanup ownership, and P2P cleanup errors
propagate. The original repeat-sweep regression fails before implementation.
All 229 Pickle tests and two authenticated HTTP upload tests pass on macOS/Linux,
with strict Clippy on both. The HTTP fixture proves failed deletion, continued
cleanup of another upload, write refusal and later recovery. Process-death
inventory and lease-owned repositories remain open.

**Upload restart recovery (19 September):** Bun claims an exclusive directory
owner before starting registry or replication writers. The lock lives through
normal serving and releases on process death or exec. Startup reclaims only
recognised regular upload files, syncs the directory, and refuses a competing
owner, unknown entry, symlinked directory or I/O error. Interrupted pushes
restart; committed image blobs are outside this sweep. The actual Bun SIGKILL
regression fails before implementation. A symlink regression also fails before
the refusal is added. All 25 store tests, 13 real first-run tests and strict
Clippy pass on macOS/Linux (two explicit rootful gates on Linux); first-run
suites take 33.31s/17.97s. The process fixture also proves a second Bun refuses
before serving without altering the live upload. All three real Linux rolling
upgrade, rollback and pause/recovery cases pass with the new owner (191.46s),
proving replacement can reacquire the directory. Lease-owned repositories and
explicit cleanup of managed volumes still remain.

**Lease release confirmation (19 September):** Relish distinguishes HTTP 202
from completed cleanup. It polls durable lease ownership until 404, within one
30-second deadline (or the runner's shorter deadline). Empty runtime inventory
cannot override a still-present lease. The actual HTTP regression fails before
the fix and covers both unavailable-owner and later-completion outcomes. All
127 testkit tests pass on macOS/Linux (2.15s/2.47s), with strict Clippy on both. Durable
cluster placement-owner history and retirement acknowledgement remain open.

**Normal artifact retirement (19 September):** Stop/Retire now propagate identity
and adoption-record removal failures before forgetting the tracked instance or
releasing its port. Identity retirement precedes record deletion; both parent
directories are synced, including retry after an uncertain sync. Blocking I/O
runs off the command-loop runtime. The command-channel regression fails first,
then covers each filesystem fault and successful retry. Three older Linux tests
now own temporary volume roots instead of relying on inaccessible production
paths. Full library qualification passes 3,308 tests on macOS (five explicit
gates, 39.61s) and 3,362 on Linux (19 explicit gates, 69.38s), with strict Clippy
on both. Rolling deploy
rollback/halt/finalisation still use separate best-effort
artifact cleanup paths; their error propagation remains open.

**Integration fixture ownership (19 September):** Agent, batch, build, ingress,
discovery and cluster harnesses now own private volume directories through agent
shutdown. This removes the production-path assumption exposed by strict artifact
cleanup. All 46 affected ordinary integration cases pass on macOS/Linux (two
additional Buildah gates remain explicit), with strict Clippy; all ten actual
Linux placement cases pass (154.71s). The VM has Buildah, so the two absence tests
run successfully as separate child processes with an empty PATH rather than
being skipped or changing installed tools.

**Rollout runtime retirement (19 September):** Rolling and blue-green workers
now share normal Stop's bounded signal/observation helpers. Failed, ignored or
stalled kills and inspection errors refuse finalisation. An initial regression
shows a failed kill still producing Complete; a stronger regression then exposes
untracked already-started replacements. The failure path now registers those
replacements for ordinary Stop/Retire while preserving the old owner. All eight
strategy/fault combinations verify both generations and later successful cleanup
(8.22s). The complete Bun suites pass 456 native/455 Linux cases (one explicit
gate each, 21.40s/37.39s), with strict Clippy on both. Rollout artifact-finalisation,
halt and rollback cleanup remain separate open work.

**Rollout artifact finalisation (19 September):** Finalisation and per-step
retirement now propagate identity/adoption-record errors before releasing ports
or forgetting instances. Failed finalisation retains started replacements and
marks every already-retired old instance Stopped before any fallible deletion.
The original regression reports Complete despite the filesystem fault; a second,
two-replica regression reproduces the next old instance remaining restartable.
Both repairs cover rolling and blue-green, each filesystem fault, retained
ownership and later successful Retire. The blanket app-removal helper, which
ignored kill failures, is removed. Unit fixtures now retain private volume roots
through agent teardown, including the two real ProcessGrill redeploy cases.
The complete Bun suites pass 457 native/456 Linux tests (one explicit gate each,
22.20s/37.40s), with strict all-target/all-feature Clippy on both.
Rollback/halt and the restart-driver hand-off during off-loop retirement remain
separate work.

**Rollout restart ordering (19 September):** A deterministic regression holds
a kill request in flight, exposes the old runtime's exit and runs the periodic
crash detector. Before the fix, the detector queues Pending and increments the
restart count. The worker now awaits an explicit command-loop retirement intent
before draining/signalling: Stopping, retry disabled, health unregistered.
Observed exit and durable artifact cleanup remain separate acknowledgements.
The regression covers stepped rolling, rolling surplus and blue-green paths.
All 458 native/457 Linux Bun tests pass (one explicit gate each, 21.88s/37.95s),
with strict all-target/all-feature Clippy on both platforms.

**Rollout identity recovery (19 September):** Three regressions reproduce
post-adoption generation reuse, counter overflow and fresh-deploy replacement of
a terminal owner whose adoption record cannot be deleted. Allocation now advances
past restored owners and checked process-local reservations, refusing exhaustion
or a closed agent channel. Replacement includes Stopped/Failed owners until
checked retirement succeeds. Structured app names disambiguate generation
suffixes. The full Bun suites pass 461 native/460 Linux tests (one explicit gate each,
21.68s/37.80s), with strict all-target/all-feature Clippy on both. Actual signed
exec preserves generation one and then deploys generation two on macOS/Linux
(19.17s/10.35s). Unsupported/inconsistent legacy adoption identities and
cross-app canonical ID collisions are separately tracked under C34.

**Workload label admission (19 September):** Configuration accepted an empty app
name and Bun created a runtime instance for uppercase `Bad`; both regressions
fail first. App/job names, their namespaces and declared namespaces now require
1–63-byte lowercase DNS labels before resource allocation. Trace shares the
same predicate. Boundary, separator, traversal-shaped, whitespace and Unicode
cases pass. Full library suites pass 3,322 macOS/3,376 Linux tests (five/19
explicit gates; 38.79s/70.52s), alongside strict Clippy on both platforms.
Recovery identity validation and global instance-ID collision refusal remain
separate open work.

**Recovery identity preflight (19 September):** A failing-first regression proves
that the old alias path adopts `web-0` into a different supervisor key, leaving
runtime retirement and record deletion looking up the wrong ID. Startup now
validates the whole inventory before adoption, stale-record removal or identity
sweeping. Canonical IDs must agree with valid structured labels, replica index,
optional generation and an explicit app-spec namespace; runtime kind is also
checked before mutation. Legacy aliases refuse without runtime calls or deletion,
in accordance with the fresh-cluster policy. Ordinary `worker-g9` and its rolling
IDs remain valid. All 463 native/462 Linux Bun tests pass (one explicit gate
each; 21.88s/39.58s), strict Clippy passes on both, and actual signed exec followed
by redeployment passes on macOS/Linux (19.19s/10.53s). Cross-app ID collision
admission remains separate.

**Rollback/halt ownership (19 September):** The failing-first cancellation
matrix reproduces a false RolledBack result and a missing replacement owner
when kill fails. The command loop now reserves each replacement and its port
before preparation; the worker records whether runtime creation was attempted.
One shared abort path performs bounded kill/exit observation off the command
loop, then checked artifact/backend/port retirement. Failures retain their owners
and produce Failed history. Healthy halted replacements enter ordinary
supervision. RolledBack requires successful cleanup with no old instance already
retired; partial cutover reports Halted. Obsolete ignored-error cleanup methods,
worker port allocation and rollback/halt command variants are removed.

All 24 strategy/policy/fault combinations pass (17.11s). A separate case retains
a healthy replacement after a partial halt and retires all remaining owners.
The older blocked-record-directory fixture correctly observes an extra retained
port; it now repairs the directory and verifies confirmed release. All 465
native/464 Linux Bun tests pass (one explicit gate each; 23.35s/44.91s), along
with all 29 actual HTTP/process integration tests (four separate gates each;
1.22s/1.94s) and strict all-target/all-feature Clippy on both platforms. The
preparation reservation is in memory: physical crash recovery before the first
runtime record and cross-app fresh-ID collision guards remain open.

**Cross-app instance-ID collision guard (19 September):** The failing-first
regression creates generation one of `worker`, then applies a fresh `worker-g1`
app. Both resolve to `default__worker-g1-0`; the old path overwrites the owner
and reports Complete. Fresh app admission now checks every replica ID before
allocating ports, and job admission checks its single ID. Foreign structured
owners are retained regardless of lifecycle state. The app/job × Running/Stopped
matrix verifies errors, unchanged records/ports and no runtime mutation. Rolling
reservations already refuse occupied IDs. Names ending in generation-like suffixes
remain valid but cannot claim another owner's ID; the encoding and compatibility
formats are unchanged. All 466 native/465 Linux Bun tests pass (one explicit gate
each; 22.82s/42.29s), with strict all-target/all-feature Clippy on both platforms.

**Confirmed cluster retirement (19 September):** The original rescheduling
regression allowed a lease to disappear while both its former and current
workers could still own runtime resources. The approved protocol records the
union of exact application/node owners in the scheduling entry, fences new
work once cleaning starts, and requires system-authenticated retirement
acknowledgements after runtime/artifact cleanup and checkpoint persistence.
Leases return 202 until their owners have retired. Placement reads and lease
inspection require a current quorum; followers preserve user credentials when
forwarding, with a one-hop loop bound. An unavailable owner remains pending.
Protocol 7/state 8 and lease schema 4 require fresh development clusters.

The missing-journal and premature API-completion regressions fail before the
wiring. Both now pass, including dropped replies, runtime failure, checkpoint
failure, ordinary-admin refusal and duplicate acknowledgements. Snapshot
restoration, late scheduling, replacement-lease isolation and the placement
history bound are covered. The real three-node paused-worker/leader-change
case passes on macOS (19.50s). Full library suites pass 3,343 macOS and 3,397
Linux tests (five/nineteen explicit platform gates); strict Clippy and both
actual-binary compatibility checks pass on both platforms. The Linux three-node
case passes in 18.79s, and all three real rolling upgrade/pause-revert/cluster
rollback cases pass in 177.58s. Operator-attested node identity retirement
is approved as a separate feature; see the [protocol and operator plan](2026-09-19-lease-retirement.md).


**Operator decommissioning (19 September):** `relish decommission-node` requires
an unscoped administrator to attest external shutdown or fencing, with a reason.
One Raft decision permanently retires the node identity, records the operator,
original time, per-lease placement counts and any released node-fault sequence,
and clears that node’s placement/fault obligations. Duplicate requests return
the original evidence. Scheduling, endpoints, membership, new join tokens and
renewal serial allocation reject retired identities. TLS checks every serial
against identity retirement; existing API connections recheck replicated state.
The command refuses stale/joint membership and loss of the remaining quorum.
Returning machines need fresh state and credentials under a new node name.

API, live TLS, renamed startup identity and node-fault regressions fail first.
Snapshot recovery, stale decisions, faults on other nodes and duplicate requests
are covered. Full library suites pass 3,351 macOS/3,405 Linux tests (five/19
explicit gates), with strict Clippy, both binary suites, compatibility checks and
13 renewal cases per platform. All four real cluster-failover cases pass
(42.82s/43.27s), and all three Linux rolling upgrade/revert/rollback cases pass
(176.56s). Generated init configs now retain their certificate’s node name.
Protocol/state advance to 8/9; lease schema remains 4. The runbook documents the
five-second caught-up security refresh and the need for external fencing.


**Placement harness cleanup (19 September):** Hosted CI at `ab11fa3` exposed a
missing production task in the placement fixture. DELETE correctly returned
202, but no cluster lease reaper existed to finish cleanup after worker
acknowledgements. The exact capacity test fails locally before the repair
(49.34s), then passes on macOS and Linux with one owned reaper per node, matching
Bun. All ten Linux placement cases pass (42.69s). The macOS run passes nine,
including the regression; a separate fault-admission convergence race remains
under investigation and is not hidden by retries or a larger cleanup timeout.


**Fault fixture convergence (19 September):** The expanded macOS placement run
caught a second race: raw gossip had recovered after fault reversal, but the
API membership cache had not. The next destructive request correctly failed
its safety check because it could not map the current leader to a live member.
Before either admission, the fixture now waits for all API caches, stable voter
sets, a shared leader, cleared reservations and that leader's quorum check.
It does not retry fault injections or relax the API's safety refusal. All ten
placement cases pass on macOS/Linux (28.48s/42.44s), with strict Clippy on both.

### C35 — Gate chaos capabilities per selected scenario

**Priority:** P2. **Wave:** 4. **Book chapters:** 08, 15.

Evidence: `src/testkit/chaos/mod.rs:chaos_preflight`.

Suite-wide NodePressure and SaturateCapacity requirements prevent otherwise supported scenarios on non-Linux runtimes.

**Completion test:** Unsupported destructive cases refuse explicitly; supported selected cases run with only their required grants. Document the actual platform matrix.

### C36 — Target and clean up the legacy chaos command precisely

**Priority:** P1. **Wave:** 4. **Book chapters:** 08, 15.

Evidence: `src/relish/chaos.rs:partition scenario`.

The command selects a target but injects through the entry client, ignores collection errors and uses blanket heal; it can affect a different node than its narration names.

**Completion test:** Route to the selected node, retain exact fault IDs and preserve unrelated faults on success, cancellation and failure; alternatively retire this legacy path in favour of the guarded catalogue.

### C37 — Require rejoin before committing the local upgrade marker

**Priority:** P1. **Wave:** 2. **Book chapters:** 14.

Evidence: `src/bin/bun.rs:upgrade_verify`; `src/config/node.rs:gossip_rejoin_secs`.

The cluster orchestrator already waits for observed gossip rejoin before marking a node Healthy. The replacement process independently commits its local boot marker after workload checks; its gossip_rejoin_secs gate remains an explicit TODO.

**Completion test:** A replacement that serves locally but cannot rejoin must not commit its local marker; honour the configured deadline and prove rollback/adoption ownership. Preserve the existing coordinator gossip gate and test both layers independently.

### C38 — Make blocked deployment replacement explicit

**Priority:** P2. **Wave:** 4. **Book chapters:** 07.

Evidence: `src/bun/deploy_operations.rs:DeployOperationStore::start`; `src/bun/agent.rs`.

A stuck health-check rollout blocks a corrective deploy. Historical supersede/cancel ergonomics and target-kind identity still need a defined contract.

**Completion test:** Cancel or supersede records a terminal outcome and releases ownership before replacement; errors show active ID/age/phase; app/job identity cannot collide.

**Ownership prerequisite completed (18 September):** An error event previously released the target before runtime rollback completed. Terminal accounting now waits for the worker's internal channel to close and its task to finish; panics remain Unknown. Full/disconnected event streams close without blocking worker accounting, and busy errors include ID, age and phase. The blocked-kill regression fails before the fix and passes for rolling and blue-green afterwards; all 98 agent, four operation-store and six deploy API tests and strict Linux all-target/all-feature Clippy pass. Cooperative cancellation is completed below.

**Naming prerequisite completed (18 September):** App and job kinds must use distinct names within a namespace. Configuration rejects collisions; runtime admission refuses the opposite kind while an instance retains ownership, and app replacement excludes jobs. The overwrite regression fails before the fix. All 99 agent, 44 supervisor and 173 configuration tests and strict Linux all-target/all-feature Clippy pass. This preserves existing runtime IDs and is not a new cluster-wide job catalogue (C34).

**Completed cancellation (18 September):** Scoped Deployer requests to `POST /v1/deploys/operations/{id}/cancel` keep ownership while signalling a cooperative token. Workers observe it between workloads/replacement steps or during read-only health waits, finish runtime work and rollback/halt, and only then record Cancelled. Late requests preserve the actual completed/failed outcome; panics remain Unknown. `relish cancel-deploy` polls up to 30 seconds and fails on pending, failed or unknown evidence. Desired state, completed work and cron registration are not undone. Request, in-flight-create, both health-gated strategies, API role/scope and repeated terminal-request tests pass; all 3,227 Linux library tests (19 privileged gates), strict Clippy, CLI parsing and a real executable terminal-evidence test pass.

### C39 — Handle development CLI paths without unwraps

**Priority:** P2. **Wave:** 3. **Book chapters:** 09.

Evidence: `src/relish/dev.rs:create`; `start`; `test`.

The older dev workflow still unwraps filesystem paths and saved node addresses. Managed quickstart fixes did not remove this separate CLI surface.

**Completion test:** Non-UTF-8 paths, incomplete/corrupt saved state and missing nodes return actionable errors and leave unrelated VMs untouched.

**Implementation:** all saved cluster inputs are validated before a Lima call;
start/stop/destroy preflight the complete owned VM set. Non-UTF-8 paths fail with
context, and shell-bound checkout paths and test filters are quoted. Real CLI
fixtures prove refusal before mutation, preserved state and literal shell
metacharacters. The invalid-byte checkout fixture runs on Linux because APFS
rejects that directory name before the CLI can inspect it.


### C40 — Own replaced rootless proxy processes

**Priority:** P2. **Wave:** 3. **Book chapters:** 01, 14.

Evidence: `src/grill/runc.rs:start_rootless_network`; `adoption`; `src/grill/rootless.rs:setup_slirp4netns`.

**Completed on PR #167.** Replacement and adoption share serialised ownership. Displaced helpers stop before replacement, repeated adoption retains the current owner, conflicting metadata fails explicitly, and retirement preserves a successor's socket. Startup-only ownership kills a helper on cancellation; async socket checks and forwarding share a two-second deadline. The cancellation regression fails against the original code. All 248 Linux runtime tests pass (12 explicit privileged gates remain), and strict Linux all-target/all-feature Clippy passes.

**Completion test:** Repeated start/adopt under the same instance retires only the displaced owned proxy; asynchronous socket checks and timeout cleanup leave no orphan forwards.

### C41 — Return a non-zero managed-status result for unhealthy nodes

**Priority:** P2. **Wave:** 4. **Book chapters:** 09.

Evidence: `src/relish/quickstart/lifecycle.rs:Action::Status`.

Missing VMs and failed API readiness are printed, but the action still returns Ok. Scripts cannot distinguish a healthy local cluster by exit code.

**Completion test:** Healthy, stopped, missing and unready nodes have documented distinct outcomes; structured output and exit status agree.

### C42 — Key alert state by labelled series

**Priority:** P2. **Wave:** 3. **Book chapters:** 06, 11.

Evidence: `src/mayo/webhook.rs:collapse_series`; `gather_latest_values`; `src/mayo/alert.rs`.

**Completed on PR #167.** Fresh values now retain `MetricKey` identities. Each rule/label set owns its state, and labels reach statuses, dashboard rows, diagnostic output and provider notifications. PagerDuty recovery targets the matching labelled incident. Missing/non-finite readings cannot resolve a firing alert; percentage components must share fresh labels. The original masking and diagnostic-collapse regressions fail before the fix. Verification: 181 Mayo tests in both feature configurations, 48 dashboard tests, 27 diagnostic tests and strict all-target/all-feature Clippy.

**Completion test:** Two label sets cross thresholds independently, retain separate pending/firing/resolved state and do not divide values across series.

### C43 — Resolve short service names in the caller namespace

**Priority:** P2. **Wave:** 3. **Book chapters:** 03.

Evidence: `src/onion/dns.rs:DnsConfig::default_namespace`.

**Completed on PR #167.** RuncGrill publishes source IP/namespace bindings before workload start and withdraws them with network ownership. UDP and TCP refuse unidentified or ambiguous short-name callers; qualified names remain explicit. The node-default setting is rejected. Adoption verifies the live namespace identity, reads the kernel address and restores ownership without reusing it. Verification: 3,197 Linux library tests, 28 DNS unit tests, 14 wire tests, strict Linux Clippy, and real two-namespace container resolution/adoption/teardown (8.78s / 3.48s). C46 and C47 record two separate follow-up findings from tracing these paths.

**Completion test:** Same-named apps in two namespaces resolve correctly from their own workloads, including TCP/UDP and qualified names; unknown source identity fails explicitly.

### C44 — Remove unused reporting worker listeners

**Priority:** P2. **Wave:** 3. **Book chapters:** 06, 14.

Linux upgrade qualification found a reporting worker holding an ephemeral TCP
listener on a port allocated to another node's API. Snapshot and rollup workers
only send messages; neither consumes incoming connections.

**Completion test:** Outbound-only workers deliver real TCP reports, respect the
node fault gate and own no receive loop. Bound upgrade-harness HTTP requests and
include the address in API bind errors. The separate intermittent cluster
upgrade stall remains a release qualification blocker under V02.

### C45 — Keep polling subsystem owners during readiness publication

**Priority:** P1. **Wave:** 2. **Book chapters:** 15.

Both supervision loops awaited the readiness tracker write inside a selected
branch. An owner already queued for the same fair lock could then never be
polled to acquire and release it. Deterministic contention tests reproduce the
stall for owned and reconstructible tasks. This is a concrete deadlock found
while investigating intermittent Linux upgrade stalls; final runtime
qualification remains V02.

**Completion test:** Readiness publication and owner execution remain concurrently
polled under contention. Retired attempts, startup panic, missing readiness and
restart budget tests retain their existing behaviour.

### C46 — Keep DNS fault effects inside their authorised namespace

**Priority:** P1. **Wave:** 2. **Book chapters:** 03, 08.

Follow-up inspection on 18 September found that `publish_dns_faults` drops the
namespace from an authorised workload fault. `DnsFaultState` keys only on the
bare app name, so a DNS fault against one tenant's Redis also affects another
tenant's Redis. Multiple fault owners also need an expiry union rather than
last-writer selection.

**Completion test:** Inject a namespaced DNS fault through the agent, observe
NXDOMAIN only for that namespace over the resolver, and prove clearing or
expiring one owner preserves another owner's effect.

**Completed on PR #167.** The fault watch retains `ServiceId` throughout, unions
owner expiries and rebuilds on exact-ID removal. Missing namespaces and
instance-scoped DNS requests refuse before recording an effect. A real resolver
regression reproduces cross-tenant NXDOMAIN before the repair and verifies
isolation and overlapping-owner cleanup afterwards. Verification: 32 DNS-filtered
library tests, 14 wire tests, both final agent regressions and strict Linux Clippy.

### C47 — Keep runtime address allocation inside the node subnet

**Priority:** P2. **Wave:** 3. **Book chapters:** 03.

Follow-up inspection on 18 September found that RuncGrill increments its
container index with wrapping arithmetic without enforcing
`MAX_CONTAINERS_PER_NODE`. Repeated creation can reach broadcast addresses,
spill into a neighbouring /23 and eventually reuse an occupied address.

**Completion test:** Exhaust the owned /23, refuse before network mutation,
release and reuse only retired addresses, and preserve allocations across
concurrent create, failed setup and adoption.

**Completed on PR #167.** The 509-slot pool uses a private, bounded, locked and
atomically persisted ownership journal; it reloads after uncertain writes and
keeps persistence owned through caller cancellation. Per-instance lifecycle
guards prevent cleanup racing a successor. Adoption claims verified kernel
addresses, and retirement proves namespace/veth absence and clears any remaining
owned nftables map entries before reuse. Missing inspection tools or uncertain
teardown retain capacity; abandoned plans require explicit runtime cleanup rather
than unsafe expiry. Rootful install instructions and development VM provisioning
include the inspection tools. Verification: the exhaustion regression fails before
the fix; 255 runtime tests, strict Linux Clippy and real cancelled-creation recovery,
adoption/duplicate refusal/reuse, failed setup and orphaned forwarding retirement
all pass (0.32s / 4.32s / 2.26s / 0.16s). Physical crash qualification remains V02.

### C48 — Confine registry upload credentials to the declared origin

**Priority:** P1. **Wave:** 4. **Book chapter:** 05.

Evidence: `src/testkit/oci.rs:resolve_location` accepts any absolute upload
`Location`, then reuses a bearer-authenticated client for PATCH and PUT.

**Completion test:** A registry response naming another origin, a protocol-relative
URL or a plaintext downgrade must fail before credentials leave the declared
origin. Relative and same-origin absolute locations must still upload successfully.

**Completed on PR #167:** Both POST and PATCH locations are parsed and compared
by origin before another authenticated request. Error responses stop the upload.
The two-server leak regression fails before the fix; six OCI unit tests, real
authenticated Pickle uploads with both supported location forms (0.20s) and
strict all-target/all-feature Clippy pass.

### C49 — Reject unrepresentable API token lifetimes

**Priority:** P1. **Wave:** 4. **Book chapter:** 04.

Found while implementing C34: token creation multiplied untrusted `ttl_days`
and added the result to `SystemTime` without checked arithmetic. Overflow
panicked; zero produced an already expired credential.

**Completed on PR #167:** Return HTTP 400 for zero and either overflow before
hashing or committing. An omitted lifetime remains explicitly non-expiring.
Both API regressions fail before the fix and pass afterwards; token unit tests
and strict Linux and macOS all-target/all-feature Clippy pass.

### C50 — Apply workload scope and permission checks to every manifest target

**Priority:** P1. **Wave:** 4. **Book chapter:** 04.

Found during C34: apply checked only app scope/permissions. Jobs reached the
agent without those checks, including jobs in manifests whose apps were
already committed. Explicit host binaries also missed the HostExec check
which inline scripts received.

**Completion test:** Out-of-scope jobs and denied Deploy/HostExec permissions
must refuse before any command or desired-state write, including mixed
manifests. In-scope and explicitly granted jobs must still be accepted.

**Completed:** Three failing-first regressions now pass. All 113 API tests
(13.97s), eight route audits and strict Linux/macOS Clippy pass.

### C51 — Restrict administrative manifest writes

**Priority:** P1. **Wave:** 4. **Book chapter:** 04.

Found during C50: apply accepted permission and namespace quota declarations
through its Deployer gate, allowing callers to rewrite their own restrictions.

**Completed:** Require an unscoped user administrator before any ordinary
policy declaration or mixed-manifest mutation. Existing lease ownership checks
remain the bounded exception for reserved test namespaces. Forward apply with
the caller's credentials and preserve upstream status/content type. Both the
admission and three-node forwarding regressions fail before their fixes.
All 114 API tests (14.25s), real three-node acceptance including leader-side
credential revocation (13.93s), and strict Linux/macOS Clippy pass.

### C52 — Confine scoped administrators on credential-management routes

**Priority:** P1. **Wave:** 4. **Book chapter:** 04.

Found during C50: a scoped administrator could create an unrestricted Admin
credential. The public API regression reproduced the widening before the fix.

**Completed:** Token create/list/revoke, join-token creation, secret rotation
and image signing now additionally require an unscoped user. Workload routes
retain their target-specific scope rules, and bootstrap remains governed by
middleware. The regression covers both scope dimensions, refuses every global
management route before mutation, and verifies unrestricted token management.
Six token API tests (2.33s), 114 API tests, 33 authentication tests, eight route
audits and strict Linux/macOS Clippy pass.

### C53 — Confine administrator overrides on test leases

**Priority:** P1. **Wave:** 4. **Book chapter:** 04.

Found while designing durable job ownership: lease inspection and release
allowed any Admin to override the exact credential owner, including administrators
scoped to another tenant. Both API regressions fail before the fix (200/204
instead of 403). The override now requires an unscoped user administrator;
exact owners retain their existing access. Eight token/lease API tests (2.29s),
48 lease-filtered library tests (one explicit gate, 1.89s) and strict
Linux/macOS Clippy pass.

### C54 — Bound retries for transient direct registry pulls

**Priority:** P1. **Wave:** 4. **Book chapters:** 05.

**Observed (18 September):** Hosted CI at `af32c3f` passed 42 privileged checks
but failed the pinned BusyBox pull with the registry's `Rate exceeded` error.
The same direct pull path is used by a fresh installation. Three hermetic
regressions fail before implementation: transient manifest and layer recovery,
and bounded attempts during persistent rate limiting.

**Completion test:** Retry only typed transient read failures with finite
attempts and one deadline; retain authentication/parse/digest failures and
atomic cache publication. Exercise a stalled response, run image-store tests
and strict Clippy, and qualify the real pinned container pull independently.

**Completed on PR #167.** Four attempts share one 30-second manifest/config or
120-second layer deadline. Only typed rate-limit and temporary gateway/service
errors retry; each layer attempt has a fresh buffer. All 33 image-store tests
pass (7.62s), including stalled-request expiry and permanent denial, as do
strict Linux/macOS Clippy and the exact real privileged pinned pull (1.93s).
The pinned upstream client discards error-response headers, so this policy does
not claim to honour `Retry-After`. Current-head hosted CI remains separate.

**Registry transport retry follow-up (19 September):** Hosted privileged Linux
at `b2001e4` failed on a Docker Hub configuration-blob request before the rootless
port-adoption assertion. The retry policy handled selected HTTP status codes but
not transport errors. A local interrupted-response regression reproduces the
failure. The repair keeps the existing attempt/deadline bounds for request and
response-stream failures, while malformed manifests, authentication refusals
and layer digest failures remain terminal. All 35 image-store tests pass on
macOS/Linux (7.70s/8.04s), with strict Clippy. The real rootless port-adoption
gate passes from a fresh image store (4.65s). Its complete corrupt-configuration
case exposed the separate C56 integrity gap.

### C55 — Bound log response bodies and own fan-out cancellation

**Priority:** P1. **Wave:** 4. **Book chapter:** 06.

Found while investigating the hosted `logs_cross_node` fixture failure at
`9747fba`: `fan_out_query` timed only response headers, then read JSON without
that deadline. Dropping its vector of task handles detached in-flight reads.
Two socket regressions fail before the fix (11 pass, two fail, 2.06s).

**Completed on PR #167:** A node's existing timeout now covers the request and body.
A `JoinSet` owns child requests and aborts them when the caller disappears;
completion still merges successful entries and records each failed node.
Both regressions pass alongside all 13 native query tests (0.12s), 62 Linux
Ketchup tests (one explicit gate, 0.44s), all five cross-node tests on Linux
(0.52s) and macOS (0.14s), and strict Linux/macOS Clippy. The independent
partial-failure fixture correction is recorded separately.

**Separate CI fixture correction:** Hosted minimum-Rust CI at `9747fba`
passes 3,658 tests and fails `partial_results_when_node_unreachable`, whose
healthy fixture returns zero rows. That two-second partial-transport test no
longer includes cold DataFusion query planning: it serves fixed entries over
actual HTTP, keeps its original deadline and checks the exact failed peer.
The other four cases retain real storage, now surfacing query errors instead
of silently returning empty rows. All five pass on Linux (0.52s) and macOS
(0.14s); strict Linux/macOS Clippy passes. Hosted validation of the repair
remains separate. The build workflow at `9747fba` passed.

**Cancellation fixture correction:** A later full native run passes 3,294 tests
and fails one fixture because macOS returns `ConnectionReset` when the cancelled
client discards unread response bytes. EOF and reset both prove socket closure;
the fixture now accepts those two outcomes, retaining the original two-second
deadline and rejecting every other result. All 13 query tests pass on macOS and
Linux (0.12s each). The corrected native library checkpoint passes 3,295 tests,
with five explicit gates (47.27s).

### C56 — Verify upstream image identity before caching

**Priority:** P1. **Wave:** 4. **Book chapter:** 05.

The transport-retry regression exposed a separate failure: ImageStore accepts a
complete configuration response whose bytes do not match the manifest's digest.
The pinned OCI client's manifest/config convenience method also parses raw
manifest bytes without proving the digest requested by the reference or selected
index descriptor. Its reported digest can come from a response header.

**Completion test:** Hash raw manifest bytes, preserve digest anchoring through
platform-index resolution, and verify configuration content before publishing a
manifest or rootfs. Exercise forged headers, changed root and child manifests,
wrong configuration bytes, valid index resolution and both direct/pull-through
consumers. Permanent integrity errors must not retry or leave accepted cache
entries. Qualify actual upstream pulls independently of hermetic fixtures.

**Completed:** Both failing-first consumers now share a raw-byte verifier. It
checks pinned roots, selected child digests/sizes and configuration digests/sizes;
Pickle keeps the exact verified bytes and removes its second manifest fetch.
The complete operation retains a 30-second deadline; integrity refusals are
terminal. All 38 image tests pass on macOS/Linux (7.68s/7.77s), all 232 Pickle
tests pass (10.00s/2.27s), and strict all-target/all-feature Clippy passes on both.
The actual cold Docker Hub pull through rootless runc and published-port adoption
passes in 3.51s. Tests cover forged digest headers, changed valid root/index/child
JSON, incorrect configuration bytes and descriptor sizes, intact index resolution,
and complete corrupt configuration responses without retries.

### C57 — Validate upstream layer lengths and allocation metadata

**Priority:** P1. **Wave:** 4. **Book chapter:** 05.

A digest-correct manifest can contain negative layer sizes or a total that
exceeds cache accounting's `u64`. The upstream adapter casts signed sizes to
unsigned descriptors and uses an untrusted descriptor for `Vec::with_capacity`.
Four HTTP-fixture regressions fail: invalid size totals are accepted, direct and
pull-through downloads ignore declared lengths, and the public blob-fetch path
panics with capacity overflow on `u64::MAX`.

**Completion test:** Refuse negative and overflowing totals before publication,
check conversion before fetching a blob, allocate from actual response bytes,
and validate both downloads and cached layer lengths. Keep digest checks intact.
Run all image/Pickle tests, strict Clippy on Linux/macOS and an actual cold pull.

**Completed:** The shared verified manifest path rejects negative and overflowing
layer totals. Blob fetch validates signed/unsigned conversion, grows buffers from
actual received bytes and checks their declared length. Direct pulls also check
cached lengths. All 42 image tests pass on macOS/Linux (7.63s/7.76s), all 232
Pickle tests pass (9.54s/2.24s), strict all-target/all-feature Clippy passes on both,
and a real cold rootless runc pull with port adoption passes in 3.39s. The four
failing-first regressions include the observed capacity-overflow panic.

### C58 — Bound and retry every pull-through upstream read

**Priority:** P1. **Wave:** 4. **Book chapter:** 05.

C54 repaired direct ImageStore reads; Pickle's upstream adapter still issued
single attempts, with no timeout for freshness HEAD or layer bodies. Four
HTTP regressions fail before the fix: transient metadata/body failures,
persistent throttling and a stalled read. Denial and integrity controls pass.

**Completion test:** Share the existing retry helper across both consumers.
Keep one 30-second HEAD/manifest/config budget and 120 seconds per layer,
four attempts maximum, fresh blob buffers and terminal permanent failures.

**Completed:** All 47 image tests pass on macOS/Linux (7.74s/8.91s), all 232
Pickle tests pass (9.17s/2.16s), and strict all-target/all-feature Clippy passes
on both. HTTP fixtures cover each read type, interrupted streams, denial,
incorrect lengths, attempt counts and the original deadline.

## Engineering follow-ups

### H01 — Remove stale wiring claims and validate documentation

**Priority:** P2. **Wave:** 5. **Book chapters:** 02, 06, 15.

Evidence: `src/meat/filter.rs`; `score.rs`; `quota.rs`; `src/mayo/scrape.rs`; `docs/roadmap.md`.

Cordon, quotas and scraping are wired, but comments say otherwise; scoring says 130/40 while constants give 150/60. Earlier phase and acceptance descriptions still drift.

**Completion test:** Correct claims against call sites, test runnable snippets, check links/commands and keep explicit shipped/planned/experimental labels. Review the book at the same time. Clarify disabled-auth workload-fault versus node-fault authorisation and client teardown guarantees; do not silently weaken either contract.

**Completed on PR #167.** Source and chapters 2/6/15 now describe the wired
scheduler, namespace quotas, configured scrape targets and evidence-based
cleanup. The roadmap identifies actual unit, process and live-catalogue tests
and includes Phase 16. Completed historical groups point to their verified C
items; unresolved catalogue, endpoint and resource-ownership work stays open.
Eleven existing Rustdoc errors are repaired. All-feature public docs build with
warnings denied, 74 relative Markdown file links resolve, and test/bench/wtf/trace
CLI help confirms the documented option names. These checks do not substitute
for the full live catalogue gate.

### H02 — Remove or deliberately expose unused helper entry points

**Priority:** P3. **Wave:** 5. **Book chapters:** 03, 07, 15.

Evidence: `src/wrapper/proxy.rs:run_proxy`; `src/onion/dns.rs:run_dns_responder`; `src/meat/autoscaler.rs:run_autoscale_loop`; `src/pickle/pull.rs:image_available_locally`.

These public helpers have no production callers; some describe a different lifecycle from the wired implementation. Wait/renew/deadline helpers also need a call-site inventory. Include the old testkit renewal helpers, unused smoker helpers and mislocated API documentation in this inventory.

**Completion test:** For each helper, delete it or identify its supported caller and tests. Keep wire compatibility; do not wire obsolete implementations merely to make them used.

**Completed inventory (19 September):**

| Entry point | Disposition and evidence |
| --- | --- |
| `wrapper::proxy::run_proxy` | Remove the unused wrapper. Bun and proxy tests use `bind_proxy`/`BoundProxy::serve`, preserving readiness and task ownership. |
| `meat::autoscaler::run_autoscale_loop` | Remove the unused alternative that advanced its tracker before confirmed Raft mutation. `cluster::orchestrate::spawn_autoscaler` owns the production loop and the tested pure decision functions remain. |
| `smoker::resource::calculate_memory_pressure_bytes` | Remove the uncalled/untested allocation calculator. Bun's supported workload path changes/restores cgroup limits; node pressure has its own owned helper. |
| `onion::dns::run_dns_responder` | Retain as a documented standalone library entry point, called by `tests/dns.rs` and `tests/ebpf.rs`. Bun uses the bound responder for readiness. |
| `pickle::pull::image_available_locally` | Retain the read-only verified-blob query. Missing/corrupt-blob unit tests and `tests/pickle_cluster.rs` cover its contract; it does not select the runtime pull path. |
| `BunClient::renew_test_lease` | Retain the supported owner-authenticated endpoint; the actual node-job recovery case in `tests/documentation_first_run.rs` calls it. The catalogue reserves a bounded case/teardown lifetime rather than running a renewal loop. |
| `TestContext::wait_for` / `wait_for_cluster`; `Deadline` | Retain: node-job, deployment and cluster cases call them. Context tests cover stalled collection; the runner constructs one absolute case deadline and separate bounded teardown. |
| `FaultRegistry::next_expiry` / `count_by_node`; `FaultType::requires_cgroups` | Retain explicitly documented, unit-tested library queries/classification. They are not Bun's expiry scheduler, capability admission or Raft node-experiment reservation. `count_by_service` also has a Bun caller. |
| `extend_grace_period` / `unseal_with_age` | Retain tested model/cryptographic primitives, explicitly excluding signed-certificate extension and supported CA restoration. Operator recovery remains F04. |
| Mislocated CA API documentation | Already corrected by C15/H01; no further implementation is needed. |

The full Linux library checkpoint passes 3,340 tests (19 explicit gates,
67.65s), plus 14 DNS integration tests (2.01s) and 16 Pickle cluster tests
(0.67s). Native autoscaler (21), Wrapper (94) and Smoker (114) suites and strict
Linux/macOS Clippy pass. This completes the helper disposition, not the deferred
operator features.

### H03 — Resolve inert configuration and wire fields explicitly

**Priority:** P2. **Wave:** 5. **Book chapters:** 02, 03, 14.

Evidence: `src/wrapper/types.rs:LeastConnections`; `worker_threads`; `src/mustard/message.rs:lamport`; `src/config/node.rs:release_url`.

Several fields promise behaviour that no reader applies. Removing bincode fields casually would break peers and persisted state.

**Completion test:** Implement the named behaviour or reject/deprecate it with compatibility tests. Preserve historical wire layout until a negotiated migration exists.

**Startup follow-up completed (18 September):** Cluster startup advertised the requested API port before binding. A real `--listen 127.0.0.1:0` first-run regression failed with no eligible scheduler node. Bun now reserves a socket before cluster startup, publishes its actual port and starts listening only after credential/bootstrap checks. Eight ordinary first-run tests (17.04s), five bootstrap-authentication tests, service-endpoint discovery and strict Linux/macOS Clippy pass. The strengthened assertion of a running workload passes in 8.81s. The original inert-field inventory remains open.

**Remaining inventory completed (19 September):** Remove the ignored
`[upgrades] release_url` key; unknown-field parsing names it, and the supported
override is `relish upgrade check --url`. Remove Wrapper's unused thread-count
and load-balancing strategy fields from the unreleased Rust API. Its selectors
remain unweighted round-robin on Bun's runtime. Remove the local gossip counter,
but retain `MembershipUpdate.lamport` in the same wire position as a reserved,
ignored field; new updates write zero. A legacy-layout byte fixture and a
maximum-timestamp/stale-incarnation test preserve compatibility. The ignored-URL
regression fails first. All 174 Linux configuration-filtered tests (0.01s),
154 gossip tests (0.34s), 94 Wrapper tests (0.42s), corresponding native suites
and strict Linux/macOS Clippy pass. Protocol/state remain 6/7. Remote resource
and cached-image propagation remain F01, not hidden behind these settings.

### H04 — Use typed alert and scheduler error contracts

**Priority:** P3. **Wave:** 5. **Book chapters:** 06, 15.

Evidence: `src/relish/client.rs:alerts`; `src/testkit/bench/suites.rs`.

**Alert contract implemented on PR #167.** Bun, Relish, the TUI and `wtf`
share a required alert envelope and an enum for API phases. Missing lists,
malformed entries and unknown phases fail collection; valid empty lists and
labelled firing responses retain their wire format. The original HTTP regression
fails before the fix. The capacity investigation also found that the benchmark
counted accepted desired-state writes before observing placement, while the
leader logged scheduler refusals instead of returning them in the apply response.
A typed wrapper around the old substring check would not fix that.
All 330 Relish tests, 19 evaluator tests and 118 API tests pass on both native
macOS and Linux; strict all-target/all-feature Clippy passes on both.

**Scheduler/capacity contract completed separately on PR #167.** Capacity applies
require one new one-replica app and ask the live leader loop through a bounded
channel. The check uses current scheduling reservations and quota usage; missing,
stale, unready or interrupted evidence is unavailable, never saturation. HTTP
422 carries a shared typed refusal; the client and benchmark match its code and
app ID. The API rechecks leadership and lease activity before returning it.
Accepted writes count only after complete cluster status observes the workload
running, with a final check of every counted app. The benchmark requires council
admission; standalone nodes cannot provide it. Deadlines and hard limits fail.

The original typed-refusal and false-success regressions fail first (11 pass,
two fail, 2.15s). All 15 capacity-filtered, 64 cluster, 124 testkit, 332 Relish and
118 API tests pass on Linux. The real three-node fixture verifies follower
forwarding, refusal without a desired-state write, accepted ProcessGrill
placement and refusal to reuse an existing app (17.86s native/21.26s Linux).
Its accepted workload declares no resource limits because ProcessGrill correctly
refuses limits it cannot enforce. This fixture is not OCI saturation evidence.
The final native library checkpoint passes 3,295 tests, with five explicit gates
(47.27s), after the separate TCP cancellation fixture repair. Strict Linux/macOS
Clippy passes. Protocol/state remain 6/7; the API contract is additive and old
or malformed refusal payloads remain failures.

**Completion test:** Shared schemas and stable error codes survive message wording changes; malformed or partial responses cannot score a successful benchmark.

### H05 — Split modules along existing ownership boundaries

**Priority:** P3. **Wave:** 5. **Book chapters:** 01, 02, 09.

Evidence: `src/bun/agent.rs`; `src/bun/api.rs`; `src/council/state_machine.rs`; `src/relish/commands.rs`.

The historical O1 refactor remains optional work, not an unfinished runtime feature.

**Completion test:** Move one owned command family per commit, preserve public behaviour and use the existing integration suite. Avoid combining refactors with correctness fixes.

### H06 — Evaluate shared DNS and duration parsers

**Priority:** P3. **Wave:** 5. **Book chapters:** 03, 09.

Evidence: `src/onion/dns.rs`; `src/relish/test_cmd.rs`; `src/relish/commands.rs`.

Historical O2 asked for evaluation, not a blind dependency migration; parsers currently have different contracts.

**Completion test:** Compatibility/fuzz corpus and measured complexity justify adopting a library or explicitly retaining the current parser. Numeric overflow still gets the immediate C17 fix.

**Completed (19 September):** An actual UDP regression shows the old DNS parser
answering a truncated QCLASS. Adopt `hickory-proto` 0.26.3 with only its `std`
feature for complete packet decoding and response encoding. Onion retains
one-question/IN-class admission, source ACLs and namespace policy. The change
removes about 60 lines of custom codec logic, replaces the separate question-end
walker, checks trailing bytes and supports EDNS0 response negotiation. The
lockfile adds one package; its minimum Rust is 1.88, below our 1.97 floor.

The wire corpus covers all truncations, wrong header counts, non-query
operations, wrong classes, overlong names, literal label dots, a compression
loop, unclaimed bytes and missing additional records. Valid requests still
succeed after each malformed packet. TCP connection refusal, EDNS0 and the
existing namespace/fault/forwarding behaviour are tested separately.

For durations, an executed twelve-input comparison with humantime 2.3.0 confirms
that it rejects bare seconds while accepting days, weeks, compound values,
fractions and microseconds outside the fault/test grammar. `--since` has a
third contract: bare numbers are epoch seconds. Retain the small parsers, with
an explicit compatibility table and arbitrary-text property test; a library
wrapper would still need the existing grammar admission and C17's checked
conversion. This is an evaluated retention decision, not a pending rewrite.

Primary references: [Hickory protocol source](https://github.com/hickory-dns/hickory-dns),
[humantime 2.3.0 syntax](https://docs.rs/humantime/2.3.0/humantime/fn.parse_duration.html)
and [DNS wire format](https://www.rfc-editor.org/rfc/rfc1035.html#section-4.1).
Validation: the original UDP regression fails before implementation; 29 DNS
unit tests and 17 live wire tests pass on macOS/Linux. The native library
checkpoint passes 3,302 tests with five explicit gates (38.52s). Duration
compatibility/property tests and strict Linux/macOS Clippy pass. `make audit`
passes for the updated 708-package lockfile with the existing dated exceptions.

**Automatic DNS port allocation (19 September):** Coverage at `df45d79`
reproduces `AddrInUse` in the shared UDP/TCP wire test: UDP's ephemeral port
selection does not reserve TCP. Port-zero binding now retries collisions up to
16 times, dropping each failed UDP reservation; explicit port conflicts still
refuse startup. The wire suite holds 64 successful pairs and checks both socket
reservations and release. All 18 wire tests pass on macOS/Linux (2.01s/2.02s),
with strict Clippy on both. Hosted coverage must rerun on the repaired head.

**Proxy echo fixture (19 September):** A full native run reproduces the
request-ID test receiving an empty second response. Its handwritten backend
advertised HTTP/1.1 persistence but closed after one read/response. The fixture
now uses axum's HTTP parser/keep-alive and owns both server tasks. The full native
suite passes 3,308 tests (five gates, 39.61s); the corrected Linux fixture passes
explicitly (0.01s), and strict Clippy passes on both platforms. No production
proxy behaviour changes.

### H07 — Add useful public API doctests

**Priority:** P3. **Wave:** 5. **Book chapters:** 01, 09, 15.

Evidence: `src/config`; `src/relish`; `docs/book`.

The doctest command passes with zero doctests, so its green status does not establish working public examples.

**Completion test:** Small configuration/client examples compile and exercise public behaviour; document Rust ownership/error types at their first use in the book.

**Completed (19 September):** Executable Rustdoc blocks on
`Config::parse` and `validate_endpoint` cover parse/semantic validation,
unknown-key refusal, remote HTTPS/local HTTP and typed remote-plaintext refusal.
Both execute without network access. Chapter 15 explains raw strings, optional
borrowed values and enum assertions. Both examples execute successfully on native Rust 1.98 and Linux Rust 1.97
with default and no-default features. All-feature public documentation builds
with Rustdoc warnings denied (46.81s).

### H08 — Identify and eliminate leaked test processes

**Priority:** P2. **Wave:** 5. **Book chapters:** 15.

Evidence: `.config/nextest.toml`; `tests/support`.

The final pre-audit no-default run reported one passing-but-leaky test without printing its identity under the current status filter.

**Completion test:** Retain per-test leak diagnostics, reproduce the offending case, repair process ownership and repeat the relevant suite with no cleanup warning. Do not hide it with a longer timeout.

**Disposition:** The retained report named
`metrics_aggregation::backfill_after_reassignment_neither_drops_nor_double_counts`,
which does not launch subprocesses. The local 0.9.140 runner predates the
[0.9.145 macOS sibling-pipe fix](https://github.com/nextest-rs/nextest/releases/tag/cargo-nextest-0.9.145).
The repository now requires that repaired version and pins it in CI. Both full
feature configurations pass without leaks on 0.9.145; future leak reports fail
at the unchanged 100 ms deadline. This disposition concerns the observed runner
symptom, not a blanket exemption for application process-ownership defects.


### H09 — Complete node-pressure diagnostic hygiene

**Priority:** P3. **Wave:** 5. **Book chapters:** 08, 15.

Evidence: `src/smoker/node_pressure.rs:helper startup`.

**Diagnostic repair implemented on PR #167.** The noisy-helper regression
fails before the fix. Continuous draining retains an 8 KiB prefix and explicit
truncation/read-error evidence, bounds readiness to 64 bytes, and owns the drain
through a JoinSet. Timeout retains partial diagnostics. Four privileged Linux
cases pass (6.41s), including actual Bun parent-process and creator-thread death
plus stale-cgroup reclamation; 114 Smoker tests (three explicit gates) and strict
Linux/macOS Clippy pass. Linux signals on creator-thread death, not just whole
parent-process death. A separate delayed-exec regression reproduces the startup
gap where `getppid()` still names a live process after the creator thread dies.
The separate fix records its kernel TID immediately before spawn, without an
await, then requires that task inside the same parent after arming the signal
and before pressure. All four privileged cases pass again (6.69s), including
this refusal and cleanup; strict Linux/macOS Clippy passes. H09 is complete.

**Completion test:** Helper failures preserve bounded complete stderr; validate and document the actual Linux parent/thread-death semantics and assert cleanup on owner termination.

### H10 — Reuse one egress observation per health tick

**Priority:** P3. **Wave:** 5. **Book chapters:** 01, 03.

Evidence: `src/bun/agent.rs:live_egress_report_state call sites`.

**Completed on PR #167.** The duplicate was in one health tick: enforcement
read the kernel, then readiness immediately repeated that observation. The
enforcement method now returns the capability it observed, and readiness uses
that value. A later tick or separate cluster report still samples afresh; map
repair retains its verification read. The instrumented call-count regression
fails first (two observations instead of one) and passes in portable and
Linux eBPF builds. All 437 native Bun tests pass (one explicit gate, 20.15s).
The real kernel hook-loss test now requires both workload stop and withdrawn
readiness, and passes within its existing four-second bound (1.37s).
Strict native/Linux all-target/all-feature Clippy passes.

### H11 — Declare and test the actual Rust/toolchain baseline

**Priority:** P2. **Wave:** 5. **Book chapters:** 01, 14.

Evidence: `Cargo.toml`; `.github/workflows/build.yml`; `docs/README.md`.

The README promises Rust 1.85+, Cargo declares no rust-version and CI uses moving stable. The promised minimum and reproducible release baseline are unqualified.

**Completion test:** Select a toolchain compatible with the locked dependency graph; test the declared minimum and pin release builds, updating documentation and upgrade/rebuild policy.

### H12 — Resolve the active Thrift dependency alert

**Priority:** P2. **Wave:** 5, before candidate publication. **Book chapters:** 05, 06, 15.

Evidence: `Cargo.lock:thrift 0.17.0`; `cargo tree --locked --offline -i thrift`;
[Dependabot alert #13](https://github.com/reliaburger/reliaburger/security/dependabot/13).

GitHub reports GHSA-2f9f-gq7v-9h6m / CVE-2026-43868, “Apache Thrift has a Memory
Allocation with Excessive Size Value Vulnerability”, as open, medium severity,
affecting versions below 0.23.0. The audit baseline compiled Thrift 0.17.0
through Parquet 54.3.1 and DataFusion 45.0.0. This is separate from the five
RustSec exceptions, and green `cargo audit` does not resolve it. The dependency
path is confirmed; reachability of the affected parser from attacker-controlled
input has not been established by this audit.

**Completion test:** trace the affected allocation through the actual Parquet
reader, identify input bounds and trust boundaries, and upgrade/remove the affected
dependency or document a reviewed, evidence-backed disposition before publishing.

**Implementation:** Thrift 0.23 plus the pinned Parquet 54.3.1 patch in
`vendor/parquet/RELIABURGER-PATCH.md`. The actual file-reader regression showed
that Parquet's private compact decoder accepted overlong metadata integers;
upgrading the external Thrift dependency alone cannot fix that separate code.
Six public-reader tests now cover integer width, unknown-field skipping,
impossible schema counts, truncated doubles and valid maximum-width values.
DataFusion/Arrow versions and the five existing RustSec exceptions are retained.
Normal archive/query and compiler qualification are required alongside the
malformed-input checks. Remote archive readers accept operator-selected paths;
local metric/log files are generated by Bun. No claim of an unauthenticated
network exploit is made from the malformed-file reproduction.

Keep storage compatibility/query tests and a bounded malformed-input regression
where the path is reachable. Reconcile GitHub and RustSec findings in CI rather
than assuming one feed covers the other. Do not dismiss the alert solely because
the other audit tool is green.

## Missing capabilities and longer-term scope

### F01 — Propagate GPU capacity and cached-image placement evidence

**Priority:** feature. **Wave:** 6. **Book chapters:** 02, 08, 12.

Evidence: `src/cluster/orchestrate.rs:build_cluster_cache`; `src/reporting/types.rs`.

The scheduler still receives zero GPUs and empty cached_images; runtime GPU detection alone does not deliver cluster GPU placement or device passthrough.

**Completion test:** Versioned capacity/locality reports affect real scheduling, GPU device access is either enforced or refused, and mixed peers remain compatible.

### F02 — Complete rootless and process-workload isolation

**Priority:** feature. **Wave:** 6. **Book chapters:** 01, 08.

Evidence: `src/grill/rootless.rs`; `src/grill/process.rs`; `docs/design/agent-bun.md`.

Rootless cgroup delegation/workload DNS and stronger process namespace/seccomp/user isolation remain outside current enforcement.

**Completion test:** Add one enforceable capability per commit with privileged acceptance; unsupported declarations continue to fail closed.

### F03 — Finish upstream image trust and worker key separation

**Priority:** feature. **Wave:** 6. **Book chapters:** 04, 10.

Evidence: `src/meat/scheduler.rs:lookup_pickle_manifest`; `docs/design/security-sesame.md`.

Pull-through/external images are exempt from cluster signing policy; clustered workers currently hold the shared master-key trust.

**Completion test:** Separate these features into their own designs/commits: upstream trust policy with digest binding, then reduced worker authority with migration and revocation tests.

### F04 — Add supported CA recovery and rotation operations

**Priority:** feature. **Wave:** 6. **Book chapters:** 04, 10.

Evidence: `src/sesame/secret.rs:unseal_with_age`; `src/sesame/identity.rs:extend_grace_period`; `docs/design/security-sesame.md`.

Age unseal and grace extension exist as helpers, but CLI recovery/CA rotation are not a supported end-to-end workflow.

**Completion test:** Recover an encrypted CA backup in an isolated cluster, rotate trust without losing valid workloads, and refuse invalid/expired authority. Define any grace policy explicitly.

### F05 — Complete namespace-scoped identity and token lifecycle

**Priority:** feature. **Wave:** 6. **Book chapters:** 04, 10.

Evidence: `docs/design/security-sesame.md`; `src/sesame/oidc.rs`; `src/sesame/token.rs`.

Per-namespace encryption keys, per-app audiences, token lifecycle automation and broader audit coverage remain planned capability families.

**Completion test:** Deliver each boundary independently with cross-namespace denial, rotation/expiry and audit-attribution tests; preserve the existing default-deny API surface.

### F06 — Complete the metrics/query and reporting architecture

**Priority:** feature. **Wave:** 6. **Book chapters:** 02, 06, 11, 12.

Evidence: `docs/design/metrics-mayo.md`; `docs/design/gossip-mustard.md`.

PromQL/remote read, additional retention/downsampling tiers, event production, versioned chunking beyond C21’s explicit admission limits and the multi-level reporting tree are planned. A flat council aggregation path already works.

**Completion test:** Specify one protocol/storage feature at a time, test compatibility and measured limits, and do not count existing SQL endpoints as these features.

### F07 — Finish cross-node views and log-stream capabilities

**Priority:** feature. **Wave:** 6. **Book chapters:** 06, 11, 13.

Evidence: `src/relish/tui/data.rs`; `src/bun/api.rs`; `docs/design/logs-ketchup.md`; `docs/design/ui-brioche.md`.

Some log streams, deployment history and dashboard views remain local; stdout/stderr distinction, richer querying and a complete live-metrics UX need explicit contracts.

**Completion test:** Remote workloads, partial member failures and namespace scope are visible consistently; each missing view/stream is implemented and tested separately.

### F08 — Complete WebSocket ingress parity and certificate automation

**Priority:** feature. **Wave:** 6. **Book chapters:** 03, 09.

Evidence: `src/wrapper/websocket.rs`; `docs/design/ingress-wrapper.md`.

The live splice does not send the built Close-1001 drain frame; X-Real-IP/X-Request-ID parity and ACME automation remain follow-up capabilities.

**Completion test:** Separate commits for bounded graceful close, proxy-owned header parity and ACME; verify active connection drain, header spoof refusal and renewal under live traffic.

### F09 — Implement packet-level delay/bandwidth faults if retained

**Priority:** feature. **Wave:** 6. **Book chapters:** 03, 08, 12.

Evidence: `src/smoker/bpf_maps.rs`; `ebpf/onion_dns.bpf.c`.

Connect hooks cannot implement delay/bandwidth or full DNS response synthesis; these operations currently refuse. TC DNS fast-path evaluation remains conditional on profiling.

**Completion test:** Measure the userspace baseline first; only ship a packet hook after measured effects, exact cleanup and verifier/kernel coverage. A documented decision not to pursue the optimisation closes the evaluation task.

### F10 — Provide explicit managed-volume retirement and runtime parity

**Priority:** feature. **Wave:** 6. **Book chapters:** 05, 09, 14.

Evidence: `src/grill`; `docs/design/agent-bun.md`.

Stopping an app intentionally retains its volumes; explicit safe cleanup is unfinished. Apple mounts, ports and adoption are implemented, but its separately gated runtime acceptance and broader parity still need qualification.

**Completion test:** Owned-volume removal requires explicit selection and preserves unrelated data; qualify adoption and upgrade on each advertised runtime.

### F11 — Finish supported Kubernetes translations

**Priority:** feature. **Wave:** 6. **Book chapters:** 09.

Evidence: `src/relish/k8s_export.rs`; `src/relish/k8s_import.rs`.

Unsupported/dropped-field reporting is implemented; lossless translation of every expressible workload/Ingress field is not.

**Completion test:** Translate one declared field family per commit, retain warnings for unsupported semantics, and round-trip namespace/resources/health/volumes/placement fixtures.

### F12 — Keep the long-term vision explicitly separate

**Priority:** future. **Wave:** 6. **Book chapters:** 09, 10.

Evidence: `docs/whitepaper.md`; `docs/design/security-sesame.md`.

Franchise, Windows/WSL, TPM/external-CA integration and multi-cluster federation remain future scope, not release bugs already fixed by documentation labels.

**Completion test:** Each needs its own acceptance contract and follow-up design before implementation. Retain visible unchecked capability tracking until delivered or deliberately removed from scope.

## Acceptance and release gates

### V01 — Qualify the complete live three-node catalogue

**Priority:** gate. **Wave:** 7. **Book chapters:** 15.

Evidence: `docs/plans/2026-07-06-plan-chaos.md:acceptance runbook`; `docs/qualification`.

Live setup/ingress and source integration tests passed; the full real catalogue/chaos/bench/wtf/trace matrix has not.

**Completion test:** Run independent runc nodes after C06-C12 and C29-C36, retain every verdict and cleanup proof, include client death/leader change and owned volumes. No unknown required case counts as passed.

### V02 — Qualify sustained TLS, storage and upgrade recovery

**Priority:** gate. **Wave:** 7. **Book chapters:** 04, 05, 14, 15.

Evidence: `docs/plans/2026-09-16-v0.1.0-release-plan.md:release gates`.

A short successful setup does not establish renewal, retention, mixed-version recovery or partial-rollout durability.

**Completion test:** Time-controlled certificate/retention tests plus sustained load, node/leader failure, interrupted deploy/upgrade and crash recovery close C01-C22/C37 on actual runtimes.

**Harness repair (18 September):** CI at `78119fd` reproduced HTTP 413 in all three cluster-upgrade cases. Strip debug symbols from a private fixture before copying, hashing and signing, retain the registry upload cap, assert the fixture size and include response bodies on upload failures. All three real Linux cases then pass in 200.22s; V02 stays open.

**Probe launch repair (18 September):** The earlier 3,276-pass library checkpoint had one ETXTBSY failure during verified candidate launch. Retry only that error while keeping the same private bytes and one ten-second deadline for launch and response. Concurrent preparation coverage also passes before the fix, so it is stress coverage rather than a deterministic reproduction. After the repair, all 3,281 Linux library tests pass (19 explicit gates, 50.55s), as do strict Linux/macOS Clippy. Sustained crash qualification remains open.

**Job crash fixture repair (20 September):** Hosted macOS at `53fd4a4`
reported retry count one and Running before the child wrote its second counter
line. A controlled workload gate reproduces that assertion failure. The fixture
now observes the child's write before SIGKILL, preserving the original recovery
assertions and bounded waits. The real recovery case passes on macOS/Linux
(12.74s/3.46s), with strict Clippy on both. The separate unknown-cron launch gap
still fails hosted macOS and remains a C34/V02 blocker.

### V03 — Publish and install the exact signed candidate

**Priority:** gate. **Wave:** 7. **Book chapters:** 09, 14.

Evidence: `scripts/release`; `.github/workflows/build.yml`; `docs/releasing.md`.

The build/signing pipeline exists, but no 0.1.0 tag or public candidate has been qualified. Signing and Pages/domain setup are external prerequisites.

**Completion test:** Use the authorised release key, verify relocated native/BPF artefacts, install downloaded bytes and promote the same bytes. Never create a tag merely to make a checklist green.

**Candidate preservation implemented (19 September):** Manual main builds run
source CI, produce/sign the complete matrix and preserve all assets with a
source/run-bound inventory. A separate manual promotion checks the externally
recorded qualification digest, exact version/tag commit, successful main build,
complete inventory and every byte before upload, then checks uploaded digests
before publishing. It never rebuilds or resigns; tag pushes no longer publish.
The 17 packaging/candidate tests pass on macOS and Linux, including refused
source/file/provenance substitutions. Both workflows pass actionlint 1.7.12.
Hosted candidate execution, pre-publication
transport for its final URLs and actual installation remain open; no tag or
release was created by this change.

**Candidate transport implemented (19 September):** Both shell installers accept
an explicit HTTPS directory and forward it to managed setup's `--release-mirror`.
Only this version's release asset URLs are mapped; tooling URLs, checksums,
embedded signatures and request bounds remain intact. Local candidate inventory
verification needs no tag. The installer and CLI regressions fail before the
feature. All 18 packaging tests, 32 managed quickstart tests and 85 CLI tests
pass on macOS/Linux, with strict Clippy. HTTP fixtures verify metadata routing,
independent tooling downloads and checksum refusal without replacing cached
bytes. The actual built CLI also refuses three invalid mirror forms before
creating setup state, and its help exposes the option. Actual staged
HTTPS/cold-host qualification still remains. The subsequent full library
checkpoint passes 3,338 tests on macOS (five explicit gates, 37.72s) and 3,392
on Linux (19 explicit gates, 76.01s); 109 changed-document relative links resolve.

### V04 — Measure repeated cold installs on the advertised host matrix

**Priority:** gate. **Wave:** 7. **Book chapters:** 09, 15.

Evidence: `docs/qualification/2026-09-17-laptop.md`; `docs/quickstart.md`.

241.75 seconds was one M2 Max development-binary run; initial signed CLI/guest binary downloads were excluded. Other host paths remain unqualified.

**Completion test:** Time from first installer byte to healthy three-node sample HTTP on repeated empty caches; record failures, hardware/network conditions, Linux host prerequisites, Intel macOS, sleep/wake, changed addresses, interruption and lifecycle cleanup.

### V05 — Review dependency exceptions before their deadline

**Priority:** gate. **Wave:** 7. **Book chapters:** 14, 15.

Evidence: `.cargo/audit.toml`; `Makefile:audit`.

The baseline audit passed with five named exceptions. An exception is a tracked
risk, not a fixed dependency. The September release review removes
`rustls-pemfile` and RUSTSEC-2025-0134 by using the existing Rustls `PemObject`
parser directly. The advisory gate failed before migration with that exception
removed. Twelve TLS unit tests, 17 client tests, seven ingress tests, three
operator file-reload tests and strict Linux/macOS Clippy pass. `make audit`
passes with four exceptions. The [18 September review](../qualification/2026-09-18-dependency-exceptions.md)
records each remaining dependency path, risk and migration option, retaining
the 18 November expiry. The audit now checks that rkyv remains inactive across
all root features and targets, and refuses failed inspection. The failing-first
gate regression passes on Linux and macOS. **V05 is complete for this lockfile;**
future dependency/feature changes and exact-candidate qualification remain gates.

**Completion test:** Before 18 November 2026, re-evaluate reachable advisories and migration options, record evidence and remove or explicitly renew each exception under the fail-closed gate.

## Reconciliation with the earlier plans

The August follow-up has 79 unchecked subitems. They are not 79 new defects:
many were implemented by later Phase 16 and release commits. Use this crosswalk
instead of treating a historical checkbox as today's verdict.

| August 6 section | Current disposition |
|---|---|
| A1–A3: startup, upgrade harness, CI | Implemented; current applicable hosted jobs pass. V05 retains the next advisory review. |
| B1–B3: registry/build auth, upgrade bearer, startup warning | Implemented in Phase 16. |
| C1–C4: lease snapshot race, Raft refusal, fault-clear auth, pressure expiry retry | Implemented. This does not close the separate cancellation/lock/durability gaps below. |
| C5: node-fault routing and quorum safety | Standalone/wrong-target refusal and durable concurrent reservations implemented (C06). |
| D1–D2: mixed-version readiness and new Raft variants | C07. Replace the old implicit-readiness suggestion with negotiated compatibility; missing evidence must not become healthy. |
| E1–E2: probe status, reachable clean wtf result | Implemented. |
| E3–E7: vanished metrics, profiles, diagnostics, ownership, readiness | C08–C10, C23–C27, C29–C32. |
| F1: testkit types, IDs, durable ownership, helpers | C11, C31–C34, H02. |
| F2: pressure cleanup and diagnostic hygiene | C12, H02, H09. |
| F3: rootless handles, repeated probes, dual-stack endpoints, typed APIs, module size | C33, C40, H04–H05, H10. Existing family-aware listener defaults remain credited. |
| F4–F5: deploy conflicts and per-case capabilities | C35, C38. |
| G1: documentation sweep | Earlier label corrections implemented; remaining drift H01. This PR reconciles progress and the release-plan overflow claim. |
| G2: chapter 15 lessons | Added with this audit; implementation explanations already existed. |
| G3: manual diagnostics | Implemented; retain command/example validation under H01. |
| G4: advisory review | September V05 review complete; four explicit exceptions retain the November deadline. |
| H1: three-node lease/leader-death acceptance | V01 explicitly retains this missing acceptance scenario; local lease tests are not a substitute. |
| H2: complete runbook | V01; amend profiles/prerequisites and retain every result. |
| Carried-forward streaming, cert lifecycle, resource leases | Streaming and C14 certificate lifecycle implemented; C34 remains. |

The July 18 M8 infrastructure/command milestone is done, but its fixtures and
complete acceptance are C30/V01. Its certificate lifecycle follow-up C14 is complete; sustained qualification remains V02.
O1–O4 are H05–H07/F09. O5's shipped/planned labels were implemented; preventing
new drift remains H01. The July 6 full chaos acceptance is V01.

The July/August reviews' residual prose is also tracked: mid-rollout adoption
(C05), rollup overlap/payload size (C20–C21), labelled alerts (C42), workload DNS
scope (C43), upstream trust/worker authority (F03), CA recovery (F04), telemetry
architecture (F06), WebSocket parity (F08), volume retirement (F10), and vision-only
features (F12). They no longer depend on someone finding an unchecked sentence
inside a checked historical group.

### Plan inventory

“Historical” means its implementation record is in the phase ledger and the
remaining known work is carried into this plan. It does not assert that every
sentence in the old proposal shipped unchanged.

| Existing document | Disposition |
|---|---|
| [2026-07-02-review-codebase.md](2026-07-02-review-codebase.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-05-ux-improvements.md](2026-07-05-ux-improvements.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-06-plan-chaos.md](2026-07-06-plan-chaos.md) | Implementation recorded in phases 15–16; full acceptance remains V01. |
| [2026-07-06-plan-optimisations.md](2026-07-06-plan-optimisations.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-06-plan-self-upgrade.md](2026-07-06-plan-self-upgrade.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-06-plan-tui.md](2026-07-06-plan-tui.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-07-plan-wiring.md](2026-07-07-plan-wiring.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-09-review-codex.md](2026-07-09-review-codex.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-09-review-design-discrepancies.md](2026-07-09-review-design-discrepancies.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-11-plan-12b-api-trust.md](2026-07-11-plan-12b-api-trust.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-11-plan-12b-consensus-persistence.md](2026-07-11-plan-12b-consensus-persistence.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-11-plan-12b-identity-safety.md](2026-07-11-plan-12b-identity-safety.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-11-plan-12b-network-policy.md](2026-07-11-plan-12b-network-policy.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-11-plan-12b-pickle-integrity.md](2026-07-11-plan-12b-pickle-integrity.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-12-plan-12b2-control-plane-directory.md](2026-07-12-plan-12b2-control-plane-directory.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-12-plan-12b2-council-disaster-recovery.md](2026-07-12-plan-12b2-council-disaster-recovery.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-12-plan-12b2-council-self-healing.md](2026-07-12-plan-12b2-council-self-healing.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-12-plan-12b2-durable-batch-build.md](2026-07-12-plan-12b2-durable-batch-build.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-12-plan-12b2-scheduler-truth.md](2026-07-12-plan-12b2-scheduler-truth.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-12-plan-12b2-transactional-desired-state.md](2026-07-12-plan-12b2-transactional-desired-state.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-13-plan-12b2-declarative-resources.md](2026-07-13-plan-12b2-declarative-resources.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-13-plan-12b3-api-authz.md](2026-07-13-plan-12b3-api-authz.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-13-plan-12b3-image-trust.md](2026-07-13-plan-12b3-image-trust.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-13-plan-12b3-node-pki-join.md](2026-07-13-plan-12b3-node-pki-join.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-13-plan-12b4-pickle-durability.md](2026-07-13-plan-12b4-pickle-durability.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-13-plan-12b4-service-catalogue.md](2026-07-13-plan-12b4-service-catalogue.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-14-plan-12b4-ingress.md](2026-07-14-plan-12b4-ingress.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-15-plan-12b5-gitops-webhook.md](2026-07-15-plan-12b5-gitops-webhook.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-15-plan-12b5-metrics-logs.md](2026-07-15-plan-12b5-metrics-logs.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-16-plan-12b6-process-workloads.md](2026-07-16-plan-12b6-process-workloads.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-16-plan-12b6-self-upgrade.md](2026-07-16-plan-12b6-self-upgrade.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-16-plan-12b6-smoker-effects.md](2026-07-16-plan-12b6-smoker-effects.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-17-plan-12b6-dns-fault.md](2026-07-17-plan-12b6-dns-fault.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-17-plan-12b6-docs-truth.md](2026-07-17-plan-12b6-docs-truth.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-17-plan-ux-improvements.md](2026-07-17-plan-ux-improvements.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-17-review-codebase-current-state.md](2026-07-17-review-codebase-current-state.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-07-18-plan-codebase-review-follow-up.md](2026-07-18-plan-codebase-review-follow-up.md) | M8/certificate/O1–O5 dispositions above; phase ledger records landed work. |
| [2026-07-19-codebase-review-fable.md](2026-07-19-codebase-review-fable.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-08-06-plan-phase15-followup.md](2026-08-06-plan-phase15-followup.md) | Section-by-section crosswalk above; superseded checklist, retained as history. |
| [2026-08-08-audit-post-phase15.md](2026-08-08-audit-post-phase15.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-08-08-doc-consistency-audit.md](2026-08-08-doc-consistency-audit.md) | Historical review/design; implementation recorded in phases 1–16; residuals consolidated above. |
| [2026-09-16-v0.1.0-release-plan.md](2026-09-16-v0.1.0-release-plan.md) | Active release contract; corrected overflow claim; completion work here, final gates V01–V05. |


### V02 follow-up: egress preparation race (20 September)

Hosted CI at a5c1890 passed 49 of 50 privileged Linux cases. The pre-start egress
case failed because live monitoring treated a Pending/Preparing instance as an
already-running workload with lost policy. The deterministic regression fails
before the repair (Pending becomes Stopped). Live checks and the kernel sweep
now defer missing-binding checks for those two states; Initialising, Starting,
HealthWait, Running, Unhealthy and Stopping still require enforcement. Existing
bindings remain monitored regardless of preparation. All 483 Linux Bun tests
pass (one gate), strict Clippy passes on macOS/Linux, and all 24 privileged eBPF
cases pass in 5.90s using the newly built binary. V02 stays open for the separate
physical process-launch gap and final candidate qualification.
