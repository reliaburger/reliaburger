# Research: moving a running container between nodes (0.4.0)

28 September 2026; revised 6 October 2026. Research and an implementation
proposal, with no product code. Release: **0.4.0, "Full container migration"**.

The original maintainer decisions are retained in section 13. The 5 October
review strengthens the outcome contract, recovery rules, built-in demonstration
and conformance gates. The full apps-and-jobs scope stays. A failed spike changes
the design or blocks the release; it does not silently weaken the contract.
The 6 October additions explain CRIU limits and select Redis and PostgreSQL as
the first two real-application demonstration and qualification targets.
All commands and configuration additions below are proposed unless section 2
explicitly identifies an existing capability.

## Progress and next work

- [x] Research CRIU/runc and the current storage/runtime integration.
- [x] Record the 28 September maintainer decisions.
- [x] Refresh code assertions against fetched **main**, not the PR's old base.
- [x] Define continuity, explicit fallback, activation and source independence.
- [x] Design a built-in demonstration and versioned conformance cases.
- [x] Select Redis first and PostgreSQL second, with concrete database assertions.
- [x] Review the revised document for cross-section consistency.
- [ ] Run the integration and design spikes in section 10.
- [ ] Set measured interruption envelopes and re-estimate after the spikes.
- [ ] Implement and qualify the full release scope.

Code baseline: `origin/main` at
[`ca2c33a25ef7dc8837ca714e20468c2ae17e5d98`](https://github.com/reliaburger/reliaburger/commit/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98),
refetched 6 October 2026. It includes the boot-relative process adoption fix
(#609). The earlier `ff854cbf` baseline is superseded. Baseline links in section 2
are pinned to this main commit; subsequent implementation must refresh them.
The branch incorporates that main commit without rewriting the PR history.

## 1. Recommendation and the continuity contract

Build one coordinated stateful-move pipeline: cordon/drain, cold relocation,
checkpoint/restore, filesystem pre-sync, memory pre-copy where supported,
post-copy where its risk is accepted, and connection-preserving network
ownership. Apps and running job executions participate. Ship the demonstration
and conformance suite with the feature.

The research's strengths remain: memory is only part of the move; node-local
volumes need an explicit data path; runtime owners and journals must survive
agent restarts; and eligibility, egress, stdio and failure testing need proving
on the actual generated OCI spec. Use the runc CLI under existing owners rather
than reconstructing its namespace and mount handling through a new CRIU client.

The advantage to demonstrate over **core Kubernetes** is a native, integrated
planned evacuation of qualified stateful workloads that retains execution,
acknowledged state and sessions within a measured interruption budget. It is
not a claim of superiority on every workload or reliability dimension. The
[current KEP-5823](https://github.com/kubernetes/enhancements/blob/master/keps/sig-node/5823-pod-level-checkpoint-restore/README.md#non-goals)
excludes cross-node restore and live migration SLOs from its initial scope
(checked 5 October 2026). That proposal is moving; our product promise must
stand on its own evidence.

### What "seamless" means

A **bounded interruption requiring no application restart, client reconnection
or manual repair**, within a declared compatibility and failure envelope.

| Dimension | Required observation for a successful live move |
|---|---|
| Execution | The same computation resumes; startup and completed job work are not replayed. |
| State | Memory and the local filesystem form a consistent cut. Migration does not lose acknowledged progress. |
| Sessions | Qualified inbound, outbound and ingress sessions remain established; clients do not reconnect. TCP retransmission is allowed. |
| Interruption | Client-observed pauses meet the selected envelope's budget, including post-restore page faults. |
| Ownership | At most one workload generation can execute and write, including during partitions and recovery. |
| Completion | No pages, storage, NAT mappings, forwarding chains or required proxy sockets remain on the source. |
| Failure | The requested guarantee is not silently weakened. An unresolved owner or missing state is reported and held for recovery. |

A frozen singleton cannot do useful work for a short interval. Do not advertise
universal zero downtime. An acceptable terminal pause may violate a game's
latency budget or a remote lease deadline. Remote clocks, transactions, tokens
and locks continue advancing while the container is frozen. A time namespace
does not pause them. Qualification covers the application's relevant timeouts.

This contract is for **planned movement with both nodes initially available**.
Unexpected source loss before all state is transferred is a separate availability
promise requiring suitable replication. Do not present migration as a substitute
for application or storage fault tolerance, a coordinated snapshot of arbitrary
external services, or exactly-once external side effects.

### Mechanisms and permitted fallback are separate

| Requested mode | Default preservation requirement | Default on failure |
|---|---|---|
| `cold` | Managed-volume data; a new computation is expected. | Recover the authoritative volume copy or wait. |
| `checkpoint` | Memory, execution and filesystem; established TCP may be closed as declared. | Abort before activation, or wait for safe recovery afterward. |
| `live` | Checkpoint guarantees plus qualified session continuity and interruption budget. | Abort before activation, or wait for safe recovery afterward. |

Checkpoint and live **do not default to cold restart**. A caller may explicitly
allow restart fallback if the workload tolerates losing memory and sessions.
That result is reported as a degraded relocation with the actual mode and
lost guarantees, never a successful continuity/conformance result. A job cold
restart is a new execution of its code even if some bookkeeping identity is
retained; it must follow its retry/side-effect policy, not impersonate a resumed
attempt that consumed no retry.

Dropping an optimisation is allowed only when the remaining mechanisms still
meet the requested outcome. Missing soft-dirty tracking can change how memory
moves; dropping TCP handoff cannot satisfy required connection continuity.
Eligibility is a fresh check and reservation, not a guarantee against a later
crash. Failures after admission follow section 5.6.

## 2. What main does today

Assertions below were checked by reading fetched main at `ca2c33a25ef7dc8837ca714e20468c2ae17e5d98`.
There is no implemented process migration pipeline or migration test group.
Existing reconciliation checkpoints are not CRIU images.

| Area | Verified current behaviour and consequence |
|---|---|
| Runtime ownership | `RuncGrill::owned_start` drives runc under durable intent/owner helpers that outlive Bun. Stdout/stderr are append-mode regular capture files. Restore must join this ownership model and preserve capture ingestion. |
| Adoption | Records identify processes by kernel boot and boot-relative start ticks on Linux; cgroup attribution also verifies launcher parentage and nested init identity. Restore gets new host evidence, never copied source PIDs/ticks. |
| Network | Rootful runc uses an external network namespace, node-local `/23` addresses, host-port DNAT and source masquerade. These are not a cluster-portable network identity. |
| Root filesystem | Immutable image lower layers have private per-instance overlay upper/work directories. The writable upper state matters to restore. |
| User namespace and policy | Rootful user mappings are uniform; cgroup-keyed egress is installed before normal start. Restore must prove enforcement before any workload instruction executes. |
| Managed volumes | Paths remain per namespace/app/mount on each node, with plain-directory, loop-ext4 and Btrfs backends. They are not per logical replica; independent movement cannot split a shared writer group or merge separate target data. |
| Logical replica identity | `Placement` now includes a cluster-wide replica `ordinal`; `InstanceIdentity` includes namespace, app, generation and ordinal. Preserve the logical replica and change its host/runtime generation, instead of inventing a fresh logical instance on every move. |
| Scheduling/home | `VolumeHome` uses `last_placed_nodes`; unavailable volume homes wait unless explicitly released. Migration must atomically replace authoritative home information with the ownership cutover. |
| Jobs | Node-local `RecordedJob` includes run generation, restart count, phase and trusted batch ownership (`spec_digest`). Raft batch records retain execution names/spec digests and replay fences. Migration extends these records rather than creating a parallel attempt-number ledger. |
| Discovery | Endpoint withdrawal acknowledgements fence address reuse. Ordinary retirement removes routes; live handoff needs its own publication/withdrawal ordering to avoid destroying retained sessions. |
| Workload credentials | CSR signing derives workload identity from instance identity and checks placement for apps. Private-key caching in restored memory still requires an explicit credential-mobility/reload design. |
| Drain/upgrades | Operator drain is still planned. Upgrade cordons and Smoker's simulated drain exist; planned Bun exec upgrades adopt running workloads. Kernel/firmware reboot is the migration maintenance case. |
| Built-in tests | `relish test` runs client-side public API cases in leased `rbtest-*` namespaces, with pinned OCI fixtures, capability evidence, deadlines and confirmed cleanup. It has development/full-runtime profiles and a separate acknowledged chaos catalogue. |
| Profile completeness | Current profile rules mark selected cases required by capability. They do not define a versioned complete migration manifest or certify every migration-compatible node pair. |
| Node fault/shutdown | Smoker's `NodeTransportGate` suppresses cluster transport traffic; it does not remove kernel NAT/proxy state. Managed `relish local` has VM start/stop support with endpoint/quorum safeguards. Source-independence qualification needs actual VM/network loss. |
| Formats | Current `compatibility::CURRENT` is protocol **40**, state **58**. New incompatible formats bump from the then-current main values; do not pin a future release to these numbers. |

### Main evidence

The following are source entry points, all pinned to the verified main revision:

- [Runtime ownership: `src/grill/runc/owned.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/grill/runc/owned.rs#L752)
- [Boot/process evidence: `src/grill/records.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/grill/records.rs#L212)
- [Capture ownership: `src/grill/process_owner.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/grill/process_owner.rs#L55)
- [Namespace addressing/NAT: `src/grill/netns.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/grill/netns.rs#L166)
- [Private overlay: `src/grill/rootfs.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/grill/rootfs.rs#L76)
- [Managed volumes: `src/grill/volume.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/grill/volume.rs#L135)
- [Replica placement: `src/meat/types.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/meat/types.rs#L202)
- [Instance identity: `src/grill/mod.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/grill/mod.rs#L95)
- [Volume homes: `src/cluster/orchestrate.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/cluster/orchestrate.rs#L985)
- [Job records: `src/bun/jobs.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/bun/jobs.rs#L48)
- [Raft batch ownership: `src/meat/batch_tracker.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/meat/batch_tracker.rs#L233)
- [Workload signing: `src/cluster/workload_identity.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/cluster/workload_identity.rs#L89)
- [Test runner: `src/relish/test_cmd.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/relish/test_cmd.rs#L43)
- [Test acceptance profiles: `src/testkit/runner.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/testkit/runner.rs#L569)
- [Evidence/cleanup report: `src/testkit/report.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/testkit/report.rs#L237)
- [Test resource policy: `src/testkit/safety.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/testkit/safety.rs#L86)
- [Transport fault: `src/smoker/node_fault.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/smoker/node_fault.rs#L12)
- [Ingress sessions: `src/wrapper/websocket.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/wrapper/websocket.rs#L61)
- [Compatibility: `src/compatibility.rs`](https://github.com/reliaburger/reliaburger/blob/ca2c33a25ef7dc8837ca714e20468c2ae17e5d98/src/compatibility.rs#L54)

## 3. CRIU, runc and ecosystem research

The primer below was added and checked against upstream documentation and the
v4.2.1 source on 6 October 2026. The subsequent research is the retained snapshot
from 28 September, with the Kubernetes proposal corrected on 5 October. Version,
architecture and support claims below are research inputs, not a qualification
of our generated spec. Recheck upstream releases and actual node capabilities
before packaging. "Unverified" means a spike must establish it. Sections 1 and
5-10 define the revised contract and override the old research's suggestions
about fallback, matching CPUs and dropping connections.

### CRIU for newcomers: what a checkpoint includes, and what it cannot capture

**CRIU means Checkpoint/Restore In Userspace.** It pauses a Linux process tree,
records its memory and execution state, together with supported kernel resources
such as threads, open file descriptors and sockets, then reconstructs that state
so the program can resume where it stopped. For example, a worker halfway through
a computation can continue with its existing variables and call stack instead
of running its startup code again. runc supplies the container integration; CRIU
supplies the process checkpoint/restore mechanism
([project introduction](https://github.com/checkpoint-restore/criu),
[checkpoint/restore design](https://criu.org/Checkpoint/Restore)).

The checkpoint boundary is a set of processes and supported resources. It does
not include an entire machine or everything those processes interact with.
Three different limitations matter to operators: a resource CRIU cannot dump,
a supported resource requiring explicit integration, and external state that
must be kept consistent separately. The tables below distinguish these; they
are a starting compatibility checklist, not an exhaustive support catalogue.

**Resources that block an ordinary checkpoint.** These apply to the researched
upstream v4.2.1 release unless a qualified plugin is explicitly mentioned.

| Resource or situation | What this means for a practitioner |
|---|---|
| An open `io_uring` instance | This asynchronous I/O interface keeps ring state in the kernel. v4.2.1 has no built-in dump handler for its descriptor; an application/runtime using it can block checkpointing even if it has no volumes. The older support proposal was closed without merging. A workload-supported alternative I/O backend must be selected and tested before starting it; Reliaburger cannot transparently switch a running process. [Released descriptor dispatch](https://github.com/checkpoint-restore/criu/blob/v4.2.1/criu/files.c#L524), [proposal #1597](https://github.com/checkpoint-restore/criu/pull/1597). |
| Hardware or driver-owned state without a supported plugin | Opening or mapping a device does not make its internal state part of the process's RAM. GPU contexts/device memory, RDMA adapters and other device dependencies need specific support; creating the same `/dev` path is insufficient. Some virtual devices are supported. GPU plugins are a separate, constrained capability, not generic GPU portability; GPUs remain outside this release's migration scope (section 4). [Device limitations](https://criu.org/What_cannot_be_checkpointed), [GPU integration](https://criu.org/GPU_Checkpointing). |
| A debugger or tracer attached through `ptrace` | An attached `gdb` or `strace` conflicts with CRIU's own use of the process tracing interface. Detach it before checkpointing and test any other tracing setup separately. [Upstream limitations](https://criu.org/What_cannot_be_checkpointed). |
| Open files on a lazily unmounted filesystem | A file may remain usable by the running process after its filesystem disappears from the mount tree, but CRIU cannot checkpoint that case ordinarily. This differs from a deleted-but-still-open file, which has dedicated handling and needs its own qualification. [Upstream limitations](https://criu.org/What_cannot_be_checkpointed). |
| Packet-mode pipes | Ordinary pipes have support; pipes created with `O_DIRECT` to retain packet boundaries are rejected, apart from a specific autofs exception. This is a pipe flag, not a blanket prohibition on direct I/O to regular files. [Released pipe handler](https://github.com/checkpoint-restore/criu/blob/v4.2.1/criu/pipes.c#L489). |
| Corked UDP sockets | v4.2.1 rejects a UDP socket using `UDP_CORK`, which batches writes into a datagram. Ordinary UDP support does not imply this variant works. [Released socket handler](https://github.com/checkpoint-restore/criu/blob/v4.2.1/criu/sk-inet.c#L494). |
| Other unsupported kernel objects or socket families | There is no generic handler for every Linux API. A new kernel feature can be usable by an application before CRIU supports its state. Qualify the actual objects and options used, rather than admitting an application solely because its language or container image is familiar. [Descriptor dispatch](https://github.com/checkpoint-restore/criu/blob/v4.2.1/criu/files.c#L524), [upstream limitations](https://criu.org/What_cannot_be_checkpointed). |

The upstream limitations wiki is not a release-specific compatibility matrix.
For example, it still lists file descriptors sent over UNIX sockets as unsupported,
while v4.2.1 has `SCM_RIGHTS` queue dump/restore handling. That does not qualify
every ancillary-message or external-peer case. Use pinned release code and real
round trips to distinguish supported, unsupported and unknown, and refresh the
matrix when upgrading ([released message handling](https://github.com/checkpoint-restore/criu/blob/v4.2.1/criu/sk-queue.c)).

**Supported mechanisms that still require Reliaburger integration.**

| Resource | Extra condition; why a flag alone is insufficient |
|---|---|
| Established TCP sessions | `--tcp-established` needs the original address available at restore and packet locking between dump and restore. CRIU restores the endpoint's TCP state; Reliaburger must preserve the route, external address/NAT and any proxy-owned sessions. Peer timeouts still run. `--tcp-close` deliberately closes connections and cannot count as live continuity. [TCP requirements](https://criu.org/TCP_connection), [released options](https://github.com/checkpoint-restore/criu/blob/v4.2.1/Documentation/criu.txt). |
| File locks and shared IPC | `--file-locks` requires all relevant lock users within the checkpoint boundary; it cannot fence an outside writer. SysV shared-memory state requires the whole relevant IPC namespace. Shared resources spanning separately moved containers need a qualified group protocol or refusal. [Lock requirements](https://criu.org/Advanced_usage), [IPC boundary](https://criu.org/What_cannot_be_checkpointed). |
| External mounts, namespaces, stdio and UNIX peers | The caller must provide mappings or inherited descriptors for supported external resources. Mapping an external block-device mount does not copy the device contents; reconnecting a UNIX socket does not reproduce a daemon's peer-side session state. Our external network namespace, capture files and active exec/attach paths need explicit integration. [External resources](https://criu.org/External_resources), [mount-device mapping](https://criu.org/External_mount_devices), [plugin boundaries](https://criu.org/Plugins). |
| Destination CPU, kernel and security environment | A captured process still needs compatible instructions/register state and the required kernel features and privileges. A successful dump is not proof of restore on an arbitrary host; a matching CPU model is not the full check. Qualify direction, architecture, namespaces, seccomp/LSM and runtime configuration as a pool. [CPU checks](https://criu.org/CLI/cmd/cpuinfo), [kernel checks](https://criu.org/CLI/cmd/check). |

**State CRIU does not turn into a portable, consistent snapshot.** The container's
ordinary root filesystem and volume contents need their own consistent copy;
checkpoint images refer to files and do not replace that copy
([filesystem boundary](https://criu.org/index.php?title=FAQ)). By the same boundary,
we cannot infer a cluster-wide transaction from a process checkpoint. A database
commit, queued message, remote lock/lease, payment or response already delivered
to a client remains an external effect. Restoring older process state cannot undo
it or guarantee exactly-once replay. Remote peers keep running while the workload
is paused; leases, deadlines and credentials may expire. These are application
and orchestration constraints even when CRIU accepts every local resource.

For example, a Redis process may have checkpointable memory and sockets, yet a
move can still fail its contract through a missing filesystem cut, lost address,
client timeout or revoked credential. A database using `io_uring` may instead
fail before a usable process dump exists. Neither case is permission to discard
state and report a successful live move.

**How the built-in tests expose the boundary.** Host-level `criu check` establishes
kernel prerequisites, not that a particular running workload can migrate
([check semantics](https://criu.org/CLI/cmd/check)). The proposed Relish demo proves
its pinned fixtures and recorded source/target configuration. Workload qualification
must also exercise the application's real resource use under load: a resource
can appear only after startup, so an image digest or initial preflight cannot
guarantee every future checkpoint. Recheck at the actual dump boundary and handle
dump-time rejection under section 5.6's ownership rules.

Add small refusal fixtures and supported-counterpart round trips to S8 and the
versioned conformance manifest: `io_uring`, attached tracing, packet pipes, corked
UDP, unsupported descriptors, and outside peers/shared-resource ownership. Report
the blocking resource, exact instance, mode and qualified configuration, with
a tested remediation where one exists. Before activation, refusal must establish
source liveness and continued state/session correctness; after possible target
execution, use held recovery and fencing. Required cases cannot pass by skipping
unsupported fixtures or silently restarting them. This is proposed qualification
work, not evidence that these behaviours are implemented today.

### CRIU research snapshot

- **Version.** The latest tag is v4.2.1 (21 July 2026, a bugfix release);
  4.2 came out on 13 November 2025, 4.1 on 25 March 2025, 4.0 (the CUDA
  plugin) on 20 September 2024 ([tags](https://github.com/checkpoint-restore/criu/tags),
  [4.2](https://criu.org/Download/criu/4.2), [4.1](https://criu.org/Download/criu/4.1)).
  CRIU came out of the OpenVZ team at Parallels (now Virtuozzo) in 2011
  ([history](https://criu.org/History)).
- **What it handles that we'd hit.**
  - Established TCP: `--tcp-established` uses `TCP_REPAIR`, but **the same IP
    must exist on the restore host** ([TCP connection](https://criu.org/TCP_connection)).
    `--tcp-close` drops them instead ([man page](https://www.mankier.com/8/criu)).
  - Sockets: TCP, UDP, UNIX, packet and netlink only
    ([what cannot be checkpointed](https://criu.org/What_cannot_be_checkpointed)).
  - Seccomp (3.13), cgroup v2 (3.15), time namespaces (3.14), memfd (3.14),
    rseq (3.17), pidfd (4.1) ([changelogs](https://www.criu.org/Changelogs)).
  - eBPF: hash and array *maps* since 3.15, **no programs** ([BPF maps](https://criu.org/BPF_Maps)).
    Our eBPF lives on the host, not in workloads, so this only bites a
    workload that loads its own.
  - File locks need `--file-locks`; SysV IPC needs the whole IPC namespace
    (runc containers have one).
  - External bind mounts are remapped with `--external mnt[...]`, and
    `--inherit-fd` accepts ttys, pipes, sockets **and files by path**
    ([man page](https://www.mankier.com/8/criu)).
  - inotify, fanotify, timerfd, signalfd and eventfd are listed as kernel
    requirements for CRIU's own test suite ([Linux kernel](https://criu.org/Linux_kernel))
    but we found no per-feature support page. Unverified; the spike covers it.
- **Limits and conditional support.** See the primer above for release-checked
  refusals, caller-supplied resources and external state. The older io_uring
  proposal remains unmerged; the released descriptor handler is stronger
  evidence than the proposal's age. POSIX message queues and other unqualified
  IPC variants need explicit S8 evidence; this snapshot does not establish their
  support. A device-specific plugin or special flag is not universal portability.
- **Privileges.** CRIU wants root. `--unprivileged` (3.18) still needs
  `CAP_SYS_ADMIN` or `CAP_CHECKPOINT_RESTORE` (Linux 5.9,
  [LWN](https://lwn.net/Articles/826546/)). Checkpointing *rootless*
  containers (CRIU itself inside a user namespace) is a draft
  ([PR #3057](https://github.com/checkpoint-restore/criu/pull/3057)). Our
  rootful containers run in a user namespace created by root, which is the
  older, supported case, but the wiki calls user-namespace support
  "partial" ([User namespace](https://criu.org/User_namespace)), so the first
  spike must prove it on our spec.
- **Memory, size, compression, encryption.** CRIU dumps anonymous and dirty
  pages, not clean file-backed ones ([memory dumping](https://criu.org/Memory_dumping_and_restoring)),
  so an image is roughly the anonymous part of RSS (our inference, no official
  formula). No released CRIU compresses: v4.2.1's `rpc.proto` has no compress
  field; `criu-dev` adds LZ4 options that no release ships yet
  ([v4.2.1 rpc.proto](https://raw.githubusercontent.com/checkpoint-restore/criu/v4.2.1/images/rpc.proto),
  [criu-dev rpc.proto](https://raw.githubusercontent.com/checkpoint-restore/criu/criu-dev/images/rpc.proto)).
  No image encryption either; TLS exists only for the page server, and
  [criu-image-streamer](https://github.com/checkpoint-restore/criu-image-streamer)
  lets you pipe images through your own compressor or cipher.
- **Clocks and CPUs.** Without a time namespace a restored process sees the
  target's `CLOCK_MONOTONIC`, which can be *behind* the source's. CRIU fixes
  this only when the container has a time namespace
  ([Time namespace](https://criu.org/Time_namespace)); the OCI spec gained
  one in 1.1 ([OCI blog](https://opencontainers.org/posts/blog/2023-07-21-oci-runtime-spec-v1-1/))
  and runc supports it from 1.2. CPU features aren't checked unless you ask
  (`--cpu-cap`, [Cpuinfo](https://criu.org/Cpuinfo)), and glibc keeps using
  whatever it detected at start-up
  ([LPC 2024](https://lpc.events/event/18/contributions/1811/)). The revised
  design requires feature checks within a qualified compatibility pool; identical
  CPU models alone do not prove portability (section 7).
- **Memory transfer capabilities (September snapshot).** Pre-dump and `--track-mem` rely on soft-dirty
  tracking, which **arm64 mainline kernels still don't have** as far as we
  can find; the 2023-24 patches weren't merged
  ([LKML](https://lkml.iu.edu/2403.1/03203.html),
  [criu#1859](https://github.com/checkpoint-restore/criu/issues/1859)). Our
  quickstart is Apple-silicon Lima, so iterative pre-copy wouldn't work there
  at all. Lazy pages use userfaultfd and only cover private anonymous memory
  ([Userfaultfd](https://criu.org/Userfaultfd)).

### runc

- Latest is v1.5.2 (25 September 2026)
  ([releases](https://github.com/opencontainers/runc/releases)).
- `runc checkpoint` takes `--image-path`, `--work-path`, `--parent-path`,
  `--leave-running`, `--tcp-established`, `--tcp-skip-in-flight`,
  `--ext-unix-sk`, `--shell-job`, `--lazy-pages`, `--status-fd`,
  `--page-server`, `--file-locks`, `--pre-dump`,
  `--manage-cgroups-mode soft|full|strict|ignore`, `--empty-ns`,
  `--auto-dedup` ([runc-checkpoint(8)](https://github.com/opencontainers/runc/blob/main/man/runc-checkpoint.8.md)).
  `runc restore` adds `--bundle`, `--detach`, `--pid-file`, `--no-subreaper`,
  `--no-pivot`, `--lsm-profile`, `--lsm-mount-context`
  ([runc-restore(8)](https://github.com/opencontainers/runc/blob/main/man/runc-restore.8.md)).
  Extra CRIU options come from `/etc/criu/runc.conf` or the `org.criu.config`
  annotation ([checkpoint-restore.md](https://github.com/opencontainers/runc/blob/main/docs/checkpoint-restore.md)).
- How it maps onto our setup (read from
  [`criu_linux.go`](https://raw.githubusercontent.com/opencontainers/runc/main/libcontainer/criu_linux.go)):
  - A network namespace given by path is dumped as external
    (`net[inode]:extRootNetNS`) and **re-joined from the new config's path on
    restore**. So the target can pre-create a fresh namespace with a fresh
    address exactly as `prepare` does today. Sockets bound to `0.0.0.0`
    survive; a socket bound to the old container address won't.
  - Bind mounts are external and **restored from the new config's source**,
    so volumes and our per-instance `resolv.conf` can live at different host
    paths on the target.
  - The restored init becomes runc's child (`RstSibling: true`), which should
    keep our "container init's parent is the launcher" source-identity check
    (`owned_workload_cgroup`) true. Needs proving.
  - **Only pipe descriptors are re-attached** (`descriptors.json` entries
    containing `pipe:` become `InheritFd`). Our stdout/stderr are regular
    files, so CRIU will try to reopen the *source's* capture file paths on the
    target, and its default file validation checks size. This is the most
    concrete integration gap we found (spike S2 in section 10).
  - The CLI warns checkpoint is untested with rootless containers, and a
    non-root restore "is unlikely to work".

### rust-criu, youki, crun

- [rust-criu](https://github.com/checkpoint-restore/rust-criu) 0.6.1 (20 June
  2026), about 210k downloads, 28 stars, MIT text in `LICENSE` but
  `license-file` in `Cargo.toml`, so crates.io shows "non-standard"
  ([crates.io API](https://crates.io/api/v1/crates/rust-criu)). It's a
  protobuf client for `criu swrk` mirroring go-criu, pins `protobuf = 3.7.2`
  (new to our tree), and exposes `dump()`/`restore()` plus setters for
  `images_dir_fd`, `tcp_established`, `external`, `inherit_fd`, cgroup mode and
  so on ([docs.rs](https://docs.rs/rust-criu/latest/rust_criu/struct.Criu.html)).
- youki depends on it and ships checkpoint, but **restore isn't merged**
  ([youki#2335](https://github.com/youki-dev/youki/issues/2335),
  [PR #3429](https://github.com/youki-dev/youki/pull/3429), open since 27
  February 2026). So "youki uses it" proves the dump half only.
- crun uses libcriu directly ([crun(1)](https://github.com/containers/crun/blob/main/crun.1.md)).

**rust-criu or `runc checkpoint`?** Shell out to runc. We already drive runc
as a CLI under owners, runc knows how to turn an OCI bundle (namespaces,
cgroups, mounts, the external netns) into CRIU options, and using rust-criu
directly would mean reimplementing runc's restore. The one thing rust-criu
would buy us, `inherit_fd` for our file-backed stdio, is better solved on our
side (section 5.4). Revisit rust-criu only if we ever drop runc.

### Everyone else

| Tool | Checkpoint | Restore | Cross-host | Notes |
|---|---|---|---|---|
| Podman | yes | yes | yes, `--export`/`--import` tarball with rootfs diff, zstd | `--tcp-established` needs the same IP; `--name` can't combine with it ([checkpoint](https://docs.podman.io/en/latest/markdown/podman-container-checkpoint.1.html), [restore](https://docs.podman.io/en/latest/markdown/podman-container-restore.1.html), [guide](https://podman.io/docs/checkpoint)) |
| Kubernetes KEP-2008 | kubelet `POST /checkpoint/...` writes a tar | **not provided by KEP-2008** | no | Beta since 1.30, still beta ([kep.yaml](https://raw.githubusercontent.com/kubernetes/enhancements/master/keps/sig-node/2008-forensic-container-checkpointing/kep.yaml), [kubelet API](https://kubernetes.io/docs/reference/node/kubelet-checkpoint-api/)). Restore happens below Kubernetes: CRI-O since 1.25, containerd 2.1 ([containerd#10365](https://github.com/containerd/containerd/pull/10365)) |
| Kubernetes KEP-5823 | proposed pod-level | proposed | **initial non-goal** | The current initial scope creates a new Pod on the same node; in-place restore, cross-node restore, live migration SLOs, shared volumes and devices are non-goals ([KEP-5823](https://github.com/kubernetes/enhancements/tree/master/keps/sig-node/5823-pod-level-checkpoint-restore)). No release availability is inferred from this proposal |
| Docker | experimental `docker checkpoint` | yes | not as a feature | [docs](https://docs.docker.com/reference/cli/docker/checkpoint/) |
| Incus | CRIU for containers | yes | yes | Docs: only "very basic containers" migrate reliably; 7.4 added snapshot-sync "near-live" moves instead ([docs](https://linuxcontainers.org/incus/docs/main/howto/move_instances/), [7.4](https://linuxiac.com/incus-7-4-adds-near-live-container-migration-for-zfs-and-btrfs/)) |
| Apple `container` | none | none | none | No checkpoint, suspend or migrate command ([command reference](https://github.com/apple/container/blob/main/docs/command-reference.md)) |

The Kubernetes [Checkpoint/Restore Working Group](https://www.kubernetes.dev/blog/2026/01/21/introducing-checkpoint-restore-wg/)
(January 2026) lists migration for maintenance among its use cases, so
Kubernetes is actively working on these use cases. The narrower native planned
migration claim in section 1 is the comparison to establish; Podman, Borg, Cast AI
and Cedana already provide checkpoint/migration mechanisms in other forms.

**ProcessGrill and Apple Container can't take part in the CRIU mode.**
Apple's runtime runs each container in a VM behind a CLI with no checkpoint
verb. ProcessGrill runs host processes with no namespaces: CRIU would have to
restore host PIDs, host paths and host sockets on another host, which is the
case with additional host-level dependencies. Both must refuse checkpoint and
live migration with a clear error. The cold-start move (section 5) could serve
them later, but this migration proposal keeps it
runc-only so there's one data path to test.

## 4. GPU checkpointing and where the demand really is

### GPUs

- NVIDIA's [cuda-checkpoint](https://github.com/NVIDIA/cuda-checkpoint/blob/main/README.md)
  suspends a process's CUDA state into host memory so CRIU can dump it like
  a CPU process. By driver branch: 550 basic; 570 CRIU 4.0+ integration and a
  lock with a timeout; 580 restore onto different GPUs; 595 arm64; 610 CUDA
  IPC. Still unsupported: UVM, IPC memory from `cuMemExportToShareableHandle()`,
  and no guarantee of a sane process after a failed checkpoint. CRIU's CUDA
  plugin shipped in 4.0 ([plugin](https://github.com/checkpoint-restore/criu/tree/criu-dev/plugins/cuda),
  [NVIDIA blog](https://developer.nvidia.com/blog/checkpointing-cuda-applications-with-criu)).
- AMD's amdgpu plugin has been in CRIU since 3.17 and wants "a very similar
  topology" on restore ([README](https://github.com/checkpoint-restore/criu/blob/criu-dev/plugins/amdgpu/README.md)).
- NCCL communicators don't survive and must be rebuilt; vLLM's proposal also
  requires host RAM at least as large as the GPU memory in use
  ([vLLM RFC #34303](https://github.com/vllm-project/vllm/issues/34303), open).
- What's shipping for LLM warm starts:
  - [GKE Pod Snapshots](https://docs.cloud.google.com/kubernetes-engine/docs/concepts/pod-snapshots):
    **gVisor**, not CRIU, plus cuda-checkpoint; single GPU except L4.
    Snapshot invalidation on driver or runtime upgrades is the recurring pain
    ([InfoQ](https://www.infoq.com/news/2026/09/gke-pod-snapshots-benchmarks/)).
  - [Modal](https://modal.com/blog/gpu-mem-snapshots) (alpha, gVisor),
    Cerebrium and Beam (gVisor; Beam's
    [PR #1918](https://github.com/beam-cloud/beta9/pull/1918) shows every runsc
    upgrade invalidating checkpoints).
  - [NVIDIA Dynamo Snapshot](https://developer.nvidia.com/blog/nvidia-dynamo-snapshot-fast-startup-for-inference-workloads-on-kubernetes/)
    (experimental): CRIU plus cuda-checkpoint, single GPU, vLLM and SGLang,
    2.8x to 7.9x faster starts, with CRIU changes "available once merged into
    upstream CRIU".
  - [Cedana](https://github.com/cedana/cedana): CRIU-based, AGPL, vendor claims
    only.
  - Research, not products: [CRIUgpu](https://arxiv.org/abs/2502.16631),
    [PhoenixOS](https://arxiv.org/abs/2405.12079),
    [Foundry](https://arxiv.org/abs/2604.06664) (which calls process-level
    C/R "heavyweight").

The maintainer's premise holds: fast GPU checkpointing to cut LLM cold starts
is real and shipping. But look at *what* ships: restoring a pre-warmed
snapshot many times on one class of machine. That's a different product from
migration. It needs a snapshot store with invalidation rules and GPUs we
don't have; Reliaburger's GPU support today is detection only
(`src/bun/gpu.rs`). Not 0.4.0.

### Demand for the other two

- **Batch.** Google's Borg team migrated evicted batch tasks with CRIU: 1-2
  minutes per migration, 90%+ success, time dominated by remote storage and
  scheduling; failures from big thread counts or memory, "different host
  environments", and unsupported features. Their recipe: drop connections,
  let the IP change, keep little local storage, and run CRIU as the task's
  user in a user namespace, because audits found a malicious task could
  hijack a root CRIU. "Works well for batch jobs... Not a great offering for
  latency-sensitive jobs"
  ([LPC 2018 slides](https://lpc.events/event/2/contributions/69/attachments/205/374/Task_Migration_at_Scale_Using_CRIU_-_LPC_2018.pdf), slides 10 and 22-31).
  HPC centres still prefer DMTCP ([NERSC](https://docs.nersc.gov/development/checkpoint-restart/)).
- **Game and dev servers.** The evidence is thin. Agones protects allocated
  game servers from scale-down rather than migrating them
  ([FAQ](https://agones.dev/site/docs/faq/)); the one CRIU game demo we found
  is a vendor's ([Cast AI](https://cast.ai/blog/introducing-container-live-migration-zero-downtime-for-stateful-kubernetes-workloads/),
  which needed a forked AWS VPC CNI to keep pod IPs). Dev-environment and
  sandbox vendors chose VM snapshots: CodeSandbox and E2B use Firecracker
  ([CodeSandbox](https://codesandbox.io/blog/cloning-microvms-using-userfaultfd),
  [E2B](https://e2b.dev/docs/sandbox/persistence)); Modal and GKE use gVisor.

So of the three, batch has the production evidence and tolerates
stop-and-copy downtime. A stateful dev or game server makes the better
*demo*, because you can watch it keep its state.

## 5. Design for Reliaburger

### 5.1 Scope, admission and ownership units

**In:** operator cordon/drain/uncordon; explicit migration; cold, checkpoint and
live modes; apps and running jobs; managed data; volume/rootfs pre-sync; tested
memory pre-copy/post-copy; connection ownership; a built-in demonstration,
conformance profiles and a release qualification lane on both architectures.

**Out:** GPUs, rootless/non-runc migration, automatic balancing/preemption
policies, universal cross-architecture restore, arbitrary coordinated external
state, and transparent movement of every possible socket/device/kernel feature.
Apple runtime work elsewhere in 0.4.0 does not imply Apple migration support.
Unsupported host-path/runtime/device cases block drain; an explicit force-stop
is reported as a stop with data left in place, not a successful move.

Drain follows **migration intent**, including for apps with no volumes. An
in-memory cache or job with `mode = "checkpoint"`/`"live"` must enter this
pipeline. Only workloads whose policy allows restart are ordinary reschedules.
For unspecified app policy, the historical cold default remains; that default
is not a promise of state continuity. An unspecified job may be restarted only
when its own retry/restart policy permits it; otherwise it blocks drain.

The move unit is a logical replica or a declared storage-sharing group. Until
per-replica volumes exist, the source must have no other instance using those
app volumes and the target must have no separate live/data ownership that would
be overwritten. If that cannot be established, block or move the whole supported
group; never silently split writers or merge copies. Lock volume identity as
well as the app, and preserve placement ordinals.

Admission checks and reserves:

1. Exact source instance/job execution, spec digest, logical identity and current
   runtime/ownership generation. No conflicting deploy, stop, snapshot restore,
   scale or migration; honour recovery fences and resource leases.
2. Target readiness, compatible placement labels, runtime/architecture/CPU and
   qualified kernel/CRIU/filesystem/namespace combination. A newer CRIU version
   and a CPU superset alone are not a complete portability proof.
3. Image availability and bounded storage, memory and transfer capacity on both
   nodes, including resident workload memory plus no-swap dump/restore buffers,
   retained copies and control-plane headroom. Reserve them; a free-space sample
   is not a reservation. Limit concurrent moves per node/link.
4. Actual required preservation and latency envelope. Report incompatible layers
   and alternate explicitly permitted outcomes in dry-run. An address collision
   cannot quietly turn a required live move into a disconnecting move.
5. Actual workload resources, network, credentials and external lease/timeout
   requirements (section 3's CRIU primer). Unsupported sessions or active
   exec/attach dependencies are refused or explicitly excluded by the selected
   profile. Host checks and an image digest do not certify later resource use;
   recheck at dump and preserve safe source/recovery outcomes on rejection.
6. For test fixtures, authenticated lease ownership throughout source, target,
   payload, route and volume lifecycle (section 9). Ordinary test permission
   never authorises draining unrelated workloads.

CRIU, same-architecture execution restore, source CPU features and dump/restore
RAM are checkpoint/live prerequisites. Cold mode instead checks target image
and persistent-data compatibility and copy capacity; it does not require CRIU
or reservation of a process dump. Every mode still requires exclusive ownership.

### 5.2 State that travels and consistent filesystem copies

| State | Cold | Checkpoint/live |
|---|---|---|
| Managed-volume contents/metadata | Carry the authoritative stopped contents. | Carry a filesystem cut compatible with the frozen execution. |
| Writable overlay upper | Starts fresh by the declared cold semantics. | Carry changes, deleted-file whiteouts, opaque-directory/xattr state and required metadata. |
| Memory, fds, namespaces | New computation. | Preserve qualified process/kernel state and open-file positions. |
| Immutable image layers | Pull and verify before stopping. | Same; exact lower-layer digest and mount features must match. |
| Capture output | Retain and ingest source tail. | Reattach target captures with uninterrupted ingestion identity; do not assume the source tail was already ingested. |
| Network | New address/sessions as declared. | Checkpoint may close TCP; live retains supported tuples and all required external mappings. |
| Credentials | New process loads current credentials. | Explicit mobility or tested reload policy; replacing mounted files alone is insufficient. |

Pre-sync is opportunistic; only the frozen final cut is authoritative. A
size/mtime-only comparison does not establish that an unchanged-sized file has
unchanged contents. Use verified content/block deltas or a conservative complete
final copy, with correct deletion, rename, hardlink, sparse-file, ownership,
ACL/xattr and supported filesystem semantics. Couple the final memory image and
filesystem manifest to the same cut/generation; in-flight writes and open or
unlinked files are spike/test cases. Multi-volume cuts stop all relevant writers.
Preserve application-acknowledged state; define the workload's durability
expectations rather than pretending unacknowledged operations were committed.

Btrfs snapshot/incremental send is a candidate for that backend. Loop-ext4 moves
must re-provision its target quota/mount bookkeeping and preserve complete image
identities; copying files without an enforced target limit is incomplete. Plain
and overlay data need explicit metadata-preserving import. Tar is a transport,
not proof of these properties. Overlay details are in the
[Linux documentation](https://www.kernel.org/doc/html/latest/filesystems/overlayfs.html#whiteouts-and-opaque-directories)
(checked 5 October 2026).

Keep a source snapshot/tombstone until retention permits deletion, but label it
with its cut and ownership generation. After target activation it is stale
recovery material, not a live fallback volume. Cleanup, snapshots and volume
retirement must respect active migration ownership and never remount that copy
from ordinary desired-state reconciliation.

### 5.3 Logical identity and source-independent networking

Preserve the logical namespace/app/ordinal or job execution. A migration id links
host/runtime generations for audit; it must not replace logical workload identity.
The app service/DNS name and logical service VIP stay stable while backend
ownership moves. Cached hostnames, node-local environment values and references
to external resources are part of workload qualification; arbitrary process
memory cannot be safely rewritten to make them portable. New host PIDs, boot
ticks, owner records and cgroup identifiers are validated on the target. Do not
transplant source host identity evidence.

For checkpoint, declared TCP closure is supplied through CRIU configuration;
prove its interaction with external namespaces and the app's recovery behaviour.
For live, maintain qualified inbound, outbound and ingress session identity.
Include IPv4/IPv6 where supported, connection states, socket queues, packet
locking between dump/restore, conntrack timeouts, MTU and reverse-path filtering
in the network qualification. UDP/QUIC and other unsupported session families
must have explicit policy; established-TCP support is not a universal session
promise. CRIU requires packet locking to prevent transient resets and restoration
of the original address ([TCP connection](https://www.criu.org/TCP_connection),
research accessed 28 September; source requirements rechecked 5 October 2026).

**Release direction: portable network ownership.** Cluster-unique movable
workload addresses (`/32`, and `/128` if IPv6 is supported) remove node-pool
collisions. They need authenticated route ownership, generation fencing and
acknowledged route changes. Stable addresses do not move source NAT state by
themselves. Specify an independent/transferable egress mapping owner with the
same externally visible address/port and necessary conntrack state. Qualify
where upstream routing permits that address to move. If a required external
mapping is intrinsically tied to the source host, that topology cannot pass the
source-independent profile; do not hide it behind source forwarding.

Wrapper's upstream and client-facing sockets are separate. Moving a backend
socket does not move the client's TCP/TLS/WebSocket session held by Wrapper.
Use an ingress owner outside the drained source, or a separately proven proxy
state/connection transfer. A profile names and tests this topology. Draining a
node hosting required ingress sockets blocks until their dependency is removed
or an explicit disconnect policy is accepted; force is not seamless completion.

**Intermediate spike: forwarding through the source.** The old borrowed-address,
CRIU tuple-set and source-conntrack tunnel is useful to test TCP_REPAIR. Reserve
addresses and fence flows, use authenticated/protected inter-node transport, and
prove packet-lock ordering. It does not meet source independence: source failure
breaks sessions, and expiring a ten-minute window resets survivors. It may be
reported as a weaker experimental outcome if explicitly requested, but cannot
pass live conformance or unblock the 0.4.0 continuity release gate. Test second
and subsequent moves to rule out forwarding chains and stale tuple leases.

**Publication and withdrawal.** Prepare target network/firewall state before
activation. Preserve old flows while new connections switch; ordinary source
retirement must not erase needed conntrack, firewall, forwarding or proxy state.
Reconcile discovery withdrawal with migration ownership; an endpoint becoming
healthy does not prove source independence. Wait for the required network
ownership acknowledgements before releasing/reusing addresses.

**Egress and time.** No restored instruction may run before its cgroup egress
policy is enforced. Pre-created cgroups or a reliable stopped-restore barrier
must be proven on runc; otherwise refuse affected workloads. Create time
namespaces before original launch when needed, and qualify timer/clock behaviour
across hosts. External deadlines continue advancing regardless.

### 5.4 Runtime owners, stdio and restart interference

Prove runc restore under the existing owner/intent lifecycle, including launcher
parentage, cgroup attribution, init identity, adoption after Bun restart, current
boot/start-tick checks, exit receipts and retirement. Source and target runtime
generations are distinct even when the logical instance name stays stable.

Regular-file captures are an integration gap: runc's descriptor reattachment
primarily handles pipes. Try CRIU inherited descriptors through `org.criu.config`;
if unavailable, use owner-managed pipes for migratable containers. Recreating
source paths/pre-sizing files is not accepted without robust validation. Preserve
capture offsets/ingestion identity and consume source tails before retirement;
never recreate a log path and count old bytes as new output.

Hold ordinary health-triggered restarts, pending deploys, exec/attach/probe
process creation and placement reconciliation while the move owns the workload.
A liveness timeout during freeze cannot authorise a second instance. Target
"migrating in" assignments suppress normal cold creation, including after agent
restart; source journals forbid resurrection of retired generations. App stop
and delete cancel desired execution, but still wait for confirmed retirement.

### 5.5 Payload security, reservations and resumability

The target pulls authenticated node-to-node chunks. Manifest entries bind
migration id, ownership generation, consistent cut, paths, image/spec digest,
lengths and content digests. Validate and import into private staging paths with
symlink/hardlink/path-traversal confinement. Publish a staged filesystem only
when the complete required manifest is verified. Flush imported file data,
directory entries, ownership journals and atomic publish/tombstone transitions
before issuing durable receipts; a matching digest of buffered bytes is not
evidence of persistence. Use the existing durable-write patterns and test crashes
between each write, rename and acknowledgement. Range requests resume encrypted
bytes/chunks, not arbitrary plaintext offsets in a single compressed tar.

Keep process dumps out of Pickle/object registries. Metadata and frozen memory
can be tar/zstd/age streams; pre-sync rounds, final filesystem deltas and lazy
pages are separate authenticated streams bound to one manifest/cut. One fixed
hash of an unfinished all-in-one stream cannot describe post-copy progress.

Plaintext dump/restore pages use bounded **no-swap** memory. Tmpfs can swap by
default: enforce `noswap` on qualified kernels or an equivalent verified host
policy, and include all plaintext staging buffers. Respect the maintainer's
no-plaintext-spill requirement; inability to enforce it refuses checkpoint/live.
See [kernel tmpfs documentation](https://www.kernel.org/doc/html/latest/filesystems/tmpfs.html)
(checked 5 October 2026). Concurrency and memory pressure must not OOM the source
workload or control plane.

A key lost on every Bun restart cannot be allowed to force cold fallback. A
runtime owner can retain the key across Bun exec/restart; full owner/node loss
needs a proven recovery path. Proposed recovery material wraps each transfer key
with the target's protected node-local key, persisting only authenticated
ciphertext with the journal. Bind recipient/node/generation and authorise unwrap
against current ownership. This is a proposed replacement for the old memory-only
transfer-key design, not a claim that this path already exists. Re-keying and
retransmitting before activation is also safe when the source cut remains intact.
After activation, key loss must not authorise replay of stale state. Spike S15
settles the recovery mechanism and threat model before implementation.

Reserve dump/restore RAM, disk, retained snapshots, quotas and concurrency before
freeze. Disk-pressure/cleanup code uses the same reservations and ownership.
Unknown migration files on startup are quarantined/reconciled, not deleted just
because a leader or Raft lookup is temporarily unavailable. Terminal ownership
and positive evidence permit cleanup. Report uncertain cleanup as failure.

### 5.6 Activation, fencing and recovery

Raft chooses ownership; node-local owners, journals and enforced runtime/storage
fences make that choice effective. Every instruction/report carries migration
id, cut, source/target generation, spec digest and an ownership epoch. Duplicate
or late messages cannot authorise an older epoch. All writers in the move unit
must stop, and ordinary reconciliation must be unable to resurrect them.

```text
Requested -> Prepared -> SourceFenced -> StateReady -> ActivationAuthorised
                                                          |
                                                          v
                                  Active -> SourceIndependent -> Completed

Before ActivationAuthorised: abort/return to source only with proof target cannot run.
At/after ActivationAuthorised: recover authoritative current execution; no stale replay.
Any phase: unresolved ownership/state -> RecoveryRequired (not successful completion).
```

- **Requested/Prepared:** reserve the target, pull images, establish capabilities,
  secure keys and staging. Source still executes; abort leaves it unchanged.
- **SourceFenced:** freeze/checkpoint or stop all source writers; persist a
  retirement fence before reporting. Complete the consistent final filesystem
  cut. Agent restart/reboot must not clear this fence. Network/external writes
  need the enforced fence, not merely a renamed local volume.
- **StateReady:** target has all required filesystem/runtime metadata and a
  verified full-memory image, or a qualified lazy-page provider satisfying the
  requested failure envelope. Placement/home and held incoming assignment are
  committed consistently with the ownership transfer.
- **ActivationAuthorised:** the one-way recovery boundary, committed **before**
  invoking anything that may run the target. runc restore can resume the app
  before returning or before a health check. Treat an error/timeout from that
  invocation as potentially active, not permission to restore the source.
- **Active:** target execution may make new writes or external actions. Target
  current state is authoritative. Health/publication is recorded separately.
- **SourceIndependent:** outstanding pages, routes, NAT/proxy dependencies and
  ownership acknowledgements are resolved. Retained stale source snapshots do
  not count as a dependency; source is unable to execute them.
- **Completed:** requested guarantees were observed, target is healthy and all
  required source dependencies are gone. Release locks/reservations according
  to retention policy. Explicitly permitted degraded relocation has a distinct
  outcome naming its actual mode and losses; it is not a continuity pass.
- **RecoveryRequired:** ownership or current state cannot be established. Keep
  the fence and authoritative-home evidence, expose reason and recovery action.
  A deadline ends retries/alerts; it does not turn missing evidence into success.

A counterexample fixes the rollback rule: source cut is 100, target acknowledges
101, then becomes unreachable. Restoring the source cut loses acknowledged 101
and might create two writers. No target timeout or command failure makes cut 100
current again. Reverse migration after activation must transfer current target
state and fence the target first. An operator choosing stale recovery is an
explicit data-loss intervention outside the continuity contract.

| Failure | Before activation authorisation | At/after activation authorisation |
|---|---|---|
| Target prepare/transfer fails | Leave source running, or safely resume its retained cut only after proving target cannot run. | Recover target current state; missing evidence holds recovery. |
| CRIU refuses dump | Establish source liveness/stop outcome positively. Abort strict mode; restart only if explicitly permitted. | Not a dump-time fallback case. |
| Source Bun restarts | Owner/journal determine actual phase; uncertainty blocks. | Do not clear source retirement fence; reconcile current epoch. |
| Source node disappears | Missing untransferred state means unavailable/recovery required, not a successful fallback to a dead home. | Complete from already independent state; missing lazy pages follow declared failure envelope. |
| Target restore returns error or times out | Applicable only if no activation was authorised and no execution could occur. | Treat as potentially executed; require positive retirement/current-state evidence before any replacement. |
| Target loses contact/crashes | Abort/return only after target cannot execute is established. | Never run old source snapshot; use current durable target data and declared recovery semantics, or hold. |
| Transfer key/agent disappears | Recover wrapped key/owner or re-key retained cut; no implicit cold restart. | Key/agent loss cannot roll back execution history; reconcile authoritative state. |
| Leader changes | Resume committed record after learning/recovery fences; duplicates are no-ops. | Same epoch and activation boundary; absence of a receipt is not absence of execution. |
| Stop/delete/cancel | Abort/retire with positive evidence. | Stop desired execution, confirm current owner retirement, retain authoritative data; cancel does not mean rewind. |
| Both nodes disappear | Hold missing state/ownership. | Hold current ownership and data; decommission is not proof of safe replay. |

Fencing must work during partition: lease/epoch checks in Raft alone cannot halt
a running isolated process. Specify durable source retirement, runtime launch
prohibition and storage/network enforcement for the selected topology, with
self-fencing/lease expiry where required. External side effects need their own
resource fencing if the workload's contract depends on it. Prove this in S12;
finite cluster observations support the invariant but do not mathematically prove
it, so use state-machine/property tests and recovery-path inspection as well.

Drain cordons, respects availability budgets, selects migration intent, and waits
for **Completed/SourceIndependent** for each moved workload. A singleton's
qualified brief interruption needs an explicit availability budget; do not claim
surge can eliminate it. Blocked or degraded moves are reported. A drain timeout
leaves the node cordoned with unresolved work; it does not tear down required
sessions. `--force` explicitly accepts stops/disconnections, and its result does
not certify seamless evacuation. `decommission-node` preserves recovery fences
and requires current authoritative state/retirement evidence.

### 5.7 Live planning, post-copy and interruption budgets

Select methods from measured capabilities and the requested contract, not a
fixed architecture label. The September research found no soft-dirty pre-copy
on mainline arm64; verify the actual kernel/CRIU combination. Qualify x86_64
pre-copy and arm64 alternatives separately; cross-architecture restore is out.
An unverified arm64 lazy-page demo is a spike target, not a shipping claim.

Pre-copy stops when the dirty set converges within the remaining freeze budget,
stops improving, or reaches a round/time limit. The old 64 MiB/5-round policy is
a tuning candidate, not a latency guarantee. At 1 Gbit/s, 64 MiB alone takes
about 0.54 seconds to transfer before filesystem delta, restore and route costs.
Use observed throughput and workload dirty rates; cap total pre-sync work so it
does not starve unrelated apps. If the budget cannot be met, abort before
activation rather than dropping required preservation.

Measure freeze, final filesystem delta, transfer, restore, route convergence,
client request pauses and recovery of throughput. Post-copy page faults can
shift delay after resume; report maximum migration-window pause and recovery
tail as well as percentiles. Bind every performance result to workload size,
dirty rate, client rate, network/storage environment and budget. No global
average hides the cutover event; preparation downloads are reported separately.
The shipped demonstration/conformance profile supplies a small fixed workload,
load and budget established by S7 and release qualification, so no practitioner
tuning is needed to try it. Custom envelopes are reported separately and cannot
silently loosen the versioned default conformance requirements.

Lazy pages may lower frozen time but depend on the provider until complete.
If the source/connection fails, missing pages can stall or terminate execution;
prove detection and bounded handling instead of assuming an automatic clean
process death. Strict mode does not respond with memory-losing cold restart.
A stronger source-failure envelope needs another authenticated recoverable page
copy, or complete transfer before activation. The ordinary planned-move profile
may explicitly accept temporary dependence while both nodes remain up, but it
cannot report completion/drain-safe before that dependence is gone. Source loss
outside that envelope reports recovery-required, not successful continuity.
Replaying an old checkpoint after external actions is forbidden even if all old
pages were replicated; restoring current execution requires current state.

## 6. Compatibility and cluster integration

Pre-1.0 policy remains: incompatible changes bump protocol/state from current
main and require a fresh cluster. There is no dependency on PR #266's old gate.
Do not use the superseded protocol 27/state 44 numbers. The reviewed main is
40/58; implementation chooses the next values when it lands.

Changes include migration/cordon records and ownership epochs in Raft snapshots
and requests; app/job preservation and fallback policy; held assignments; job
execution ownership transitions; volume-home records; node capability evidence;
node-local migration journals and encrypted recovery keys; network lease/route
ownership; runtime/time namespace records; authz routes and lease ownership;
and schema-versioned test manifests/results. Existing serde/bincode formats must
be audited where those facts are carried. Progress may use authenticated node
routes, but do not promise current reporting frames remain untouched until the
placement/capability design is settled.

Preparation, source fencing, activation and home changes respect current main's
recovery fences, desired-state transaction rules, trusted batch spec digests and
capacity reservations. Migration must not reintroduce uncertain-job retries,
stale-worker writes, incomplete snapshot restores or address resurrection fixed
on main. Refresh evidence after further main changes before implementing.

## 7. Security and credential continuity

Memory contains plaintext secrets, TLS/session keys, passwords and tokens.
Use authenticated/encrypted transfer and protected no-swap staging; no registry
publication. Bind every stream to migration/cut/recipient and enforce bounded
extraction and digest validation. CRIU is new privileged attack surface even
though Bun already runs rootful runc. Pin/qualify packaging, keep opt-in workload
scope, and test sandbox/profile compatibility when profiles are added.

`migrate` needs app-scoped deploy permission; both checkpoint **and live** require
memory/exec-equivalent authority. Drain, node-state faults, force-stop and
cancelling another principal's move require their current administrative grants.
A test lease narrows ownership; it does not expand permission. Update the server
authz matrix for every new route and for indirect live moves through drain.

**Credential continuity remains a required design spike, not a documentation
footnote.** A restored process can retain the old key/certificate in memory.
Replacing a mounted file and immediately revoking that serial can break fresh
handshakes. Qualify either:

- authorised mobility of the logical workload credential, with fenced source
  ownership, protected transfer and subsequent rotation; this explicitly changes
  the "private key never leaves the node" invariant; or
- application cooperation that reloads fresh credentials before the old serial
  is revoked; require a tested reload hook and scope the transparency claim.

Choose and specify the supported path in S14. Do not simultaneously retain a
cached certificate on the target and revoke it as if it belonged only to the
source. Existing sessions may not revalidate revocation; test fresh authenticated
connections after completion too. There is no arbitrary safe memory rewriting
of cached credentials. Target identity and host ownership evidence remain
separate; a retired source must not mint replacement credentials.

CPU superset checks (`criu cpuinfo dump`, restore `--cpu-cap=cpu`) are defence in
depth, not complete portability. Qualify architecture, runtime/image format,
kernel, namespace/cgroup features, page size and relevant filesystem/device
support as a migration pool. Runtime feature probes determine optimisations.

## 8. Operator UX and evidence

Proposed commands (none implies an implemented migration verb today):

```text
relish migrate default/cache --to node-3
relish migrate default/cache --to node-3 --mode live
relish migrate default/cache --to node-3 --dry-run
relish migrate default/render --execution <id> --to node-3
relish migrate status [<migration-id>]
relish migrate cancel <migration-id>
relish drain node-2 [--timeout 30m] [--force]
relish drain status node-2
relish uncordon node-2
```

Job selection must resolve main's exact namespace/execution/run generation,
not just an ambiguous attempt number. CLI syntax is finalised with that binding.
The following config shape is proposed; parser/schema work remains:

```toml
[app.cache.migration]
mode = "live"                     # cold | checkpoint | live
fallback = "abort"                # default for checkpoint/live; restart is explicit
max_interruption = "500ms"        # illustrative qualified budget, not a measurement
```

Live implies required memory/execution/filesystem/session continuity and source
independence at completion. Checkpoint allows declared TCP closure. Cold is
restart semantics. If weaker experimental forwarding is exposed, it uses an
explicit weaker profile/outcome; do not make `mode = "live"` secretly select it.
`fallback = "abort"` means abort before activation; after activation, unresolved
state means held recovery, not replay. Restart policy does not waive fencing or
permit acknowledged persistent-data loss. Jobs retain their retry semantics.

Human status leads with the outcome: source/target, required guarantees, current
phase, observed interruption, unresolved dependencies and action needed. Detailed
status includes mechanism choices, frozen/transfer/restore/route times, bytes by
kind, pre-copy rounds/dirty sizes, outstanding lazy pages, original/active epochs,
retained sessions and explicit degradation. A healthy target is not "done" while
the source is still a network/page dependency.

Metrics/events include requested/admitted/refused, source fenced, state verified,
activation authorised, target active, source independent, completed/degraded and
recovery required; separate client-observed pause from source freeze duration.
Bound metric cardinality; per-migration identifiers belong in events/status
unless a bounded diagnostic series is needed. `wtf`/lint cover all modes,
unsupported hosts, stale reservations, missing no-swap policy, unresolved epochs,
credential reload requirements and orphaned owned resources. A matching CPU model
alone neither establishes nor rejects full compatibility.

Document the contract and the test command in the manual, quickstart and website,
and explain the implementation and assertions in the 0.4.0 book chapter. Product
flows explain preservation and pauses; CRIU plumbing goes in detailed diagnostics.

## 9. Built-in demonstration and cluster conformance

### 9.1 One command against the practitioner's cluster

Proposed front door:

```sh
relish test --filter migration
```

Reuse `src/testkit` rather than a second demonstration engine. Add a short
`migration` group whose first two application targets are **Redis, then PostgreSQL**
(section 9.2.1). It checks fresh capabilities, chooses a compatible pair, stages
digest-pinned database images through existing image infrastructure, leases an
isolated namespace, creates state, opens real client traffic, moves each database
A to B and back, observes the outcome and confirms cleanup. Run the targets
sequentially; serialise their moves and bound
resource use; default execution does not cordon/drain real nodes, restart Bun or
inject faults. Small bounded heap/disk data and a few minutes are targets to
measure, not a promised runtime. No operator TOML, Redis/SQL scripts, SSH, image
builder or external load generator is required.

The command automatically provisions both databases, credentials, schema/data
and built-in client observations, and removes its owned resources afterward.
Print separate Redis and PostgreSQL verdicts and the combined result. If either
required target is unavailable, refused or untested, the two-target demonstration
cannot pass. A later target-specific selection is explicitly partial, not the
default demonstration or strict conformance. These are future tests, not claims
that either database is already qualified.

Only qualified nodes run the fixtures. Known absence reports **not demonstrated**
with actionable reasons; collection failure is unknown. An explicitly requested
migration demonstration returns nonzero if it cannot exercise its core live
case. Optional subcases in broader development runs may be typed skips, but
never a headline migration pass. Preflight is observational and does not install
CRIU/change host configuration on an existing cluster; managed appliance/guest
packaging supplies prerequisites. Image staging and baseline checks happen
before timed migration.

Illustrative human output (these are not measurements):

```text
Testing migration: Redis, then PostgreSQL
  Source: worker-2   Target: worker-3

Redis:
Created random state in memory; no managed volume or persistence.
Opened a connection that stays open throughout the move.

Moving worker-2 -> worker-3...
  State preserved
  Connection preserved; no reconnection
  Longest response pause: 184 ms

Moving worker-3 -> worker-2...
  State preserved
  Connection preserved; no reconnection
  Longest response pause: 201 ms

PostgreSQL:
  Committed rows preserved across both moves
  Open transactions committed on their original sessions
  Active queries completed without resubmission
  Longest response pause: <measured per move>

PASS: Redis and PostgreSQL preserved their required state and sessions.
Cleanup confirmed.
```

The headline states the tested paths and budget. Correctness can pass while a
configured latency contract fails; print both verdicts. Add a small latency
trace if the terminal supports it, with the same underlying report in JSON.
Do not infer a percentile/SLO from one or two cutovers. `relish bench` reuses
the scenario machinery for repeated/larger performance envelopes.

### 9.2 Fixture and independent observations

Extend the existing test workload infrastructure with a small real OCI migration
fixture, packaged and signed/pinned for arm64 and x86_64. Main's ordinary host
`testapp` and BusyBox HTTP fixture are starting points, not proof of CRIU support.
Use normal migration APIs, runtime owners and data paths; no fixture-specific
migration implementation or restart reconstruction.

The fixture provides:

- Memory-only random data/challenge supplied **after startup**, never baked into
  its image, startup args, env or disk. The observer retains expected hashes;
  cold start cannot reconstruct the state automatically.
- A fresh startup identifier and continuously advancing computation. Stable
  logical identity or PID alone cannot distinguish replay of a stale checkpoint.
- A managed-volume journal, writable-root files, renamed/deleted entries and
  selected metadata; an open descriptor advances across a move.
- A persistent raw TCP session, a separately exercised ingress/WebSocket session,
  and an outbound session to an observer peer that is not on the moving source.
  The short database demo covers its declared native client paths (9.2.1);
  mechanism conformance covers the additional required ingress/outbound paths.
  New connections after cutover check publication/credentials.
- Numbered operations with response/state hash-chain evidence. A separate observer
  records requests, acknowledgements, unresolved responses and committed state.

Keep client retries/reconnection/resumption off in continuity cases. Record socket
opens, disconnects and session identities directly. A library silently reconnecting
would invalidate the connection assertion. The observer inside `relish` retains
an acknowledgement ledger; where cluster reachability requires a client peer,
place it on an independent node and collect its ledger. The migrated workload
cannot be the sole authority for whether progress was lost. Local API forwarding
is for management; traffic assertions use the declared real data-plane path.

Track acknowledged and unresolved operations separately; a reply lost in transit
is not proof that the operation was uncommitted. Compare acknowledged effects
with current state and an independent append sink; detect duplicate external
submissions without hiding them behind automatic retries/idempotent replay.
Some uncertain operations may require reconciliation. Measure maximum request
pause in each migration window and throughput recovery using the observer
monotonic clock, not cross-host wall-clock subtraction, a whole-run average or
unrelated successful requests. Observe an unmodified session across
both moves to reveal source-forwarding chains.

Also test a **memory-only app with no managed volume**, so drain cannot silently
classify it as stateless. Exercise additional kernel/fd metadata cases in the
full suite. The custom fixture remains mechanism/diagnostic coverage; the first
two practitioner-facing application targets and required real-application checks
are Redis and PostgreSQL below. A custom-fixture pass cannot substitute for either
database or establish general compatibility.

### 9.2.1 First two application targets: Redis and PostgreSQL

Pin actual Redis and PostgreSQL server images for arm64 and x86_64, with exact
version, digest and tested configuration in a versioned fixture manifest.
Provision them through ordinary app/runtime/storage/migration APIs under test
leases. Bundle the bounded protocol clients and observation logic in the test
runner; users should not need `redis-cli`, `psql`, SQL files or a load generator.
Do not patch either database to reconstruct lost state or substitute replication,
promotion, process restart or reconnect for movement of the original execution.

**Target 1: Redis.** The short demonstration starts a standalone memory-only
server with persistence disabled and no managed volume. Inject random values
after startup and continuously issue numbered writes/reads on an original TCP
connection during A->B->A. The independent ledger verifies every acknowledged
non-expiring value and sequence; no restart can reload the dataset from disk.
Observe server run identity as supporting evidence alongside the original socket
and data, not as the sole proof. Expiring keys are tested separately against
elapsed observer time and a declared tolerance: migration must not reset TTLs
or pretend that external time stopped.

The full conformance/release variants add managed-volume RDB/AOF persistence,
background saves and AOF rewrites under concurrent writes. Include active child
processes and all relevant files in the consistent cut; revalidate data and
persistence artifacts afterward. Redis documents these separate persistence
mechanisms and their forked background work
([persistence](https://redis.io/docs/latest/operate/oss_and_stack/management/persistence/)).
CRIU's Redis/containerd example is precedent, not proof of our source-off or
session contract ([example](https://criu.org/index.php?mobileaction=toggle_view_mobile&title=Containerd)).

**Target 2: PostgreSQL.** Start with a pinned PostgreSQL 18 build, explicitly
`io_method=worker`, a small managed-volume dataset, ordinary shared-memory settings
and no third-party extensions. Worker mode is the documented default and avoids
the specific io_uring descriptor blocker; this is a candidate configuration, not
evidence that the remaining kernel/runtime resources can restore
([I/O methods](https://www.postgresql.org/docs/18/runtime-config-resource.html#GUC-IO-METHOD)).
Keep durable commit settings; do not disable fsync or weaken acknowledgements to
make the demonstration easier. Move the whole server process tree, shared state,
data directory and WAL at a validated consistent cut, including any separately
configured storage paths. No disk-only crash recovery can stand in for preserved
in-memory transactions and sessions.

The short PostgreSQL case runs continuous numbered committed writes while a
separate original session holds an open transaction across each cutover. Check
its uncommitted work/session-local state, then commit once on that same connection
and verify the rows from another observer session. Also prove a bounded long-running
query is active before cutover and that its original request completes without
resubmission. Give that query its own bounded execution deadline; continuous
short probes measure cutover interruption, not its deliberate execution time.
All previously acknowledged commits must remain present; unresolved
commit responses are reconciled by operation identity and reported separately,
not treated as failed transactions or automatically retried. Repeated moves must
preserve both data and these session behaviours.

Full qualification adds concurrent vacuum/checkpoint activity, contention, larger
WAL/data and dirty-memory rates, and verifies storage integrity and continued
durability after movement. Tests must observe the intended background activity
overlapping cutover, not merely issue a command that finishes beforehand. A real
io_uring-configured negative case belongs in S8: it must refuse safely on the
researched unsupported CRIU release, not silently change backend or restart.

Both targets use section 9.2's independent acknowledgement/session observations,
per-cutover maximum pause, lease cleanup and fresh-connection checks. Their full
qualification includes actual drain followed by source shutdown/restart under
section 9.5. Report database version/configuration, persistence/I/O mode, workload
size and tested architecture/backend/direction. Passing these bounded standalone
configurations does not certify Redis Cluster/Sentinel, PostgreSQL replication/HA,
arbitrary extensions, other I/O methods or all production dataset sizes. Expand
those claims only with additional named cases and recorded evidence.

### 9.3 Leases, interruption and cleanup

Migration of lease-owned fixtures is required. Authenticate the current owner,
carry lease id and bounded lifetime across all migration records, reservations,
source/target copies, images and routes, and renew only under existing policy.
The source's administrative retirement fence survives lease expiry. Expiry or
Ctrl-C cancels desired execution and retires whichever generation may be active;
it never authorises rollback or drops a fence before stop evidence. Release
must collect resources on both nodes after any cutover phase. Node loss leaves
cleanup unknown/failed until confirmed; neither a vanished CLI nor missing
leader data justifies deleting authoritative state.

Do not evade this by deploying unleased fixtures. Existing server policy and
roles still apply. Basic fixture moves use the least applicable workload grants;
actual node drain/shutdown/fault injection requires administrative node-state
authority and explicit acknowledgement. Test resources/cordons/faults are owned
and reversed without clearing another owner's state. A cleanup failure makes
the run nonzero even when migration assertions passed; retain an evidence report
and resource identifiers without secrets for follow-up.

### 9.4 Versioned migration conformance

Proposed strict command:

```sh
relish test --profile migration
relish --output json test --profile migration
```

Add a versioned **required-case manifest** and profile expansion, not just another
`profile_requires_case` branch. `--profile migration` selects the complete required
non-disruptive set. A filter excluding a required case either refuses the conformance
request or produces an explicitly partial/non-conformant report; all selected cases
passing is insufficient. The strict profile fails on required skips, unknown or
inferred-only evidence, deadlines, unexpected restart/degradation, latency-budget
violations and unconfirmed cleanup. Architecture-dependent optimisations are
not mandatory when another proven method satisfies the same contract.

| Contract area | Required case/observation |
|---|---|
| Cold data relocation | Managed volumes move correctly; restart and fresh-root semantics are explicit. |
| Checkpoint | Memory, computation, open-fd state and filesystem cut survive; declared TCP closure is tested, not confused with live. |
| Live | Original computation and qualified inbound/outbound/ingress sessions survive A->B->A with no restart/reconnect. |
| In-memory-only intent | No-volume checkpoint/live apps follow migration policy during actual drain qualification. |
| Database targets | Required Redis memory-only and persistence variants, and PostgreSQL worker-mode durable writes/open transactions/active queries, as specified in 9.2.1. Neither database can be replaced by the custom fixture or omitted from a complete manifest. |
| Storage | Each claimed backend preserves content/deletions/metadata and quota enforcement; copied data is authoritative only after validated cutover. |
| Identity | Logical replica/job execution is retained; fresh authenticated operations succeed after movement and rotation/reload. |
| Jobs | Exact execution/run/retry ownership survives; completed work and observed external effects are not replayed. |
| Admission/refusal | Unsupported modes/features, conflicts, resource reservations and real incompatible targets refuse safely. Named CRIU resource fixtures test refusal, source continuation and supported counterparts; a later dump-time rejection establishes source outcome under section 5.6. |
| Lifecycle | Duplicate requests, cancellation, disconnected CLI and lease expiry retire resources correctly; no forbidden old generation starts. |
| Required observability | The report binds observed state/session/latency results to exact nodes, paths and contract; source independence is independently qualified. |

Missing heterogeneous hardware is not a fake refusal-test pass. Core/model tests
cover synthetic incompatible feature matrices; cluster reports state which actual
incompatible targets were exercised. Capability claims are available/unavailable/
unknown with fresh evidence, never inferred from a generic API error.

Conformance scopes migration pools by architecture, CPU/features, runtime, CRIU,
kernel/page size, storage and network topology. Directed compatibility can be
asymmetric. Record tested directions and paths: the short demo's one pair is not
a certificate for every node. Full qualification covers every node with suitable
incoming/outgoing evidence and representative directed capability-class/backend/
network edges; where that cannot cover a declared pool, report it unqualified.
Use bounded planning rather than automatically claiming all N*(N-1) pairs were
tested. A pass is for the recorded contract/version/configuration/run, not an
indefinite certificate after hosts, routes or binaries change.

Report schema additions: contract/manifest version and selection completeness;
build and per-node runtime/CRIU/kernel/architecture fingerprints; node/pool/direction,
image digest, storage backend, network/ingress/egress paths; workload state size,
database version/configuration and persistence/I/O mode where applicable;
dirty/client rate and interruption budget; acknowledgement/state/session evidence;
per-cutover maximum pause and recovery tail; methods used and outstanding source
dependencies; refusal/recovery/degradation; and independent cleanup. Redact memory,
credentials and sensitive payload contents. Bump the report schema intentionally.

The normal migration profile establishes planned-move behaviour while the source
is available and validates dependency receipts. **Full cluster migration conformance
also requires the recovery/source-shutdown qualification below**; a normal-profile
pass alone must say source shutdown was not exercised. Fold non-disruptive cases
into 0.4.0's `full-runc` acceptance and include all tiers in the release gate.

### 9.5 Recovery conformance and the source-off test

| Level | Purpose | Default execution |
|---|---|---|
| Demonstration | Small observable A->B->A move on the practitioner's nodes. | Only owned fixtures move. |
| Planned-move conformance | Complete versioned cold/checkpoint/live apps/jobs checks. | Isolated workload operations; no node shutdown. |
| Recovery/source-independence conformance | Ownership, current-state recovery, real drain and source loss. | Explicit acknowledged disruptive suite on suitable nodes. |
| Release qualification/soak | Larger repeated moves, real applications, both architectures/backends and shutdown. | Disposable release clusters; retained artefacts. |

Proposed recovery entry point:

```sh
relish test --chaos --profile migration-recovery --yes
```

Extend the existing chaos catalogue with named migration scenarios and that
versioned manifest. The profile expands to the complete required recovery and
source-off selection. Existing chaos filters select scenario names, not integration
group names; a filtered recovery run is partial and cannot certify the manifest.
The ordinary `--profile migration` result and this recovery report are combined
only when their contract/build/pool fingerprints match. Consent cannot widen
server policy. Serialize faults and select node sets that preserve required quorum and independent observers. Never drain or shut
down an arbitrary practitioner node as part of the short demo.

Recovery cases include leader change, source/target Bun crash, uncertain restore,
partition at each activation boundary, source loss during lazy-page service,
target loss after acknowledged writes, repeated/late instructions, stop/delete,
lease expiry and return of the old source. They assert the declared outcome:
pre-activation safe abort, current-state recovery, or explicit held unavailability
outside the failure envelope. A deliberately induced, observed recovery-required
response can satisfy a recovery case only with evidence of the expected fences,
refusal to replay and bounded reporting; an observer that cannot establish those
facts yields unknown and fails the run. Never require a green memory-losing restart.
At-most-one-writer checks combine external observations, generation receipts and
model tests; finite sampling alone is not proof.

**Decisive source-off scenario:**

1. Allocate a disposable, authorised source node and fixture/observer topology.
   Keep the CLI entry route and necessary quorum/observers off the stopped source,
   except separately declared tests that cover moving those dependencies too.
2. Populate memory/filesystem state, open inbound/outbound/ingress sessions and
   record acknowledgements independently under continuous traffic.
3. Run the actual `relish drain` path, including a no-volume memory workload and
   a running job. Include both Redis and PostgreSQL in required database
   qualification, preserving their original sessions and database assertions.
   Verify it reports source-independent completion.
4. Immediately stop the source VM or use an equivalent proven loss of its complete
   data plane. Do not replace this with Bun exit or `NodeTransportGate` quiescence;
   those can leave NAT/tunnel/proxy dependencies alive.
5. Continue the **same** sessions and check acknowledged state, active ownership
   and interruption budget. Then restart the source and verify retired generations
   cannot resurrect. Restore only the cordon/fault changes owned by this run.

Managed laptop clusters use their existing VM lifecycle machinery with independent
management routing and quorum safeguards. Other clusters need an authorised
external shutdown adapter/equivalent demonstrably complete isolation. If unavailable,
report source-off coverage untested and withhold full conformance; do not make an
SSH/hypervisor setup prerequisite for the ordinary demonstration. A retained
source-forwarding window cannot pass this scenario by sleeping until sessions end
or resetting them before power-off. Recovery evidence continues beyond the shutdown.

## 10. Spikes, tests and qualification

None of the following are implementation results. Run them on the then-current
main generated OCI spec, qualified runc/CRIU packages and real kernels. Do not
assume the September soak still owns particular VMs; acquire suitable disposable
resources. x86_64 pre-copy and arm64 methods need separate hardware evidence.

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
| S16 | Tiny signed/pinned mechanism fixture plus Redis first and PostgreSQL worker-mode second, on both architectures. Prove memory-only/persistent data, original sessions, open SQL transactions and active queries with independent ledgers and no reconnection/retry masking (9.2.1). |
| S17 | Migration under authenticated test leases; expiry/stop/cleanup at every boundary on both nodes. |
| S18 | Versioned profile completeness, directional pools and recorded evidence; partial/skip/unknown cannot certify conformance. |
| S19 | Actual drain followed by source VM shutdown/restart under live traffic, with independent entry/observer topology and no resurrection. |

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
Match `make test-linux`/`make test-cluster` and add a migration gate only if ownership
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

**Release/soak:** repeat A->B->A under load with the volume writer, memory-only
counter and the required Redis/PostgreSQL fixtures; exercise main's recovery
faults and each database's source-off case. Cover both architectures, every claimed
backend, ingress/egress
paths, jobs and credential continuity. Retain acknowledgement/session ledgers,
per-cutover latencies, generation transitions, fingerprints and cleanup outcomes.
Retries must not erase first failures. No pass on an empty run or skipped-only lane.
Main's release qualification scripts own hardware/soak execution and save records
in `docs/qualification/`; reuse the versioned conformance manifest so release
checks cannot drift into a separate weaker demonstration.

**Packaging:** package pinned qualified CRIU/runc in the appliance/managed Linux
guests, check Ubuntu 26.04 availability before depending on a PPA, and expose
runtime capabilities in `wtf`/dry-run. No automatic PPA installation on an existing
cluster from `relish test`. Verify signed mechanism and Redis/PostgreSQL fixture
image availability before timing; support existing mirrors/local staging rather
than depending on a mutable tag.

## 11. Implementation order and effort

The old **23-29 focused weeks** was for the source-forwarding/automatic-cold-fallback
design. It is historical, not the estimate for this revised contract. Portable
network/NAT/ingress ownership, partition-safe activation, key/credential recovery,
lease integration and conformance add material work. Re-estimate after S1-S19
resolve the architecture; do not mechanically add a few weeks to the old total.

Work in dependency order, behind meaningful tests, while keeping the full 0.4.0
release acceptance scope:

1. Refresh main evidence; implement fixture/observer and ownership/policy/model
   tests; prepare Redis then PostgreSQL fixtures and run their feasibility spikes
   before locking the transport/network design.
2. Cordon/drain ownership and status, held assignments, exclusive source fencing,
   activation and safe recovery. Cold managed-data moves provide the first path.
3. Stable logical/storage/job identities, consistent copy and capacity reservations.
4. Checkpoint runtime/stdio/time/egress, secure no-swap transfer and recoverable
   journals/keys; credential continuity. Establish real Redis and PostgreSQL
   checkpoint/state/session results before extending live guarantees.
5. Qualified memory/filesystem pre-copy and optional post-copy with honest failure
   envelope; interruption measurements.
6. Source-independent addresses, egress mapping and ingress ownership; actual
   source-off acceptance and repeated moves.
7. Demonstration/catalogue/lease ownership, versioned conformance, capability/pool
   evidence, JSON reports and benchmark envelopes throughout the work, not bolted
   on after the mechanisms.
8. Required Redis/PostgreSQL recovery/soak on both architectures/backends,
   documentation and the new 0.4.0 book chapter; final release qualification.

Intermediate mechanisms can land with their actual weaker guarantees, but cannot
be advertised as live-conformant or used to declare a seamless drain complete.
GPU warm starts, non-runc/rootless migration and automatic rebalancing remain later
projects. Source-independent network ownership is no longer deferred if needed
to satisfy this release's continuity contract.

## 12. Consistency review and unresolved feasibility

Review completed 5 October against fetched main `ca2c33a25ef7dc8837ca714e20468c2ae17e5d98`.
CRIU primer and refusal coverage reviewed 6 October against the same freshly
fetched main and upstream v4.2.1; main has not advanced since the preceding review.
The same day's database revision makes Redis and PostgreSQL required application
targets in the demo, strict manifest, source-off qualification and implementation
order; it does not record completed database migration experiments.
This is a design/document review, not completed migration qualification.
Validation on this refreshed checkout: `git diff --check`, section/fence/local-link
checks, all 19 pinned-main source references and proposed TOML syntax passed.
`make ci` passed (formatting, both Clippy configurations, portable tests,
doctests, CI-policy tests and ignored-test ownership); localhost test servers
required execution outside the sandbox. Source/test/build files match the
reviewed main exactly. No real CRIU/migration or source-shutdown experiment was
run, and the proposed migration commands/profiles remain unimplemented.

| Old weakness | Revised rule and proving work |
|---|---|
| Automatic cold fallback can empty memory-only Redis or replay a job. | Required outcomes and explicit restart fallback; separate degraded result. Sections 1, 5.1, 5.6, 8-10. |
| Target may run before a health/restore receipt, then stale source rollback loses writes. | Activation authorisation is the recovery boundary before restore; current-state recovery/fencing. S12 and recovery cases. |
| Ten-minute source forwarding conflicts with shutdown/session continuity. | Prototype only; portable address plus egress/ingress ownership; S11/S19 release gates. |
| Short freeze conceals lazy-page stalls and source-loss risk. | Client-window latency plus explicit provider/failure envelope; no implicit cold restart. S7/S10. |
| No-volume apps treated as stateless during drain. | Migration intent takes precedence; dedicated memory-only drain case. |
| Fresh instance identity assumes main still lacks ordinals. | Stable logical replica/job execution; fresh host generation. Main evidence and S3/S5. |
| New certificate plus immediate old revocation breaks cached identity. | Credential mobility/reload design and fresh-handshake qualification. S14. |
| Size/mtime/tar assumed complete filesystem consistency. | Verified final cut and metadata/import/quota semantics. S13. |
| Tmpfs assumed never spills; lost transfer key forced restart. | Enforced no-swap reservations and recoverable protected keys; S15. |
| Test fixture leases forbidden; source node fault mistaken for power-off. | Authenticated cross-node lease lifecycle; actual VM/data-plane loss. S17/S19. |
| Selected green tests mistaken for whole-cluster conformance. | Versioned manifest completeness, pools/directions, observed evidence and tiers. S18. |
| CRIU installation or a successful fixture implies arbitrary workloads can move. | Explain dump blockers, conditional resources and external-state boundaries; workload-specific and late-resource qualification with refusal fixtures. Section 3, S8 and strict conformance. |
| Real database compatibility is deferred behind a synthetic fixture. | Redis first and PostgreSQL second are required targets, with memory-only state, durable data, open transactions, active queries and source-off evidence; 9.2.1 and S16. |
| Every deadline assumed to yield terminal successful ownership. | Held recovery/unavailability when proof is missing; model tests permit it. |
| Stale baseline/protocol and job attempt design. | Main 40/58 and existing job generations/spec digests/replay fences, refreshed before implementation. |

Remaining design blockers: partition-enforced ownership in each topology;
portable external NAT and proxy sessions; restore on the real userns/overlay/
egress spec; consistent filesystem/kernel-fd restore; credential handling;
protected key/no-swap recovery; and quantified interruption envelopes. The named
spikes resolve them. No feasibility or performance result is invented here.
If the selected network topology cannot move a required external address, or a
workload cannot meet its timeout/credential contract, refuse that combination
and state the scope. The full release promise waits for passing source-off and
conformance evidence rather than rewriting the success criteria around a demo.

## 13. Decisions and revisions

### Original maintainer decisions, 28 September 2026

1. Ship the full drain/cold/checkpoint/live stack in 0.4.0, for apps and jobs.
2. Pre-1.0 has no backwards compatibility; bump formats and start fresh, without
   PR #266's gate.
3. App/job mode is `cold | checkpoint | live`; unspecified apps move cold.
4. Host-path apps block drain; explicit force stops them with data left in place.
5. Plaintext process dumps must fit in memory and never spill to disk.
6. Use upstream CRIU packaging, minimum 4.2, after verifying the guest OS package.
7. Revoke the source instance's workload certificate on completion.
8. Refuse CPUs missing source features, with a leader check and restore check.
9. Write a new 0.4.0 book chapter.

### Review incorporated with maintainer authorisation, 5 October 2026

The maintainer requested that the PR incorporate the continuity/recovery review,
built-in demonstration and cluster conformance proposal, then be checked for
consistency against current main. This revision keeps the full release intent
and distinguishes historical choices from required new design work:

- Checkpoint/live default to preserving state, with restart fallback only by
  explicit policy. Jobs that cannot safely restart block a cold drain. Successful
  resumed jobs retain their execution/retry identity; cold jobs do not claim it.
- Source forwarding and layer dropping cannot satisfy required live continuity.
  Source independence, including NAT/ingress, is a release acceptance gate.
- Source snapshots become stale once activation is authorised; no timeout/error
  authorises stale replay. Positive fences/current-state evidence govern recovery.
- Original certificate decision 7 needs S14 revision: preserving a cached target
  credential and revoking it immediately are incompatible. Authorised credential
  mobility or a tested reload-before-revocation path must be selected; this is
  unresolved design, not an assertion that the invariant has already changed.
- Original CPU check is retained as defence in depth within qualified pools.
- The former memory-only transfer-key choice is replaced by a proposed protected
  recovery design in S15. No plaintext dump-on-disk exception is introduced.
- `relish test` gains the one-command demonstration and versioned conformance;
  lease-owned fixtures must migrate safely. Actual source shutdown and recovery
  are separate acknowledged cases and mandatory full-conformance/release evidence.
- The old effort estimate is superseded pending the expanded spikes. All code
  assertions use fetched main, and the merge preserves the PR's existing history.

### Database demonstration targets, authorised 6 October 2026

The maintainer selected Redis and PostgreSQL as the first two demonstration
targets. Section 9.2.1 defines their initial configurations and observations;
both are required in the one-command demonstration and versioned conformance,
with persistence/background-work and source-off variants in full qualification.
Database versions and image digests are pinned during implementation/qualification;
no unrun experiment or universal database compatibility is asserted here.
