# Design and implementation discrepancies

**Review baseline:** 32ef8822c184 (9 July 2026)  
**Compared:** [the whitepaper](../whitepaper.md), the component documents in
[docs/design](../design/), and the executable Bun/Relish paths.

This is a discrepancy register, not a second roadmap. It asks a narrower
question: if an operator reads the whitepaper or a design document today, can
the shipped binary actually provide the behaviour described? The answer is
often “the building blocks exist, but the production wiring stops short”.
That distinction matters: a well-tested library is useful, but it is not a
cluster feature until Bun starts it and handles its failures.

The implementation is in much better shape than an early-phase prototype. The
single-node agent, durable Raft state, HMAC-protected gossip when a master key
is configured, eBPF and DNS opt-ins, registry catalog persistence/GC, log
forwarding, cluster placement, GitOps, metrics rollups, and self-upgrade all
have real runtime paths. This document does not repeat items that have been
explicitly deferred to a later phase and accurately labelled as such in
[progress.md](../progress.md).

It does record where the whitepaper/design contract remains ahead of the
running system, where an existing implementation violates a stated invariant,
or where the documents describe superseded internals. “Priority” is impact if
an operator relied on the documented behaviour, not a judgement about which
feature ought to be built next.

## Executive assessment

Reliaburger currently works best as a small, trusted cluster with a local
control plane. It does **not yet** meet the whitepaper's production claims for
a 2–200 node cluster, let alone its 10,000-node architectural headroom. The
largest reason is that the production control-plane discovery path depends on
local Raft metrics, which worker nodes outside the seven-voter council do not
receive. The closely related service catalogue and ingress table are also
node-local.

The most serious discrepancies are therefore not missing UI polish. They are:

1. workers beyond the council cannot reliably find the leader and converge
   desired placement;
2. service discovery and ingress cannot route a service hosted on another
   node, and service identity is not namespace-safe;
3. transport security described as normal cluster operation is still optional
   or absent on the Raft, API, reporting, and registry listeners;
4. the deployment controller described in the design is not the controller
   which the binary uses; and
5. several documents still read as an implementation specification for
   containerd/bincode/in-kernel DNS, whereas the code intentionally uses
   runc-compatible Grill paths, JSON Raft payloads, and a userspace resolver.

## Status legend

| Status | Meaning |
|---|---|
| **Breach** | A current document promises an operational behaviour which the binary does not provide, or code violates an explicit invariant. |
| **Incomplete target** | The design describes an intentional end state; a real subsystem is present, but material production wiring/semantics are absent. |
| **Document drift** | The code deliberately took a different, reasonable path. The document needs updating so it stops prescribing a false implementation. |
| **Deferred scope** | Explicitly deferred work. It remains a gap against the whitepaper, but should not be reported as an undisclosed code bug. |

## Register

| ID | Priority | Status | Discrepancy | Evidence |
|---|---:|---|---|---|
| D1 | P0 | Breach | Non-council workers cannot discover the Raft leader, so placement reconciliation and reporting do not cover clusters larger than the council. | [Whitepaper §8.1](../whitepaper.md#81-two-layer-architecture); src/cluster/orchestrate.rs:400-418; src/cluster/runtime.rs:1-13,358-394 |
| D2 | P0 | Breach | The council reconciler only adds voters; it never removes dead or unsuitable voters, contrary to the dynamic self-healing council. | [Whitepaper §8](../whitepaper.md#8-leader-election--state-management); src/cluster/runtime.rs:439-480,505-555 |
| D3 | P0 | Breach | Onion's authoritative service map and Wrapper routes are node-local; identically named apps in two namespaces collide. | [Whitepaper §§9–10](../whitepaper.md#9-networking); src/onion/service_map.rs:20-64; src/bun/agent.rs:375-380,2776-2796,3407-3417 |
| D4 | P0 | Incomplete target | Normal cluster traffic is not mutually authenticated or encrypted. Raft is raw TCP, reporting is raw TCP, the node API has no mTLS, and Pickle's OCI listener has no auth/TLS layer. | [Whitepaper §11](../whitepaper.md#11-security); [Sesame design](../design/security-sesame.md); src/cluster/runtime.rs:26,225,269-304; src/pickle/api.rs:82-99; src/bin/bun.rs:995-1001 |
| D5 | P1 | Breach | Service discovery promises namespace/service correctness and a kernel data plane; the map has one global string key and VIP collisions are not detected. | [Whitepaper §10](../whitepaper.md#10-service-discovery-onion); [Onion design](../design/discovery-onion.md); src/onion/service_map.rs:20-64; src/onion/vip.rs |
| D6 | P1 | Document drift | The whitepaper says eBPF answers DNS queries in-kernel with no DNS process. The implementation intentionally uses a userspace UDP resolver and rewrites container resolv.conf. | [Whitepaper §10](../whitepaper.md#10-service-discovery-onion); src/onion/dns.rs:1-17,53-99; src/bin/bun.rs:540-560 |
| D7 | P1 | Incomplete target | Wrapper does not supply automatic ACME or cluster-CA certificates, and its routes only reflect local instances. A self-signed localhost certificate is not the advertised production ingress. | [Whitepaper §9.2](../whitepaper.md#92-built-in-ingress-wrapper); [Wrapper design](../design/ingress-wrapper.md); src/wrapper/tls.rs:1-37; src/bin/bun.rs:563-587; src/bun/agent.rs:3407-3417 |
| D8 | P1 | Incomplete target | Sesame authentication is role-only: namespace/app token scopes carried in AuthContext are not checked by authorize. A new cluster also permits every protected request until its first token is created. | [Sesame design](../design/security-sesame.md); src/sesame/auth.rs:145-207; src/bun/api.rs:253-258 |
| D9 | P1 | Deferred scope | CRL checking, certificate rotation/enforcement, encrypted Raft-at-rest, and secure listeners are designed but do not protect the normal network path. Workload identity depends on configured wrapping material and still lacks the design's complete lifecycle. | [Whitepaper §§11.1–11.2](../whitepaper.md#111-certificate-authority-hierarchy-sesame); [Sesame design](../design/security-sesame.md); src/cluster/runtime.rs:91-121,221-226; docs/progress.md:350-357 |
| D10 | P1 | Incomplete target | The production deployment path submits a fire-and-forget local deploy and marks an assignment applied once the channel accepts it. It does not use the durable, health-gated deploy controller/rollback state machine described in the deployments design. | [Deployments design](../design/deployments.md); src/cluster/orchestrate.rs:430-462; src/meat/orchestrator.rs:1-80 |
| D11 | P1 | Incomplete target | Pickle accepts a push before the advertised synchronous peer-replication durability guarantee. A failed Raft proposal is logged and tolerated; replication is reconciled later. P2P layer pulls are not a normal workload image path. | [Whitepaper §12](../whitepaper.md#12-built-in-image-registry-pickle); [Pickle design](../design/registry-pickle.md); src/pickle/api.rs:37-80; src/bin/bun.rs:1113-1210 |
| D12 | P1 | Breach | Lettuce runs only on the Raft leader rather than an independently elected coordinator, applies resources one by one rather than atomically, and silently skips jobs, namespaces, and permissions. | [Lettuce design §§1–3](../design/gitops-lettuce.md); src/lettuce/runner.rs:1-7,30-60,100-175 |
| D13 | P1 | Incomplete target | The observability design's per-app Prometheus collection, scalable hierarchy, PromQL, remote object storage, and complete UI are not all present in the live path. The current metrics/log stores are local Parquet/DataFusion stores; configured log export is filesystem-only. | [Mayo design](../design/metrics-mayo.md); [Ketchup design](../design/logs-ketchup.md); src/bin/bun.rs:603-731; src/mayo/store.rs:81-101; docs/progress.md:361-369 |
| D14 | P1 | Deferred scope | Relish is a command-only CLI. The no-arguments Ratatui TUI, event/trace views, and several documented operations are not shipped. | [Relish design §5](../design/cli-relish.md); src/bin/relish.rs:594-630; docs/progress.md:355-357,395-404 |
| D15 | P1 | Incomplete target | The whitepaper's GPU-first resource claim is not met: Bun's sole detector is a stub reporting no GPUs, so GPU placement and device isolation cannot work. | [Whitepaper §2](../whitepaper.md#2-why-reliaburger); [Bun design §5.4](../design/agent-bun.md); src/bun/gpu.rs:1-42 |
| D16 | P1 | Breach | The documented managed-volume lifecycle does not run in Bun. The helper can create/loop-mount a directory, but no production caller creates the host directory before Grill mounts it. | [Whitepaper §5](../whitepaper.md#5-core-concepts); [Bun design](../design/agent-bun.md); src/grill/volume.rs:24-75; docs/progress.md:370-371 |
| D17 | P1 | Breach | Process workload policy can be configured but Bun constructs the supervisor with the default, all-allowed configuration. A configuration deploy can therefore run an arbitrary host executable through ProcessGrill. | [Whitepaper §15](../whitepaper.md#15-process-workloads); [Bun design](../design/agent-bun.md); src/bun/supervisor.rs:80-111,135-151; src/config/node.rs:24-28; docs/progress.md:370-371 |
| D18 | P1 | Incomplete target | The high-throughput batch and distributed build designs are library/CLI shapes, not a complete cluster execution pipeline. The current build handler is synchronous and local when buildah is available; batch execution remains a Phase 12 item. | [Whitepaper §6](../whitepaper.md#6-jobs); [Meat design](../design/scheduler-meat.md); src/bin/relish.rs; docs/progress.md:380-387 |
| D19 | P2 | Breach | The full Brioche contract is ahead of the server. Node detail uses a hard-coded alive state and app listing; per-app charts lack per-process metrics. Public UI pages can expose plaintext environment values, while a browser has no corresponding token flow for protected metrics/log requests. | [Brioche design](../design/ui-brioche.md); src/bun/api.rs; src/brioche/node_detail.rs; docs/progress.md:221-229,361-369 |
| D20 | P2 | Document drift | The self-upgrade design describes reporting-tree directives, gossip version discovery, leadership transfer, and reverse-order rollback. The implementation deliberately uses authenticated HTTP, version polling, leader-last in-place upgrade, and leader-last rollback. | [Bun design §5.5](../design/agent-bun.md#55-self-upgrade-sequence); [upgrade plan](2026-07-06-plan-self-upgrade.md); src/upgrade/; docs/progress.md:409-424 |
| D21 | P2 | Incomplete target | Full-council loss recovery, external encrypted backups, automatic disk-pressure council resignation, and the whitepaper's reconstruction learning threshold are not in the production cluster runtime. | [Whitepaper §§8.2–8.3](../whitepaper.md#82-reconstructable-state); [Mustard design](../design/gossip-mustard.md); src/cluster/runtime.rs:1-13,439-555 |
| D22 | P2 | Document drift | Several documents still imply containerd, bincode Raft/reporting compatibility, and a separate/dedicated ingress arrangement. Current runtime uses the Grill runtime abstraction, self-describing JSON for Raft state, a userspace DNS server, and Bun's Tokio runtime. | [Whitepaper quick start](../whitepaper.md); [Bun design](../design/agent-bun.md); src/cluster/runtime.rs:1-39; docs/progress.md:302-310 |

The following sections give enough context to act on the entries without
turning this file into a replacement design document.

## D1 — The current control plane does not scale past the council

The whitepaper's two-layer architecture is sound: gossip carries compact
membership/resource information for every node, while a small Raft council
owns desired state. The running implementation instead derives the current
leader's API and reporting address solely from local Raft metrics. Its own
comment calls out the consequence: nodes outside the council “don't learn a
leader” and skip the placement pull (src/cluster/orchestrate.rs:400-418).
The reporting runtime makes the same intentional MVP choice: a flat star to
the leader, rather than workers → council → leader
(src/cluster/runtime.rs:1-13,358-394).

That makes the advertised 2–200-node practical range unsafe to claim. A
seven-member cluster can converge; an eighth node can retain an old workload
but cannot reliably fetch assignments or report capacity. It also makes the
autoscaler and cluster-wide observability incomplete exactly when scale makes
them useful.

**Resolution:** publish the leader's API/reporting endpoints through a
gossip-visible, authenticated control-plane record, then implement the
consistent-hash reporting parent assignment. Tests must deploy to a worker
outside a full council, fail over the leader, and show that the worker receives
a new assignment plus reports after the change.

## D2 — Council membership only grows

compute_desired_council() starts with current.clone() and only inserts
selected candidates. The following reconciler can add/promote learners but
has no removal/demotion branch (src/cluster/runtime.rs:439-555). Retaining
the leader avoids a known leadership-demotion failure, but permanently
retaining all voters is not a dynamic council selected for availability,
resources, and zone diversity. A dead voter can ultimately prevent quorum
recovery or consume the fixed seven seats.

**Resolution:** model a leader-safe membership transition: add a replacement
as learner, wait for catch-up, promote it, transfer/elect leadership where
necessary, then remove the unhealthy/undesired voter. Make the survivor set
and failure policy explicit in the gossip and council documents.

## D3–D7 — Network data-plane scope and ingress differ from the contract

ServiceMap is a HashMap keyed by a single app-name string. It rejects the
second registration for the same app name regardless of namespace and derives
the VIP from the app name alone (src/onion/service_map.rs:20-64). Bun mutates
this map as local instances start/stop, then builds its local Wrapper routing
table and eBPF backend map from it
(src/bun/agent.rs:2776-2796,3407-3417). Nothing carries the global backend
catalogue to a node which has no local instance. This contradicts both the
multi-node service-discovery story and Wrapper's promise that any ingress node
can route any application.

There is a second, deliberate difference. eBPF can rewrite socket destinations
but cannot construct a DNS response packet in the used hooks; the replacement
userspace resolver is well explained in src/onion/dns.rs:1-17 and is a
sensible engineering decision. The whitepaper must say so. It should also say
that DNS and eBPF are opt-ins requiring appropriate host privileges, rather
than presenting them as an invisible universal data plane.

Ingress starts only when ingress is enabled, and its current certificate is
self-signed for localhost/127.0.0.1 (src/wrapper/tls.rs:20-37). It cannot
supply the promised ACME or cluster CA behaviour. Calling that “automatic TLS”
in the whitepaper is materially misleading.

**Resolution:** make (namespace, service) the key throughout ServiceMap, DNS,
eBPF, ingress, firewall and API types; include collision handling in VIP
allocation. Replicate/derive a global backend catalogue from reporting or
durable desired/runtime state. Separately, either implement ACME/Ingress-CA
issuance or change the product claim to “TLS listener with operator-supplied
certificates”.

## D4, D8 and D9 — Security primitives exceed enforcement

The project has solid crypto building blocks, but the network boundary does
not yet use the described security model:

- serve_raft_rpc and TcpReportingTransport are TCP transports, not TLS
  transports (src/cluster/runtime.rs:26,225,269-304);
- Pickle constructs a bare OCI router and binds it directly
  (src/pickle/api.rs:82-99 and src/bin/bun.rs:995-1001);
- API Bearer authentication is attached to protected routes, but accepts every
  request while the user-token store is empty (src/sesame/auth.rs:171-183);
- authorize() checks only a role even though AuthContext includes app and
  namespace scopes (src/sesame/auth.rs:197-207); and
- the progress tracker correctly keeps mTLS and CRL enforcement in the
  deferred security backlog (docs/progress.md:350-357).

This is not a reason to remove the crypto code. It is a reason to introduce a
clear **secure cluster mode**. Once initialisation succeeds, it should require
authenticated API/registry access, mTLS peer identity on every internal
listener, certificate expiry/CRL verification, and a fail-closed policy. If
the bootstrap-open mode remains, bind it to loopback and make it an explicit
operator choice, not the default for externally reachable API addresses.

## D10 — Deploy status is admission, not successful reconciliation

The deployments design describes a persisted state machine with health-gated
steps and rollback. The cluster reconciler instead sends AgentCommand::Deploy
to a local channel, spawns a task to discard the event stream, and records the
assignment fingerprint immediately after that send succeeds
(src/cluster/orchestrate.rs:430-462). A failed image pull, port allocation, or
start can therefore look converged until some unrelated change alters the
fingerprint. The library controller in src/meat/orchestrator.rs cannot fix
this because it is not the controller used by this path.

**Resolution:** define an idempotent DeploymentAttempt identity in Raft (or
the authoritative desired/actual state), report terminal phase and health
back, and advance applied only after the requested generation reaches a
terminal success. Use the same controller for manual deploy, placement
reconciliation, rolling, blue-green and rollback; otherwise those semantics
will continue to diverge.

## D11 — Pickle's durability claim is asynchronous in the failure case

The design says a successful push synchronously replicates to N peers so it
survives a node loss. record_commit() applies/persists locally first. Its Raft
write is explicitly best effort; a failure logs an error but does not fail the
push, leaving a later replication loop to repair it
(src/pickle/api.rs:37-80). That is a reasonable availability choice, but it is
not synchronous durability and must be presented as such. The planned
multi-source P2P image path is also not what normally starts deployed
workloads.

**Resolution:** either wait for the catalog commit plus required peer layer
acknowledgements before returning a successful push, or return/record a
distinct “locally accepted, replication pending” status and downgrade the
durability claim. Registry authentication belongs in the same change set as
the secure listener work.

## D12 — GitOps is a useful runner, not yet the designed coordinator

spawn_gitops_sync() is leader-only (src/lettuce/runner.rs:30-60). The design
instead specifies an independently elected council coordinator, with failover
state stored in Raft. The runner also writes each change separately and says
in code that jobs, namespaces and permissions are skipped
(src/lettuce/runner.rs:100-175). Thus a commit can be partially applied yet be
marked as the last applied commit. That violates GitOps' most important
property: being able to explain exactly what state a commit produced.

**Resolution:** either simplify the design to make GitOps a leader duty and
state explicitly that applies are incremental, or implement the coordinator
state machine and a transaction/batch boundary. At minimum, do not advance the
commit marker when any desired change is skipped or fails, and surface a
machine-readable partial result in both API and Brioche.

## D13 and D19 — Observability works locally; product claims are broader

Bun creates a real MayoStore, a Parquet/DataFusion LogStore, forwards
container records into it, collects node metrics, and can export files
(src/bin/bun.rs:603-731). That is good useful functionality. It is not yet the
design's complete observability service:

- there is no production caller for the per-app Prometheus scrape path;
- the system has no PromQL implementation;
- object-store export is filesystem-only, so s3:// and gs:// are not cloud
  exports (docs/progress.md:361-371);
- cross-node deduplication only handles adjacent duplicates (an acknowledged
  open finding); and
- the UI still contains placeholder node and per-app information rather than
  the full dashboard contract.

Do not make a hidden performance claim here. MayoStore can re-query durable
Parquet, but the product needs retention, bounded query cost, and an explicit
cluster query model before it can make the scale claims in the whitepaper.

**Resolution:** choose a smaller, accurate near-term promise (“node metrics
and structured logs with cluster rollups”) or finish the query/export/UI
features as a coherent observability milestone. The documentation should not
present dashboard diagrams as currently operable screens until their data
routes are authenticated and populated.

## D14, D15–D18 — User-facing and workload claims need qualification

The Relish design documents a no-argument interactive TUI. Relish currently
parses a required subcommand (src/bin/relish.rs:594-630); Phase 13 accurately
tracks the missing UI. This is a deferred scope gap, not a hidden defect, but
the whitepaper should not demonstrate the TUI as an available interface.

Three workload gaps are more consequential:

- GPU scheduling cannot be “first class” while StubGpuDetector always returns
  an empty list (src/bun/gpu.rs:1-42).
- The VolumeManager is capable of setting up a managed volume, but Bun does
  not call it. Generated mounts can therefore reference a missing directory;
  the current tracker records this as M21 (docs/progress.md:370-371).
- NodeConfig carries process_workloads, but the production supervisor is built
  with ProcessWorkloadsConfig::default() rather than that configuration
  (src/bun/supervisor.rs:80-111). A declared allowlist does not constrain host
  process workloads.

Batch scheduling and build jobs similarly have algorithms/configuration, not
the fully distributed dispatch and completion lifecycle described by the
whitepaper. Keep them in the Phase 12 wording until end-to-end binary tests
can submit, schedule, execute, report, retry and query a batch/build.

## D20 — Self-upgrade is documented as two different systems

This is a healthy example of an intentional divergence which needs a single
source of truth. The Bun design now honestly documents the changes: directives
travel over token-authenticated HTTP, version discovery polls /v1/version, the
leader upgrades itself last without leadership transfer, and rollback uses the
same order (docs/design/agent-bun.md:966-992). The old sequence below it still
shows reporting-tree directives and a different order.

**Resolution:** replace the old sequence, rather than merely preceding it
with a caveat. Also close the outstanding cluster post-upgrade verification:
docs/progress.md:421-424 says it verifies adopted workloads but not explicit
gossip rejoin. That is the observable safety condition operators actually
need.

## D21 — Recovery promises have no executable counterpart

The whitepaper promises leader reconstruction with a 95%/15-second learning
period, pre-seeded catastrophic recovery candidates, backup/restore, and disk
pressure council step-down. The current cluster runtime begins gossip, Raft, a
flat reporting star and an add-only council reconciler; it contains no runtime
state machine for those recovery features
(src/cluster/runtime.rs:1-13,439-555). Normal Raft quorum loss is survivable
for already-running apps, but that is weaker than automatic recovery from
total council loss.

**Resolution:** mark §8.2–§8.3 as an architecture proposal with a phase
reference, or write the recovery protocol before retaining the production
availability claim. Disaster recovery deserves external black-box tests; a
unit test of candidate selection cannot establish a safe split-brain story.

## D22 — Treat prose as versioned implementation documentation

Some mismatch is inevitable in a fast-moving build, but three forms are
particularly confusing to a reader trying to learn from this project:

1. the whitepaper quick-start still names containerd, while the project
   implements its own Grill runtime abstraction and commonly uses runc;
2. the agent design describes bincode compatibility constraints although the
   implementation changed Raft payloads to self-describing JSON (recorded in
   docs/progress.md:302-310); and
3. the original in-kernel DNS description remains alongside the actual,
   deliberately userspace resolver.

Add a short “implemented in release X” box to each design chapter and move
superseded alternatives into a decision log. This codebase is also a Rust and
distributed-systems book: preserving an old design beside new code without a
clear status marker teaches the wrong lesson.

## Recommended order of work

1. **Make a worker outside the council a first-class node.** Publish leader
   endpoints through authenticated gossip, build the reporting tree, remove
   stale council voters safely, and prove placement/recovery across at least
   eight nodes.
2. **Establish one global, namespaced runtime catalogue.** Feed Onion,
   Wrapper, scheduling and status from it. This eliminates the most visible
   multi-node routing and identity failures at once.
3. **Ship a fail-closed secure-cluster mode.** mTLS for every inter-node and
   registry API, persisted node identity, scope checks, CRL/expiry checks,
   authenticated bootstrap, and no silent non-eBPF security mode.
4. **Unify deploy execution.** Replace channel-admission bookkeeping with the
   durable deployment controller and terminal generation results.
5. **Correct the documents now.** Label deferred capability clearly, replace
   obsolete internal designs, and do not use 10,000-node/automatic-TLS/GPU/TUI
   language as present tense until the corresponding end-to-end tests exist.
6. **Then finish the vertical features.** Registry synchronous durability,
   GitOps atomic/coordinator semantics, workload safety, recovery, and the
   observability/TUI surface can each become a coherent, testable phase.

## What currently matches well

For balance, these comparisons found several areas where the live path now
matches the intended direction:

- Bun is genuinely a single binary that starts the agent, cluster runtime,
  scheduler, placement reconciler, log/metrics collection, DNS/ingress opt-ins,
  registry work, GitOps loop and upgrade manager.
- Raft is durable rather than the old in-memory prototype, and gossip derives
  an HMAC key when cluster master material is configured.
- The eBPF and DNS paths are wired when enabled; the userspace DNS choice is
  clearly explained in source and is technically more honest than pretending
  packet synthesis is possible in the chosen BPF hooks.
- Pickle catalog persistence, a replication/GC loop, log forwarding, metrics
  rollups, GitOps polling/webhooks, and practical leader-last self-upgrade are
  real runtime features rather than library-only APIs.

Those are good foundations. The important next step is to align their failure
semantics, security boundary and documentation with the promise of a
production cluster.

