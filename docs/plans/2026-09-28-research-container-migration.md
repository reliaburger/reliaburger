# Research: moving a running container between nodes (0.2.0)

28 September 2026. Research and a recommendation only; no product code.
Awaiting maintainer review.

## Progress (for whoever resumes this)

- [x] Branch `research/container-migration` from `origin/main` (ff854cbf)
- [x] 1. Recommendation (summary)
- [x] 2. What the codebase does today (verified by reading the code)
- [x] 3. CRIU, runc and the ecosystem today (sourced)
- [x] 4. GPU checkpointing and where the demand really is (sourced)
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
CRIU with NVIDIA patches that aren't upstream yet, we have no GPU hardware, and CUDA
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
cold-start move (section 5) could serve them later, but 0.2.0 keeps it
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
(`src/bun/gpu.rs`). Not 0.2.0, not 0.3.

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
