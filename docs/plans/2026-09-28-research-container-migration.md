# Research: moving a running container between nodes (0.4.0)

28 September 2026. Research and a recommendation only; no product code.
The maintainer answered the open questions the same day. The
[decisions](#13-decisions-28-september-2026) are at the end, and the scope,
design and estimate below follow them. The release is **0.4.0**, "Full
container migration".

## Progress (for whoever resumes this)

- [x] Branch `research/container-migration` from `origin/main` (ff854cbf)
- [x] 1. Recommendation (summary)
- [x] 2. What the codebase does today (verified by reading the code)
- [x] 3. CRIU, runc and the ecosystem today (sourced)
- [x] 4. GPU checkpointing and where the demand really is (sourced)
- [x] 5. Design for Reliaburger
- [x] 6. Compatibility (pre-1.0: bump and start fresh)
- [x] 7. Security
- [x] 8. Observability and UX
- [x] 9. Demo
- [x] 10. Testing plan
- [x] 11. Effort and phasing
- [x] 12. Where the initial analysis holds and where it doesn't
- [x] 13. Open questions for the maintainer
- [x] Draft PR opened (#268)
- [x] Maintainer decisions recorded (28 September 2026); scope, phasing and
      effort revised for the full version

All sections are written and the decisions are in. Next step: after the
release soak frees the Lima VMs, and with an x86_64 Linux host for the
pre-dump work, spikes S1-S11 (section 10). If a spike shows a piece can't
work on our spec, go back to the maintainer rather than drop it quietly.

## 1. Recommendation

*Revised after the maintainer's decisions of 28 September 2026. The first
draft proposed shipping cold moves and drain in 0.2.0 with CRIU as an
experimental opt-in, and leaving pre-dump, lazy pages, TCP handoff and jobs
for later. The maintainer said no: 0.4.0 ships the whole thing.*

0.4.0, "Full container migration", ships every layer of the move, for apps
and jobs:

1. **Drain and uncordon.** `relish drain <node>` cordons a node and empties
   it; `relish uncordon` puts it back. Stateless instances are rescheduled;
   stateful ones are migrated.
2. **Cold moves that carry volumes.** Stop the source, copy its managed
   volumes, cold-start on the target. This is what drain does unless the app
   asks for more.
3. **CRIU checkpoint and restore** (`mode = "checkpoint"`): the same move,
   plus the process memory, through `runc checkpoint` and `runc restore`.
4. **Live moves** (`mode = "live"`): volume pre-sync while the app runs,
   iterative memory pre-dumps, lazy pages (post-copy restore) and TCP
   handoff, so established connections survive and the frozen time covers
   the last delta, not the whole memory.

When CRIU refuses at dump time, a move falls back to cold on the target with
the data already moved; within live mode, each layer whose precondition is
missing is dropped on its own (section 5.7).

Why still build it in that order? Because each layer needs the one below
it. The fallback the maintainer asked for (a clean restart when CRIU says
no) is itself a volume-carrying move, and Reliaburger can't do one today.
Managed volumes are node-local and the scheduler sends an app back to the
node that holds them (`VolumeHome`, `src/cluster/orchestrate.rs`). Incus just
shipped the same layering as "near-live" migration after calling CRIU
fragile ([Incus 7.4](https://linuxiac.com/incus-7-4-adds-near-live-container-migration-for-zfs-and-btrfs/), accessed 28 Sep 2026).

Two corrections to the brief still stand. First, there is no drain to hook
into: `relish drain` is still marked *planned* in the whitepaper, and the
only "node drain" in the code is a chaos fault. Second, planned binary
upgrades don't need migration at all, because runc owners outlive Bun and
the new Bun adopts running instances. The maintenance case that does need it
is a host reboot (kernel, firmware, hardware).

Two hard limits shape the design:

- **Pre-dump needs soft-dirty tracking, which arm64 mainline kernels don't
  have.** Live mode therefore takes two shapes, chosen per migration from
  what the source node reports (`criu check --feature mem_dirty_track`): on
  x86_64, pre-dump iterations then a short final dump; on arm64, a single
  dump with lazy pages, so the frozen time is still small but the memory
  streams in after restore. The Apple-silicon quickstart can demo live mode;
  pre-dump itself needs x86_64 hardware to test (the 0.3.0 Wyse 3040s are
  x86_64, and so are GitHub's hosted runners). Section 5.7.
- **TCP handoff collides with node-local addresses.** Container addresses
  come from a per-node `/23` and are never routed between nodes; clients
  reach instances at `node_ip:host_port`. Keeping a connection means keeping
  its 4-tuple on both ends. The design (section 5.3) takes the container
  address over on the target and tunnels the handed-off connections through
  the source for a bounded window. It needs no cluster-wide routing change,
  at the price of the source staying up until those connections end or the
  window closes.

Still runc-rootful only. Position it for long-running batch work first (the
use with production evidence), then single-instance dev and game servers.
GPU warm starts stay out: every shipping product we could verify uses gVisor
or CRIU with NVIDIA patches that aren't upstream yet, we have no GPU
hardware, and CUDA checkpointing needs host RAM at least as large as the GPU
memory in use.

Effort: roughly **23 to 29 focused weeks** for one engineer, about twice the
first draft's 12 to 16, and about half of it CRIU-specific now. See
[section 11](#11-effort-and-phasing).

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

## 3. CRIU, runc and the ecosystem today

All links accessed 28 September 2026. "Unverified" means we couldn't find a
primary source; treat it as a spike question, not a fact.

### CRIU

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
- **What it refuses.** io_uring (the GSoC 2021 work was never finished,
  [PR #1597](https://github.com/checkpoint-restore/criu/pull/1597)), POSIX
  message queues, physical devices other than the null/zero/tun family,
  ptraced tasks ([what cannot be checkpointed](https://criu.org/What_cannot_be_checkpointed),
  [GSoC ideas](https://criu.org/Google_Summer_of_Code_Ideas)). io_uring is the
  one that'll surprise people: recent runtimes and databases use it.
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
  ([LPC 2024](https://lpc.events/event/18/contributions/1811/)). Migrate only
  between identical CPU models, or accept the crash.
- **Lower downtime later.** Pre-dump and `--track-mem` rely on soft-dirty
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
| Kubernetes KEP-2008 | kubelet `POST /checkpoint/...` writes a tar | **none in core** | no | Beta since 1.30, still beta ([kep.yaml](https://raw.githubusercontent.com/kubernetes/enhancements/master/keps/sig-node/2008-forensic-container-checkpointing/kep.yaml), [kubelet API](https://kubernetes.io/docs/reference/node/kubelet-checkpoint-api/)). Restore happens below Kubernetes: CRI-O since 1.25, containerd 2.1 ([containerd#10365](https://github.com/containerd/containerd/pull/10365)) |
| Kubernetes KEP-5823 | pod-level | yes | **explicit non-goal** | Alpha scope is same node, same pod object; cross-node restore, volumes and devices are non-goals ([KEP-5823](https://github.com/kubernetes/enhancements/tree/master/keps/sig-node/5823-pod-level-checkpoint-restore)). A [secondary source](https://palark.com/blog/kubernetes-1-37-release-features/) puts alpha in 1.37 |
| Docker | experimental `docker checkpoint` | yes | not as a feature | [docs](https://docs.docker.com/reference/cli/docker/checkpoint/) |
| Incus | CRIU for containers | yes | yes | Docs: only "very basic containers" migrate reliably; 7.4 added snapshot-sync "near-live" moves instead ([docs](https://linuxcontainers.org/incus/docs/main/howto/move_instances/), [7.4](https://linuxiac.com/incus-7-4-adds-near-live-container-migration-for-zfs-and-btrfs/)) |
| Apple `container` | none | none | none | No checkpoint, suspend or migrate command ([command reference](https://github.com/apple/container/blob/main/docs/command-reference.md)) |

The Kubernetes [Checkpoint/Restore Working Group](https://www.kubernetes.dev/blog/2026/01/21/introducing-checkpoint-restore-wg/)
(January 2026) lists migration for maintenance among its use cases, so
cross-node restore in a mainstream orchestrator's *core* is genuinely
unshipped. That's the honest version of "more than anyone else ships": true
for core Kubernetes, not true for Podman, Borg, Cast AI or Cedana.

**ProcessGrill and Apple Container can't take part in the CRIU mode.**
Apple's runtime runs each container in a VM behind a CLI with no checkpoint
verb. ProcessGrill runs host processes with no namespaces: CRIU would have to
restore host PIDs, host paths and host sockets on another host, which is the
case CRIU is worst at. Both refuse `--checkpoint` with a clear error. The
cold-start move (section 5) could serve them later, but 0.4.0 keeps it
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

### 5.1 Scope for 0.4.0

The maintainer's "checkpoint-on-drain" survives, with two changes: the drain
has to be built, and the restart fallback has to be a *move*.

**In:**

- `relish drain <node>` and `relish uncordon <node>`: cordon the node (no new
  placements), then empty it. Instances without managed volumes are
  rescheduled the way a rolling deploy would, respecting `max_unavailable`
  (the whitepaper's promise for drain). Instances with managed volumes, and
  running job attempts, are *migrated*. Instances with a host-path volume
  block the drain; `relish drain --force` stops them, with their data left
  in place.
- `relish migrate <namespace>/<app> --to <node> [--instance <id>]
  [--mode cold|checkpoint|live]`: move one instance on purpose, and
  `relish migrate <namespace>/<job> --attempt <n> --to <node>` for a running
  job attempt.
- Three modes, one pipeline, opted into per app or job with
  `[app.<name>.migration] mode = "cold" | "checkpoint" | "live"` (and
  `[job.<name>.migration]`). A drain moves anything that hasn't opted in
  cold.
  - **cold**: stop the source, copy its managed volumes, cold-start on the
    target. Works for any rootful runc app or job.
  - **checkpoint**: the same, plus `runc checkpoint` on the source, the CRIU
    image and the writable rootfs layer in the payload, and `runc restore`
    on the target. If CRIU refuses at dump time, the source is still running
    (CRIU resumes it on failure) and the migration continues as **cold**. If
    restore fails on the target, it cold-starts there with the volumes
    already moved.
  - **live**: checkpoint, plus volume pre-sync while the app runs, memory
    pre-dump iterations (x86_64) or lazy pages (both architectures), and TCP
    handoff (section 5.7).
- Jobs: a running attempt moves without spending a retry. It keeps its
  attempt number; the job's durable attempt ledger (`src/bun/jobs.rs`) and
  the batch tracker in Raft record the node change under the migration id,
  so a crash mid-move can neither count it as a failure nor run it twice.
- Rootful runc only.

**Out, explicitly:**

- GPUs, rootless runc, Apple Container, ProcessGrill: `migrate` refuses them
  up front, and drain reports them as blocking (or stops them with
  `--force`).
- Automatic triggers: rebalancing, spot/preemption notices, autoscaling.
  Rebalancing is a scheduler policy question we shouldn't answer in the same
  release as the mechanism.
- Binary upgrades. They don't restart workloads (section 2).
- Handing off a connection that doesn't come through the source node's
  address or masquerade (a client on the source node that dialled the
  container address directly). It's reset.

**Eligibility** (checked by the leader before anything happens, reported by
`relish migrate --dry-run`):

1. The app runs on runc, rootful, with no GPU and no host-path volume.
2. The instance is the only instance of its app on the source, and the target
   runs none. Managed volumes are per app per node, so moving one of two
   instances would either strand the other's volume or merge two copies on the
   target. Singletons are the target workload anyway.
3. No rollout, stop, delete, upgrade, test lease or other migration is active
   for the app or either node.
4. The target is alive, ready, not cordoned, matches placement labels, has
   the resources, and has disk (and tmpfs, for checkpoint mode) for the payload.
5. Checkpoint and live modes: both nodes report CRIU 4.2 or newer and the
   source's CRIU is no newer than the target's; the target's CPU has every
   feature the source's had (section 7); and the source can hold the
   plaintext dump in memory (section 5.5). A mode that fails these is
   **refused** with the reason, never silently downgraded, and
   `relish migrate --dry-run` says which modes would pass. During a drain,
   an opted-in instance that fails them is listed as blocking, and the
   operator can move it cold explicitly.
6. Live mode's extra layers each have their own precondition (section 5.7).
   A missing one drops that layer, not the move, and the status says which
   layers ran.

### 5.2 What travels

| State | Where it lives on the source | Cold | Checkpoint | Live |
|---|---|---|---|---|
| Managed volumes | `volumes/<ns>/<app>/...` (plain, loop ext4, Btrfs) | tar stream | tar stream | pre-synced while running, last delta after the freeze |
| Writable rootfs layer | `<bundle>/rootfs-upper` | not moved (a cold restart starts fresh today too) | tar stream (restored files reference it) | pre-synced, last delta after the freeze |
| Process memory, fds, namespaces | kernel | none | CRIU image | pre-dumps and a final dump, or lazy pages from the source's page server |
| Image layers | image store | target pulls through Pickle **before** the source stops | same | same |
| Captured stdout/stderr | `output.stdout`/`output.stderr` | stays; Ketchup has already ingested it | stays, see 5.4 | same |
| Network identity | `/23` per node | new address and netns on the target | same; netns is external to CRIU | the container address is lent to the target for the handoff window (5.3) |
| Workload certificate | per instance, key generated on the node | new instance, new certificate | new certificate; old one revoked (section 7) | same |

The copy of each volume stays on the source, renamed out of the way
(`volumes/.migrated/<migration-id>/...`), until the migration completes and a
retention period passes. That's what makes the fallbacks below possible, and
it's also the one-writer rule from PR #267 in another form: the source copy
is never mounted again unless the migration resolves *back* to the source.

A plain `tar` of a stopped volume is consistent for every backend, so cold
and checkpoint modes don't need `btrfs send`. Live mode pre-syncs while the
app runs, then sends only the last delta after the freeze, as Incus 7.4
does: Btrfs volumes with a snapshot and incremental `btrfs send`, the other
backends with repeated rsync-style passes (a file whose size or mtime
changed goes again). The final pass runs after the freeze, so it's
consistent for every backend.

### 5.3 Identity and networking

- **Instance identity.** Placements carry no instance identity, so the
  target allocates a fresh local `InstanceId` as it would for any new
  replica. A `MigrationId` links the two in events, logs and status. The
  app's service name, DNS name and service VIP don't change.
- **Discovery.** The source instance retires the normal way, so its routes
  go through the existing withdrawal ledger and every consumer acknowledges
  them before the address is reused. The target instance is published by the
  leader once its health check passes. Nothing new is needed here, and that's
  one of the reasons to keep addresses node-local.
- **TCP in cold and checkpoint modes.** Drop. `runc checkpoint` has no
  `--tcp-close` flag, so we pass CRIU's `tcp-close` through the
  `org.criu.config` annotation (a small CRIU config file Reliaburger writes
  per checkpoint). Without it, CRIU refuses to dump a process with an
  established connection.
- **TCP handoff in live mode.** CRIU's `--tcp-established` restores a
  connection with `TCP_REPAIR`, but only if the restored socket keeps its
  4-tuple: the same local address in the target's netns, and the peer's
  packets still reaching it. Neither holds today. Container addresses come
  from a per-node `/23`, are never routed between nodes, and two nodes whose
  names hash alike can own identical-looking ranges. Every client, inside or
  outside the cluster, reaches an instance at `node_ip:host_port` through the
  source's nftables DNAT, and outbound traffic leaves through the source's
  masquerade. Both directions of every existing connection are pinned to the
  source node.

  Two designs fit:

  1. **Forward through the source (proposed).** The target's netns borrows
     the instance's container address for the migration: the target
     reserves it in its `network_leases` journal under the migration id, and
     refuses the handoff if it collides with an address of its own (the move
     then drops connections and takes a fresh address, as checkpoint mode
     does). Before restoring, the target reads the handed-off sockets'
     4-tuples from the CRIU image into an nftables set. An IPIP tunnel
     between the two node addresses carries those flows and only those.
     Inbound packets still arrive at the source, pass its existing DNAT and
     conntrack entry, and go down the tunnel instead of to a local veth; the
     target policy-routes replies for tuples in the set back up the tunnel,
     where the source's conntrack reverses the NAT exactly as before.
     Outbound connections work the same way through the source's
     masquerade. New connections never touch the tunnel: Onion publishes the
     target's own `node_ip:host_port` backend once it's healthy, and the
     source's is withdrawn through the existing ledger. The tunnel lasts for
     a handoff window (default 10 minutes, per app), then comes down and any
     survivors are reset. The source can't drain away while connections
     still use it, so `relish drain` waits for the window (or `--timeout`),
     and if the source dies the handed-off connections break, which is what
     happens to them today anyway. Cost: about 3-4 weeks, most of it the
     fencing (the borrowed address goes back through the same lease and
     withdrawal rules as any other, `src/onion/lease.rs`) and the crash
     cases. Nothing changes in how the rest of the cluster routes.
  2. **Cluster-routable instance addresses.** Give every instance a
     cluster-unique `/32` allocated by the leader and routed between nodes
     (routes over gossip, or an overlay), so an address can simply move.
     Cast AI needed a forked AWS VPC CNI for the same thing. It takes the
     source out of the path, but it rewrites Onion's routing and its lease
     invariant ("container addresses are never routed between nodes"), the
     firewall's keys and every address test, for one feature. About 6-8
     weeks, and a new class of split-brain bugs. Not for 0.4.0; it's the
     answer if handoff through the source proves too limiting.

  Ingress (Wrapper) holds its own upstream connections to the instance, and
  from the source's point of view they're ordinary handed-off connections,
  so they survive too. Spike S11 proves the tunnel and `TCP_REPAIR` together
  on our spec.
- **eBPF and firewall.** Onion's maps and the Sesame firewall live on the
  host and key on container addresses and cgroup ids, so nothing about them is
  inside the CRIU image. The target programs them for its new instance as
  usual. The catch: Bun installs cgroup-keyed egress policy between `runc
  create` and `runc start`, and `runc restore` has no such gap. The plan is
  to pre-create the cgroup, install the policy for its id, then restore into
  it (spike S4). If that can't be made reliable, checkpoint mode refuses apps
  with an egress policy.
- **Time.** Checkpoint-mode containers get a time namespace (OCI 1.1,
  runc 1.2+) so CRIU can keep `CLOCK_MONOTONIC` continuous across hosts.

### 5.4 The stdio problem

runc only re-attaches pipe descriptors on restore. Our containers write
stdout and stderr to regular files in the owner's directory, so CRIU would
try to reopen the source's file paths on the target, where they don't exist
(or have a different size, which fails file validation). Three ways out, in
order of preference, to be settled by spike S2:

1. **CRIU `inherit-fd` through `org.criu.config`.** If runc's `criu swrk`
   child inherits runc's own stdout and stderr (which the owner points at the
   *target's* capture files), a config line mapping the image's file paths to
   those descriptors makes CRIU reuse them. Smallest change, if runc doesn't
   close them first.
2. **Pipes for checkpoint-mode containers.** Give the container pipes and
   have the owner copy them into the capture files. runc then handles
   restore for us. It changes the owner, which is some of the most
   carefully fenced code in the tree, so it's the fallback.
3. **Recreate the path.** Make the target's capture file path identical and
   pre-size it. Fragile; last resort.

### 5.5 Transfer

- **Channel.** The target *pulls* the payload from the source over a new
  node-to-node route on the Bun API (`System` principal, node-id mTLS, as the
  other node routes). Pulling lets the target control disk use and resume a
  broken transfer with a range request.
- **Not Pickle.** Pickle already moves build contexts as blobs, so it's
  tempting. But its store is catalogued, replicated, garbage-collected and
  readable by any node that can pull. A process-memory dump must not land in
  a registry, even briefly. Pickle's only job here is making sure the image
  layers are on the target before the source stops.
- **Format.** One stream: a tar with `criu/`, `rootfs-upper/` and
  `volumes/<mount>/` entries, zstd-compressed (CRIU has no released
  compression), then age-encrypted to a per-migration X25519 key the target
  generates and holds only in memory. The source reports the stream's SHA-256
  and length; the leader commits them; the target refuses a stream that
  doesn't match. The `age`, `tar` and `zstd` crates are already in
  `Cargo.lock`.
- **At rest.** CRIU writes plaintext image files, so the source dumps into a
  private tmpfs sized from the instance's memory limit. If the node doesn't
  have that much free memory, checkpoint and live modes are refused (see
  below). The encrypted stream may spool to disk on either side;
  plaintext only exists in tmpfs, during dump and during restore.
- **Space.** The leader checks both nodes' reported free space against the
  instance's volume usage plus memory limit plus a margin before starting.
  Bun's disk-pressure monitor refuses to accept a payload that would push the
  data directory over its threshold.
- **Cleanup.** Every payload file and tmpfs is named by migration id. On
  start, Bun removes any whose migration is terminal or unknown to Raft; the
  source's tombstoned volumes go after the retention period.
- **Expected downtime.** Stop-and-copy downtime is dump + transfer + restore,
  and transfer dominates: roughly the anonymous memory plus the volume size
  over the link. A 256 MiB in-memory Redis should be seconds; a 10 GiB
  volume is 90 seconds or more on 1 Gbit/s. That's why live mode pre-syncs
  volumes as well as memory: for most stateful apps the volume, not the
  memory, sets the downtime.
- **Not enough memory for the dump.** If the source can't hold the plaintext
  dump in tmpfs (the instance's memory limit plus a margin, against what the
  node reports free), checkpoint and live modes are refused (decision 6).
  Plaintext never spills to disk.

### 5.6 Control plane

The leader orchestrates, as it does for everything else. A migration is a
Raft record the leader advances; each node does its idempotent part when its
placement poll shows an instruction, and reports back over a node route. The
reporting frames are bincode and stay untouched.

```text
Requested ──► Prepared ──► SourceStopped ──► Transferred ──► Completed
    │             │              │                 │
    └──► Aborted ◄┘              └──► FellBack ◄───┘
```

- **Requested.** The leader checked eligibility and reserved the target's
  resources. The migration holds the app: deploys, scaling and autoscale
  wait.
- **Prepared.** The target pulled the image, checked space and tmpfs, and
  generated the transfer key. Its public half goes into the record.
- **SourceStopped.** The one-way door. The source runs `runc checkpoint` (or
  a plain stop in cold mode, or after a refused dump) under the instance's
  owner, tombstones its volumes, and reports the intent generation as
  `Retired` with the payload digest. The leader commits it and, *in the same
  entry*, moves the placement from source to target. From here on the source
  must never run the app again unless the record says so.
- **Transferred.** The target has the whole payload and it matches the
  digest.
- **Completed.** The target restored (or cold-started) and passed its health
  check. The leader updates `last_placed_nodes`, records the downtime, and
  releases the app.
- **FellBack.** Something failed after the one-way door. The record names
  where the app ended up and why.
- **Aborted.** Something failed before it. Nothing changed.

Crash handling, step by step:

| What fails | When | Result |
|---|---|---|
| Target unreachable, dies, or can't prepare | before `SourceStopped` | `Aborted`; the source never stopped |
| CRIU refuses the dump | during stop | The process is still running; the migration switches to cold and stops it normally |
| Source Bun crashes during the dump | during stop | The owner outlives Bun; on restart Bun reads the intent journal: still running means the dump failed (retry cold), retired means report it |
| Source node dies | before `SourceStopped` | `Aborted`; the app waits for its volume home, exactly as node loss does today |
| Source node dies | after `SourceStopped`, before `Transferred` | The payload isn't complete and the data is on the dead node: wait for it until the deadline, then `FellBack` to the source (the app stays homed there) |
| Transfer fails or digest mismatches | after `SourceStopped` | Retry with backoff; then `FellBack` to the source: un-tombstone the volumes, move the placement back, restore from the local CRIU image if there is one, else cold start |
| Restore fails on the target | after `Transferred` | Cold start on the target with the moved volumes (the maintainer's fallback); `Completed` with `mode = cold` and the reason |
| Target dies after `SourceStopped` | any later phase | `FellBack` to the source, as above. The source copy is intact because it was only renamed |
| Target Bun restarts mid-transfer | after `SourceStopped` | The in-memory transfer key is gone, so the payload can't be decrypted: the target cold-starts from a fresh volume transfer, never from half a payload |
| Leader changes | any | The new leader reads the record and continues after its learning period; every step is keyed by migration id and the source's intent generation, so repeats are no-ops |
| Both nodes die | after `SourceStopped` | The app stays down; `relish migrate cancel` resolves it to whichever node comes back with the data |

Every phase has a deadline in the record, so nothing waits forever.

**Interactions.**

- *Deploys.* A deploy of a migrating app waits until the migration is
  terminal. A migration of an app mid-rollout is refused.
- *Stop and delete.* They win: before `SourceStopped` the migration aborts;
  after it, the migration is cancelled to whichever node holds the data and
  the stop applies there.
- *The placement reconciler.* The target must not cold-start the replica it
  was just assigned while the payload is in flight: its `NodeAssignments`
  carry a "migrating in" instruction that holds the slot. The source gets
  "migrating out", which turns its retirement into a checkpoint instead of a
  plain stop. The applied-placements checkpoint records both, so an agent
  restart doesn't mistake the held slot for a pending deploy (the #267
  failure mode).
- *Upgrades, leases, decommission.* No migration starts during a cluster
  upgrade; `relish decommission-node` resolves any migration touching the
  node; lease-owned apps can't be migrated.

**How drain uses it.** `relish drain` commits a cordon, then works through
the node's instances: stateless ones are rescheduled with surge, singletons
with managed volumes become migrations (checkpoint if the app opted in, cold
otherwise), running job attempts move the same way, and anything
ineligible (host-path volumes, GPUs, runtimes that can't move) is listed as
blocking with the reason. `relish drain --force` stops those instead, with
their data left in place. The drain finishes when the node runs nothing but
system services and no handoff window still routes through it.

### 5.7 Live mode

Live mode is checkpoint mode with extra layers, each with its own
precondition, each dropped on its own when the precondition fails:

| Layer | What it does | Needs | Without it |
|---|---|---|---|
| Volume and rootfs pre-sync | Copy while running; send the last delta after the freeze | nothing new | always available |
| Memory pre-dump iterations | `runc checkpoint --pre-dump` with `--parent-path`, repeated while the dirty set shrinks, then a final dump of the last dirty pages | soft-dirty tracking on the source (`criu check --feature mem_dirty_track`): x86_64 yes, arm64 mainline no | lazy pages alone |
| Lazy pages (post-copy) | The source dumps and serves pages from a CRIU page server; the target restores at once and faults the rest in through `userfaultfd` | `userfaultfd` on the target (`criu check --feature uffd-noncoop`); CRIU's page server with TLS | a full dump before restore, as checkpoint mode |
| TCP handoff | Section 5.3 | borrowed address free on the target; IPIP between the two nodes | connections dropped, fresh address |

**Pre-dump policy.** Stop iterating when the dirty set is under a threshold
(default 64 MiB), stops shrinking by at least a quarter per round, or hits a
maximum number of rounds (default 5); then freeze. A process that dirties
memory faster than the link carries it never converges, and that's exactly
when lazy pages are the better tool, so an x86_64 live move that hits the
round limit finishes the remainder with lazy pages.

**arm64.** Soft-dirty patches for arm64 were posted in 2023-24 and not
merged. Emulating it with repeated full dumps would cost more than it
saves. So on arm64, live mode is volume pre-sync plus lazy pages plus TCP
handoff: the freeze covers one dump of the process state *without* its
pages, and the pages follow. That's the Apple-silicon quickstart's shape,
and the demo works on it. Pre-dump is x86_64-only, reported per node and
shown by `relish migrate --dry-run`; the release's x86_64 qualification
(GitHub's hosted runners, cloud VMs or the Wyse 3040s from 0.3.0) covers it.
If arm64 gains soft-dirty upstream, the capability check picks it up with
no code change.

**Lazy pages bring a new failure mode.** Until the last page arrives, the
restored process depends on the source. If the source dies or the page
server connection breaks, the process on the target faults on a missing page
and dies. Volumes and the rootfs delta have already moved, so the fallback
is a cold start on the target with the data, and the status says the memory
was lost. The page server uses CRIU's TLS with a per-migration key pair
whose fingerprints are in the Raft record, so pages are encrypted in transit
like the rest of the payload. The migration isn't `Completed` until the last
page has arrived and the source has let go.


## 6. Compatibility (pre-1.0: bump and start fresh)

The first draft proposed riding PR #266's "finalise cluster features" gate.
That question is moot: the maintainer decided on 28 September 2026 that
there's no backwards compatibility before 1.0.0 (PR #254's `CLAUDE.md` and
`docs/releasing.md`). 0.4.0 is a development release, so its incompatible
changes bump `protocol` and `state` in `src/compatibility.rs`, nodes refuse
older peers and older state, and upgrading from 0.3.x means a fresh cluster.
No gate, no migration, no additive-field rules.

What changes, so the bump is deliberate and the tests know where to look:

| Change | Encoding | Handling |
|---|---|---|
| `RaftRequest::MigrationStart`, `MigrationAdvance`, `MigrationFinish`, `NodeCordon`, `NodeUncordon` | JSON enum variants | protocol and state bump |
| `CouncilResponse` variants for the above, if any | JSON | same bump; prefer reusing `Ok`/`Applied`/`Refused` |
| `DesiredState.migrations`, `DesiredState.cordoned_nodes` | JSON snapshot | state bump |
| `[app.<name>.migration]` and `[job.<name>.migration]` | TOML/JSON, `deny_unknown_fields` | protocol bump |
| Batch tracker attempt records gain the migration id | JSON | state bump |
| `NodeAssignments.migrations` ("migrating in/out" instructions) | JSON | protocol bump |
| Node routes: prepare, payload, report, handoff | HTTP | new routes in the `authz.rs` matrix |
| `OciSpec` time namespace and offsets | node-local JSON | covered by the state bump |
| Node-local migration journal (payload, tombstones, transfer state, borrowed addresses) | new file | covered by the state bump |
| `StateReport` and reporting frames | bincode | not touched; progress goes over the new node route |
| CRIU image format | not ours | record the source's CRIU version; refuse when the target's is older |

## 7. Security

- **A checkpoint is a memory dump.** It holds whatever the process holds:
  decrypted `EnvValue::Encrypted` secrets, TLS private keys, database passwords,
  session tokens. Treat the payload like a secret: tmpfs for plaintext,
  age-encrypted to a per-migration key the target holds only in memory,
  node-id mTLS in transit, a digest committed in Raft, wiped when the
  migration is terminal. It never goes to Pickle or object storage.
- **It breaks an existing invariant.** Workload identity says "the private
  key never leaves the node" (`src/cluster/workload_identity.rs`). A
  restored process has the source instance's key in memory. The target
  instance gets its own certificate as usual, and on `Completed` the leader
  revokes the source instance's serial through the existing CRL
  (`RevokeCertificate`). Apps that cache their identity must reload it from
  the mounted file; we document that. Cold mode doesn't have this problem.
- **Who can trigger it.** `relish migrate` needs a `Deployer` token scoped to
  the app, like deploy and stop. `--mode checkpoint` additionally needs the
  `exec` permission for that app, because reading a process's memory is at
  least as powerful as exec'ing into it. `relish drain`, `uncordon` and
  cancelling someone else's migration need `Admin`. No new
  `PermissionAction` variant: the existing `deploy` and `exec` cover it, which
  also keeps permission specs compatible.
- **CRIU runs as root on both nodes.** Bun already runs rootful runc as root,
  so there's no new privilege, but there is new attack surface: Google's
  audit found a malicious task could hijack a root CRIU (LPC 2018, slide 31).
  runc's restore doesn't support unprivileged CRIU. So checkpoint mode stays
  opt-in per app, for workloads the operator trusts, and we pin a minimum
  CRIU version and surface it in `relish wtf`.
- **Seccomp and AppArmor.** The generated spec has neither today. CRIU
  restores seccomp filters, and `runc restore --lsm-profile` exists for when
  we add AppArmor. Nothing to do in 0.4.0 beyond a test that keeps the
  checkpoint path honest when a profile appears.
- **CPU features.** Restore is refused on a CPU that lacks any feature the
  source's CPU had (decision 9). Each node reports its feature set with
  `criu cpuinfo dump`; the leader checks that the target's is a superset
  before it starts, and the restore runs with `--cpu-cap=cpu` so CRIU checks
  again on the target. glibc and JITs keep using whatever they detected at
  start-up, so the model needn't match, but nothing may be missing.
- **Borrowed addresses.** TCP handoff lends the source's container address
  to the target for a window. It's reserved and released through the same
  lease and withdrawal rules as any other address, and the tunnel carries
  only the tuples read from the dump, so it can't reach anything else on the
  source.
- **Egress.** A restored process must not run, even briefly, without its
  egress policy. Section 5.3's ordering (spike S4) is a security requirement,
  not a nicety.

## 8. Observability and UX

**Commands.**

```text
relish migrate default/cache --to node-3                 # the app's mode; cold if it has none
relish migrate default/cache --to node-3 --mode checkpoint
relish migrate default/cache --to node-3 --mode live
relish migrate default/cache --to node-3 --dry-run        # eligibility per mode and layer
relish migrate default/render --attempt 7 --to node-3     # a running job attempt
relish migrate status [<migration-id>]
relish migrate cancel <migration-id>
relish drain node-2 [--timeout 30m] [--force]
relish drain status node-2
relish uncordon node-2
```

The opt-in in the app file:

```toml
[app.cache.migration]
mode = "live"            # "cold" (what drain does without this), "checkpoint" or "live"
handoff_window = "10m"   # live only: how long handed-off connections route via the source
```

**Status** shows the phase, the mode and which live layers ran (and whether
it fell back, with CRIU's reason), pre-dump rounds and their dirty sizes,
bytes moved per kind, lazy pages still outstanding, handed-off connections
still routed through the source, and the frozen time so far.

**Events** (Bun's event log and `relish status`): requested, prepared, source
stopped (dump duration, payload size), transferred, restored or cold-started,
fell back (reason), aborted (reason), completed (downtime).

**Metrics** (Mayo):

- `reliaburger_migration_total{mode, outcome}`
- `reliaburger_migration_downtime_seconds` (source frozen to target healthy)
- `reliaburger_migration_payload_bytes{kind="memory|rootfs|volume"}`
- `reliaburger_migration_phase_seconds{phase}`
- `reliaburger_migration_fallback_total{reason}`
- `reliaburger_migration_predump_rounds` and `reliaburger_migration_lazy_pages_outstanding`
- `reliaburger_migration_handoff_connections{migration}`

**`relish wtf` checks:** CRIU missing or below the minimum on a runc node
(`criu check`); an app opted into checkpoint that uses a host-path volume, a
GPU or rootless runc; nodes with different CPU models hosting a
checkpoint-enabled app; a migration past its deadline; orphaned payloads or
tombstoned volumes past retention; a cordoned node that's been cordoned for
days.

**`relish lint`** rejects `mode = "checkpoint"` together with a host-path
volume or a GPU.

**Docs.** A manual chapter ("Moving workloads"), a `docs/design/` section in
`deployments.md` (drain) and `agent-bun.md` (checkpoint and live), and a new
book chapter for 0.4.0 (decision 10). The chapter should tell the honest
story: why the volume move comes first, what CRIU refuses, why pre-dump is
x86_64-only, why handed-off connections still route through the source, and
why the fallback is the real feature.

## 9. Demo

We have no GPU hardware, so the GPU story stays out of the demo. It isn't
needed: the thing a checkpoint saves that a volume doesn't is *memory*, and
an in-memory cache shows that better than anything.

**The workload.** Redis with persistence off (`save ""`, `appendonly no`),
the same pinned image the runc tests already use
(`runc_redis_persists_to_a_managed_volume_across_restarts` in
`src/grill/runc.rs`), 256 MiB of keys, one replica, `mode = "checkpoint"`.
Its data lives *only* in memory, so a cold start would lose all of it. That's
the "stateful dev or game server" case in miniature, and nobody needs it
explained.

**The script** (three-node quickstart cluster):

```sh
relish apply examples/migration/container-redis-memory.toml
# DEBUG POPULATE is disabled by default since Redis 7, so fill it with a script
relish exec default/cache -- redis-cli EVAL \
  "for i=1,1000000 do redis.call('SET','key:'..i,string.rep('x',256)) end" 0
relish exec default/cache -- redis-cli DBSIZE          # 1000000
relish migrate default/cache --to node-3 --mode checkpoint
relish migrate status                                   # phase, bytes, frozen time
relish exec default/cache -- redis-cli DBSIZE          # still 1000000, now on node-3
relish exec default/cache -- redis-cli INFO server | grep uptime_in_seconds   # didn't reset
```

Then the fallback, which is the part that makes it trustworthy:

```sh
relish apply examples/migration/container-io-uring.toml   # a workload CRIU refuses
relish migrate default/uring --to node-3 --mode checkpoint
relish migrate status          # "fell back to cold: CRIU cannot dump io_uring"
```

Then the live move, with a client connection that has to survive it:

```sh
relish exec default/cache-client -- redis-cli -h cache -r -1 -i 0.1 INCR counter &
relish migrate default/cache --to node-2 --mode live
relish migrate status          # pre-dump rounds or lazy pages, frozen time, handed-off connections
```

And the drain, with the soak's append-only writer on a managed volume:

```sh
relish drain node-2            # writer moves cold, cache moves with its memory
relish drain status node-2     # empty; nothing blocking
```

**Success criteria.**

- `DBSIZE` and a sample of values match before and after; `uptime_in_seconds`
  keeps counting.
- Frozen time for the 256 MiB cache under 10 seconds in checkpoint mode on
  the Apple-silicon quickstart, and under 1 second in live mode (to be
  confirmed by spikes S7 and S10; targets, not measurements).
- The `INCR` client's connection never resets during the live move, and
  the counter never goes backwards.
- The io_uring workload ends up running on the target with its volume data,
  and the status names the reason.
- The writer's sequence file has no gap and no repeated line (the PR #267
  check), and no instance of it ever runs on two nodes at once.
- No payload, tmpfs or tombstone is left behind after the retention period.

## 10. Testing plan

Tests first, as always, but the spikes come before the tests, because three
of them can change the design. None of them can run until the release soak
is over and the Lima VMs are free.

**Spikes** (runc 1.5.x and CRIU 4.2.1, both architectures, our generated
spec):

| Spike | Question | If it fails |
|---|---|---|
| S1 | Does `runc checkpoint`/`restore` work on our spec: user namespace, external netns, private overlay root, bind-mounted volumes and `resolv.conf`? | Checkpoint mode waits; cold mode ships alone |
| S2 | Can restored stdio reach the target's capture files (section 5.4)? | Pipes through the owner, or checkpoint mode waits |
| S3 | Restore into a *new* bundle, container id, netns and address, with `tcp-close` from `org.criu.config` | Checkpoint mode waits |
| S4 | Pre-create the cgroup, install egress policy for its id, restore into it | Checkpoint mode refuses apps with egress rules |
| S5 | Does the restored init still pass `owned_workload_cgroup`'s identity check, and can a new Bun adopt a restored generation? | Adjust the check; likely small |
| S6 | Time namespace keeps `CLOCK_MONOTONIC` continuous across two VMs | Refuse checkpoint mode for apps that care, document |
| S7 | Payload size and frozen time for Redis at 256 MiB and 1 GiB | Tune targets |
| S8 | io_uring, POSIX mqueues and a GPU-less `/dev/nvidia*` bind are refused cleanly and the process keeps running | Tighten eligibility |
| S9 | `runc checkpoint --pre-dump` with `--parent-path` on our spec, on x86_64; dirty-set convergence for Redis under write load; `mem_dirty_track` reported false on arm64 | Pre-dump waits; live mode is lazy pages only everywhere |
| S10 | Lazy pages: `runc checkpoint --lazy-pages` with a TLS page server, `runc restore` with `criu lazy-pages` on the target, on both architectures; what the process sees when the page server dies | Live mode is pre-dump only (x86_64); back to the maintainer |
| S11 | TCP handoff: borrowed address in the target netns, IPIP tunnel, 4-tuple set from the image, `--tcp-established` restore, source conntrack reversing the NAT | Back to the maintainer before building the routable-address design |

**Unit tests** (portable, written first):

- The migration state machine: every valid and invalid transition, and a
  proptest that interleaves node crashes, leader changes and repeated
  reports and checks the invariants: never two writers, the placement moves
  exactly once or not at all, every migration ends terminal before its
  deadline.
- The fallback table in section 5.6, one test per row.
- Eligibility: each refusal reason, including the singleton-per-node rule.
- The Raft apply: the `SourceStopped` entry moves the placement atomically;
  `Completed` updates `last_placed_nodes`; `decommission-node` resolves a
  migration.
- The compatibility bump: fixtures take their pair from
  `compatibility::CURRENT`, and a 0.3.x peer and 0.3.x state are refused.
- Live-mode planning: layer selection from reported capabilities, pre-dump
  convergence and round limits, and the fall-through to lazy pages.
- Job attempts: a moved attempt keeps its number and spends no retry,
  including across a crash at every phase.
- CPU features: a target missing any source feature is refused.
- Handoff: tuple sets from a sample image, borrowed-address collisions, and
  the window's expiry.
- Payload manifest, digest and encryption round-trip; a tampered stream is
  refused.
- The authz matrix gains the new routes (its existing test fails otherwise).

**Portable integration** (`tests/suite/`): `MockGrill` gains fake checkpoint
and restore, and the in-process cluster harness drives a migration through a
leader change, a target crash after `SourceStopped` and a source crash
mid-dump.

**Gated Linux** (`make test-linux`, a new `tests/migration_runc.rs` next to
`tests/owned_runc.rs`): real runc and CRIU on one host, restoring into a
different bundle, instance id and namespace, which exercises everything but
the network hop; kill Bun mid-dump and mid-restore and check adoption; the
io_uring refusal path.

**Gated cluster** (`make test-cluster` on three Lima VMs): a real cross-node
migration with a managed volume in both modes; power off the source
mid-dump; power off the target mid-restore; kill the leader mid-transfer;
drain a node with a mix of stateless, cold, checkpoint and live apps and a
running job; a live move with an open client connection; kill the source
mid lazy-page transfer.

**Gated x86_64** (GitHub's hosted runners if CRIU works there, otherwise
cloud VMs or the Wyse 3040s): pre-dump iterations and their convergence,
because the Apple-silicon Lima VMs can't run them.

**Soak.** Add a migration loop to the V02 sustained qualification: every N
minutes migrate the soak writer (cold) and a checkpoint-enabled in-memory
counter between nodes, including across the existing power-off faults. The
writer check catches a double writer; the counter must never go backwards;
no orphaned payloads at the end.

**CI.** Unit and portable tests run everywhere. CRIU needs root and a
suitable kernel; GitHub's `ubuntu-latest` runners can install it from the
upstream PPA, so we should *try* the gated Linux migration tests in the
existing "privileged Linux" job, and keep them Lima-only if the runner
refuses. Unverified until someone tries.

**Packaging.** Add `criu` to `scripts/release/guest-images.json` from the
upstream PPA (Ubuntu 24.04 has no archive package), pin the version, and make
`relish wtf` report it.

## 11. Effort and phasing

Rough, for one engineer (or agent plus reviewer) working in this codebase's
style: owners, fences, receipts, crash tests at every step. Revised for the
full version the maintainer asked for.

| Piece | Weeks |
|---|---|
| Spikes S1-S11 (after the soak; S9 needs x86_64) | 2-2.5 |
| Compatibility bump and its tests (no gate) | 0.25 |
| Cordon, `relish drain` for stateless apps, `uncordon`, `--force`, status | 2 |
| Cold move: Raft record and state machine, node instructions, payload route, volume tar and tombstones, crash recovery, drain integration | 4-5 |
| Checkpoint mode: runc checkpoint/restore under owners, stdio, cgroup and egress ordering, tmpfs and encryption, time namespace, CPU feature check, CRIU packaging, certificate revocation | 3-4 |
| Jobs: moving a running attempt without spending a retry, attempt ledger and batch tracker | 1.5-2 |
| Live: volume and rootfs pre-sync (Btrfs send, rsync-style passes elsewhere) | 1.5-2 |
| Live: memory pre-dump iterations, convergence policy, capability reporting (x86_64) | 1.5-2 |
| Live: lazy pages, TLS page server, the source-loss fallback | 2-3 |
| Live: TCP handoff through the source (section 5.3) | 3-4 |
| Observability, `wtf`, lint, manual, design docs, the new book chapter, soak loop, qualification on both architectures | 2-2.5 |
| **Total** | **23-29** |

That's roughly five and a half to seven months for one engineer, about
twice the first draft. CRIU-specific work is now about half of it (checkpoint
mode, pre-dump, lazy pages and most of the handoff), up from a quarter. The
routable-address alternative for TCP handoff would add another 3-4 weeks on
top of the handoff line and isn't in the total.

**Phasing inside 0.4.0.** Each step lands on `main` behind its own tests and
is usable on its own; the release waits for all of them.

1. Spikes S1-S11. Any failure goes back to the maintainer.
2. Cordon, drain and uncordon for stateless apps.
3. Cold moves with volumes, and drain moving stateful apps.
4. Checkpoint mode.
5. Jobs, in all three modes.
6. Live: pre-sync, then lazy pages, then pre-dump (x86_64).
7. Live: TCP handoff.
8. Observability, docs, the book chapter, the soak loop and qualification.

**Later, each its own design:** GPU warm starts (a snapshot store with
invalidation rules, driver and CPU matching, and hardware to test on);
cluster-routable instance addresses, if handoff through the source proves
too limiting; automatic rebalancing and preemption-driven moves; rootless
and non-runc runtimes.

## 12. Where the initial analysis holds and where it doesn't

**Holds:**

- Stop-and-copy first, pre-dump later. Google ran Borg's CRIU migrations
  that way for batch and called it good enough there.
- Opt-in per workload, with a clean fallback when CRIU refuses. CRIU refuses
  plenty (io_uring, mqueues, devices), and even Incus calls it fragile.
- Don't sell it for stateless web apps; they should just be rescheduled.
- GPU warm-start demand is real and shipping (GKE, Modal, Dynamo).
- It's demo-able.

**Doesn't hold, or needs adjusting:**

- *"Checkpoint-on-drain"*: Reliaburger has no drain. `relish drain` is
  still planned in the whitepaper; the only node drain in the code is a
  chaos fault. Build it first.
- *"Fall back to a normal restart"*: there's no normal restart elsewhere for
  a stateful app. Managed volumes are node-local and the scheduler sends the
  app back to them. The fallback is a volume-carrying move, and that move is
  most of the work.
- *"More than anyone else ships"*: true against core Kubernetes (KEP-2008 is
  checkpoint-only; KEP-5823 makes cross-node restore a non-goal). Not true
  against Podman's export/import, Incus, Borg in 2018, or Cast AI and Cedana
  today.
- *"youki uses rust-criu for its checkpoint support"*: for checkpoint only.
  Restore has been an open PR since February. We should shell out to runc,
  which already speaks CRIU for us.
- *"Iterative pre-dump"*: pre-dump needs soft-dirty tracking, which arm64
  mainline doesn't have, and our quickstart is Apple silicon. So it's
  x86_64-only, with lazy pages covering arm64, and volume pre-sync matters
  at least as much for downtime.
- *GPU as a headline use*: every shipping LLM warm-start product we could
  verify restores a pre-warmed snapshot on the same class of machine, mostly
  via gVisor. That's a snapshot product, not migration, and we have no GPUs.
  Lead with batch, where there's evidence; use a memory-only server for the
  demo.
- *Upgrades as a trigger*: they don't restart workloads. Reboots do.
- *Effort*: the brief reads as a CRIU feature. It's a stateful-move feature
  with CRIU on top, and with every layer in it's 23 to 29 weeks.
- *Unmentioned*: a checkpoint carries workload private keys off the node,
  which contradicts the workload-identity design and needs revocation.

## 13. Decisions (28 September 2026)

The maintainer answered the ten open questions on 28 September 2026. The
questions as asked are kept in the PR history; the answers are:

1. **Framing.** No to "move first, CRIU opt-in only". 0.4.0 implements the
   whole thing: drain, cold moves with volumes, CRIU checkpoint and
   restore, pre-dump iterations, lazy pages and TCP handoff.
2. **Gate.** Moot. There's no backwards compatibility before 1.0.0, so
   there's no finalisation gate to share: 0.4.0 bumps `protocol` and `state`
   and needs a fresh cluster (section 6).
3. **Jobs.** Both apps and jobs, in 0.4.0.
4. **Opt-in shape.** `[app.<name>.migration] mode = "cold" | "checkpoint" |
   "live"` (and the same for jobs). A drain moves anything that hasn't opted
   in cold.
5. **Host-path apps during drain.** They block the drain; `relish drain
   --force` stops them.
6. **Plaintext on disk.** Refuse checkpoint and live modes when memory can't
   hold the plaintext dump. Never spill it to disk.
7. **CRIU distribution.** From the upstream PPA, minimum version 4.2. (The
   0.3.0 quickstart guest moves to Ubuntu 26.04; check the PPA publishes for
   it before then.)
8. **Identity.** Revoke the source instance's workload certificate when the
   migration completes.
9. **CPU policy.** Refuse restore on a CPU missing any feature the source
   had (section 7).
10. **Book.** A new chapter for 0.4.0.
