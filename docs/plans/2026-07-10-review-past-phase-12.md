# Past Phase 12: consolidated review and the road to a real cluster

_10 July 2026. Baseline: current `main` (post-Phase-12). Inputs: the
[codebase walkthrough](2026-07-09-review-codex.md) (findings H1–H8, M1–M5),
the [design discrepancy register](2026-07-09-review-design-discrepancies.md)
(D1–D22), and the "Beyond Phase 11b" backlog that used to live in
[progress.md](../progress.md) (C5(b), L17, X6, M3–M24, X8, and the ~24 Lows
from the [2 July review](2026-07-02-review-codebase.md))._

Three documents were tracking what's left. That's two too many. This write-up
merges them into one list, verifies each item against the code as of today,
and groups the survivors into themes sized for a single PR each. The result
is tracked as **Stage 5 — Multi-Node Correctness & Security** under Phase 11b
in [progress.md](../progress.md). Phases 13–15 (TUI, Self-Upgrade, Testing)
keep their numbering.

## Where we stand after Phase 12

Both 9 July reviews agree on the shape of the problem, and it's worth saying
plainly: the single-node story is in good shape, and the multi-node story is
not. Every subsystem exists and most are wired. What's missing is the
connective tissue that makes seven-plus nodes behave like one cluster: workers
outside the council can't find the leader, the service catalogue is
node-local, internal traffic is plaintext, and "deployed" means "the command
queue accepted it" rather than "it's running".

Phase 12 closed some of the backlog before this write-up existed. Verified
against the code today:

| Item | Was | Now |
|---|---|---|
| M7 | A crashed app with no health check stayed `Running` forever | Fixed: `check_apps()` polls exit state for non-job apps too (`src/bun/agent.rs:3395`) |
| M20 | Log export was filesystem-only; `s3://` treated as a local dir | Fixed: `object_store` built with `aws`/`gcp` features; `src/ketchup/remote_query.rs` handles `s3://`/`gs://` |
| M21 / codex M2 | Managed volume host dirs never created | Fixed: Phase 12 E0 creates them in `spawn_blocking`, deploy fails closed |
| Batch/build (part of codex M5) | `/v1/batch` returned not-wired; build was synchronous | Fixed: batch tracker + dispatch, async build runner with delegation |

Everything else from the three sources is still open. I re-checked the
suspicious ones rather than trusting the reviews: the service map is still
keyed by bare app name (`src/onion/service_map.rs:44` — namespace is stored
in the entry, but two namespaces with the same app name still collide), the
supervisor is still built with `ProcessWorkloadsConfig::default()`
(`src/bun/supervisor.rs:96`), token scopes are still read by nobody outside
`src/sesame/auth.rs`, and the alert webhook still sends one generic payload
no matter what the config URL points at.

## The consolidated open list

Sixteen themes. Each is one PR: a coherent change with its own tests, small
enough to review, big enough to actually close its findings. IDs refer to the
source reviews: H/M-codex from the [walkthrough](2026-07-09-review-codex.md),
D from the [discrepancy register](2026-07-09-review-design-discrepancies.md),
M/L/X/C5 from the [2 July review](2026-07-02-review-codebase.md) backlog.

### Cluster correctness (the blockers)

**1. Leader discovery beyond the council** (H1, D1). The placement
reconciler and reporting derive the leader solely from local Raft metrics
(`src/cluster/orchestrate.rs:402-413`), so node 8 of a 7-voter council never
fetches assignments. Publish the leader's API/reporting endpoints through a
gossip-visible, authenticated control-plane record. The test that matters: an
8+ node cluster schedules, runs and removes a workload on a non-council
worker, across a leader failover.

**2. Council self-healing** (H2, D2). `compute_desired_council` starts from
`current.clone()` and only ever adds (`src/cluster/runtime.rs:471`). Dead
voters stay forever and can cost quorum while healthy spares sit idle.
Separate "never remove the current leader in this change" from "never remove
a voter": add a learner, catch up, promote, then remove the dead voter via
joint consensus. Test quorum recovery after losing voters with spares
available.

**3. Namespaced global service catalogue** (H3, codex M1, D3, D5, D7-routes;
backlog M5, M6). Service identity is a bare app-name string and the VIP is a
bare hash into 65,534 slots with no collision check (`src/onion/vip.rs:37`).
Backends are only ever local, so a node without a replica can't resolve or
route the service. Introduce `ServiceId { namespace, name }` through Onion,
DNS, eBPF, Wrapper and the firewall; allocate VIPs collision-aware; replicate
service endpoints cluster-wide and feed local DNS/eBPF/ingress from a read
cache. Test: resolve and call an app with no replica on the calling node.

**4. Deployment controller correctness** (H7, D10, codex M3). The placement
reconciler marks an assignment applied the moment the command queue accepts
it and drains the event stream into the void (`src/cluster/orchestrate.rs:459`).
A failed image pull looks converged until the fingerprint happens to change.
Give deployment a terminal outcome, reconcile against `Supervisor` state,
retry with backoff, and report failures to the leader for rescheduling. While
in there, move deploy I/O (init polling, image work, rolling health waits)
off the agent event loop so a hung runtime doesn't stall health checks.

### Security enforcement (before any non-loopback deployment)

**5. AuthZ enforcement and bootstrap lockdown** (H4, D8, part of H5). Token
scopes are stored and never checked; a namespace-scoped token works
everywhere its role allows. And until the first token exists the API is
fail-open, so binding beyond loopback pre-bootstrap hands out admin. Put
scope checks in reusable extractors so new handlers can't forget them, and
refuse non-loopback binds until bootstrap completes (or require an explicit
one-time credential).

**6. Transport security** (H5, D4; deferred C5(b), L17). Raft, reporting,
the node API and the registry all speak plaintext TCP; the registry has no
auth at all despite cross-node replication needing a reachable bind. mTLS on
every internal listener, a registry auth layer, and CRL/expiry checks at
connect time. This is the Stage 3b deferral coming due.

**7. Dashboard security boundary** (H6, D19). Brioche routes are public and
render plain env values verbatim (`DATABASE_URL` and friends), while its
fetches to protected logs/metrics routes will 401 once auth is live. Either
authenticate the UI or make it an explicitly redacted status surface. Don't
render env values by default either way, and fix the hard-coded node-detail
data while in there.

**8. Process-workload policy enforcement** (H8, D17; backlog M23). The
allowlist and `mount_isolation` config parse and then do nothing: Bun builds
the supervisor with the default all-allowed config (`src/bun/supervisor.rs:96`).
Any deploy-capable principal can run arbitrary host commands through
ProcessGrill. Wire the configured policy in and enforce it before OCI spec
generation.

### Correctness under load and failure

**9. Observability storage engine** (codex M4, D13; backlog M3, M4). Every
Mayo/Ketchup query loads every Parquet file into memory; flushes write
synchronously under the store's write lock; Pickle hashes whole blobs on the
async runtime (`src/pickle/store.rs:83`); dedup only catches adjacent
duplicates (`src/ketchup/query.rs:18`). Use DataFusion's Parquet tables with
predicate pushdown, keep a bounded recent buffer, push blocking work to
`spawn_blocking`, and dedup across nodes properly.

**10. Observability and CLI small fixes** (backlog M19, M24, X8). Slack and
PagerDuty webhook formats are configured but never applied; `KetchupStore`'s
calendar maths can emit month 13; `relish logs-export` copies files behind
the agent's back, racing its export checkpoint. Three small fixes, one PR.

**11. GitOps atomicity** (D12). Lettuce applies resources one by one,
silently skips jobs, namespaces and permissions, and then advances the commit
marker anyway. That breaks the one property GitOps exists for: knowing what
state a commit produced. Don't advance the marker on a partial apply, and
surface a machine-readable partial result.

**12. Pickle durability semantics** (D11). A push returns success before the
Raft commit is durable; a failed proposal is logged and repaired later. Either
wait for the catalogue commit plus peer acks, or return an explicit "locally
accepted, replication pending" status and stop claiming synchronous
durability.

### Features and honesty

**13. Feature completeness or explicit re-scope** (D15; backlog M22; codex
M5 leftovers). The GPU detector is a stub that always reports none, rootless
mode silently drops resource limits (`src/grill/rootless.rs:97`) and
slirp4netns has no callers, Apple adoption is deferred, CIDR egress entries
are rejected. For each: implement it, or document it as unsupported next to
the feature claim. No third option.

**14. Recovery story** (D21). The whitepaper promises full-council-loss
recovery, encrypted backups and disk-pressure council resignation. None of it
has a runtime path. Build it, or downgrade whitepaper §8.2–8.3 to a clearly
labelled proposal with a phase reference. Disaster recovery needs black-box
tests, not a unit test of candidate selection.

**15. Docs truth pass** (D6, D20, D22; qualifiers from D14, D15, D18, D21).
The whitepaper still describes in-kernel DNS, containerd and bincode where
the code deliberately went userspace resolver, Grill and JSON. The old
self-upgrade sequence still sits beside the new one. Replace superseded
designs rather than caveating them, add an "implemented as of" marker per
design chapter, and move present-tense claims about the TUI, GPUs, 10,000
nodes and automatic TLS into future tense until the tests exist.

**16. Low-findings sweep** (the ~24 Lows, [2 July review §Low](2026-07-02-review-codebase.md)).
Blob-cache re-verify, `parse_num` overflow, weak `raft_id_from_name` hash,
non-constant-time join-token compare, `verify_jwt` skipping `aud`/`iss`,
keyless verify ignoring cert validity, `manifest_put` not checking referenced
blobs, non-expiring upload sessions, fan-out swallowing node failures, IPv6
Host mangling, git arg-injection, the diff engine re-adding jobs each sync,
and the rest. One batch PR, each fix with a test.

## Recommended order

Both reviews land on the same sequencing, and I agree with it:

1. **Themes 1–4 first.** These make multi-node operation *incorrect*, not
   merely incomplete. Until they're done, the honest claim is "small trusted
   cluster where every node is a voter".
2. **Themes 5–8 before any non-loopback deployment.** Scope checks, bootstrap
   lockdown, mTLS, and closing the two arbitrary-execution surfaces (process
   allowlist, open dashboard).
3. **Themes 9–12** make convergence and durability true under ordinary
   failure and slow I/O.
4. **Themes 13–16** finish or honestly re-scope the rest, then make the
   documents match the binary.

## Absorbed from "Beyond Phase 11b"

The progress.md section this write-up replaces listed every un-staged finding.
Mapping, so nothing is silently dropped:

| Old item | Where it went |
|---|---|
| C5(b) mTLS on Raft/API | Theme 6 |
| L17 CRL at connect time | Theme 6 |
| X6 no-args TUI | Not Stage 5 — remains Phase 13 ([plan](2026-07-06-plan-tui.md)) |
| M3 blob SHA on async runtime | Theme 9 |
| M4 adjacent-only dedup | Theme 9 |
| M5 VIP collisions | Theme 3 |
| M6 service map ignores namespace | Theme 3 |
| M7 crashed app stays Running | **Fixed in Phase 12** (`src/bun/agent.rs:3395`) |
| M19 webhook formats unapplied | Theme 10 |
| M20 log export filesystem-only | **Fixed in Phase 12** (object_store aws/gcp) |
| M21 managed volumes never created | **Fixed in Phase 12** (E0) |
| M22 rootless limits dropped | Theme 13 |
| M23 process allowlist unenforced | Theme 8 |
| M24 month-13 calendar maths | Theme 10 |
| X8 logs-export race | Theme 10 |
| ~24 Lows | Theme 16 |

The codex review's H1–H8 map to themes 1, 2, 3, 5, 6, 7, 4 and 8
respectively; its M1–M5 to themes 3, (fixed), 4, 9 and 13/15. The
discrepancy register's D-items are cited inline above; D9, D14, D18 and D20
are qualification/docs work inside themes 6, 15 and 13.

One more thing worth keeping in mind: this is also a book project. Several of
these themes (the global service catalogue, mTLS everywhere, the deployment
controller) are exactly the chapters readers will want. Stage 5 isn't a
detour from the book; it's the material for making chapters 2–4 and 7 true.
