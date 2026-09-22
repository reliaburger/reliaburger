# Remote withdrawal ledger — 22 September 2026

Continue C34 on `codex/codebase-completion-fixes`, PR167. Standalone discovery
recovery, original runtime/report identities and durable consumer registration
are already implemented. Do not repeat those audits.

## Checkpoint 1: replicated withdrawal obligations — complete

- Add a monotonically increasing catalogue publication generation to replicated
  state. Identical publications are no-ops; overflow refuses without mutation.
- On catalogue replacement, retain only removed or changed backend exposures,
  plus removed service allocations. Unchanged backends do not acquire spurious
  drain obligations. Retain their exact original VIP, port, node and execution
  identity, indexed by the publication generation that exposed them.
- Capture the durable consumer census in the same Raft transition. Consumers
  register before receiving placements; census entries never disappear through
  gossip expiry. A later registration need not confirm an already withdrawn
  publication it was never permitted to read.
- Keep older withdrawals through later publications and snapshots. Bound retained
  obligations and refuse overflow atomically; do not evict cleanup evidence.
- Permanent operator decommission removes only that consumer's obligations.
  If the node also produced endpoints, journal their removal for the remaining
  consumers in the same transition. Validate the entire transition before changing
  placements, leases or retirement records.
- Test these transitions first, including snapshot recovery, replacement on the
  same host port, repeated publication, late registration, capacity refusal and
  decommission. Update compatibility, the book, progress and the session handoff.

This checkpoint records obligations. It does not add a receipt endpoint or permit
physical allocation reuse. Those depend on the next checkpoints.

## Checkpoint 2: allocation reservations and authenticated receipts

- Reserve withdrawn VIPs in allocation and enforce reservation checks in Raft,
  including deletion/replacement in one publication. Prevent stale writers from
  replacing newer publication state.
- Expose publication and retirement identities to registered consumers. Accept
  only authenticated, identity-bound receipts for the exact withdrawn generation;
  future, unknown and mismatched receipts cannot discharge another obligation.
- Keep producer retirement decisions tied to committed state, including active
  re-publication and all outstanding generations; absence of a local entry alone
  is not proof of global withdrawal.

## Checkpoint 3: consumer and producer integration

Persist consumer ownership before publishing. Withdraw original userspace/kernel
entries, cancel captured requests, and wait for positive guard release before
sending receipts. Recovery must reconcile original state before replay. Gate the
common Process host-port and owned-Runc address release paths on the committed
result. Failed or cancelled operations retain ownership for retry. Final producer
release must also prevent stale reports from republishing an already retired
execution after its address has been reused; checking only one momentary absence
from the catalogue is insufficient.

Qualify offline consumers, delayed receipts, reuse, leader replacement, consumer
restart and permanent decommission. Then finish actual OCI interruption/reboot
qualification and production runtime selection, followed by V01–V04. Keep C34
unchecked until all of these boundaries pass.


Checkpoint 1 was committed early as `0ea5702` after focused/native library tests.
Broader qualification now passes: 388 native/400 Linux library tests, ten
agent/compatibility cases and three real failover/decommission cases per platform,
plus strict Clippy/formatting. State format 30, protocol 16. The session handoff
records timings and logs. Continue with checkpoint 2; C34 remains open.


Checkpoint 2's allocation reservations are complete (`2481943`). Six contracts
fail first; fourteen focused cases, 394 native/406 Linux library cases, ten
agent/compatibility cases and three failover/decommission cases per platform pass
with strict checks. Both current/departing and already-withdrawn VIPs are reserved;
Raft independently rejects invalid, aliased and prematurely reused allocations.
Formats remain 16/30. Continue with stale-publication guards and authenticated
receipts; no consumer receipt endpoint or physical release permission exists yet.


Stale-publication protection is complete (`6643aed`). PublishEndpoints requires
the generation from the candidate's original desired-state snapshot. Raft rejects
mismatches before mutation, including no-op requests. Four contracts fail first;
five focused tests, 398 native/410 Linux library cases, ten agent/compatibility
cases and three real failover/decommission cases per platform pass, with strict
Clippy and formatting. The request/log shape advances compatibility to protocol
17/state 31. Continue generation-bound consumer instructions and authenticated
receipts.

Consumer-specific instructions are complete (`8aa0163`). The
placement response carries required generation, catalogue and instruction fields;
only the requesting consumer's obligations are included. Late enrolment doesn't
inherit older withdrawals, and reads don't acknowledge them. Two contracts fail
first (0.115s); both focused tests pass (0.057s). Native/Linux pass 402/414
affected library cases, ten agent/compatibility cases each, real cross-node
discovery through leader replacement and strict Clippy/formatting. Protocol
18/state 31. Authenticated receipts and durable consumer/producer integration remain separate.

### Next receipt boundary

The receipt endpoint must require both system authority and the current TLS node
identity, validated against a linearizable security read. Derive the consumer
identity from the certificate; don't trust a caller-supplied node name or forward
through a follower's certificate. A plaintext development registration confers no
permission to discharge obligations. Only a confirmed Raft result can acknowledge
completion; timeout, cancellation or write refusal retains uncertainty.

Each receipt names one original withdrawal generation. Applying it removes only
that consumer from that generation, retaining the registration and every other
generation/consumer. Raft must reject retired or unregistered identities and
current/future generations. Define retry behaviour explicitly: an already absent
historical obligation may return success without changing state, because generation
identities never repeat. This must not be used as evidence that any current or
later publication has drained. Tests must cover replay after snapshot, late
readers, wrong identities, missing/revoked TLS credentials and unavailable leaders.

### Receipt implementation checkpoint

The endpoint and Raft mutation are complete (`000db00`). Three API
contracts fail first (0.066s), followed by two Raft snapshot/refusal contracts
(0.092s). The body contains only compatibility and generation; the handler derives
the consumer from a freshly validated TLS certificate. Only Applied returns 204.
Historical retries leave all other obligations unchanged; registration is retained.
All five focused tests pass (0.055s). Native/Linux qualification passes 404/416
affected library cases, thirteen integration cases each, three real
failover/decommission cases each, strict Clippy and formatting. The new durable
command advances compatibility to protocol 19/state 32. Consumer proof production,
cancellation recovery and producer release remain open.

### Consumer integration handoff

The receipt service is only the receiving boundary. The next implementation must
preserve the evidence that permits a consumer to call it:

- `src/cluster/orchestrate.rs` currently decodes generation/instructions but sends
  only catalogue/ingress through `AgentCommand::SyncClusterCatalog`. The checked
  agent result is now implemented; next carry the complete generation-bound update.
- `src/bun/agent.rs` now builds a candidate routing table before changing cluster
  views and reports failure through the command reply. Durable consumer ownership
  must precede this publication, and its result alone is not drain proof.
- Add durable consumer ownership before any attempted publication. Bind it to
  the enrolled node/cluster, reject delayed older responses and retain original
  exposures across replacement. The existing `DiscoveryJournal` tracks local
  service/runtime ownership; remote consumer records cannot simply be treated as
  local runtime references. Keep filesystem claims alive through cancelled I/O.
- Routing/DNS currently overlay the remote catalogue, while eBPF synchronisation
  uses the local map. Audit each actual publication surface. Withdraw old routes
  before draining captured HTTP/WebSocket requests, then require actual guard
  release through `SharedDrains`, not merely cancellation or a missing record.
  Account for this node's own backends separately from the remote overlay. The
  merge currently retains a local service's allocation and adds remote allocations
  independently (`ServiceMap::with_cluster_catalog_excluding_node`). Consumer
  recovery must validate the effective local-plus-remote view and correlate its
  exact installed VIPs; validating the incoming catalogue alone does not prove
  that this merged view has no allocation conflicts.
- Persist completed local withdrawal before sending the receipt. Failed, timed
  out or cancelled requests retain replayable evidence. Recovery reconciles old
  ownership before publishing or acknowledging anything; newer publications and
  unrelated generations must remain intact during replay.
- Keep producer address/host-port release closed until committed state confirms
  every relevant withdrawal, and fence stale reports from reintroducing a retired
  execution. Receipt acceptance alone does not implement that producer gate.

Tests should interrupt consumer persistence, publication, drain and receipt waits;
retain an offline consumer through leader replacement; then verify bounded retries,
restart replay, and operator-fenced permanent identity retirement.

### Checked cluster publication checkpoint

The cluster publisher now validates allocations and builds a complete candidate
routing table before changing catalogue, DNS or ingress views. Rejected candidates
preserve the last confirmed publication; corrected updates can retry. A checked
oneshot reply prevents placement work from proceeding on queue acceptance alone.
The existing deadline covers queueing and confirmation, including lost replies.
Three contracts fail first (0.115s); six focused cases pass (6.225s). Implementation
is committed early as `0e80491`. The broader native run identified one older fake
agent missing its new reply (456/457 passed); `866a52f` corrects it. Final
native/Linux qualification passes 457/469 affected library cases, thirteen
integrations and three real failover/decommission cases each, with strict Clippy
and formatting. This internal channel change leaves protocol
19/state 32 unchanged. Durable consumer ownership before publication and positive
drain proof before receipts remain the next work.


### Consumer ownership storage checkpoint

The exclusive discovery journal now has an optional, identity-bound consumer
inventory. It preserves original committed catalogue generations and the exact
proposed merged service views. Updates retain the entire previous history;
changed identity, stale or rewritten generations, missing remote backends and
conflicting allocations refuse before writing. Limits are 1,024 attempts and
65,536 combined service/backend records, plus the existing 16 MiB checkpoint cap.
No history eviction or completion inference is permitted.

This is storage groundwork, not publication integration. Use the existing journal
worker and exclusive claim rather than introducing a second independently written
store. Standalone reconciliation and fresh enablement refuse existing consumer
ownership. The schema advances to 2 and state to 33 (protocol remains 19).

Next: pass committed generations and withdrawal instructions to Bun, establish
the stable enrolment identity, persist before attempted publication, and reconcile
old ownership before new publication on restart. Capture ingress destinations and
request guards as well as the service view. Add explicit confirmed-withdrawal and
receipt phases before allowing bounded compaction; the current append-only store
must refuse at capacity. Producer release still needs committed proof and stale
execution-report fencing.

Storage implementation is committed as `6b105b0`. Five contracts fail first
(0.157s); all 24 journal/fresh-enable cases pass (0.711s). Qualification passes
349 native/361 Linux affected library cases (18.497s/23.954s), thirteen integration
cases per platform (5.487s/5.025s), strict all-target/all-feature Clippy and
formatting. Both pipelines are reaped. Logs and scripts are
`/tmp/rb-consumer-ownership-{native,linux}.{log,sh}`. Publication and recovery
integration remain open; this checkpoint enables no new cleanup authority.


### Consumer generation fence checkpoint

The reconciler now carries the committed catalogue generation into Bun. The agent
refuses lower generations and changed catalogues claiming the same generation,
including delayed otherwise-identical updates. Identical newer catalogues still
advance its confirmation. Invalid candidates leave both views and fence unchanged.
Generation zero requires an empty catalogue. Merged-view validation refuses local
and remote allocation collisions before publication. Four regression contracts
fail first (0.095s); eight focused agent/HTTP polling cases pass (4.057s).

The fence is in memory; recovery must restore it from original consumer evidence
before new publication. Ingress can change independently at the same catalogue
generation, so its ordering and original exposure history remain separate work.
No new withdrawal receipt or release authority is enabled by this change.

The fence is committed as `edf551a`. Native/Linux qualification passes 490/502
affected library cases (18.398s/32.257s), thirteen integration cases each
(5.121s/5.053s), three real failover/decommission cases each (71.409s/71.044s),
strict all-target/all-feature Clippy and formatting. Both pipelines are reaped.
Evidence: `/tmp/rb-consumer-generation-{red,focused,native,linux}.log`.


### Producer release checkpoint

`887c77a` adds permanent, node-scoped execution fences to Raft. Retirement atomically
removes fenced endpoints and records withdrawals. Release waits for every historical
consumer obligation for the execution, including uncorrelated original endpoints.
Both scheduler and Raft reject delayed reports for retired executions; a different
runtime generation can reuse the instance/address. Snapshots and bounded capacity
retain fences permanently; overflow refuses without partial mutation. Protocol
20/state 34 record the new request/response and durable state.

The authenticated producer API derives identity from current TLS, requires system
authority and quorum-backed security state, returns 202 while pending, and names
the exact execution on release. The agent's opt-in durable path verifies original
runtime inventory and the bounded HTTPS confirmation before records, host ports or
Runc addresses can be released. Runc release permission is persisted first.
Cancellation retains ownership; automatic restart uses the same gate. The binary
supplies transport, while production activation still waits for consumer recovery.

Focused evidence: three initial Raft, three initial API and two initial agent/client
contracts fail before implementation. Nine final producer library cases (0.362s)
and six receipt/producer API cases (0.057s) pass. Broader native/Linux qualification
follows. Completing consumer recovery, receipt production and service VIP retirement
is still required before enabling the production profile.


Producer API/agent integration is committed as `ac76b5c`, with qualification
follow-ups `c609b4f` and `968bd3e`. Native/Linux functional qualification passes
770/782 affected library cases (18.584s/32.414s), sixteen integrations each
(5.941s/5.018s), 45 recovery/runtime cases each (26.071s/23.710s), and three real
failover/decommission cases each (71.241s/71.081s). Final focused reruns pass nine
cases per platform (0.384s/0.643s), and strict all-target/all-feature Clippy and
formatting pass. All pipelines are reaped. The production binary supplies the
client, but durable discovery remains opt-in until consumer recovery is complete;
existing real-cluster checks do not qualify that unfinished end-to-end loop.
