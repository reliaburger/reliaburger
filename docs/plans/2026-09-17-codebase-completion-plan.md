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

There are **71 tracked work packages**: 43 correctness/contract items, 11
engineering follow-ups, 12 capability families and five acceptance/release gates.
The correctness items include 19 P1 priorities; these are engineering priorities, not
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
| Build, docs and distribution | Installer, signed-release tooling, static website and hosted build matrix | H01–H11; V03–V05 |

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
5. **Finish engineering follow-ups.** H01–H11. Optional module/parser changes can
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
until those dispositions and V01–V04 evidence exist.

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

### C07 — Define and enforce mixed-version compatibility

**Priority:** P1. **Wave:** 2. **Book chapters:** 02, 14.

Evidence: `src/bun/api.rs:test lease creation`; `src/council/types.rs`; `src/cluster/orchestrate.rs:build_cluster_cache`.

Lease writes have no peer-schema negotiation/upgrade barrier, while old nodes lacking readiness are fenced. Historical D1/D2 recommendations were not implemented.

**Completion test:** A supported old/new pair upgrades with safe scheduling and decodable logs/snapshots; unsupported peers and new writes are refused before replication. Never infer healthy readiness from missing evidence.

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

**Priority:** P1. **Wave:** 2. **Book chapters:** 03, 04, 15.

Evidence: `src/wrapper/tls.rs:IngressCertResolver::key_for`; `src/bin/bun.rs:security refresh`.

The ingress resolver returns cached keys without expiry checks; node/API leaves are loaded at process start and report restart_required. Workload rotation does not cover these consumers.

**Completion test:** Advance time through renewal/expiry and replace configured cert files while serving TLS; prove new connections get valid leaves, old connections drain safely and diagnostics name unsupported consumers.

### C15 — Make ingress serials unique across nodes and restarts

**Priority:** P2. **Wave:** 2. **Book chapters:** 04.

Evidence: `src/wrapper/tls.rs:INGRESS_SERIAL`.

The atomic counter starts at the same fixed value in every process. The earlier fix guarantees only process-local uniqueness, not per-issuer uniqueness.

**Completion test:** Mint under one CA from separate processes and after restart; no repeated serials. Specify revocation ownership and persistence rather than claiming the current counter solves it.

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

Four cases return unknown unconditionally. Development and full profiles share a failed_any verdict, so a complete catalogue cannot be a clean pass.

**Completion test:** Implement runnable registry, secret/config and workload-identity fixtures in separate commits. Full profiles require their observations; development reports supported skips/warnings honestly and never converts missing evidence to success.

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

### C34 — Define lease ownership for the remaining test resources

**Priority:** P2. **Wave:** 4. **Book chapters:** 15.

Evidence: `src/testkit/lease.rs:LeasedResource`; `src/testkit/bench/suites.rs`.

Only apps and namespaces are leased. Jobs, images, tokens, mounts and node effects need their own authority/cleanup contracts; cold-cache image benchmarks cannot safely evict arbitrary images.

**Completion test:** Extend one resource family per commit with durable ownership, renewal bounds, leader/client-death cleanup and refusal to delete pre-existing resources.

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

### C39 — Handle development CLI paths without unwraps

**Priority:** P2. **Wave:** 3. **Book chapters:** 09.

Evidence: `src/relish/dev.rs:create`; `start`; `test`.

The older dev workflow still unwraps filesystem paths and saved node addresses. Managed quickstart fixes did not remove this separate CLI surface.

**Completion test:** Non-UTF-8 paths, incomplete/corrupt saved state and missing nodes return actionable errors and leave unrelated VMs untouched.

### C40 — Own replaced rootless proxy processes

**Priority:** P2. **Wave:** 3. **Book chapters:** 01, 14.

Evidence: `src/grill/runc.rs:start_rootless_network`; `adoption`; `src/grill/rootless.rs:setup_slirp4netns`.

Map insertion discards a previous slirp handle without explicit shutdown; socket readiness polls synchronous exists on the async runtime.

**Completion test:** Repeated start/adopt under the same instance retires only the displaced owned proxy; asynchronous socket checks and timeout cleanup leave no orphan forwards.

### C41 — Return a non-zero managed-status result for unhealthy nodes

**Priority:** P2. **Wave:** 4. **Book chapters:** 09.

Evidence: `src/relish/quickstart/lifecycle.rs:Action::Status`.

Missing VMs and failed API readiness are printed, but the action still returns Ok. Scripts cannot distinguish a healthy local cluster by exit code.

**Completion test:** Healthy, stopped, missing and unready nodes have documented distinct outcomes; structured output and exit status agree.

### C42 — Key alert state by labelled series

**Priority:** P2. **Wave:** 3. **Book chapters:** 06, 11.

Evidence: `src/mayo/webhook.rs:collapse_series`; `gather_latest_values`; `src/mayo/alert.rs`.

Fresh per-series values are collapsed back to one value per metric name, so separate labelled workloads do not get independent alert state.

**Completion test:** Two label sets cross thresholds independently, retain separate pending/firing/resolved state and do not divide values across series.

### C43 — Resolve short service names in the caller namespace

**Priority:** P2. **Wave:** 3. **Book chapters:** 03.

Evidence: `src/onion/dns.rs:DnsConfig::default_namespace`.

Bare app.internal names use the node's configured default namespace, not the source workload's namespace.

**Completion test:** Same-named apps in two namespaces resolve correctly from their own workloads, including TCP/UDP and qualified names; unknown source identity fails explicitly.

## Engineering follow-ups

### H01 — Remove stale wiring claims and validate documentation

**Priority:** P2. **Wave:** 5. **Book chapters:** 02, 06, 15.

Evidence: `src/meat/filter.rs`; `score.rs`; `quota.rs`; `src/mayo/scrape.rs`; `docs/roadmap.md`.

Cordon, quotas and scraping are wired, but comments say otherwise; scoring says 130/40 while constants give 150/60. Earlier phase and acceptance descriptions still drift.

**Completion test:** Correct claims against call sites, test runnable snippets, check links/commands and keep explicit shipped/planned/experimental labels. Review the book at the same time. Clarify disabled-auth workload-fault versus node-fault authorisation and client teardown guarantees; do not silently weaken either contract.

### H02 — Remove or deliberately expose unused helper entry points

**Priority:** P3. **Wave:** 5. **Book chapters:** 03, 07, 15.

Evidence: `src/wrapper/proxy.rs:run_proxy`; `src/onion/dns.rs:run_dns_responder`; `src/meat/autoscaler.rs:run_autoscale_loop`; `src/pickle/pull.rs:image_available_locally`.

These public helpers have no production callers; some describe a different lifecycle from the wired implementation. Wait/renew/deadline helpers also need a call-site inventory. Include the old testkit renewal helpers, unused smoker helpers and mislocated API documentation in this inventory.

**Completion test:** For each helper, delete it or identify its supported caller and tests. Keep wire compatibility; do not wire obsolete implementations merely to make them used.

### H03 — Resolve inert configuration and wire fields explicitly

**Priority:** P2. **Wave:** 5. **Book chapters:** 02, 03, 14.

Evidence: `src/wrapper/types.rs:LeastConnections`; `worker_threads`; `src/mustard/message.rs:lamport`; `src/config/node.rs:release_url`.

Several fields promise behaviour that no reader applies. Removing bincode fields casually would break peers and persisted state.

**Completion test:** Implement the named behaviour or reject/deprecate it with compatibility tests. Preserve historical wire layout until a negotiated migration exists.

### H04 — Use typed alert and scheduler error contracts

**Priority:** P3. **Wave:** 5. **Book chapters:** 06, 15.

Evidence: `src/relish/client.rs:alerts`; `src/testkit/bench/suites.rs`.

Alerts are untyped JSON values; capacity benchmarking recognises a no-eligible-nodes error by substring.

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

### H07 — Add useful public API doctests

**Priority:** P3. **Wave:** 5. **Book chapters:** 01, 09, 15.

Evidence: `src/config`; `src/relish`; `docs/book`.

The doctest command passes with zero doctests, so its green status does not establish working public examples.

**Completion test:** Small configuration/client examples compile and exercise public behaviour; document Rust ownership/error types at their first use in the book.

### H08 — Identify and eliminate leaked test processes

**Priority:** P2. **Wave:** 5. **Book chapters:** 15.

Evidence: `.config/nextest.toml`; `tests/support`.

The final pre-audit no-default run reported one passing-but-leaky test without printing its identity under the current status filter.

**Completion test:** Retain per-test leak diagnostics, reproduce the offending case, repair process ownership and repeat the relevant suite with no cleanup warning. Do not hide it with a longer timeout.

### H09 — Complete node-pressure diagnostic hygiene

**Priority:** P3. **Wave:** 5. **Book chapters:** 08, 15.

Evidence: `src/smoker/node_pressure.rs:helper startup`.

The legacy 100 ms stderr drain and parent-death commentary remain after the functional startup fix.

**Completion test:** Helper failures preserve bounded complete stderr; validate and document the actual Linux parent/thread-death semantics and assert cleanup on owner termination.

### H10 — Reuse one egress observation per health tick

**Priority:** P3. **Wave:** 5. **Book chapters:** 01, 03.

Evidence: `src/bun/agent.rs:live_egress_report_state call sites`.

The same expensive probe is called for readiness and again for reporting in a health cycle, which can also give inconsistent snapshots.

**Completion test:** One observation feeds both consumers, preserving fail-closed freshness and health deadlines; instrument call counts rather than timing a microbenchmark.

### H11 — Declare and test the actual Rust/toolchain baseline

**Priority:** P2. **Wave:** 5. **Book chapters:** 01, 14.

Evidence: `Cargo.toml`; `.github/workflows/build.yml`; `docs/README.md`.

The README promises Rust 1.85+, Cargo declares no rust-version and CI uses moving stable. The promised minimum and reproducible release baseline are unqualified.

**Completion test:** Select a toolchain compatible with the locked dependency graph; test the declared minimum and pin release builds, updating documentation and upgrade/rebuild policy.

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

PromQL/remote read, additional retention/downsampling tiers and the multi-level reporting tree are planned. A flat council aggregation path already works.

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

### V03 — Publish and install the exact signed candidate

**Priority:** gate. **Wave:** 7. **Book chapters:** 09, 14.

Evidence: `scripts/release`; `.github/workflows/build.yml`; `docs/releasing.md`.

The build/signing pipeline exists, but no 0.1.0 tag or public candidate has been qualified. Signing and Pages/domain setup are external prerequisites.

**Completion test:** Use the authorised release key, verify relocated native/BPF artefacts, install downloaded bytes and promote the same bytes. Never create a tag merely to make a checklist green.

### V04 — Measure repeated cold installs on the advertised host matrix

**Priority:** gate. **Wave:** 7. **Book chapters:** 09, 15.

Evidence: `docs/qualification/2026-09-17-laptop.md`; `docs/quickstart.md`.

241.75 seconds was one M2 Max development-binary run; initial signed CLI/guest binary downloads were excluded. Other host paths remain unqualified.

**Completion test:** Time from first installer byte to healthy three-node sample HTTP on repeated empty caches; record failures, hardware/network conditions, Linux host prerequisites, Intel macOS, sleep/wake, changed addresses, interruption and lifecycle cleanup.

### V05 — Review dependency exceptions before their deadline

**Priority:** gate. **Wave:** 7. **Book chapters:** 14, 15.

Evidence: `.cargo/audit.toml`; `Makefile:audit`.

The current audit passes with five named exceptions. An exception is a tracked risk, not a fixed dependency.

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
| C5: node-fault routing and quorum safety | Standalone/wrong-target refusal implemented; concurrent reservations remain C06. |
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
| G4: advisory review | August review implemented; five exceptions remain under V05's November deadline. |
| H1: three-node lease/leader-death acceptance | V01 explicitly retains this missing acceptance scenario; local lease tests are not a substitute. |
| H2: complete runbook | V01; amend profiles/prerequisites and retain every result. |
| Carried-forward streaming, cert lifecycle, resource leases | Streaming implemented; C14 and C34 remain. |

The July 18 M8 infrastructure/command milestone is done, but its fixtures and
complete acceptance are C30/V01. Its certificate lifecycle follow-up is C14.
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
