# OCI ownership before agent adoption

C34 remains open after foreground process ownership. This plan covers the
remaining runtime work on PR #167. It does not change the agreed release
contract: uncertain cleanup retains ownership, and operator decommission only
clears obligations after external fencing and permanent identity retirement.

## What the code still permits

`RuncGrill::start` directly spawns `runc run`. Bun can die before storing its
child handle or application adoption record. `launcher_exited` currently treats
a missing in-memory entry as no launcher, even when a durable network reservation
survives. Inspecting absent OCI state cannot exclude a delayed launcher creating
that state afterwards.

Rootless preparation directly spawns slirp4netns. Its cancellation guard handles
a dropped future within a live Bun, but cannot cover Bun SIGKILL before the
first adoption record. Recovery still uses a recorded PID and start time to
signal an old helper.

Network setup and teardown run external `ip` and `nft` commands. Killing their
immediate Bun caller does not establish that every admitted mutator has finished.
A durable address reservation identifies the resource owner; it does not prove
that no older command can change the resource after cleanup inspects it.

The original OCI input must also survive preparation. Runc adjusts the rootfs,
network configuration and environment while preparing a bundle. Recovery must
compare agent intent with the original input, not those derived values.

## Implementation order

Each item gets its own tests, book explanation and commit. Production selection
changes only once the corresponding recovery contract is implemented.

- [x] Add the explicit `OwnedCommands` foundation using the existing durable
  owner. Five new integration contracts cover preparation before activation,
  recovered inventory, timeout followed by explicit retirement, preserved
  output/exit status, invalid-input refusal and bounded output after completion.
  All 42 macOS/43 Linux adapter, owner and recovery cases pass with strict Clippy.
- [ ] Use the adapter for external runtime commands, keeping records under their
  owning runtime generation. Integrate complete inventory and actual retirement
  evidence before changing production selection. A timeout or missing socket
  must never become successful command retirement.
- [x] Keep production runtime files under the configured node: Runc bundles
  and state below its instance directory, and image cache in the actual selected
  images directory. The regression fails first; 27 macOS/31 Linux startup,
  compatibility and real Bun-crash cases plus strict Clippy pass. Three real
  rootful registry catalogue cases pass with confirmed cleanup (38.19s).
  State 20 refuses older development layouts.
- [x] Refuse duplicate preparation and pre-existing OCI state before changing
  bundles. Both rootless regressions fail first; all 304 selected Linux runtime
  tests pass with strict Clippy on Linux and macOS. Confirmed retirement permits
  replacement. Durable pre-adoption intent remains the next step.
- [ ] Journal original Runc intent before preparation can allocate resources.
  Include the instance generation and runtime configuration needed for recovery.
  Publish a validated complete inventory for the agent's existing reconciliation
  path. Missing, malformed or conflicting records must refuse.
  - [x] Add the journal primitive and exclusive generation claims. Seven contracts
    plus a real process-death fixture cover persistence, stale callers, separate
    adapters, invalid inventory, configuration conflicts and publication failure.
    All 13 selected journal/command cases and strict Clippy pass on macOS/Linux.
    Production integration and complete agent inventory remain open.
- [ ] Keep the per-instance lifecycle guard through the completion of each owned
  create/start/cleanup operation. A cancelled caller cannot leave a blocking
  worker or command able to mutate resources after a successor acquires that
  guard. Bind queued operations to their original generation.
  - [x] Connect generation-bound short commands, durable admission sealing and
    positive draining under cancelled callers and actual caller SIGKILL. All 17
    selected command/journal cases and strict Clippy pass on macOS/Linux. Real
    runtime command-path integration remains separate.
- [ ] Move `runc run` and rootless slirp4netns behind durable owners. Reconstruct
  runtime state, exit outcomes, logs and forwarding from those records before
  relying on agent adoption metadata. Remove recovered-PID signalling from the
  production recovery path.
  - [x] Persist exact launcher/helper bindings before activation. Original exit/log
    recovery, incomplete bindings, actual caller SIGKILL and stale shared handles
    pass within 27 ownership cases and strict Clippy on macOS/Linux. Production
    launcher/helper integration remains open.
  - [x] Integrate the opt-in rootful Runc adapter. Six real Linux cases plus a
    subprocess fixture pass: abandoned preparation, short exits/logs, caller
    SIGKILL during exec, live adoption, cancelled preparation and a completed
    log reader that must not block replacement. The latter fails before its fix.
    All 304 Linux runtime tests, 34 macOS/36 Linux selected ownership cases and
    strict Clippy on both pass. Production selection and rootless remain open.
  - [x] Permit replacement of positively retired network helpers while preserving
    the launcher's one-attempt rule. Required journal format 3 retains previous
    helper bindings; interrupted preparations are cancelled and corrupt history
    refuses. Five new contracts pass within 39 macOS/41 Linux ownership cases,
    all six real rootful cases plus their fixture pass, and strict Clippy passes
    on both. Actual slirp integration remains open.
  - [x] Integrate the opt-in rootless adapter. Five actual unprivileged Linux
    cases plus a namespace-refusal contract and subprocess fixture pass (3.79s).
    The runtime pins verified namespace descriptors, gates the first workload
    instruction on network readiness, recovers published ports without repeating
    the launcher and drains interrupted helper startup after actual caller death.
    All six rootful cases plus their fixture pass again (5.22s), 37 macOS/40 Linux
    selected regressions pass, and strict Clippy passes on both. CI now includes
    these rootless cases with a static BusyBox fixture. Production remains open.
- [ ] Route namespace, link and forwarding mutations through owned commands.
  Before confirming cleanup, retire every admitted command, then verify OCI
  state, helper sockets, mounts, namespaces, links and owned forwarding state.
  Keep the address reservation and original intent until every check succeeds.
  - [x] Qualify the generation-bound network executor, cancellation and actual
    caller-SIGKILL recovery: 304 Linux runtime tests, 21 selected integration
    cases, both privileged network cases and strict Clippy pass. macOS passes
    19 selected integration cases and strict Clippy. Production wiring remains open.
- [ ] Join runtime reconciliation to discovery and egress cleanup. Restore only
  verified source bindings; withdraw bindings before confirming retirement.
  Audit persistent kernel state created before agent adoption independently of
  the external-command changes.
  - [x] Require positive kernel egress retirement. Three real frozen-map tests
    reproduce ignored destination/flag deletion errors and premature removal of
    an agent adoption record. The repair retains bindings and records through
    repeated failed cleanup and propagates enumeration errors. All 27 physical
    Linux eBPF cases pass (5.67s), all 202 selected agent/egress library cases pass
    on both macOS and Linux, and strict Clippy passes on both.
  - [ ] Qualify the lifetime of unpinned cgroup links across actual Bun death,
    and restore or fence protected workloads before adoption.
- [ ] Apply the same pre-adoption reasoning to Apple Container CLI operations.
  Its experimental status does not justify falsely confirming cleanup while an
  older invocation can still create a container.

## Qualification

Start with reproductions of delayed launcher publication and late network
mutation. Exercise real Bun SIGKILL before adoption, restart without a PID
record, short jobs, lost owners, cancellation and generation replacement. Use
real rootful and rootless Linux containers, not only fake command outputs.
Helper loss must retain uncertainty even when named resources appear absent.

Re-run privileged networking/storage checks, job recovery and real rolling
upgrade/rollback after integration. Then qualify the complete three-node
catalogue (V01), sustained recovery (V02), the preserved signed candidate (V03)
and repeated cold installs on the advertised host matrix (V04). Passing a
runtime unit suite does not close those release gates.


## Remaining adapter integration

The integrated rootful and rootless adapters remain opt-in. Production selection
still awaits discovery/Apple recovery and full qualification. Complete these remaining steps:

1. Completed: extend generation-bound whole-operation workers to rootless
   preparation, launch and cleanup. Rootless mode creates no host overlay mount
   and does not inspect or mutate the rootful namespace/link reservations.
2. Completed: bind actual slirp4netns startup and recovery to the original
   generation. Verify the container belongs to the authenticated launcher owner,
   retain namespace descriptors across exec and use a bounded OCI createRuntime
   hook so even a short job sees a ready network. Unassociated prepared commands
   remain discoverable and are cancelled during cleanup.
3. Completed for runtime resources: extend exec/retirement ordering to rootless helpers.
   Rootful exec now uses the launcher owner's auxiliary-command protocol without
   holding the adapter mutex while waiting for output.
   Close normal command admission before retiring all launchers, auxiliary exec
   attempts and network helpers. Then run owned cleanup commands and inspect
   OCI state, mounts, namespaces, forwarding and discovery. Release address
   reservations only after every admitted mutator has positively retired.
4. Completed for the opt-in adapters: preserve generation fencing. Queued
   calls, separately constructed adapters and stale pre-sealing clones must
   retain their tested refusal semantics.
5. Completed: prune positively retired short-command records under the same
   exclusive claim, so recurring runtime observations cannot grow the journal
   indefinitely. The failing growth regression and four pruning contracts pass
   within 34 macOS/36 Linux selected cases, strict Clippy on both platforms, and
   both privileged network cases after pruning.
   Failed or uncertain attempts must remain discoverable. Move terminal records
   atomically to a separate private garbage directory before deleting files, so
   a crash during deletion cannot corrupt the active command inventory. Fence
   delayed starts with the original operation and owner locks; command IDs are
   never reused. Preserve launcher/helper records for outcomes and logs.
6. Switch production selection and complete agent launch-inventory recovery only
   after these paths pass actual Bun-death, missing-adoption-record, short-job,
   rootful/rootless and rolling-upgrade qualification.

## Keep kernel policy across Bun death

The real loader-death regression now demonstrates a release blocker. A probe in
an isolated cgroup receives `PermissionDenied` while the loader is alive, then
connects to the same local listener after SIGKILL. Bun owns the same loader path.
The current objects are unpinned and their cgroup links close with Bun's file
descriptors. Surviving containers can therefore outlive enforcement.

Complete this before production OCI selection:

- [x] Retain all four cgroup links and their maps in a private, versioned bpffs
  directory. Publish original cgroup identity and ownership metadata first; hold
  an exclusive node-level claim throughout loading and mutation. Unknown or
  conflicting pinned state must refuse recovery.
- [x] Reopen the same maps and replace attached programs without a detach gap.
  Validate link type, target cgroup and program/map ownership before updating.
  Observe actual kernel link state when reporting enforcement capability.
- [x] Keep ordinary ephemeral loaders available for isolated tests. Build an
  explicitly persistent object for the owned loader; avoid changing the default
  loader into a source of global pins.
- [ ] Reconcile retained egress, namespace and service maps against original
  runtime intent and adoption records before serving recovered workloads. Do not
  erase policy merely because the new Bun has not rebuilt its in-memory bindings.
  Egress ownership now precedes map programming and is restored before adoption;
  original runtime intent also identifies policy created before its final record.
  Namespace/service reconciliation and verified container source identity remain
  open, followed by actual Bun/upgrade qualification.
- [x] Qualify actual loader SIGKILL and retained-map recovery. The previously
  failing isolated-cgroup probe now remains denied during absence and after
  recovery, then connects only after explicit policy removal. All 32 physical
  kernel cases pass (6.85s), including conflicting owners, cgroup mismatch,
  partial preparation, missing active pins, interrupted retirement, wrong map
  layout and same-layout foreign map identity. Strict Linux/macOS Clippy passes.
  Format 1 pins are opt-in; production loading is unchanged.
- [ ] Qualify actual Bun SIGKILL, blocked traffic during absence,
  restoration with the same maps, partial startup, conflicting identities,
  repeated restart and upgrade, plus positive per-workload cleanup. Wire production
  selection only after that evidence passes and advance the state format.
- [ ] Distinguish host reboot from loader death. bpffs objects disappear at boot,
  while normal-storage ownership survives. Use positive boot identity and runtime
  absence evidence before reinitialising; missing pins during the same boot must
  continue to refuse recovery.

The frozen-map repair is complete and remains separate: it proves live cleanup
propagates kernel failures and retains its owner. It does not prove kernel policy
survives process death. Apple daemon-command recovery also remains open.


## Agent policy ownership follow-up

The public agent regression reproduced a second part of the same boundary:
deployment had no durable policy checkpoint, and adoption restored the workload
without its egress binding. The next live check stopped it. The repair persists
original policy ownership before map writes and validates it against runtime
intent and adoption records. A retired marker survives the interval between
confirmed kernel cleanup and final metadata removal. Missing ownership refuses
even when the runtime reports absence. Uncertain writes block later kernel
rewrites until restart reloads durable authority. All 38 physical kernel tests
pass (8.83s), including checkpoint-write refusal, missing ownership/enforcement,
interrupted metadata cleanup and repeated frozen-map recovery. Four portable
checkpoint contracts and 206 affected tests pass on each platform, with strict
Clippy on Linux and macOS. Actual Bun/upgrade and production selection remain open.

Source identity now has a separate runtime query. The owned rootful adapter
verifies the container's original cgroup, nested init and authenticated launcher
parent through retained process/cgroup descriptors, then rechecks launcher
ownership. Namespace reconciliation, trace, source-specific faults and recovered
egress checks use that query. Unsupported runtimes expose no verified identity;
conflicts refuse. Legacy and rootless Runc do not claim verified cgroup support.
Actual Bun production selection remains pending the remaining integration gates.


Application restart now retires predecessor adoption and policy records before
creating a successor. Two failing-first regressions pause at creation and inject
record-removal failure; both then pass within 208 affected tests on macOS/Linux.
Cleanup failure leaves the application Pending for retry. Automatic restarts
retain the logical workload's identity credentials and validate its mount source;
explicit retirement still removes that identity. Strict Clippy passes on both
platforms. Actual Bun/Runc crash-boundary qualification remains open.


## OCI cgroup path correction

The real-container regression reproduces a generated-path mismatch: Bun prepares
`/sys/fs/cgroup/<workload>`, but the old OCI value places the process in
`/sys/fs/cgroup/sys/fs/cgroup/<workload>`. Application, job and init generators
now emit the hierarchy path required by OCI; restart and policy recovery perform
an explicit, validated conversion back to the host path. State 21 refuses old
ambiguous development records. Container source identity still needs verified
runtime evidence; fixing the path alone does not establish launcher attribution
or discovery cleanup.

Qualification: all eight owned-Runc cases pass (7.11s), including the corrected
first-instruction cgroup check, and all 38 physical kernel cases pass (9.48s).
The 207 affected library tests pass on macOS/Linux (17.958s/24.152s). Both actual
binary compatibility tests and strict all-target/all-feature Clippy pass on each
platform. Format changes are explicit; the protocol remains 14 and state is 21.


## Bounded command observation under load

Hosted macOS CI at `97aecda` reports one failure among 3,906 tests. Concurrent
native reproduction captures a BrokenPipe error from the owned-command status
poll (16 failures in 200 diagnostic attempts). The wait now retries transient
control failures within its existing deadline, requiring positive terminal
evidence before reading output. Empty EOF has a distinct transport error;
malformed responses and invalid ownership still refuse. Both deterministic
regressions fail before the fix: interrupted polling returns prematurely, and
an unreachable owner returns immediately instead of exhausting the bounded wait.
Runtime activation is never retried by this observation loop.

After repair, all 53 macOS/54 Linux selected command, process-recovery, owner and
Runc-command tests pass (28.139s/21.417s), with strict all-target/all-feature
Clippy on both. The same 200-run concurrent output reproduction now passes with
no failures. Hosted validation of the new head remains a separate checkpoint.


## Source identity and the remaining discovery boundary

The real-container regressions distinguish the workload's cgroup from its
launcher, recover the same identity, return no live identity after retirement,
and refuse a container moved into another cgroup. The public-agent kernel-map
regression first reproduces the missing workload namespace binding.

Remaining discovery work must publish original source ownership before startup,
including jobs and applications that expose no service port. The current
namespace reconciliation derives entries from service records and runs after
startup. Its map-write/removal failures and backend-map removal also need
positive, retryable cleanup evidence. Recover retained maps before adoption;
remove only bindings whose runtime and original ownership are positively
retired. Qualify stopped owners, partial programming, Bun death, shared rollout
state and frozen-map refusal before selecting persistent maps in production.

Source-identity qualification: all ten physical owned-Runc cases pass (8.17s),
including recovery, positive retirement and moved-cgroup refusal. All 39 physical
kernel cases pass (8.78s); the public-agent namespace entry now uses the workload
cgroup and leaves the launcher unbound. All 170 affected library tests pass on
macOS/Linux (17.819s/22.973s), with strict all-target/all-feature Clippy on both.
The privileged CI filter and host-network serialisation now select the complete
owned-Runc binary, including tests without a `runc_` name prefix.


## Rollout cgroup isolation

Actual Runc reproduces a shared-cgroup retirement failure: two live generations
use the same app/ordinal path, and retiring the predecessor stops the successor.
The public deployment regression also shows identical stored OCI paths before
and after rolling replacement. Correct instance allocation must include the
deployment generation. Resource faults and CPU diagnostics must follow that
exact path as well. Preserve the original stored path for runtime recovery;
never reconstruct a canary's path by discarding its generation.

Qualification: all 189 affected library tests pass on macOS/Linux
(17.744s/23.827s), both actual-binary compatibility tests pass on each platform,
and strict all-target/all-feature Clippy passes on both. All eleven owned-Runc
cases pass (11.46s), as do all 39 physical kernel cases (8.79s). Both rolling
and blue-green deployment paths have distinct original cgroups. State 22
refuses older rollout records that could share a predecessor's cgroup.
The strengthened physical check also executes a command inside the successor
after predecessor retirement. All eleven Runc cases pass again (9.91s).

## Firewall deletion evidence

The physical frozen-map regression reproduces false success from both namespace
and firewall deletion helpers: they discarded the kernel's removal result.
The helpers now accept only successful deletion or explicit key absence, and
propagate every other error. Repeated removal of an ordinary entry remains
idempotent. Agent bookkeeping, durable source ownership and retirement ordering
remain open; this change does not claim that callers already preserve every
failed discovery obligation.

All 41 physical Linux kernel cases pass after the fix (9.03s), including both
new deletion contracts. Strict all-target/all-feature Clippy and formatting
pass on Linux and macOS.

## Retaining reconciliation obligations

Extract the agent's namespace/firewall reconciliation into the shared concrete
map operation used by its physical regression. Preserve the existing behaviour
for the failing-first run: frozen-map deletion is ignored and the remembered
keys are replaced by the desired sets. Repair by remembering attempted writes,
propagating errors and forgetting only confirmed deletions. Retire obsolete
allow rules before namespace identities. A repeated frozen-map test must retain
both obligations on allow removal failure, or only the namespace obligation
when that final deletion fails. This is in-memory evidence; durable source and
backend retirement remain open.

All 44 physical kernel cases pass (10.50s), including repeated frozen-map
cleanup, partial publication, preservation of unrelated entries and repeated
successful retirement. Strict Clippy and formatting pass on Linux and macOS.

## Outbound-only source selection

The portable regression reproduces an omitted namespace identity for a verified
worker cgroup when that worker has no service record. Explicit `allow_from`
resolution also incorrectly requires a service record for the source. Source
identities must come from the verified workload inventory; service entries only
identify destinations. Use the same namespace-name hash as service registration.
The public-agent kernel regression deploys a portless application and inspects
its map entry. This correction does not claim pre-start publication or durable
job/source recovery; those lifecycle boundaries remain open.

Qualification: the public-agent map is initially absent for a portless workload.
After the repair, all 45 physical kernel cases pass (9.86s), as do all eight
portable firewall resolver tests and strict Linux/macOS Clippy and formatting.

## Positive backend withdrawal during Stop/Retire

The physical frozen-backend regression first gets `Ok(())` twice from Retire
while the kernel entry survives and its workload record is discarded. Stop now
requires successful deletion of the original service entry's exact VIP/port
before retiring workload metadata. Refusal preserves the service key and stopped
owners. After deletion, userspace backends are removed before artifact cleanup;
a later artifact failure leaves the empty service key available for retry.
Rollout finalisation also propagates deletion failure. Durable service ownership
and per-instance rolling backend replacement remain open.

Qualification: all 47 physical kernel cases pass (10.13s), including repeated
refusal and confirmed backend absence. All 160 agent tests pass on macOS/Linux
(17.835s/22.915s), with strict all-target/all-feature Clippy and formatting on both.

## Source ownership before execution

Both controlled-start regressions reproduce missing namespace identity at the
application/job execution boundary. Extending them to an explicitly allowed
cross-namespace destination also reproduces missing grants before Start.
Checkpoint schema 2 records original source namespaces, including workloads with
no external allowlist. Source ownership does not enable external egress filtering.
The agent installs the namespace and existing services' grants before allowing
Start, propagating map failures. Recovery restores owned source keys before
adoption and refuses unowned retained entries or missing live enforcement.
Retirement removes a source's grants before its namespace, preserving the
checkpoint and adoption record on failure. Unknown namespace entries are no
longer swept merely because memory lacks an owner.

Source-only live monitoring now stops workloads when their original namespace
binding or hooks disappear. All 55 physical kernel cases pass (12.66s), including
checkpoint refusal, source-only adoption, erased owner identity, live namespace
loss and repeated frozen-map recovery. All 174 affected library tests pass on
macOS/Linux (17.877s/25.116s), both binary compatibility tests pass, and strict
all-target/all-feature Clippy and formatting pass on both. State 23 and policy
checkpoint schema 2 exclude older development records. Hosted CI passes all 19
executed checks at the preceding `e4eab5c` checkpoint (two skipped); this feature
still needs its own hosted run.

Actual runtime cancellation/retry boundaries remain open. Inspect the
init-container path separately: it currently shares the
parent's cgroup and needs physical qualification that init exit cannot invalidate
the parent's prepared policy before the main process starts. Do not select the
persistent production paths until these boundaries are covered.

## Namespace-safe destination identity

The unit regression gives `permitted/database` and `private/database` the same
firewall ID. The physical connection regression then confirms that a grant to
the former also permits a TCP connection to the latter. Both fail before repair.

The destination identity now reuses the service's allocated VIP as an opaque
32-bit integer. Local registration already resolves VIP collisions before
returning the entry; remote-only views use the catalogue's chosen VIP. This
avoids a second allocation scheme or another truncated name hash. Tests cover
both collision-probed local allocation and an explicit remote catalogue address.
State 24 and kernel ownership manifest 2 refuse retained bare-name grants.

All 57 physical kernel tests pass (12.83s), including the actual denied/allowed
connections and old-manifest refusal. All 111 native and 123 Linux affected
library tests pass, as do both binary compatibility cases on each platform.
Strict all-target/all-feature Clippy and formatting pass on both platforms.
Retiring stale grants before any VIP reuse remains part of the separate durable
service-ownership work.

## Init-container policy lifetime

The actual owned-Runc regression runs two short initialisers followed by the main
workload, using a local BusyBox fixture. Before the fix, only the first Start
observes the expected namespace and egress flag. Runc removes the shared cgroup
on exit, so subsequent Starts use a new identity without policy. Refresh the
existing checked pre-start policy after each successful init, before authorising
the next container. Failed publication must prevent that next Start.

This item covers successful init sequencing. Keep failed/interrupted init
creation, launch and cleanup in the remaining lifecycle qualification: establish
that parent retirement cannot clear policy or report completion while an
uncertain initialiser can still execute, including recovery ordering across the
complete original runtime inventory.

Qualification: all 58 physical kernel cases pass (24.52s), including the real
owned-Runc initialisers and main workload. All 174 affected library tests pass
on macOS/Linux (17.865s/23.270s), with strict Clippy and formatting on both. The
Linux lint pass required removing an unused import from the new fixture; no
product behaviour changed after the passing tests.

## Initialiser ownership through failed deployment

The agent regression reproduces parent Retire succeeding without observing an
initialiser whose inspection fails. Track initialisers before runtime creation,
retain them until positive termination and require their cleanup before lifting
parent policy or deleting parent records. Use a reserved auxiliary identity
separator that cannot collide with an ordinary DNS-label workload name.

After controller failure, the original runtime launch inventory supplies the
initialisers that never acquired adoption records. Retirement must first stop
all unacknowledged launches; only then may a second pass retire their policy and
metadata. Otherwise visiting a parent before its live initialiser can remove the
shared cgroup policy even though recovery later refuses the initialiser.

The implemented registry reserves children before creation and forgets them only
after confirmed kill/exit. Parent artifact cleanup handles retained children
first. Reserved `__init-N` IDs cannot collide with ordinary workload labels;
state 25 refuses old development identities. The physical test also aborts the
controller task, refuses recovery once, then removes the fault and confirms
retirement. It specifically checks that the refused recovery preserves namespace
policy. This is real Runc/kernel qualification, not yet actual Bun process death.

## Independent routes for simultaneous containers

The address-retirement regression exposed a second defect while keeping the
original container alive: traffic from the host could not reach the successor.
A separate two-namespace test reproduces complete packet loss to the second
address. Both host veths had connected routes covering the full node allocation.

Use `/32` endpoints with explicit host-to-container routes, a direct gateway
route inside each namespace and a default route through that gateway. The node
still allocates from its `/23` pool. Adoption requires the new endpoint shape,
and state 26 excludes older development networks. Keep this fix separate from
backend withdrawal; its packet test requires neither Bun nor eBPF.

## Backend and reusable-address retirement ordering

Owned Runc cleanup releases its rootful address reservation, including when
state inspection observes an exited launcher. The original Agent Stop withdrew
backends after runtime retirement; the HTTP regression below now reproduces the
resulting connection leak. Durable backend records alone do not establish the
required ordering: the route must be withdrawn before its destination address
can be reused. Include natural exits and per-instance rollout retirement in the
remaining qualification.

Also combine live-policy loss with refused backend withdrawal. Safety fencing
must still stop execution, while retaining any address that an old route can
reach. Simply moving a route deletion earlier in ordinary Stop does not prove
that combined failure safe. Runtime execution, address ownership and discovery
retirement need separately confirmed boundaries.

Initialiser-retirement qualification: all 59 physical kernel tests pass (26.59s),
all 176 affected library tests pass on macOS/Linux (17.767s/22.899s), and both
binary compatibility tests plus strict Clippy and formatting pass on each.
Actual Bun process death and cancellation before/during runtime activation remain
explicitly open. The physical failure fixture aborts the controller task and
retains the actual owned runtimes and kernel maps for recovery.

## Explicit Stop before address reuse

The physical regression confirms the inspected ordering defect: freeze backend
removal, Retire the original service, then start a portless container that serves
its identity on the same internal port. The old VIP returns HTTP 200 with the
successor's identity. The test checks the direct endpoints as positive controls.

Move confirmed kernel withdrawal, userspace backend removal and routing
publication before runtime Stop. Refusal keeps the original runtime/address
owned; a failed Stop is not evidence that execution ceased. This closes the
explicit Stop/Retire ordering case. Natural exits, per-instance rollout cleanup,
durable backend ownership and stale cluster routing views remain open. In
particular, qualify how a worker excludes its own advertised catalogue backends
when it has no council metrics identity.

Qualification: all 60 kernel cases pass (33.28s), including the old-VIP HTTP
regression with positive direct-endpoint controls and the original VIP checked
before injecting failure. All 207 macOS/227 Linux affected library cases pass
(17.773s/23.946s), with both compatibility tests and strict Clippy/formatting on
each platform. The separate route fix also passes all four network cases and
eleven Runc cases. These results do not close the remaining combined failure,
natural-exit, rollout or durable discovery ownership cases above.

## Per-instance rollout withdrawal

The frozen-map rolling regression reproduces the original destination being
retired before confirmed withdrawal. Move checked single-instance backend
publication into BeginRetire, before supervision is fenced and before the worker
can drain/stop. Preserve the original service entry on refused kernel writes,
and keep replacement backends in the proposed entry. Final artifact cleanup
must also refuse unconfirmed withdrawal. Qualify rolling, blue-green and the
successful replacement path using real HTTP responses and runtime state.

Natural exits, complete durable discovery ownership and combined live-policy
loss remain separate. In particular, a stopped runtime cannot itself prove an
address safe to reuse while any retained route still reaches it.

Qualification: all 63 physical kernel cases pass (59.53s), including both refused
strategies and a successful rolling replacement. All 207 affected library tests
pass on macOS/Linux (17.799s/22.892s), with strict Clippy and formatting on both;
both Linux binary compatibility cases also pass. Protocol/state remain 14/26.

## Checked deployment backend publication

Freezing the backend map before a fresh deploy reproduces Complete despite an
absent kernel backend (0.26s). Add a checked publication result to fresh and
rolling finalisation, propagate failure to the client and retain runtime
ownership. Restart finalisation must route the same failure through its existing
bounded retry/cleanup policy. This does not establish durable service ownership
or confirm every intermediate health/replacement publication; those remain open.

Qualification: all 64 physical kernel cases pass (61.76s), including refused
fresh publication. All 207 affected library tests pass on macOS/Linux
(17.697s/23.243s), with strict Clippy and formatting on both platforms; both Linux
binary compatibility cases also pass. Protocol/state remain 14/26.
