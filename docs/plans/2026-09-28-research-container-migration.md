# Research: moving a running container between nodes (0.2.0)

28 September 2026. Research and a recommendation only; no product code.
Awaiting maintainer review.

## Progress (for whoever resumes this)

- [x] Branch `research/container-migration` from `origin/main` (ff854cbf)
- [x] 1. Recommendation (summary)
- [x] 2. What the codebase does today (verified by reading the code)
- [ ] 3. CRIU, runc and the ecosystem today (sourced)
- [ ] 4. GPU checkpointing and where the demand really is (sourced)
- [ ] 5. Design for Reliaburger
- [ ] 6. Compatibility against the post-0.1.0 policy
- [ ] 7. Security
- [ ] 8. Observability and UX
- [ ] 9. Demo
- [ ] 10. Testing plan
- [ ] 11. Effort and phasing
- [ ] 12. Where the initial analysis holds and where it doesn't
- [ ] 13. Open questions for the maintainer
- [ ] Draft PR opened

Sections are filled in order and committed as each one lands.

## 1. Recommendation

Build the *move* first and the *memory* second. For 0.2.0, ship
`relish migrate` and a real `relish drain` that move one runc instance to
another node **stop-first**, carrying its managed volumes and its writable
rootfs layer. Add an opt-in `checkpoint` mode on top that also carries the
process memory with CRIU (through `runc checkpoint`/`runc restore`), and
falls back to a cold start **on the target, with the data already moved**
when CRIU refuses. Label the CRIU part experimental.

Why this order? Because the fallback the maintainer asked for (a clean
restart when CRIU says no) is itself a volume-carrying move, and Reliaburger
can't do one today. Managed volumes are node-local and the scheduler sends an
app back to the node that holds them (`VolumeHome`, `src/cluster/orchestrate.rs`).
So the "boring" half is the half every stateful app needs, it's
deterministic, and it's what Incus just shipped as "near-live" migration
after calling CRIU fragile ([Incus 7.4](https://linuxiac.com/incus-7-4-adds-near-live-container-migration-for-zfs-and-btrfs/), accessed 28 Sep 2026).
CRIU then turns a cold start into a warm one for the workloads it can handle.

Two corrections to the brief. First, there is no drain to hook into:
`relish drain` is still marked *planned* in the whitepaper, and the only
"node drain" in the code is a chaos fault. Second, planned binary upgrades
don't need migration at all, because runc owners outlive Bun and the new
Bun adopts running instances. The maintenance case that does need it is a
host reboot (kernel, firmware, hardware).

Keep it runc-rootful only. Drop established TCP connections and give the
instance a new address on the target; clients reconnect, which is what Google
did for Borg's CRIU migrations ([LPC 2018 slides](https://lpc.events/event/2/contributions/69/attachments/205/374/Task_Migration_at_Scale_Using_CRIU_-_LPC_2018.pdf), accessed 28 Sep 2026).
Position it for long-running batch jobs first (the only use with production
evidence), then single-instance dev and game servers. GPU warm starts are a
real market, but every shipping product we could verify uses gVisor or
NVIDIA's own CRIU fork-in-progress, we have no GPU hardware, and CUDA
checkpointing needs host RAM at least as large as GPU memory in use. That's
0.4 at the earliest.

Effort: roughly 11 to 14 focused weeks for 0.2.0, of which only about a third
is CRIU. See [section 11](#11-effort-and-phasing).

## 2. What the codebase does today

Everything here was checked against `origin/main` at `ff854cbf` on 28
September 2026. Nothing in `src/` or `docs/` mentions CRIU or checkpointing a
process; the word "checkpoint" only appears for the reconciler's
`applied-placements.json`.

**Runc runs in the foreground under an owner.** `RuncGrill::owned_start`
(`src/grill/runc/owned.rs`) launches `runc --root <state> run --bundle <dir> <id>`
as the `Launcher` role of a durable intent generation
(`src/grill/runc_intent.rs`). The owner is Bun re-executed as a helper
(`__process-exec-gate`) and survives Bun restarts. The container's stdout and
stderr are **regular files** opened in append mode (`output.stdout`,
`output.stderr`, `src/grill/process_owner.rs`); stdin is a pipe. That matters
below: runc's restore only re-attaches *pipe* descriptors.

**Every instance gets its own network namespace and a node-local address.**
`netns.rs` gives node *N* a `/23` from `10.0.0.0/8` and hands containers
addresses from it; the spec's `network` namespace is pointed at the
pre-created path. Addresses are not portable between nodes. Host ports are
published with nftables DNAT per instance.

**Writable image roots are private overlays.** `rootfs::mount_private` mounts
the shared, content-addressed image as the lower layer and keeps
`rootfs-upper` beside the bundle. Anything the process wrote outside its
volumes lives there and would have to travel with it.

**Rootful containers share one user namespace mapping.** Container ids
`0..65536` map to host `2_000_000_000..` on every node (`src/grill/userns.rs`),
so ownership is the same on the source and the target. There's no seccomp or
AppArmor profile in the generated OCI spec today.

**Managed volumes are per app, per node, not per instance.** The host path is
`volumes/<namespace>/<app>/<mount path>` (`VolumeManager::create_managed_volume`,
`src/grill/volume.rs`), with three backends: plain directory, sparse file +
ext4 + loop mount, or Btrfs subvolume with a qgroup limit (`src/grill/btrfs.rs`).
PR #267 found two instances of one app overlapping on one volume during a
surge-first rollout and made volume apps roll stop-first. Snapshots
(`src/grill/snapshot.rs`) are Btrfs-only and can be tarred and uploaded to
object storage. Host-path volumes are bind mounts of whatever the operator
named.

**Placements carry no instance identity.** A `Placement` is a node id and
reserved resources (`src/meat/types.rs`); `/v1/placements/{node}` serves an
app and a replica *count* per node (`NodeAssignment`,
`src/cluster/orchestrate.rs`), and the node names instances locally
(`InstanceIdentity`: namespace, app, optional deploy generation, ordinal). The
orchestration is desired-state and idempotent by design: "there is no
per-instance RPC whose failure needs bespoke bookkeeping". Migration is
exactly that kind of RPC, so it needs its own durable state machine.

**Volumes pull apps home.** When an app has no placement left, the leader
reserves it on the nodes in `DesiredState::last_placed_nodes`, the nodes
that hold its managed volumes, and waits for room there rather than place it
elsewhere.

**There's no operator drain or cordon.** The whitepaper lists
`relish drain <node>` as planned. What exists: the upgrade cordon
(`apply_upgrade_cordon`, used during rolling binary upgrades), the Smoker
fault `relish fault node-drain` (simulated graceful departure), and
`relish decommission-node`, which requires the operator to attest that the
node's workloads are already stopped.

**Upgrades keep workloads running.** Self-upgrade execs the new Bun, which
adopts the running instances through their owners (the V02 soak journal in
PR #267 shows "adopted 1 running instance(s)"). A reboot is different: the
intent journal refuses a generation "from a previous kernel boot" and the
instance cold-starts.

**Discovery withdraws before addresses are reused.** Endpoints are published
cluster-wide by the leader (`RaftRequest::PublishEndpoints`) from health
reports; a retiring instance's routes are withdrawn and every consumer
acknowledges the withdrawal (`AcknowledgeEndpointWithdrawal`,
`src/onion/withdrawal.rs`) before its address can be reused. A migrated
instance is a new endpoint on the target and a withdrawn one on the source,
which this machinery already handles.

**Egress rules are keyed on the workload cgroup and programmed before start.**
`honours_cgroup_path` lets Bun program egress between `create` and `start`.
`runc restore` has no such gap: it creates and resumes in one call.

**Formats.** Raft log entries and snapshots are JSON (`src/council/durable_log.rs`),
but `RaftRequest` is an enum, so a new variant is incompatible. `AppSpec`,
`DeploySpec` and `VolumeSpec` carry `#[serde(deny_unknown_fields)]`, so even an
optional field there is incompatible. `NodeAssignments` has no
`deny_unknown_fields` and already grew an optional `ingress` field. The
`IntentConfiguration` and owner records are node-local and also
`deny_unknown_fields`. `CURRENT` is protocol 27, state 44.

**Test infrastructure.** Guest images are Ubuntu 24.04 (arm64 and x86_64) with
`runc uidmap btrfs-progs nftables iptables iproute2`
(`scripts/release/guest-images.json`). No CRIU. Ubuntu 24.04 has no `criu`
package in the archive at all; the upstream PPA publishes 4.2.1 for noble
([Launchpad criu](https://launchpad.net/ubuntu/+source/criu),
[CRIU PPA](https://launchpad.net/~criu/+archive/ubuntu/ppa), both accessed 28 Sep 2026).
CI runs on `ubuntu-latest` and has a "privileged Linux" job.
