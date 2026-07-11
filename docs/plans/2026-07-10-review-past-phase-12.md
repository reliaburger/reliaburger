# Post-Phase 12 review: implementation truth and the Phase 12b backlog

_Review started 10 July 2026, refreshed 11 July 2026, and re-validated
against the merged Stage 5 work. The baseline is now **main with PR #77
("Stage 5: mTLS on internal listeners, CRL enforcement, and dashboard auth")
merged** — so the mTLS/CRL/dashboard-auth findings the earlier draft (baseline
8310d63) listed as open are now fixed and recorded in "Fixed on the merged
Stage 5 work" below, not double-counted. Every remaining finding in this
register was re-checked against the code by parallel validation agents; the
handful the earlier draft over-stated are corrected inline (JOB5, DEP5, DEP8,
CP8, CP12) and the one it under-stated (REG1) is confirmed real._

This review consolidates three earlier sources:

- the [9 July code walkthrough](2026-07-09-review-codex.md), with H1-H8 and
  codex M1-M5;
- the [9 July design discrepancy register](2026-07-09-review-design-discrepancies.md),
  with D1-D22; and
- the old "Beyond Phase 11b" section from
  [progress.md](../progress.md), including C5(b), L17, M3-M24, X8 and the
  24 Low findings from the [2 July review](2026-07-02-review-codebase.md).

The filenames matter. The two 9 July reviews were renamed after their first
drafts. All links in this document use the current date-first names.

## Executive assessment

Reliaburger has a broad implementation and a large test suite. That isn't the
same thing as a production-ready cluster. The present binary works best as a
small, trusted deployment where the nodes are voters, operators control the
network boundary, eBPF-dependent policy is treated as optional, and a human
can repair failed convergence.

The highest-risk problems are more concrete than missing polish:

1. Batch and build internal endpoints cross the trust boundary. A caller can
   submit host-capable work, control callback or local registry destinations,
   and in one path cause Bun to send its Admin-equivalent service token to an
   attacker-controlled URL.
2. Namespace isolation is advertised but the firewall maps have no production
   writers. Empty maps fail open. Rolling deploys also skip egress policy and
   IPv6 bypasses it completely.
3. Secret rotation can select the old key for new encryption, then delete that
   key without re-encrypting stored ciphertext.
4. Pickle does not treat the raw manifest as a referenced blob. GC can delete
   a tagged manifest and leave the catalogue pointing at a 404.
5. A corrupt Raft snapshot is treated as absent even though compaction may
   already have removed the log needed to rebuild it.
6. The cluster controller still equates queue admission with deployment
   success, keeps stale reports, ignores reconstruction corrections and cannot
   reliably direct workers outside the council.

There are good foundations, and the security boundary is materially stronger
than the first draft found. The merged Stage 5 work (PR #77) took the PKI from
a tested library to a live boundary: Bun now loads the node identity, builds
the TLS configs, refreshes the CRL from Raft, and runs **mutual TLS on the
Raft RPC, reporting and agent-API listeners** behind a `require_mtls` mode
matrix, with a Node-CA-pinned client verifier and read-only session auth on
the dashboard. What remains is real but narrower: the **Pickle registry** is
still an unauthenticated plaintext listener, API **scope** checks and the
bootstrap fail-open are unchanged, peer certs are not yet bound to an expected
node id, and the non-mTLS correctness/data-plane findings below are untouched.

## Fixed on the merged Stage 5 work (PR #77)

Re-validated against the merged code; these earlier findings are now closed
and must **not** be re-listed as open below:

- **PKI1** — Bun loads the identity, builds the server/client TLS configs,
  wraps the Raft/reporting/API listeners and refreshes `CrlHandle`;
  `[security] require_mtls` drives the mode matrix (`src/bin/bun.rs`,
  `src/cluster/runtime.rs`).
- **PKI2** — the client-cert verifier is anchored on the **Node CA**, so a
  workload certificate can no longer authenticate as a node
  (`src/sesame/mtls.rs`, `build_mtls_server_config` / `build_api_server_config`).
- **AUTH6 / H6** — `/` and `/ui/*` sit behind auth; a token is exchanged for a
  read-only `HttpOnly` session cookie (`sesame::session`, `brioche::login`).
- **AUTH3 (partial)** — a session-cookie path now exists; the empty-token-store
  bootstrap fail-open **remains open** (kept in Theme 12 below).
- **L17 (image signatures)** — `verify_keyless` now checks the signing chain
  against the CRL and fails closed on a revoked cert (`src/pickle/signing.rs`).
  The other keyless gaps (validity, EKU, issuer, SPIFFE, every-intermediate)
  stay open under IMG3 / Theme 14.

## Method and severity

The audit followed production paths from src/bin/bun.rs and src/bin/relish.rs
into the agent, Raft runtime, API handlers and long-lived workers. Library
helpers counted as implemented only when a binary calls them and handles their
failure. Configuration fields were checked the same way: parsing a field is
not wiring it.

The severity labels used below are:

| Priority | Meaning |
|---|---|
| P0 | Remote execution, credential disclosure, irreversible data loss or a fail-open security boundary |
| P1 | Cluster-wide correctness, availability or security failure under ordinary operation |
| P2 | Material reliability, resource-exhaustion, incomplete feature or misleading operator result |
| P3 | Bounded correctness, portability or maintainability defect |

The default test suite is useful regression evidence, but many findings exist
because tests exercise a pure helper without driving Bun's production path.
The Linux eBPF, runc, Btrfs, rootless, Buildah, multi-node and upgrade gates
remain essential acceptance tests for Phase 12b.

## Reconciliation with the 9 July reviews

### Code walkthrough

| Existing finding | Status on this baseline | Phase 12b owner |
|---|---|---:|
| H1: non-council workers do not learn the leader | Open | 1 |
| H2: council membership only grows | Open | 2 |
| H3: Onion and Wrapper use node-local endpoints | Open | 4 |
| H4: token scopes are ignored | Open | 8 |
| H5: the security boundary is plaintext/fail-open | Mostly fixed by Stage 5 (mTLS on Raft/reporting/API, session auth); registry TLS + scope/bootstrap remain | 12, 13, 17 |
| H6: public Brioche leaks data and cannot call protected panels | Fixed by Stage 5 (session auth + route lockdown); real dashboard data + AUTH8 remain | 12 |
| H7: placement acceptance is recorded as deployment success | Open and broader than first reported | 6 |
| H8: process-workload policy is not passed to production | Open | 12 |
| codex M1: namespace/VIP collision | Open | 4 |
| codex M2: managed volumes are not prepared | Fixed in Phase 12 | - |
| codex M3: long deploy work blocks the Bun event loop | Open | 6 |
| codex M4: metrics/log queries load whole history | Open | 17 |
| codex M5: advertised platform features are incomplete | Partial: batch/build execute, other gaps remain | 5, 13, 15, 16, 21 |

All eight High findings remain open or only partly addressed. Phase 12 fixed
managed-volume preparation and added batch/build execution, but the new
execution paths introduced serious authorisation, durability and input
validation defects.

### Old deferred backlog

Three corrections are important:

- M7, non-job crash monitoring, is fixed by the production check_apps path.
- M21, managed-volume directory preparation, is fixed by Phase 12 E0.
- M20 is not fixed. AWS and GCP object-store features are enabled, and remote
  query/snapshot code can use them, but src/ketchup/export.rs still turns the
  configured destination into a PathBuf and calls std::fs::copy. An s3:// or
  gs:// log-export target is still a local path.

X6, the no-arguments Relish TUI, remains solely in Phase 13. It is not
duplicated in Phase 12b.

## Findings

### Control plane, membership and recovery

| ID | Priority | Finding and evidence |
|---|---:|---|
| CP1 | P1 | Workers outside the voter set derive no leader API/reporting route. Placement and reporting both depend on local Raft metrics in src/cluster/orchestrate.rs and src/cluster/runtime.rs. An eighth node in a seven-voter council can stop converging. |
| CP2 | P1 | Council reconciliation starts with every current voter and has no removal path. Dead or unsuitable voters permanently occupy seats and can prevent recovery even when healthy spares exist. |
| CP3 | P0 | Snapshot decode errors are treated as "no snapshot" in src/council/state_machine.rs, while durable compaction deletes the covered log. A corrupt snapshot can therefore boot an empty desired/security state. Read and initialisation errors are also swallowed in freshness/bootstrap checks. |
| CP4 | P1 | Reconstruction results are calculated and discarded. The diff reduces placement to app/node pairs, so one desired replica missing from two colocated replicas can look healthy. |
| CP5 | P1 | Stale reports stay in AggregatedState, satisfy reconstruction coverage and feed scheduling. Sender wall-clock timestamps allow a future timestamp to stay fresh. Reports need a receive-time epoch tied to leadership. |
| CP6 | P1 | Every supervisor entry is reported as running and consumes capacity, including stopped, failed and completed jobs. This hides missing replicas and eventually starves scheduling. |
| CP7 | P1 | Node labels parse from node.toml but are not carried in MembershipUpdate. Mustard creates members with empty labels while the live scheduler filters on them. Required placement labels and zone-aware council choice are inert. |
| CP8 | P1 | **(Re-validated: partly overstated.)** `filter_nodes` does check resources/labels/readiness at placement time; the real defect is that the scheduler builds a fresh resource cache per app and never re-validates against changed node state, so concurrently-planned apps don't reserve against each other and old allocations can double-count. |
| CP9 | P1 | SWIM self-refutation marks the local record Suspect/Dead before broadcasting Alive, does not restore it locally and increments from the wrong incarnation. Ping/Ack has no probe nonce, so an old ACK can satisfy a new probe. |
| CP10 | P2 | Raft IDs use a weak DJB2-derived value; reporting parent assignment relies on DefaultHasher stability; stale aggregator entries are never evicted; reporting and gossip loops can spin after their watch/transport closes. |
| CP11 | P1 | Raft and reporting accept unbounded connections, caller-sized frames and no read/write deadline. **(Stage 5 now requires mTLS on these listeners when `require_mtls` is set, which shuts out anonymous connections; the per-connection handshake/read deadlines and frame bounds are still missing.)** |
| CP12 | P1 | **(Re-validated: partly overstated.)** A disk-pressure check exists (`src/bun/disk_pressure.rs`); what remains design-only is full-council-loss recovery, encrypted external backup/restore and disk-pressure council *resignation*. |

### Desired state, scheduling and deployment

| ID | Priority | Finding and evidence |
|---|---:|---|
| DEP1 | P1 | Instance IDs omit namespace. default/api and payments/api can use the same api-0 key in Supervisor; rolling and job IDs have the same class of collision. Stop, restart, adoption and status can affect the wrong workload. |
| DEP2 | P1 | Cluster stop sends only AgentCommand::Stop. It never proposes the existing AppDelete Raft request, so the desired app remains and reconciliation can resurrect it. A leader without a local replica can return 404 instead of deleting cluster state. |
| DEP3 | P1 | Placement becomes applied when the command enters the queue; deploy events are discarded. Stop discards its response and erases bookkeeping immediately. The applied map is process-local, so a restart forgets work it must reconcile. |
| DEP4 | P1 | Image preparation, init polling, runtime operations and rolling health waits run serially in the central agent task. One slow deploy delays health checks, restarts and every later command. |
| DEP5 | P1 | Rolling deployment bypasses egress programming and does not use Wrapper's drain tracker. max_unavailable, surge, drain_timeout and automatic rollback do not reliably govern the live path. **(Re-validation inconclusive — the rolling path needs a closer read; treat the egress-skip half as confirmed via NET6, the drain/surge half as unverified.)** |
| DEP6 | P2 | Ordinary stop can report Stopped without waiting for exit and escalating. Container state and supervisor state can diverge. |
| DEP7 | P1 | Config::validate and apply handle apps/jobs but ignore or partially ignore builds, namespaces and permissions. Quota and build-namespace helpers have no production caller. Parsed policy is not desired state. |
| DEP8 | P1 | **(Re-validated: partly overstated.)** min>max is clamped, not a panic, and the metric query includes the namespace. The real defects stand: cooldown is checked before the Raft write commits, stale overrides survive a baseline change/deletion, and per-app metrics are normally absent (OBS3) so scaling is usually inert. |
| DEP9 | P3 | Numeric size/resource parsing can overflow rather than returning a validation error. |

### Batch and distributed build

| ID | Priority | Finding and evidence |
|---|---:|---|
| JOB1 | P0 | POST /v1/batch/run accepts a full JobSpec and caller-controlled callback_base_url. Completion sends the cluster service token to that URL. Missing route-level authorisation means a ReadOnly token, or an anonymous caller during bootstrap, can execute work and steal the Admin-equivalent token. |
| JOB2 | P0 | Build run accepts caller-controlled registry_port and context_digest. The digest becomes part of a temp path without strict OCI parsing; ../ can escape the build directory. Bun fetches a caller-selected localhost service and unpacks its tar as a privileged process. |
| JOB3 | P1 | Batch has two namespace sources. The CLI can deploy in one namespace while the tracker watches another for an hour. Non-success dispatch/callback responses are ignored, unreachable jobs can stay pending forever, report accepts forged/unknown state, unschedulable jobs are omitted and callback delivery has no durable retry. |
| JOB4 | P1 | Batch/build trackers and IDs are memory-only and are lost or reused after leader restart; garbage collection is not driven. HashMap iteration makes some batch allocation non-deterministic. |
| JOB5 | P2 | **(Re-validated: NOT reproduced as written.)** The delegated builder fetches the context from a per-node registry address, not a hardcoded localhost, so the "cross-node build can't find the context" claim did not hold on re-check. The residual concern — that a locally-uploaded `_buildcontext` is neither replicated nor catalogued — is worth confirming under Theme 11, but this is not the P1 the draft described. |
| JOB6 | P1 | Build temp directories are digest-derived, shared by concurrent builds and never reliably cleaned. Response bodies are buffered, sparse/oversized archives are unbounded, Dockerfile paths can escape the intended context, timeout does not guarantee the Buildah process is killed, and CLI polling has no terminal bound/cancellation. |
| JOB7 | P1 | Required signing is best effort. The build path creates an ephemeral external key that no policy trusts; AttachSignature can no-op for an unknown digest while the build still reports success. |

### API, browser and internal-principal security

| ID | Priority | Finding and evidence |
|---|---:|---|
| AUTH1 | P1 | AuthContext carries app/namespace scopes, but authorize checks only role. A scoped Deployer can mutate every namespace. |
| AUTH2 | P1 | Several mutation/internal handlers do not enforce the correct role: snapshot mutation, rollback, batch submit/run/report and build submit/run are representative examples. Internal run/report routes need node identity, not an ordinary user role. |
| AUTH3 | P1 | The protected router deliberately accepts everything while no user tokens exist. A non-loopback bootstrap bind is therefore anonymous Admin access unless a separate boundary happens to contain it. (Stage 5 added a session-cookie path but did **not** change this bootstrap fail-open.) |
| AUTH4 | P1 | The shared service token derives from the cluster master key and maps to unrestricted Admin on every protected route. Compromising one worker grants cluster-wide token, secret, upgrade and chaos privileges. |
| AUTH5 | P1 | Bearer verification performs Argon2 synchronously while holding the token-store read lock and tries hashes across the list. Invalid anonymous requests can block Tokio workers and multiply memory use by token count. |
| AUTH6 | ~~P1~~ | **FIXED by Stage 5** — `/` and `/ui/*` are behind auth; a read-only `HttpOnly` session cookie (`sesame::session`) carries HTMX/metrics/SSE. Env values were already masked. Kept only for the remaining real-dashboard-data (AUTH7) and attribute-XSS (AUTH8) parts. |
| AUTH7 | P2 | Dashboard and node detail return hard-coded cluster name, node count/state or empty node lists instead of authoritative data. |
| AUTH8 | P1 | Single-quoted chart attributes escape neither apostrophes nor the full attribute context. An app/label value can break the attribute and create an injection/XSS sink. |

### PKI, join, workload identity and secrets

| ID | Priority | Finding and evidence |
|---|---:|---|
| PKI1 | ~~P1~~ | **FIXED by Stage 5** — Bun loads the identity, builds both TLS configs, wraps the Raft/reporting/API listeners and refreshes `CrlHandle`; `require_mtls` drives the mode matrix (`src/bin/bun.rs`, `src/cluster/runtime.rs`). |
| PKI2 | ~~P1~~ | **FIXED by Stage 5** — the client-cert verifier is anchored on the Node CA (not the Root), so a workload cert can't authenticate as a node (`src/sesame/mtls.rs`). |
| PKI3 | P1 | The client verifier skips server-name/expected-node identity binding. Any valid node certificate can impersonate the leader or another peer after one node is compromised. Node certificates do carry node-id SANs despite contrary comments. **(Still open after Stage 5 — `PinnedChainServerVerifier` deliberately skips the hostname/node-id check.)** |
| PKI4 | P1 | 8310d63 fixes the discarded-identity dead end: the joiner receives and persists a bundle, with optional root-fingerprint pinning. The issuer still generates and returns the private key, default operation is TOFU, and token validation/consumption is not atomic. A CSR-based join should keep the private key on the joining node. |
| PKI5 | P1 | Join reconstructs a Node CA issuer name that differs from the stored intermediate subject. Signature-only tests miss normal path-building failure. Token validation/consumption and serial allocation are not one atomic Raft operation, so concurrent use can duplicate issuance. |
| PKI6 | P1 | Workload certificate validity is truncated to calendar dates; a one-hour certificate usually has equal midnight not-before/not-after values. CSR validation checks that the expected SPIFFE URI exists but signs attacker-supplied extra SANs unchanged. |
| PKI7 | P1 | Workload identity uses an app-scoped durable directory, then writes another identity subdirectory. Replicas overwrite the same key/JWT, rolling replacement drops tracking and adopted workloads restore identity=None. It needs per-instance tmpfs, ownership, cleanup and restart-safe rotation. |
| PKI8 | P0 | Secret rotation appends a new key after marking the old one read-only, while selectors return the first key. Finalisation deletes old keys without re-encrypting existing ciphertext. A restart can make every encrypted app secret undecryptable. Malformed rotation JSON currently falls into a rotation request instead of failing. |
| PKI9 | P1 | Bootstrap/security artefacts are written with ordinary fs::write; the master key is chmodded, but the bootstrap and sealed-root artefacts can be created too broadly. Identity temp files use a predictable name and may be world-readable before chmod; the directory and five-file bundle are not transactionally installed or validated on load. |
| PKI10 | P2 | OIDC verification omits issuer, audience, algorithm/kid and issued-at constraints and has no production authentication path. Join-token comparison is not constant-time. |

### Service discovery, DNS and network enforcement

| ID | Priority | Finding and evidence |
|---|---:|---|
| NET1 | P1 | ServiceMap uses a bare app-name key and local instances only. Namespace collisions overwrite identity; a node with no local replica has no remote backend for DNS, eBPF or ingress. |
| NET2 | P1 | VIP allocation hashes into a fixed range without collision/exhaustion handling. runc container address allocation wraps without safe release/exhaustion semantics. |
| NET3 | P1 | DNS defaults to 127.0.0.53, which runc containers cannot reach through their network namespace. Bun writes it to resolv.conf anyway and starts the bind as a detached best-effort task. |
| NET4 | P1 | Userspace DNS returns 127.128/16 VIPs, but only the eBPF connect hook translates them. With eBPF disabled, the default, resolution succeeds to an unusable loopback address. |
| NET5 | P0 | Sesame builds namespace firewall entries, but production never writes firewall_map or cgroup_namespace_map. The kernel checks isolation only when the source map entry exists, so empty maps permit every cross-namespace connection. |
| NET6 | P0 | Egress is applied after fresh workload start and not at all during rolling redeploy. Missing PID/cgroup, DNS and map errors only warn; required policy therefore fails open. Stop removes only the enabled flag and leaves stale destinations that can attach to a reused cgroup inode. |
| NET7 | P1 | The policy hook is connect4 only and egress parsing discards IPv6. A dual-stack workload bypasses the entire allowlist over IPv6. CIDR entries are also rejected rather than enforced. |
| NET8 | P1 | BPF map access unwraps missing maps and discards update/remove errors. nftables interpolates administrator CIDRs; the perimeter is IPv4-only, trusts node IPs for all host services and invokes nft without a timeout. |
| NET9 | P2 | A non-loopback DNS bind can become an open recursive reflector. UDP truncation has no TCP listener, so large answers retry against a closed port. |

### Ingress

| ID | Priority | Finding and evidence |
|---|---:|---|
| ING1 | P1 | IngressSpec.tls never reaches PathRoute. HTTP and HTTPS use the same router, so routes configured for TLS are served in plaintext. Automatic ACME/cluster-CA issuance remains absent. |
| ING2 | P1 | TLS handshakes spawn without a semaphore/deadline. Connection accounting covers only one HTTP handler and releases a WebSocket permit after the 101 response while the splice remains alive. |
| ING3 | P1 | Requests are buffered up to 10 MiB and backend responses are collected without a bound. An unauthenticated backend can exhaust memory; streaming, SSE and gRPC do not meet the documented contract. |
| ING4 | P1 | The drain tracker is library-only. Rolling deployment kills old instances directly, so active HTTP/WebSocket traffic is not honoured through drain_timeout. |
| ING5 | P2 | Client X-Forwarded values are forwarded rather than replaced for untrusted peers. /api matches /apievil, equal routes depend on HashMap order, rate buckets are shared across routes and zero/overflow rates are unvalidated. IPv6 Host parsing is also incorrect. |

### Image trust and Pickle

| ID | Priority | Finding and evidence |
|---|---:|---|
| IMG1 | P1 | Scheduler trust lookup strips an OCI reference to basename/tag. A nested local repository can miss policy, while an unrelated external image can be checked against a signed local basename. Resolve a canonical digest and pass that immutable identity to the runtime. |
| IMG2 | P1 | Nodes without a local Council handle silently skip require_signatures, including non-voter workers and standalone nodes. Policy must use an authoritative projection and fail closed. |
| IMG3 | P1 | Keyless verification checks leaf-to-first and last-to-root but not every intermediate link. **Stage 5 added the CRL/revocation check (L17)**; it still omits validity, EKU, issuer and SPIFFE/OIDC identity binding, and the every-intermediate link. |
| REG1 | P0 | **(Re-validated CONFIRMED.)** `manifest_put` stores the raw manifest as a content-addressed blob (`write_blob`, src/pickle/api.rs), so it is in GC's swept set (`list_blobs`), but `ImageManifest::all_digests()` returns only `[config, …layers]` — never the manifest's own digest — and GC (src/pickle/gc.rs) protects only `all_digests()`. A tagged manifest's own blob is therefore orphaned and can be deleted after grace, leaving the catalogue pointing at a 404. |
| REG2 | P1 | The registry has two unsynchronised catalogues: some writes propose to Council, while tag GET/list read node-local state. Remote commits do not populate that object and a non-council push can remain local. |
| REG3 | P1 | Manifest PUT can return 201 for invalid JSON, invalid config/layer descriptors or missing referenced blobs. The misleading missing-layer test currently asserts Created, not rejection. |
| REG4 | P1 | Registry routes are unauthenticated/plaintext, buffer up to 512 MiB per request, perform synchronous whole-blob I/O/hashing and have no principal/repository quota, aggregate upload limit or expiry. |
| REG5 | P1 | Blob/catalogue persistence writes directly to final or predictable temp paths without a complete fsync/rename transaction. Cache hits trust existence without digest revalidation; shared tag rootfs generations can delete/re-extract each other. |
| REG6 | P1 | Peer replication accepts arbitrary absolute redirect destinations, allowing a compromised peer to make another node PUT bytes to an attacker URL. Peer body reads are unbounded/outside the timeout. |
| REG7 | P1 | Redundancy can report success with zero peers, and GC approval is not revalidated against catalogue references immediately before deletion. Push success does not match the documented synchronous commit/replica guarantee. |
| REG8 | P2 | One-segment routes do not support normal team/app repository names even though config and trust-policy examples do. Upload sessions never expire. |

### Metrics, logs and integrations

| ID | Priority | Finding and evidence |
|---|---:|---|
| OBS1 | P0 | Metrics/app and rollup metric names are interpolated into DataFusion SQL. A crafted name can remove namespace/time predicates and trigger expensive cross-tenant queries. |
| OBS2 | P1 | Production rollups are ingested into an unbounded Vec, never flushed/pruned and lost on restart. File counters are not recovered. Windows are not epoch-aligned and ingestion has no node/window idempotency, so reassignment double-counts. |
| OBS3 | P1 | Production collects node metrics only. Process/app metrics and Prometheus scraping have no live caller, so autoscaling normally has no app-labelled signal. Cluster/app fan-out helpers are also not used by the advertised endpoints. |
| OBS4 | P1 | Missing telemetry is treated as not breaching and can resolve a firing alert. Slack/PagerDuty destinations receive the generic payload instead of provider contracts. Zero interval values can panic Tokio tasks. |
| OBS5 | P1 | Metrics and logs materialise whole archive history, write final Parquet files synchronously and hold async locks across expensive work. A partial/corrupt file can break every query. Raw log SQL has no table, row, time or memory guardrail. |
| OBS6 | P1 | Log fan-out concatenates unencoded values and turns HTTP/JSON/task failures into empty success. Merge identity is only adjacent timestamp/line, so valid events from two replicas can be deleted while separated duplicates survive. |
| OBS7 | P1 | Log export is still filesystem copy despite s3:// and gs:// configuration. Relish maintains a second checkpoint behind Bun's back. Reused filenames after retention can be skipped forever by the checkpoint, and shutdown does not force a final metrics/log flush. |
| OBS8 | P2 | Bun constructs and drops the legacy KetchupStore while APIs use LogStore. Its calendar/index code and logs.max_file_size_mb configure neither live path. Dead and duplicate storage should be removed or deliberately consolidated. |

### GitOps

| ID | Priority | Finding and evidence |
|---|---:|---|
| GIT1 | P1 | Runner advances last_applied after PartialSuccess, skipped generic resources and ignored Raft write failures. The repository can permanently report convergence to a commit it never applied. |
| GIT2 | P1 | Diff keys apps by bare name, defaults namespaces in one path and re-adds every current job on each sync. Duplicate names across namespaces and jobs never converge correctly. |
| GIT3 | P1 | WebhookValidator implements HMAC, replay and rate checks but has no production caller. The live endpoint ignores the signature/body and sits behind bearer auth that ordinary Git providers cannot supply. |
| GIT4 | P2 | Existing clones are reused without verifying remote/branch changes, Git paths lack an option terminator, duplicate resource files overwrite by HashMap order, backoff/coordinator helpers are unused and error results are not durable. |

### Runtime, fault injection and self-upgrade

| ID | Priority | Finding and evidence |
|---|---:|---|
| RUN1 | P1 | Bun constructs Supervisor with the permissive default ProcessWorkloadsConfig. An empty allowlist allows everything; host exec/script and mount isolation are not enforced before runtime creation. |
| RUN2 | P1 | Rootless mode drops resource limits and does not call its slirp helpers. GPU detection is a stub, gpu_enabled is ineffective and Apple adoption remains unfinished. Unsupported capabilities should fail closed rather than silently weaken a spec. |
| CHAOS1 | P1 | Memory pressure, disk I/O throttle, node drain and node kill can report success without applying an effect. CPU stress runs in Bun's cgroup rather than the target workload. Clear/expiry does not reverse every persistent effect or resume paused workloads. |
| UPG1 | P1 | Scheduler cordoning is a tested helper with no production call. quorum_headroom_ok counts configured voters rather than live availability, so an upgrade can remove the vote that keeps quorum. |
| UPG2 | P1 | Upgrade requests trust client-supplied roles/addresses, post-upgrade verification does not prove gossip rejoin, Apple/rootless adoption is incomplete and the progress/book leadership-transfer description differs from the in-place implementation. |

## Design and documentation divergence

The discrepancy register remains materially current:

- D1-D5 and D7-D13 are live product gaps mapped above.
- D6 is a deliberate implementation change: DNS is a userspace responder, not
  an in-kernel DNS server. The whitepaper/design must describe the code.
- D14 remains Phase 13. The no-args TUI must not appear as delivered or be
  duplicated in Phase 12b.
- D15, D17 and D19 remain incomplete platform/UI claims.
- D16 is fixed by managed-volume wiring.
- D18 has an execution pipeline now, but batch/build security and durability
  are not complete.
- D20 needs both code and prose work: upgrade claims leadership transfer and
  convergence that the current in-place path does not prove.
- D21 remains a recovery gap.
- D22 remains broad document drift around containerd/Grill, bincode/JSON,
  ingress topology and userspace DNS.

The security book additions (chapters 4 and 10) that described Bun loading
identity, joiners writing it and a CRL refresh ticker are now **true** —
Stage 5 (PR #77) made them so. One correction landed with that work: the book
had claimed node certificates carry no SANs, whereas they carry a node-id CN
SAN; the prose was fixed and per-node-id binding recorded as the open PKI3
follow-up.

Several old progress checkboxes describe helpers rather than behaviour. The
most consequential examples are node labels, reconstruction corrections,
Smoker resource/node faults, rootless networking, object-store log export,
GitOps atomicity and upgrade leadership transfer.

## Phase 12b: PR-sized remediation themes

The findings above map to 22 themes. A theme is intended to land as one
reviewable PR with tests written first and the relevant book chapter updated
in the same PR.

### Stop the bleeding

1. **Internal API trust boundary.** Role/scope guards for batch/build and all
   mutation routes; node-identity-only run/report endpoints; server-owned
   callbacks; no service-token disclosure; strict build digest/path/archive
   sandboxing.
2. **Secret and workload-identity safety.** Generation-aware secret rotation
   with re-encryption acknowledgements; exact certificate validity; server-
   derived SANs; per-instance tmpfs identity and restart-safe cleanup.
3. **Pickle reference integrity.** Raw manifest ownership through GC and
   replication; descriptor/blob validation before commit; canonical repository
   and immutable digest identity.
4. **Network policy enforcement.** Production namespace-firewall writers;
   policy before process start and on redeploy; IPv6/CIDR support; fail-closed
   map reconciliation and complete cleanup.
5. **Consensus persistence safety.** Versioned/checksummed snapshots, strict
   index matching, propagated durable-store errors and a hard startup failure
   when compacted state cannot be reconstructed.

### Make the cluster converge

6. **Control-plane directory and reporting.** Leader API/reporting discovery
   for non-voters, stable identities/parent assignment, stale eviction,
   terminal-state filtering, closed-channel handling and supervised tasks.
7. **Council membership self-healing.** Learner, catch-up, promotion and
   leader-safe joint-consensus removal of dead/unsuitable voters.
8. **Council disaster recovery.** Full-council recovery, encrypted external
   backup/restore, reconstruction threshold semantics and disk-pressure
   resignation.
9. **Scheduler truth and autoscaling.** Authenticated node labels/resources,
   one reservation cache per pass, generation/eligibility validation, daemon
   convergence, quotas, autoscale validation/window/override semantics and
   upgrade cordon input.
10. **Transactional desired state and deployments.** Namespaced instance
    identity, AppDelete on cluster stop, count/generation-aware reconstruction,
    terminal deploy/stop outcomes, retry/reschedule and work outside the
    central Bun loop.
11. **Durable batch and build execution.** One namespace source, durable
    trackers/IDs, bounded dispatch/callback retry, truthful unschedulable and
    timeout state, transferable build context, isolated tempdirs, killed
    timeouts and signing as a required terminal result.

### Secure every boundary

12. **API authorisation and Brioche.** Central role/scope matrix, fail-closed
    bootstrap, per-node internal capabilities, bounded off-runtime Argon2,
    real dashboard data and context-correct HTML encoding. (Authenticated
    read-only browser sessions + route lockdown + env redaction already landed
    with Stage 5, closing AUTH6/H6.)
13. **Node PKI, join and mTLS.** **The listener half is done (Stage 5):** Bun
    loads the identity, runs mTLS on Raft/reporting/API with a Node-CA-pinned
    client verifier and a Raft-refreshed CRL, behind the `require_mtls` matrix.
    What remains: a CSR-based atomic join that keeps the key on the joiner,
    expected-peer node-id binding, transactional/secure bundle install, and
    per-connection deadlines/frame bounds. (Registry auth/TLS is Theme 17.)
14. **Image trust policy.** Canonical OCI references, policy projection to
    workers/standalone mode, complete WebPKI/keyless validation, OIDC
    issuer/audience/identity binding and persistent usable build signing.

### Finish the data plane

15. **Global namespaced service catalogue and DNS.** ServiceId across Onion,
    DNS, eBPF, Wrapper and firewall; collision-safe VIP/IP lifecycle; replicated
    healthy endpoints; a container-reachable resolver; TCP/ACL hardening and
    either a portable non-eBPF path or explicit fail-fast dependency.
16. **Ingress transport and draining.** Honour per-route TLS and ACME/CA
    policy; HTTP redirects; streaming proxy bodies; lifetime connection
    permits/timeouts; trusted proxy headers; deterministic route/rate keys and
    deployment drain integration.
17. **Pickle storage and replication durability.** Authenticated streaming
    uploads, quotas/expiry, atomic fsync/rename, cache re-verification,
    same-origin redirects, bounded peer reads, acknowledged durability,
    last-moment GC revalidation and multi-segment repositories.

### Make automation and observability truthful

18. **Metrics, logs and object storage.** Parameterised/bounded DataFusion
    queries, durable aligned rollups, per-app collection, honest fan-out,
    stable event identity, asynchronous flush/compaction, provider webhook
    payloads, real S3/GCS export, one Bun-owned checkpoint and final shutdown
    flush.
19. **GitOps convergence and webhook security.** HMAC/replay/rate validation,
    namespace-aware deterministic diff, unified resource apply, no commit
    advancement after skip/failure, stable job semantics and safe/revalidated
    Git operations.

### Close platform and source-of-truth gaps

20. **Process workloads and platform capabilities.** Default-deny host
    exec/script, allowlist and mount isolation, rootless networking/resources,
    GPU detection/device isolation and explicit errors for unsupported runtime
    capabilities.
21. **Smoker and self-upgrade convergence.** Real target-cgroup fault effects
    with reversible cleanup; upgrade cordon; live-quorum checks; server-derived
    node metadata; gossip rejoin and Apple/rootless adoption.
22. **Documentation and book truth pass.** Correct deliberate design changes,
    qualify unimplemented claims, remove contradictory historical designs and
    update each affected chapter. X6 remains in Phase 13.

## Acceptance gates

Every Phase 12b theme needs a binary-driven regression test. The minimum
cross-theme gates are:

- an 8+ node cluster schedules, stops and reschedules a workload on a
  non-council worker through leader failover;
- voter loss triggers learner-first replacement without losing quorum;
- compact, corrupt, restart fails closed instead of booting empty;
- an endpoint/role/scope matrix proves ReadOnly, Deployer, Admin and node
  principals cannot cross their boundaries;
- valid, foreign-CA, workload-CA, wrong-node, expired and revoked certificates
  exercise real API/Raft/reporting/registry handshakes;
- same-name apps in two namespaces deploy, resolve, roll and stop independently;
- Linux tests prove namespace and egress policy deny by default on fresh and
  rolling deployment, including IPv6;
- push, GC, leader loss and peer pull preserve a tagged OCI image;
- delegated build from a non-builder entry node succeeds without trusting
  caller paths/ports;
- batch leader restart, lost callback, duplicate report and unreachable worker
  reach durable terminal state;
- GitOps partial apply never advances last_applied;
- metrics/log injection, long-history query, duplicate merge and partial-node
  failure stay bounded and truthful;
- every advertised Smoker fault produces a measurable target effect and clear
  removes it; and
- cargo fmt, clippy with warnings denied, the full default suite and the
  Linux/macOS gated suites all pass.

## Complete mapping of the former backlog

| Old item | Resolution |
|---|---|
| C5(b) mTLS | **Raft/reporting/API listeners done (Stage 5, PR #77)**; registry TLS/auth (Theme 17), atomic CSR-based join + expected-peer binding (Theme 13) remain |
| L17 CRL | **Keyless image-signature CRL done (Stage 5)**; peer-TLS revocation refresh also done; the remaining keyless validity/EKU/issuer/SPIFFE checks are Theme 14 |
| X6 TUI | Phase 13 only |
| M3 blocking blob/storage work | Themes 17-18 |
| M4 adjacent-only dedup | Theme 18 |
| M5/M6 VIP and namespace collision | Theme 15 |
| M7 non-job crash monitoring | Fixed |
| M19 provider webhook payloads | Theme 18 |
| M20 object-store log export | Theme 18, still open |
| M21 managed-volume preparation | Fixed |
| M22 rootless limits/network | Theme 20 |
| M23 process policy | Theme 20 |
| M24 calendar calculation | Theme 18 or remove the dead legacy store |
| X8 competing logs-export checkpoint | Theme 18 |

The 24 earlier Low findings are not hidden in one miscellaneous sweep:

| Low finding | Theme |
|---|---:|
| Cached blob accepted without digest recheck | 17 |
| Shared tag rootfs delete/re-extract | 17 |
| Numeric parse overflow | 9 |
| Container IP wrap/no safe release | 15 |
| Stop records completion too early | 10 |
| Reporting watch-close spin | 6 |
| Gossip transport-close spin | 6 |
| Weak DJB2 Raft ID | 6 |
| Batch HashMap allocation order | 11 |
| Reporting DefaultHasher stability | 6 |
| Aggregator never evicts departed nodes | 6 |
| Non-constant-time join-token compare | 13 |
| OIDC issuer/audience omitted | 14 |
| Keyless validity/SPIFFE/chain omissions | 14 |
| Manifest accepts missing referenced blobs | 3 |
| Upload sessions never expire | 17 |
| Fan-out values are not URL-encoded | 18 |
| Ketchup fan-out hides node failures | 18 |
| Single-quoted chart attribute escaping | 12 |
| Missing BPF maps can panic | 4 |
| nftables CIDR interpolation | 4 |
| Git option/argument injection | 19 |
| IPv6 Host parsing | 16 |
| GitOps re-adds jobs every sync | 19 |

Nothing from the removed section is silently dropped. Fixed items are recorded
as fixed, TUI stays in its roadmap phase, and every open item has one Phase
12b owner.
