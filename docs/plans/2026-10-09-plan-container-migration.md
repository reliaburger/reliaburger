# Plan: moving running workloads between nodes

9 October 2026. Releases: 0.4.0, 0.4.1 and 0.5.0. Pull request:
[#268](https://github.com/reliaburger/reliaburger/pull/268). GitHub milestone:
[0.4.0](https://github.com/reliaburger/reliaburger/milestone/7).

This is the delivery plan: what we'll build, in what order, and which tests
prove it. The research behind it, the continuity contract and the long-form
design live in
[Research: moving a running container between nodes](2026-09-28-research-container-migration.md),
which we cite as "the research" below. Nothing here is implemented yet.

## Who this is for

Picture a team of three running a dozen apps on four machines in a cupboard, or
a shop with one small cluster per site. They have a PostgreSQL that isn't
replicated, because running Patroni for one database is more work than the
database. They have a Redis that's "just a cache" until it's cold on a Monday
morning. A kernel CVE lands. Today they schedule downtime, warn their users and
reboot one box at a time at 7 a.m.

That's the user. Small clusters, stateful singletons, nobody on call for the
orchestrator, and nodes that need patching every week. Reliaburger already
targets them: one binary, a laptop cluster from one command, and bare metal in
an hour (0.3.0). What they lack is a way to take a node down without taking
their singletons down with it.

The pitch we're building towards is **"your nodes patch and reboot themselves
every week, and your database doesn't notice."** Stage 2 delivers that with a
short pause and reconnecting clients; stage 3 removes the reconnect.

Who it isn't for:

- Workloads that are already replicated with their own failover. A rolling
  restart serves them better, and drain does exactly that for them.
- Large cloud fleets on VPC networking, where nodes are cattle and workloads
  are built to die. Live moves are out of scope there (see the network design).
- GPU workloads and fast LLM warm starts, which want a snapshot store, not
  migration (research, section 4).

Kubernetes doesn't move a running container between nodes today; the
checkpoint/restore KEP excludes cross-node restore from its initial scope
(research, section 1). That's a useful contrast, but it isn't the reason to
build this. The reason is that our users can't patch their nodes without an
outage.

## Decisions

These are the decisions in force. The research's section 12 keeps the history
of how we got here (28 September, 5 and 6 October).

1. Moves use the runc CLI under the existing runtime owners, not a new CRIU
   client. ProcessGrill and the Apple runtime refuse checkpoint and live moves.
2. A move has a mode: `cold` (restart, carry managed data), `checkpoint`
   (memory, execution and filesystem; TCP may close) or `live` (checkpoint plus
   preserved sessions within an interruption budget). The research's section 1
   defines what each mode must preserve.
3. Checkpoint and live never fall back to a cold restart unless the workload's
   policy allows it explicitly, and then the result is reported as degraded.
4. Target activation is the one-way recovery boundary. After it, no timeout or
   error authorises restoring the source's older checkpoint. Uncertain ownership
   holds the workload as recovery-required (research, section 5.6).
5. Plaintext process dumps stay in bounded no-swap memory and never reach disk.
   Transfers are authenticated and encrypted.
6. CRIU runs without root if S21 shows it can, and otherwise in a confined
   helper that can't see Bun's state or keys. Checkpointing is opt-in per
   workload and can be switched off per node (research, section 7).
7. Pre-1.0 rules apply: an incompatible format bumps the compatibility
   generation from the then-current main, with a fresh cluster and no migration
   code.
8. Host-path apps block drain. `--force` stops them with their data left in
   place, and says so.
9. GPUs, rootless runtimes, automatic rebalancing and cross-architecture restore
   are out of scope.
10. The work ships in three stages and releases, 0.4.0, 0.4.1 and 0.5.0, not as
   one release (9 October, below). A spike
   that fails removes its mode from the series; it never holds back the modes
   that already work.
11. The book gets a new chapter for this work, written alongside each step.

## Scope: three stages, each shippable

The 28 September decision put everything into one 0.4.0: drain, cold moves,
checkpoint, pre-copy, post-copy, TCP handoff, movable addresses, NAT and ingress
ownership, credential mobility, recoverable keys, a conformance framework, and
Redis and PostgreSQL on both architectures. The research's old estimate for a
weaker version of that was 23 to 29 weeks, and the 5 October contract withdrew
it without a replacement. A release whose gate depends on open research can
stall indefinitely. Incus, which has shipped CRIU moves for years, still says
only "very basic containers" move reliably, and Borg chose to drop connections
rather than preserve them.

So we ship in three stages. Each stage is useful on its own, has its own exit
test, and never weakens the contract of the stage before it.

| Stage (release) | What the operator gets | Modes | Exit test |
|---|---|---|---|
| 1. Planned maintenance (0.4.0) | Cordon, drain and uncordon. Restartable workloads reschedule; volume apps carry their data; memory-only apps keep their memory. Connections close and clients reconnect. | `cold`, `checkpoint` | Drain a node running a memory-only Redis and a volume app; both come back on another node with every acknowledged write, and the drained node holds nothing. |
| 2. Maintenance without downtime (0.4.1) | The appliance's A/B OS update drains, reboots and uncordons each node in turn, moving workloads ahead of the reboot. | `cold`, `checkpoint` | A rolling OS update of a three-node cluster keeps Redis's data and a volume app's data across every reboot. |
| 3. Live moves (0.5.0) | Moves that keep established connections and meet an interruption budget, with no dependency left on the source. | `live` | The research's source-off test (section 9.5) with Redis and PostgreSQL. |

Stage 3 starts only after spike S11 says yes (see the go/no-go rules in the
spikes). If they say no, live moves come off the roadmap rather than holding
up stages 1 and 2, and the manual says connections reconnect.

Jobs follow the same stages. In stage 1, bulk array tasks with attempts left
requeue under their own retry policy (#642 already documents them as
at-least-once), and single-attempt work such as deployment hooks blocks drain
until a checkpoint move can carry it. The research's section 5.1 has the
admission rules.

## Rolling OS updates and mixed kernels

Stage 2 is where this feature pays for itself. 0.3.0's appliance builds a signed
OS image every week and installs it into the inactive A/B slot
([roadmap](../roadmap.md), merge train
[#490](https://github.com/reliaburger/reliaburger/pull/490)). Today, activating
that slot means rebooting the node and restarting everything on it. With moves,
the update controller can drain each node with migration intent, wait until
every workload reports source-independent completion, reboot into the new slot,
check health and uncordon, one node at a time.

That rollout is also the hardest case for compatibility, and the research
didn't name it. Halfway through, the cluster runs two kernels (and possibly two
CRIU builds), and every move crosses between them. So:

- **Restore across kernels is a named, qualified direction.** New spike S20:
  checkpoint on the current appliance kernel and restore on the next weekly
  build's kernel, on both architectures, with Redis and the mechanism fixture.
  The reverse direction (new to old) matters when an A/B update rolls back; it
  is refused unless S20 qualifies it.
- **CRIU versions too.** If the weekly image bumps CRIU, a dump from the older
  CRIU must restore on the newer one. A newer-to-older restore is refused.
- **The pool fingerprint records the kernel and CRIU versions**, and compatibility
  is directional (research, section 9.4). Admission refuses a direction that
  hasn't been qualified, naming it, rather than finding out during restore.
- **Prefer updated targets.** The planner moves workloads to nodes already on the
  new image, so most workloads move once per rollout and always old-to-new.
  Only the first node's workloads move between two old-image nodes.
- **Stop on doubt.** A move that ends in recovery-required pauses the rollout and
  leaves the node cordoned. The operator sees which workload and why.
- **Same hardware.** A rolling update doesn't change CPUs, so the CPU feature check
  stays defence in depth here, not the main risk.

Stage 2 depends on 0.3.0 shipping the A/B update path. If 0.3.0 slips, stage 2
still delivers `relish drain` plus a documented manual reboot loop on Ubuntu
nodes, and the automated rollout follows when the appliance lands.

## Metrics, logs and alerts

A move must not make a workload's history disappear or page someone for a
planned pause. Today both Mayo and Ketchup find an app's data through its current
placement, so they'd do both. The research's section 8 has the design: queries
fan out to every node that hosted the app within retention, `instance` stays the
logical replica while `node` changes, the freeze budget is excused from alerts
and health restarts but an overrun isn't, recovery-required raises a critical
alert, and every move phase is an event on the dashboard timeline. Each stage's
exit test checks logs and metrics across the move, not just the workload.

## The demonstration

`relish test --filter move` moves **Redis** A to B to A on the operator's own
cluster and passes or fails on Redis alone. PostgreSQL, with its open
transaction and in-flight query, is required in `relish test --profile move` and
in release qualification, but it doesn't gate the demo. Redis is the case
people recognise and the cheapest one to make work everywhere, including the
arm64 laptop cluster; PostgreSQL is the case that proves the contract.

What the demo asserts follows the stages: stage 1 proves the in-memory data
survives and reports one reconnect per move; stage 3 also requires the original
connection to survive. The research's sections 9.1 and 9.2.1 have the detail.

## The laptop cluster comes first

Most people meet Reliaburger through `curl | sh`: three Lima VMs on a laptop,
usually an Apple-silicon Mac. If the move demo doesn't work there, most people
will never see it. So **every stage's demo must pass on the default quickstart
cluster**, on Apple silicon (Lima's VZ driver, arm64 guests) and on Linux (QEMU,
x86_64 guests), before that stage ships. A bare-metal pass doesn't count
instead.

What that means in practice:

- **CRIU in the guest image.** `scripts/release/build_guest_image.sh` builds the
  Ubuntu 24.04 guest from a pinned package list, and the VMs install nothing at
  first boot. CRIU joins that list at a pinned version of at least 4.2. If
  Ubuntu's archive doesn't carry a suitable build for 24.04, the image build
  installs our own pinned, signed one; first boot still installs nothing. The
  guest-image qualification record gains `criu check --all` from both
  architectures.
- **The guest kernel.** S1 runs `criu check --all` in the Lima guest on both
  architectures and records what's missing: checkpoint/restore support, time
  namespaces, `userfaultfd`, `TCP_REPAIR`. arm64 mainline kernels have no
  soft-dirty tracking (research, section 3), so on Apple silicon there's no
  pre-copy and every checkpoint move is a full stop-and-copy.
- **The pause on a Mac.** Without pre-copy, the pause is dump, transfer and
  restore of the whole working set. For the demo's 64 MiB Redis, between two
  VMs on one laptop, we expect well under a second; S7 measures it and the
  manual prints the measurement, not this estimate.
- **Memory.** Quickstart VMs have 2 GiB each. The dump stays in no-swap memory on
  both nodes, so the demo's Redis stays small, and admission refuses a move
  whose dump wouldn't fit, naming the shortfall, rather than letting the source
  get OOM-killed.
- **What it doesn't prove.** Three VMs on one host share one virtual switch. The
  laptop demo proves the mechanism and the contract, not behaviour on a real
  network; release qualification adds bare-metal and multi-host runs.

## Naming

"Migration" already means three things in Reliaburger. A `run_before` job is a
schema migration in the manual (`docs/manual/01_deploy-an-app.md`), the
Kubernetes importer prints a "migration report", and the cluster apply plan for
prerequisite jobs is called cluster migration
([plan](2026-10-04-cluster-migration-prerequisites.md)). Adding a fourth would
make `relish test --filter migration` and every error message ambiguous.

So everything the operator types or reads says **move**:

| Surface | Name |
|---|---|
| Command | `relish move <namespace>/<app> --to <node>`, `relish move status`, `relish move cancel` |
| App and job policy | `[app.<name>.move]` and `[job.<name>.move]`, with `mode = "cold" \| "checkpoint" \| "live"` |
| Built-in tests | `relish test --filter move`, `--profile move`, `--chaos --profile move-recovery` |
| Records and metrics | `MoveRecord`, `move_id`, `reliaburger_moves_total` |

Prose can still call the feature "container migration" or "live migration",
because that's what people search for. The roadmap headline keeps it. Drain,
cordon and uncordon keep their usual names; `relish fault node-drain` stays the
simulated Smoker fault and the manual says how it differs from a real drain.

## Spikes

None of these are results yet. Run them on the then-current main's generated
OCI spec, pinned runc and CRIU packages and real kernels. x86_64 and arm64 need
separate evidence.

The research listed nineteen spikes with no order. Three of them decide whether
a whole stage exists, so they run first, each with a time box, a written "yes"
and a stated consequence for "no". Every spike lands as a pull request: a gated
test (`make test-linux` or `make test-cluster`) that encodes the finding, plus a
short record in `docs/qualification/`. A spike that overruns its time box counts
as "no" for the stage until someone reopens it on purpose.

### Wave 1: does stage 1 exist? (before any checkpoint code)

| Spike | Time box | "Yes" means | "No" means |
|---|---|---|---|
| S12: partition-safe fencing and activation (built as the first step of Cold moves) | 1 week | A model and property test show at most one generation can run across leader change, partition and Bun restart, using main's existing runtime and storage fences. | No moves ship beyond the Drain milestone. Redesign before going further. |
| S-A: S1 + S5 + S21, restore on our real spec | 1 week | `runc checkpoint`/`restore` round-trips a container with our rootful user namespace, external network namespace and private overlay, the runtime owner adopts it (S5), and CRIU runs unprivileged or in the confined helper (S21), on x86_64 and on the arm64 Lima guest. | Checkpoint mode is dropped; stage 1 ships cold moves only. |
| S-B: S2 + S4, capture streams and egress ordering | 1 week | Capture files keep ingesting across restore (S2), and the restored workload can't run an instruction before its cgroup egress policy is enforced (S4). | S2: checkpoint waits for owner-managed pipes. S4: workloads with egress policy refuse checkpoint moves. |

### Wave 2: inside stages 1 and 2

S13 (consistent filesystem cut, Cold moves), S15 (no-swap staging and key
recovery), S8 (refusal fixtures) and S6 (clocks, all Checkpoint moves), S17 (test
leases, Drain that keeps state) and S14 (credentials, Moves across updates) run
as the first task of the step that needs them, within that step's week.
Until S14 picks a credential path, apps that hold a workload identity
certificate refuse checkpoint moves rather than restoring a key we then revoke.

S20 (restore across kernel and CRIU versions) runs at the start of stage 2; if
it fails, stage 2 ships drain with cold moves only for mixed-kernel rollouts.

### Wave 3: does stage 3 (0.5.0) exist? (after 0.4.1 ships)

| Spike | Time box | "Yes" means | "No" means |
|---|---|---|---|
| S11: movable network ownership | 2 weeks | The network design below keeps an established in-cluster TCP session, an outside client's session on the same L2 segment, and an outbound session across A to B to A, with the source switched off afterwards. | Live moves come off the roadmap. Stages 1 and 2 stand. |
| S7 + S9: interruption envelope | 1 week | On both architectures, a 256 MiB Redis moves with a client-observed pause we're willing to print in the manual (target: under a second on x86_64 with pre-copy, under three on arm64 without it). | Live mode ships only where the envelope holds; elsewhere `live` is refused. |

S10 (lazy pages), S16 (database fixtures), S18 (conformance completeness) and S19
(source-off) follow inside 0.5.0's milestones.

### The network design S11 tests

Today an established in-cluster connection isn't addressed to the workload at
all. Onion's eBPF `connect()` hook rewrites the service VIP to the backend's
**node IP and host port**, and host-port DNAT forwards it into the container
([discovery design](../design/discovery-onion.md)). Every client socket
therefore names the source node's address, and no amount of TCP_REPAIR on the
target can answer for it. Outbound connections leave through source masquerade,
so the remote peer sees the source node's IP too. Live moves need the workload
to own its addresses instead.

Proposed design, which S11 confirms or rejects:

- **A movable workload address.** Each replica with `mode = "live"` gets a
  cluster-unique `/32` from a dedicated range, recorded in Raft with an
  ownership generation. Onion's backend map points at that address and the
  container port, with no host-port DNAT, and every node installs a host route
  for the address via its current owner. A move updates the route with the
  activation epoch; nodes acknowledge before the source releases the address.
- **Clients outside the cluster** reach live workloads through Wrapper, or
  directly on the same L2 segment, where the owning node answers ARP (NDP for
  IPv6) for the address and sends a gratuitous announcement at activation.
- **Outbound traffic** from a live workload uses its own address as the source,
  without masquerade, so its sessions don't depend on the source node's NAT
  table. Where the upstream network won't route that address back, outbound
  sessions aren't preserved and the move reports it.
- **Ingress.** Wrapper holds the client-side socket, so a live move only keeps
  an ingress session if Wrapper runs on another node. Drain refuses to call a
  node source-independent while it still terminates required ingress sessions.
- **Where it works.** The quickstart's Lima network, a bare-metal LAN (the
  0.3.0 Wyse cluster) and any routed network the operator controls. Cloud VPCs
  that drop traffic for unknown `/32`s need the provider's route API; that's out
  of scope, and a pool on such a network reports live moves as unqualified.

The forwarding-through-the-source prototype from the research (section 5.3)
stays a TCP_REPAIR test harness only.

### Spike reference

| Spike | Question and consequence |
|---|---|
| S1 | Checkpoint/restore with rootful userns, external netns, private overlay, volume backends and resolv.conf. Unsupported combinations block their claimed mode. |
| S2 | Inherited/file/pipe stdio and capture ingestion across restore. Resolve owner changes before checkpoint ships. |
| S3 | Preserve logical ordinal/job execution with new runtime/host identity, time namespaces and declared checkpoint TCP closure. |
| S4 | Enforce cgroup egress before any restored workload executes; reject unsupported ordering. |
| S5 | Restored init parentage, boot/start-tick evidence, Bun adoption, stop/exit receipts and stale-generation rejection. |
| S6 | Cross-host timers/monotonic time and remote timeout/lease behaviour within a declared budget. |
| S7 | Memory/volume/rootfs payload and client-observed pause for realistic sizes/rates; set envelopes from results. |
| S8 | Release-specific resource matrix and refusal fixtures for io_uring, devices, attached tracers, packet pipes, corked UDP and unsupported IPC/socket/exec dependencies; supported-counterpart round trips. Exercise late resource acquisition, actual dump rejection and source continuation/state/session correctness; no implicit cold fallback. |
| S9 | Actual dirty tracking/pre-copy and convergence on qualified x86_64/arm64 hosts; select from capabilities. |
| S10 | Lazy-page TLS/manifest binding, source/provider loss, fault stalls, provider independence and optional redundant-page recovery. |
| S11 | TCP_REPAIR with packet locking, borrowed-address prototype and source-independent address/NAT ownership, ingress and repeated moves. Prototype success alone cannot pass the release gate. |
| S12 | Partition-safe source retirement and target activation with current main's runtime/storage/recovery fences; no timeout-authorised second writer or stale replay. |
| S13 | Consistent frozen filesystem cut, metadata/whiteouts/unlinked files, same-size/mtime modifications, quota and loop/Btrfs provisioning. |
| S14 | Credential mobility or tested reload/rotation, fresh authenticated sessions and source retirement authority. |
| S15 | No-swap reservations, protected transfer-key recovery across Bun/owner/node loss, staged import confinement and interruption cleanup. |
| S16 | Tiny signed/pinned mechanism fixture plus Redis (the demo gate) and PostgreSQL worker-mode (conformance), on both architectures. Prove memory-only/persistent data, original sessions, open SQL transactions and active queries with independent ledgers and no reconnection/retry masking (research, section 9.2.1). |
| S17 | Migration under authenticated test leases; expiry/stop/cleanup at every boundary on both nodes. |
| S18 | Versioned profile completeness, directional pools and recorded evidence; partial/skip/unknown cannot certify conformance. |
| S19 | Actual drain followed by source VM shutdown/restart under live traffic, with independent entry/observer topology and no resurrection. |
| S21 | Can runc drive CRIU unprivileged in the container's user namespace with `CAP_CHECKPOINT_RESTORE` on our spec? If not, prove the confined helper: reduced capabilities, private mount namespace masking Bun's state and keys, cgroup limits, no inherited descriptors. |
| S20 | Restore from the current appliance kernel and CRIU to the next weekly build's, on both architectures; new-to-old refused unless qualified. Runs at the start of stage 2. |

## Tests

**Portable/model tests first:** state transitions and epochs; delayed/duplicate
messages; target activation uncertainty; stale source rollback forbidden;
capability/policy refusal; job execution/retry binding; manifests/encryption/
metadata import; reservation concurrency; lease expiry/cancel; profile completeness
and verdict aggregation. A property test interleaves leader changes, partitions
and crashes and checks exclusive ownership and acknowledged-state monotonicity.
It must allow recovery-required unavailability: universal terminal success by a
deadline is not an invariant.

**Portable integration:** use the existing in-process cluster/public API harness
in `tests/suite/` with mock runtime outcomes, including restore that executes then
returns an error and an isolated target that is still running. These tests protect
orchestration; they do not count as real CRIU or connection-preservation evidence.

**Gated Linux/cluster:** real runc/CRIU with current owners on one host for runtime
integration, then actual cross-node movement and data paths on both architectures.
Match `make test-linux`/`make test-cluster` and add a `test-move` gate only if ownership
and discoverability need it. Once selected, missing prerequisites fail/refuse;
never return early as a passing gated test. CI initially proves which hosted
runners can support CRIU; unavailable hardware gets an explicit owner and release
qualification lane, not a green mock substitute.

**Built-in operator cases:** short demonstration and complete migration profile
run the same Redis/PostgreSQL fixtures/scenarios/assertions with different
coverage/load, alongside mechanism fixtures in conformance. Recovery
cases share observations but use explicit chaos selection and node-state authority.
Wire catalogue, capabilities, lease/authz routes, reports and built-in manual
consistently; verify the documented commands select real required cases.

**Release/soak:** no separate move soak. "Move" and "drain" become fault kinds
in the V02 sustained schedule, so the release's existing soak repeats A->B->A
under load with the volume writer, memory-only counter and the required
Redis/PostgreSQL fixtures (see "Keeping qualification affordable"); exercise
main's recovery faults and each database's source-off case. Cover both architectures, every claimed
backend, ingress/egress
paths, jobs and credential continuity. Retain acknowledgement/session ledgers,
per-cutover latencies, generation transitions, fingerprints and cleanup outcomes.
Retries must not erase first failures. No pass on an empty run or skipped-only lane.
Main's release qualification scripts own hardware/soak execution and save records
in `docs/qualification/`; reuse the versioned conformance manifest so release
checks cannot drift into a separate weaker demonstration.

**Packaging:** package pinned qualified CRIU/runc in the quickstart guest image
(see "The laptop cluster comes first"), the appliance and managed Linux guests, check Ubuntu 26.04 availability before depending on a PPA, and expose
runtime capabilities in `wtf`/dry-run. No automatic PPA installation on an existing
cluster from `relish test`. Verify signed mechanism and Redis/PostgreSQL fixture
image availability before timing; support existing mirrors/local staging rather
than depending on a mutable tag.

## Releases and milestones

Three releases, one per stage. Inside each release, **milestones** are things
an operator can use, each with its own exit test, and each milestone is built as
a few **steps**: pull requests of about a week for a developer who knows the
codebase, so reviews stay small. Spike PRs carry no release of their own; they
land a gated test and a qualification record, and decide whether the milestone
they open goes ahead.

| Release | Stage | Milestones |
|---|---|---|
| 0.4.0 | 1. Planned maintenance | Drain; Cold moves; Checkpoint moves; Drain that keeps state |
| 0.4.1 | 2. Maintenance without downtime | Moves across updates; Rolling OS updates |
| 0.5.0 | 3. Live moves | Movable addresses; Live sessions; Live qualification |

Live moves get their own minor release because "connections survive" is a
headline of its own, and because 0.5.0 simply doesn't happen if S11 says no. On
GitHub, each release is a milestone, each named milestone below is an issue,
and each step's pull request says "Fixes #N" against it.

That's about 26 developer-weeks in sequence: 14 for 0.4.0, 4 for 0.4.1 and 8
for 0.5.0. With a second person on the spikes, 0.4.0 comes down to about 10.
Treat the total as a plan to check against, not a promise: the research's
estimate was withdrawn for good reasons, and the spikes exist to find out what
we don't know.

### Keeping qualification affordable

A release runs the full [release runbook](../releasing.md): a staged install on
Apple silicon, the V02 fast tier (about 90 minutes) after every round of fixes,
and one 8-hour final tier. That's affordable three times, not twenty. So:

- **A step merges on its tests.** Failing tests first, `make ci` plus the gated
  target it touches, and anything unfinished refused with a named reason, so main
  stays releasable as it is today.
- **The compressed soak runs on main after each step merges**, through the
  existing `soak.yml` workflow on a hosted runner (90 minutes, no human time).
  Slow regressions surface while the change is fresh, so a release needs fewer
  rounds of fixes and fast-tier reruns.
- **Moves join the V02 soak instead of getting their own.** "Move" and "drain"
  become fault kinds in the sustained schedule, with Redis and a volume writer
  among the workloads. The release's existing soak then covers repeated moves
  under load; the research's separate move soak goes away.
- **The laptop demo is a release check**, run once per release on the quickstart
  cluster on Apple silicon and on Linux, not after every step.

Every step still follows the project rules: the manual
(`docs/manual/12_operations.md` for drain and moves) and README updated, a
compatibility bump from the then-current main where formats change, and the new
book chapter (working title "Moving day", before the Rust appendix) extended in
the same PR.

### 0.4.0: planned maintenance

#### Drain

`relish cordon`, `relish uncordon`, `relish drain <node> [--timeout] [--force]`
and `relish drain status`, for workloads that can restart. One step.

An operator cordon becomes a Raft record, generalising today's upgrade cordon
(`apply_upgrade_cordon`), behind the administrative node-state grant. Drain
reschedules replicas that may restart, surging before stopping where the app has
more than one replica, and requeues bulk array tasks with attempts left.
Everything it can't move yet (host-path apps, managed volumes, single-attempt
jobs, `mode = "checkpoint"` or `"live"`) blocks the drain with a named reason,
and `--force` stops it and says so. Tests first: the scheduler places nothing on
a cordoned node across leader change; a property test shows the drain planner
never drops an app below its availability budget; a cluster test drains one node
of three and uncordons it.

**Exit:** `relish drain` empties a quickstart node of restartable workloads and
names the rest.

#### Cold moves

`relish move <ns>/<app> --to <node> --mode cold`, `relish move status` and
`relish move cancel`, and drain of apps with managed volumes on every backend.
Three steps.

1. **Move records and fencing** (spike S12 built as product code). A
   `MoveRecord` in Raft carries the move id, ownership epoch, logical replica
   (namespace, app, ordinal), source and target generations and phase, following
   the research's state machine (section 5.6). Targets hold an incoming
   assignment that ordinary reconciliation can't duplicate; the source persists a
   retirement fence in its node journal before it reports. Cold moves of replicas
   without volumes work. Tests first: a property test interleaves leader changes,
   partitions, duplicate and late messages and Bun restarts, and checks that at
   most one generation runs and no stale replay happens; `tests/suite/` covers a
   mock restore that runs and then reports an error. **Go or no-go:** if the
   property test can't be made to hold with main's fences, stop and redesign;
   0.4.0 then ships Drain alone.
2. **Plain-directory volumes.** Node-to-node chunked transfer over the existing
   node mTLS identity, bound to the move manifest; import into a private staging
   path with path-traversal and link confinement; fsync, rename and a durable
   receipt before the volume home switches, atomically with activation; a source
   tombstone labelled with its cut and epoch, kept until retention allows
   deletion. Ownership, modes, xattrs, ACLs, hardlinks, sparse files and
   deletions survive (S13). Tests first: a crash between each write, rename and
   acknowledgement; a path-traversal manifest is refused.
3. **Loop-ext4 and Btrfs.** Loop-ext4 re-provisions the target image and quota
   bookkeeping; Btrfs uses snapshot and incremental `send`/`receive` for
   pre-sync, then a final send after the stop. Tests first: the target quota is
   enforced after the move; a same-size, same-mtime change is caught.

**Exit:** drain a node running a volume app that journals its writes; every
acknowledged write is on the target, on each backend (`make test-linux`).

#### Checkpoint moves

`relish move --mode checkpoint`: memory, execution and filesystem move; TCP
connections close, as the checkpoint contract declares. Two spikes, then four
steps.

1. **Spike S-A: restore on our real spec, and CRIU without root.** `runc
   checkpoint` and `restore` of a container built by today's `RuncGrill`, with
   the rootful user namespace, external network namespace and private overlay
   (S1), adopted by the runtime owner afterwards (S5), on x86_64 and in the arm64
   Lima guest. In the same week, S21: can CRIU run unprivileged in the
   container's user namespace? **Go or no-go** for this milestone: if it fails,
   Checkpoint moves drops out and Drain that keeps state uses cold moves only.
2. **Spike S-B: capture streams and egress ordering.** S2 picks CRIU inherited
   descriptors or owner-managed pipes for move-enabled apps' stdout and stderr.
   S4 proves the restored workload can't execute before its cgroup egress policy
   is in place, or names the workloads that must refuse.
3. **CRIU in the images, capabilities and dry-run.** A pinned CRIU of at least
   4.2 joins the guest image and the Linux packages. Each node probes and reports
   its move capabilities (CRIU and runc versions, `criu check`, kernel, page
   size, CPU features, soft-dirty, `userfaultfd`, time namespaces, TCP repair) as
   a pool fingerprint. `node.toml` can switch checkpointing off. `relish wtf`
   shows the capabilities, and `relish move --dry-run` explains what would move
   and what refuses.
4. **Checkpoint and restore in place under runtime owners.** Through the
   invocation S-A chose, with S-B's capture and egress results, time namespaces
   created at launch for move-enabled apps (with S6's cross-host timer checks),
   adoption after a Bun restart mid-restore, and an audit event per dump and
   restore. Test: `make test-linux` checkpoints a counter app, restores it on the
   same node and the counter carries on; a Bun restart during restore resolves
   to exactly one running generation.
5. **Cross-node, without volumes.** The dump goes into reserved no-swap staging
   (S15), travels encrypted and bound to the move manifest with the overlay upper
   directory, and restores on the target at activation. Apps holding a workload
   identity certificate refuse until Moves across updates. Tests first: admission
   refuses a move whose dump wouldn't fit in memory; a lost transfer key before
   activation re-keys rather than restarting.
6. **Volumes and safe refusals.** The final volume cut and the memory image share
   one frozen cut and generation. S8's refusal fixtures (io_uring, an attached
   tracer, a packet-mode pipe, a corked UDP socket) refuse with the blocking
   resource named and the source still running, including a resource that
   appears after admission and is caught at dump time.

**Exit:** a memory-only counter app moves A to B to A on the quickstart cluster
on both architectures and keeps counting; a volume app keeps its writes and file
offset; every refusal fixture leaves its source healthy.

#### Drain that keeps state

Drain follows each workload's move policy, and you can watch it work. Four
steps.

1. **Drain with migration intent, and the Redis demo.** Checkpoint apps move by
   checkpoint, memory-only apps with no volume included; `live` apps block until
   0.5.0 unless their fallback allows checkpoint. Singletons need an explicit
   availability budget for the pause. `relish test --filter move` ships with the
   Redis fixture under test leases (S17), asserting stage 1's contract: every
   acknowledged write survives and one reconnect per move is reported.
2. **Metrics, logs and alerts across moves.** The research's section 8 design:
   queries fan out to every node that hosted the app within retention, `instance`
   stays the logical replica, only the declared freeze is excused from alerts and
   health restarts, recovery-required raises a critical alert, and move phases
   appear on the dashboard timeline.
3. **Job tasks.** A running single-attempt task moves with its run id, task index
   and attempt intact, and the worker ledgers on both nodes record the handover
   without consuming a retry (#642's model). Drain stops blocking on hooks and
   single-attempt jobs whose policy allows checkpoint.
4. **Recovery and source-off.** `relish test --chaos --profile move-recovery
   --yes` ships: Bun crashes at each phase, a leader change, a partition at
   activation, target loss after acknowledged writes, a late duplicate
   instruction, and the decisive case: drain, stop the source VM, check the
   workload, restart the source and confirm the retired generation can't come
   back. Recovery-required gets its operator view and action. "Move" and "drain"
   join the V02 soak's fault kinds.

**Exit, and the 0.4.0 release gate:** stage 1's exit test, the Redis demo and
the recovery profile on the quickstart cluster on Apple silicon and Linux,
`relish logs` showing lines from both nodes across a move, a job task finishing
once after moving mid-computation, and the release runbook's soak with moves in
its schedule.

### 0.4.1: maintenance without downtime

#### Moves across updates

Moves work between nodes on different appliance builds, and apps with workload
identity can move. A spike, then one step.

1. **Spike S-C: restore across kernel and CRIU versions (S20).** Checkpoint on
   the current appliance kernel and CRIU, restore on the next weekly build's, on
   both architectures. The result sets which directions admission allows; if it
   fails, mixed-kernel rollouts use cold moves only.
2. **Credential continuity (S14).** Pick and build one path: moving the
   workload's credential with the fenced source, or a reload hook that fetches a
   fresh certificate before the old serial is revoked. The refusal for apps with
   workload identity lifts.

**Exit:** a checkpoint move from an old-build node to a new-build node succeeds,
the reverse is refused by name, and a moved app's fresh authenticated connection
succeeds with its new certificate.

#### Rolling OS updates

The appliance updates itself node by node without restarting its singletons.
Two steps.

1. **The rollout.** The A/B update drains each node with migration intent,
   prefers targets already on the new image, waits for source-independent
   completion, reboots, checks health and uncordons. A recovery-required move
   pauses the rollout. Off the appliance, `relish drain` and `relish uncordon`
   around a manual reboot do the same, and the manual shows the loop. Depends on
   0.3.0's A/B updates; if they haven't landed, this step ships the manual loop
   and the controller follows them.
2. **The conformance profile, with PostgreSQL.** `relish test --profile move`
   ships with a versioned required-case manifest (S18): a filtered run is
   reported as partial, and missing, skipped or unknown cases fail it.
   PostgreSQL 18 in worker I/O mode joins as a required case: committed writes
   survive a checkpoint move, and clients reconnect.

**Exit, and the 0.4.1 release gate:** stage 2's exit test, and `--profile move`
passing on the quickstart cluster.

### 0.5.0: live moves

0.5.0 starts only if S11 says yes.

#### Movable addresses

Live-mode replicas own an address that follows them. A spike, then one step.

1. **Spike S-D: movable network ownership (S11), two weeks.** Prototype the
   network design above: a routed `/32` per replica with ownership generations,
   ARP on the L2 segment, no masquerade, then TCP_REPAIR with packet locking
   across A to B to A with the source switched off. **Go or no-go** for 0.5.0.
2. **Workload addresses.** A cluster-unique address with a Raft ownership
   generation; Onion's backend map points at it directly instead of node IP and
   host port; every node routes it via its current owner and acknowledges route
   changes; the owner answers ARP on the L2 segment.

**Exit:** with a cold move, the address follows the replica and new connections
reach it from every node.

#### Live sessions

`mode = "live"`: established connections survive. Two steps.

1. **Inbound sessions.** `--tcp-established` with packet locking from freeze to
   activation, and the route switch at activation, for workloads whose sessions
   are all inbound. The Redis demo adds stage 3's assertion: the original
   connection survives.
2. **Outbound and ingress sessions.** Live workloads send from their own address
   without masquerade, so outbound sessions don't depend on the source's NAT
   table; drain won't call a node source-independent while it still terminates
   required ingress sessions.

**Exit:** the Redis demo keeps the same client socket across A to B to A on the
quickstart cluster, and an outbound session and an ingress WebSocket survive a
move followed by switching the source off.

#### Live qualification

Live moves meet a stated pause, and PostgreSQL proves the contract. A spike,
then two steps.

1. **Spike S-E: interruption envelope (S7, S9).** Measure the client-observed
   pause for a 256 MiB Redis on x86_64 with pre-copy and on arm64 without it,
   and set the envelopes the manual will print.
2. **Pre-copy and interruption budgets.** Iterative pre-dump on hosts with
   soft-dirty tracking, stopping when the dirty set fits the budget, stops
   shrinking or hits a round limit; `max_interruption` in the app's move policy,
   admission against measured throughput, and the observed pause in status and
   events. `relish bench` gains move envelopes.
3. **Live PostgreSQL and source-off (S16, S19).** PostgreSQL's open transaction
   and in-flight query survive A to B to A on their original sessions, and the
   full recovery profile runs against live moves, source-off included.

**Exit, and the 0.5.0 release gate:** stage 3's exit test, the S-E Redis within
its envelope on x86_64 with arm64's full-copy pause reported honestly.

Lazy pages (S10) aren't scheduled. They only help when pre-copy can't converge,
and they leave the workload depending on the source until every page arrives,
so they wait for evidence that someone needs them.
