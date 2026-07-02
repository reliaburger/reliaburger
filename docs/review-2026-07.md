# Codebase Review — July 2026

A full verification pass against the claims in [progress.md](progress.md), run across
six parallel subsystem reviews. Every finding below was checked against the source and
cited as `file:line`. This document is the backlog: work items reference these IDs.

## Baseline health

- `cargo build` — clean.
- `cargo clippy --all-targets` — zero warnings.
- `cargo test` — **1590 passed, 0 failed, 1 ignored** (the ignored one is the 10k-node
  gossip scale test).
- Book chapters 1–11 substantial, 12 partial (`[~]`), 13–15 stubs — matches progress.md.

The green suite is misleading. Tests pass because each subsystem is exercised as an
**isolated library**. The dominant finding is that a large fraction of progress.md's
"done" items are **library-only**: implemented and unit-tested, but never wired into the
`bun`/`relish` binaries.

### The structural root cause

`router()` in `src/bun/api.rs:81-102` hardcodes `council`, `membership`, `ketchup`,
`rollup_store`, and `gitops_webhook_tx` to `None`; `src/bin/bun.rs:90-92` hardcodes
`wrapping_ikm: None`. Those six `None`s silently disable API auth, workload identity,
tokens, secret rotation, cross-node queries, rollups, GitOps, and container-log storage —
*even under `bun --cluster`*. Fixing these is a precondition for Stage 3 below.

---

## Critical

| ID | Issue | Location |
|----|-------|----------|
| **C1** | Path traversal in OCI whiteout unpacking — a malicious image layer with `../` whiteout entries (`.wh.` / `.wh..wh..opq`) deletes files outside the rootfs. Regular entries are protected by `unpack_in`; whiteouts are not. | `src/grill/image.rs:339-377` |
| **C2** | Path traversal via `upload_id` — axum percent-decodes the path segment, so `PATCH …/uploads/..%2F..%2F..%2Fpath` appends request-body bytes to any writable file, and `PUT …?digest=` renames an arbitrary readable file into the blob store then serves it (digest-verified exfiltration). Registry is unauthenticated on `0.0.0.0`. | `src/pickle/store.rs:44-46,116-166`; `src/bin/bun.rs:461` |
| **C3** | Raft split-brain on restart — in-memory vote+log (`MemLogStore`) is wired into production. A restarted bootstrap node re-runs `initialize()` on the now-empty store and forms a fresh single-node cluster (electing itself leader) while surviving peers still run the old cluster → two leaders. openraft's safety contract requires durable vote+log. | `src/cluster/runtime.rs:124,141-145`; `src/council/log_store.rs:145-154` |
| **C4** | Firewall reconcile destroys container networking — the perimeter ruleset opens with `delete table ip reliaburger`, which also wipes the container port-mapping DNAT/masquerade rules that live in the same table. Fires on every membership change. (Introduced by the "flush before re-apply" fix.) | `src/firewall/rules.rs:84-88` vs `src/grill/netns.rs:390-460` |
| **C5** | Nothing is enforced by security: (a) `auth_middleware` never attached to any router and fails **open** on an empty token store; (b) no mTLS on any listener — Raft RPC is plaintext TCP; (c) gossip HMAC never signed/verified (`GossipMessage.hmac` stays zeroed); (d) container secrets never decrypted — `build_env` hardcodes `build_env_with_decryptor(spec, None)`, so containers receive the literal `ENC[AGE:...]`. | `src/sesame/auth.rs:100,114-117`; `src/sesame/mtls.rs`; `src/mustard/message.rs:28-45`; `src/grill/oci.rs:181` |

---

## High

### Wired paths with real bugs

| ID | Issue | Location |
|----|-------|----------|
| **H1** | Restart re-drive broken for apps on every runtime — old container never stopped before re-create; ProcessGrill rejects same-id create (stale `Running`), runc/apple never `delete`, so the instance wedges in `Preparing` forever and the old process leaks. Only ProcessGrill *jobs* work. Test only asserts `restart_count > 0`, masking it. | `src/bun/supervisor.rs:364-419`; `src/bun/agent.rs:1671-1686`; `src/grill/process.rs:74-82`; `tests/integration.rs:429-435` |
| **H2** | Rolling redeploy leaves service with 0 backends — unregisters the app, re-registers with an empty backend list, never `add_backend`s the new instances; also inserts `health_config: None` (no re-probe) and leaks the host port. → `relish resolve` empty, ingress 502. | `src/bun/agent.rs:1057-1112,1061-1090` |
| **H3** | Event loop stalls — `FollowLogs` awaited inline; init-container wait polls forever with no timeout; per-replica `sleep(≤5s)` during deploy runs inside the select arm. Any one freezes health checks, restarts, firewall reconcile, and all API commands. | `src/bun/agent.rs:2156-2174,1366-1373,974-976` |
| **H4** | SWIM: suspect updates disseminated with the **prober's** incarnation, not the target's. Lower → peers discard suspicion (detection stops propagating); higher → overrides fresher Alive state. | `src/mustard/protocol.rs:287` |
| **H5** | SWIM: a node never refutes being declared `Dead` (only `Suspect`). A false Dead is unrecoverable until the 60s reap. | `src/mustard/protocol.rs:333-335` |
| **H6** | SWIM: suspicion→Dead timeout measured from `last_ack`, not from suspicion start. For gossip-learned peers `last_ack` is stale, so one dropped probe → Suspect → immediate Dead, bypassing the refutation window. | `src/mustard/protocol.rs:428-432` |
| **H7** | SWIM: membership `watch` only publishes when member **count** changes — Alive→Suspect→Dead keeps count constant, so the council reconciler and `relish nodes` see a dead node as Alive for up to 60s. | `src/mustard/protocol.rs:123-131` |
| **H8** | Scheduler places all replicas of an app on one node — bin-pack weight (50) dominates spread (10); spread can only win within 20pp utilisation. Untested (`schedule_fixed_replicas_places_all` doesn't assert distinct nodes). | `src/meat/score.rs:16-20`; `src/meat/scheduler.rs:78-112` |
| **H9** | Metrics/log Parquet is write-only and clobbered on restart (`flush_counter` resets to 0 with fixed dirs), never reloaded at startup, and in-memory batches grow unbounded (`prune` deletes files, never evicts batches). Export checkpoint is keyed by filename, so post-restart files are treated as already-exported. | `src/mayo/store.rs:57-63,139`; `src/ketchup/log_store.rs:100-106,199`; `src/ketchup/export.rs:94` |
| **H10** | Container stdout/stderr never reaches any log store — the only `LogStore.append` calls are two startup seed lines. Every Ketchup query/SQL/export/cross-node endpoint operates on those two lines. `relish logs` works only because `/v1/logs/{app}/{ns}` bypasses Ketchup and asks the runtime directly. | `src/bin/bun.rs:182-196` |
| **H11** | `relish fmt` corrupts nested-table configs — `append_section`/`append_fields` don't recurse, so `[app.web.health]` formats to invalid TOML that fails to re-parse, written back over the user's file. Affects `health`/`ingress`/`autoscale`/`deploy`/`placement`. | `src/relish/fmt.rs:49-74`; `src/relish/commands.rs:704-706` |
| **H12** | `is_key_trusted` always returns `true` — GitOps commit-signing allowlist not enforced; any valid signature accepted even when its fingerprint matches no trusted key. (Latent: Lettuce unwired.) | `src/lettuce/verify.rs:78-87` |
| **H13** | Exit-code tracking is ProcessGrill-only — runc/apple inherit `exit_code() -> None`; `check_jobs` treats `None` as failure, so on those runtimes every *successful* job is retried then marked Failed. `logs`/`follow_logs`/`exec` also unimplemented on runc → empty logs, failed exec. | `src/grill/mod.rs:120-168`; `src/grill/runc.rs:109-318`; `src/bun/agent.rs:1602` |

### Confirmed library-only (claimed done, unreachable from the binary)

Each of these is implemented and unit-tested but has no production caller. Wiring each is
a Stage-4 item; the integration test must drive the **binary**, not the library.

| ID | Subsystem | Evidence |
|----|-----------|----------|
| **L1** | Meat scheduler (no binary path schedules onto a remote node; deploys are always local) | `src/meat/scheduler.rs`; only `tests/scheduling.rs` + `src/meat` tests |
| **L2** | Deploy orchestrator / rolling / blue-green (`DeployOrchestrator`, `execute_blue_green`; no production `DeployDriver`, trait is synchronous) | `src/meat/orchestrator.rs:243`; `src/meat/blue_green.rs` |
| **L3** | Autoscaler (`run_autoscale_loop` never spawned; `AutoscaleOverride` never produced) | `src/meat/autoscaler.rs:213` |
| **L4** | State reconstruction (`ReconstructionController` never invoked; no `Correction` consumer) | `src/reconstruction/controller.rs:6` |
| **L5** | Council selection algorithm (`select_council_candidates` never called; runtime uses naive sort-by-id + truncate-to-7, which can truncate out and demote the current leader) | `src/council/selection.rs:70-130`; `src/cluster/runtime.rs:280-295` |
| **L6** | Reporting tree beyond flat-star — leader's `aggregated_rx` is held alive but read by nothing; multi-level `AggregatedReport` handler unreachable (no sender). StateReport carries zeroed resource usage/cached_specs/event_log. | `src/cluster/runtime.rs:55,219-256`; `src/reporting/aggregator.rs:82-89`; `src/reporting/worker.rs:203-216` |
| **L7** | Entire Wrapper ingress proxy — `run_proxy`, `RateLimiter`, `DrainTracker`, TLS config, WebSocket all have zero production callers; no HTTP(S) listener is ever bound. "Dedicated tokio runtime for DDoS isolation" does not exist. | `src/wrapper/proxy.rs:33`; `rate_limit.rs`; `draining.rs`; `tls.rs`; `websocket.rs:74-84` |
| **L8** | Onion eBPF loaders — `onion_ebpf` is permanently `None`; `ebpf` feature not in default build; `.bpf.o` objects never installed to a runtime location. | `src/bun/agent.rs:332,366`; `Cargo.toml:72`; `src/onion/ebpf/loader.rs:32` |
| **L9** | DNS responder (`run_dns_responder` never spawned; no container `resolv.conf` points at it) | `src/onion/dns.rs:45` |
| **L10** | Pickle replication + peer-pull + GC — `replicate_manifest`/`select_peers`/`pull_layer_from_peer`/`gc_sweep` have no callers; push stores locally with hardcoded `holder_nodes: {0}`; catalog is `ManifestCatalog::default()` per boot (metadata lost on restart, not Raft-replicated). | `src/pickle/replication.rs`; `src/pickle/pull.rs`; `src/pickle/gc.rs:47`; `src/bin/bun.rs:374` |
| **L11** | Mayo rollups + Prometheus scrape — `RollupWorker` never spawned; `ReportAggregator` gets `rollup_store: None`; `fan_out_cluster_query`/`scrape_endpoint` no callers. `/v1/metrics/cluster` returns "no rollup store configured" forever. | `src/mayo/rollup_worker.rs`; `src/cluster/runtime.rs:168-173`; `src/mayo/query_fanout.rs`; `src/mayo/scrape.rs` |
| **L12** | Ketchup flat-file store — `KetchupStore` created as `_ketchup_store` then dropped; `SparseIndex` written but never read (queries scan whole files); `detect_json`/`enforce_retention` no callers. | `src/bin/bun.rs:175`; `src/ketchup/store.rs:83-117` |
| **L13** | Lettuce GitOps — `execute_sync`/`GitRepo`/`WebhookValidator`/`select_coordinator` no callers; `/v1/gitops/webhook` always 503 (`gitops_webhook_tx` = `None`); `[gitops]` config never read; Raft `GitOpsSyncUpdate` never produced. | `src/lettuce/*`; `src/bun/api.rs:97,1702-1716`; `src/config/node.rs:29` |
| **L14** | Smoker fault injection — `InjectFault` only inserts into the registry; `evaluate_safety`, `smoker::process`/`resource`/`node`, and `bpf_maps` writers have no callers. Faults never kill/pause/pressure/delay anything; safety rails run only in tests. | `src/bun/agent.rs:680-685,804-855`; `src/smoker/safety.rs:13` |
| **L15** | Chaos partition — `InjectPartition` only records a registry entry; transport `.blocklist()` never populated in the running agent. `relish chaos` reports success but partitions nothing. | `src/bun/agent.rs:640-664`; `src/mustard/transport.rs`; `src/council/network.rs:334` |
| **L16** | Egress allowlists — `resolve_egress_entries`/`egress_to_bpf_entries` no callers; supervisor sets `egress: None`. `[egress] allow=[...]` has no effect. | `src/sesame/egress.rs`; `src/bun/supervisor.rs:472` |
| **L17** | CRL checks + image-signature verification — `cert::check_crl` and `pickle::signing::verify_signature` never called; `require_signatures` never consulted. | `src/sesame/cert.rs:162`; `src/pickle/signing.rs`; state machine `state_machine.rs:167` |
| **L18** | Secret encryption at container startup, workload identity CSR flow, `/v1/identity/*`, token list/revoke, secret rotate — all reachable only past the six `None`s (C5, and `wrapping_ikm`/`council`). CSR path dead-ends at "no wrapping IKM available". | `src/council/node.rs:209-212`; `src/bun/api.rs:98,1726-1882` |

### CLI / API mismatches

| ID | Issue | Location |
|----|-------|----------|
| **X1** | `relish build` posts context to `:9117` (bun API, no `/v2` routes) instead of the pickle port, *and* `/v1/build` is a permanent 501. | `src/relish/commands.rs:880`; `src/pickle/build.rs:226`; `src/bun/api.rs:1687` |
| **X2** | `relish token create` is local-only — never issues `CreateApiToken` to Raft, so the token can never be validated or listed. | `src/relish/commands.rs:944-991` |
| **X3** | `relish rollback` calls no endpoint — fetches history and prints "(use `relish apply`…)". | `src/relish/commands.rs:584-617` |
| **X4** | `relish logs --grep/--since/--json-field` parsed then discarded (bound to `_grep`/`_since`/`_json_field`), despite the server supporting `grep`. | `src/bin/relish.rs:510-513` |
| **X5** | Dry-run fallback exits 0 — `relish apply`/`deploy`/`rollback` return `Ok(())` when the agent is unreachable (CI/script hazard: a dead agent makes deploys "succeed"). | `src/relish/commands.rs:46-53,535-542` |
| **X6** | `relish` no-args claims a TUI (doc comment) but `command` is required and no TUI module exists → usage error. | `src/bin/relish.rs:4,20-21` |
| **X7** | `relish secret pubkey` reads `sesame-state.json`; `relish init` writes `{cluster}-security-bootstrap.json`. Always errors "run `relish init` first". | `src/relish/commands.rs:286,999-1007` |
| **X8** | `relish logs-export` copies files directly, racing the agent's own export checkpoint; never contacts the agent. | `src/relish/commands.rs:110-162` |

### Dead config (parsed by `NodeConfig`, read by nothing)

`[resources]` (`reserved_cpu`/`reserved_memory`/`gpu_enabled`), `[reconstruction]` (whole
section), `[gitops]`, `[process_workloads]` (`with_process_config` never called),
`[node] labels` (never fed into gossip), `[storage] data`/`volumes` (agent hardcodes the
default), most of `[images]` (`max_storage`/`redundancy`/`gc_*`/`trust_policy.*`) and
`[metrics]` (`scrape_interval_secs`/`object_store_url`/`rollup_*`), `[logs] max_file_size_mb`.
See `src/config/node.rs` and the wiring audit for exact lines.

---

## Medium

| ID | Issue | Location |
|----|-------|----------|
| M1 | SQL injection into DataFusion from query params — breaks tenant isolation (read other namespaces' metrics/logs) and errors on any `'`. `/v1/metrics?name=`, `/v1/metrics/app/{app}/{ns}`, log `grep`. | `src/mayo/store.rs:229-234`; `src/bun/api.rs:1576-1590`; `src/ketchup/log_store.rs:322-334` |
| M2 | GC TOCTOU (latent) — two nodes each holding one of two copies both see `holders.len()==2` and both delete → total loss; mid-push blobs (empty holder set) classified "orphaned" and deleted while a manifest referencing them is in flight. | `src/pickle/gc.rs:96-104` |
| M3 | Blocking I/O + CPU on the tokio runtime — whole-blob `std::fs` read/write + full SHA-256 in async handlers (up to 512 MB buffered); Parquet encode+write under the store write-lock; `export_logs` blocking `fs::copy` while holding the LogStore read-lock. No `spawn_blocking`. | `src/pickle/api.rs:42,94,149,302`; `src/mayo/store.rs:128-136`; `src/bin/bun.rs:277-278` |
| M4 | Log merge/dedup key wrong — `merge_log_entries` dedups only *adjacent* `(ts,line)`: cross-node dupes survive when another same-second line sorts between them, and genuinely repeated identical lines within a second are collapsed. Same flaw in `merge_metrics_results`. | `src/ketchup/query.rs:13-21`; `src/mayo/query_fanout.rs:16-26` |
| M5 | VIP collisions unhandled — SipHash into 65,534 slots, ~50% collision odds near ~300 apps; `register_app` does no collision check; colliding apps on the same port silently cross-route. `name_to_id` truncation feeds firewall `app_id` → cross-namespace access. | `src/onion/vip.rs:31-40,83-87`; `src/onion/service_map.rs:32-60` |
| M6 | Service map keyed by app name only, ignoring namespace — same name in two namespaces collides; second `register_app` error discarded. | `src/onion/service_map.rs:16`; `src/bun/agent.rs:1110` |
| M7 | Service-map staleness — only jobs are monitored for exit; a crashed app without a health check stays `Running`/healthy forever; `remove_backend` only on explicit stop. | `src/bun/agent.rs:1575-1581,1756` |
| M8 | DNS responder fragility — `recv_from`/`result?` kills it on any socket error; serialised inline upstream forwarding (one slow upstream stalls all DNS); replies not validated vs query ID/source (spoofable); 512-byte buffer truncates EDNS0 without TC; QTYPE ignored (AAAA/MX get A); unmatched `.internal` leaks to public upstream. | `src/onion/dns.rs:51-186` |
| M9 | Crashed non-job apps never detected/restarted (nothing polls `grill.state()` for apps); instances stuck in `HealthWait` forever (no failure branch from HealthWait). | `src/bun/agent.rs:1571-1584`; `src/bun/health.rs:126-154` |
| M10 | Ports never released (`PortAllocator::release` no callers) — stop/remove/failed-deploy/redeploy all leak until pool exhaustion. | `src/grill/port.rs:73` |
| M11 | Health probes hardcode `127.0.0.1:{container port}` — ignore `host_port`/`container_ip`; with runc netns (and no port mapping) every health-checked app flaps unhealthy. | `src/bun/agent.rs:1525`; `src/bun/health.rs:44-55` |
| M12 | SIGTERM never triggers graceful shutdown (only `ctrl_c()` registered) → under systemd stop, `shutdown_all` never runs, workload processes orphaned. | `src/bin/bun.rs:498-504` |
| M13 | runc resource leaks — no `runc delete` anywhere; netns/veth torn down only in `kill()`, never on `stop()`/natural exit; `entries` map only grows. | `src/grill/runc.rs:226-288` |
| M14 | Duplicate cert serials under concurrent CSR — write `AllocateSerial` then read back `next_serial-1`; response doesn't return the allocated value → structural race. | `src/council/node.rs:253-255` |
| M15 | Council reconciler can wedge (blocking `add_learner` on an unreachable peer) and discards all errors. | `src/cluster/runtime.rs:313-318` |
| M16 | Rollback leaks the failed step's new instance; blue-green routing-swap error wedges in non-terminal `RoutingSwitching`; deploy `Halted`/`Reverting` are dead ends; `max_surge`/`max_unavailable` parsed but ignored. | `src/meat/orchestrator.rs:172-199`; `src/meat/blue_green.rs:101`; `src/meat/deploy_types.rs:286-358` |
| M17 | K8s import silently drops `command`/`args`, `env.valueFrom` (secret/configmap refs), and namespace (keyed by name only → cross-namespace overwrite). | `src/relish/k8s_import.rs:247-296,390-493` |
| M18 | Firewall: reconcile triggers on node **count** not membership (node swap leaves new node blocked); standalone mode (count 0 == initial) → firewall never applied despite `enabled: true`; rules drop TCP only but gossip is UDP → gossip port externally reachable. | `src/bun/agent.rs:343,1783`; `src/firewall/rules.rs:122-132` |
| M19 | Alert webhooks are generic-only — Slack (`{"text":…}`) and PagerDuty (`routing_key`) payloads never formatted per `dest_type`; both would reject/drop. Dispatch loop itself is wired. | `src/mayo/webhook.rs:112-140` |
| M20 | Log export to S3/GCS is filesystem-only — `object_store` has only the `fs` feature; an `s3://` destination is treated as a literal local dir. | `src/ketchup/export.rs:59-111`; `Cargo.toml:60` |
| M21 | Managed volumes half-implemented — mount entries generated but the host dir is never created; `VolumeManager` (incl. loop-mount size enforcement) has no callers → runc bind-mount to a nonexistent source fails create. | `src/grill/oci.rs:285-306`; `src/grill/volume.rs` |
| M22 | Rootless: resource limits silently dropped (claimed systemd-run mechanism doesn't exist); slirp4netns support has zero callers → rootless containers get an empty netns (no connectivity/port-forward). | `src/grill/rootless.rs:95-97,193-249` |
| M23 | Process-workload allowlist + `mount_isolation` never enforced (`with_process_config` no callers; `ProcessManager::prepare_script`/`cleanup` unused — scripts run via `oci::build_args`). | `src/bun/supervisor.rs:88-102`; `src/grill/process_workload.rs:84-136` |
| M24 | `KetchupStore::today()` calendar math is wrong (`month = days%365/30+1` can yield 13; day/year drift) → invalid dates like `2026-13-05`. (Dead path today.) | `src/ketchup/store.rs:47-48` |
| M25 | Workload private key written world-readable (`0644`) — should be `0600`. | `src/sesame/identity.rs:205-218` |

---

## Low

Container blob-cache poisoning on crash (truncated blob skipped as cached, no re-verify)
`image.rs:223-250`; unpack clears shared rootfs under a running container `image.rs:288-297`;
`parse_num` unchecked multiply panics/wraps `config/types.rs:67`; container IPs never reused,
index wraps after 509 into neighbouring subnets `runc.rs:124-129`; `stop_app` reports Stopped
before exit / no SIGKILL escalation `agent.rs:1736-1752`; ReportWorker busy-loops if council
sender drops `worker.rs:114-118`; gossip UDP `recv` error busy-loops `protocol.rs:205-210`;
`raft_id_from_name` weak djb2 hash (collision merges nodes) `identity.rs:16-21`; batch
allocation nondeterministic (HashMap iteration) `batch.rs:60-70`; `assign_parent` relies on
`DefaultHasher` cross-version determinism (collides with Phase 14 upgrades) `assignment.rs:29`;
aggregator never evicts departed nodes `aggregator.rs:79`; non-constant-time join-token hash
compare `ca.rs:388`; `verify_jwt` skips `aud`/`iss` `oidc.rs:108-144`; keyless verify ignores
cert validity + SPIFFE binding `signing.rs:147-187`; `manifest_put` never verifies referenced
blobs exist `api.rs:385`; upload sessions never expire `store.rs`; fan-out query params not
URL-encoded `query.rs:52`; fan-out swallows node failures as empty results `query.rs:65-73`;
chart JSON in single-quoted HTML attr but `escape_html` doesn't escape `'` (safe only because
titles are hardcoded) `app_detail.rs:126-133`; `maps.rs` `.unwrap()` on missing BPF map;
nftables injection via unvalidated `admin_cidrs` `rules.rs:109-116`; git arg-injection via
config-supplied url/branch (no `--` separator) `git.rs:38-47`; routing lookup mangles IPv6
Host headers `routing.rs:117`; diff engine re-adds every job each sync `diff.rs:101-111`.

---

## Resolution plan

Five stages, sequenced by risk. Each fix lands as its own commit with a test; wiring items
in Stage 4 must add an integration test that drives the **binary**, not the library.

### Stage 0 — Security & data-loss stop-the-bleed
C1, C2 (reject `..` path components / sanitise the upload-id segment), C4 (give the perimeter
firewall its own nft table, or reconcile without flushing the container table), C5(d) secret
decryption in the wired container path, bind the registry to loopback or require auth.

### Stage 1 — Correct the wired single-node path
H1 restart re-drive (stop old container first, drive all runtimes), H2 redeploy backend/health/
port tracking, H3 move blocking work off the event loop + init timeout, H13 exit-code/logs/exec
on runc & apple, H9/H10 pipe container stdout/stderr into the log store + reload Parquet on
startup + bound in-memory batches, M9–M13 (crash detection, port release, probe target, SIGTERM,
runc cleanup).

### Stage 2 — Cluster safety
C3 durable Raft log+vote and don't `initialize()` a previously-bootstrapped/non-empty node;
H4–H7 the four SWIM bugs + membership-watch trigger; L5/M15 council reconciler that can't demote
the leader and can't wedge; M14 serial allocation.

### Stage 3 — Enforce the security that's claimed done
C5(a–c) attach auth middleware, mTLS on Raft/API listeners, gossip HMAC; load SecurityState +
`wrapping_ikm` from the bootstrap `relish init` writes; populate the `ApiState` `None` fields.
This lights up L17/L18 (identity, tokens, secret rotation, CRL, signature verification) and
fixes X2/X7.

### Stage 4 — Wire the remaining library-only subsystems (one at a time, binary-driven test each)
L1 scheduler→placement→remote dispatch · L2 deploy orchestrator · L7 ingress proxy listener ·
L8/L9 eBPF loader + DNS responder · L10/M2 replication + GC schedule · L11 rollups · L13 GitOps
sync loop · L14/L15 Smoker actual injection + chaos blocklists · L16 egress. Fix H8 scheduler
spread weighting, H11 `relish fmt`, H12 trusted-key check, M17 K8s import fidelity, and the X-series
CLI mismatches as their subsystems come online.

### Throughout
Correct progress.md (tag library-only-vs-wired), and fix the misleading tests (restart H1,
chaos "worker isolation" injects no fault) so they assert real behaviour.
