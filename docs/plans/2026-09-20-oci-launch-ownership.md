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

The primitives do not yet switch production Runc. Connect them in this order:

1. Retain a generation-bound executor for each Runc instance, and keep the whole
   create/start/cleanup worker alive through caller cancellation. Holding a
   command claim alone does not cover a cancelled rootfs blocking worker.
2. Persist distinct foreground-launcher and rootless-helper command references
   before either can activate. Recover their original specifications, actual
   outcomes and logs independently of agent PID records. Unassociated prepared
   commands remain discoverable and must be cancelled during cleanup.
3. Route Runc exec through the launcher owner's existing auxiliary-command
   protocol. Release the adapter mutex before waiting for output: the owner must
   fence exec admission and retire its children, allowing concurrent cleanup.
   Close normal command admission before retiring all launchers, auxiliary exec
   attempts and network helpers. Then run owned cleanup commands and inspect
   OCI state, mounts, namespaces, forwarding and discovery. Release address
   reservations only after every admitted mutator has positively retired.
4. Preserve generation fencing across queued calls and separately constructed
   adapters. A clone created before sealing cannot obtain cleanup authority.
5. Prune positively retired short-command records under the same exclusive
   claim, so recurring runtime observations cannot grow the journal indefinitely.
   Failed or uncertain attempts must remain discoverable.
6. Switch production selection and complete agent launch-inventory recovery only
   after these paths pass actual Bun-death, missing-adoption-record, short-job,
   rootful/rootless and rolling-upgrade qualification.
