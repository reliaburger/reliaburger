# Research: moving a running container between nodes (0.2.0)

28 September 2026. Research and a recommendation only; no product code.
Awaiting maintainer review.

## Progress (for whoever resumes this)

- [x] Branch `research/container-migration` from `origin/main` (ff854cbf)
- [x] 1. Recommendation (summary)
- [x] 2. What the codebase does today (verified by reading the code)
- [x] 3. CRIU, runc and the ecosystem today (sourced)
- [x] 4. GPU checkpointing and where the demand really is (sourced)
- [x] 5. Design for Reliaburger
- [x] 6. Compatibility against the post-0.1.0 policy
- [x] 7. Security
- [x] 8. Observability and UX
- [x] 9. Demo
- [x] 10. Testing plan
- [x] 11. Effort and phasing
- [x] 12. Where the initial analysis holds and where it doesn't
- [x] 13. Open questions for the maintainer
- [x] Draft PR opened (#268)

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

Effort: roughly 12 to 16 focused weeks for 0.2.0, of which only about a quarter
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

## 5. Design for Reliaburger

### 5.1 Scope for 0.2.0

The maintainer's "checkpoint-on-drain" survives, with two changes: the drain
has to be built, and the restart fallback has to be a *move*.

**In:**

- `relish drain <node>` and `relish uncordon <node>`: cordon the node (no new
  placements), then empty it. Instances without managed volumes are
  rescheduled the way a rolling deploy would, respecting `max_unavailable`
  (the whitepaper's promise for drain). Instances with managed volumes are
  *migrated*.
- `relish migrate <namespace>/<app> --to <node> [--instance <id>]
  [--mode checkpoint|cold]`: move one instance on purpose.
- Two modes, one pipeline:
  - **cold**: stop the source, copy its managed volumes, cold-start on the
    target. Works for any rootful runc app. This is the default for drain.
  - **checkpoint** (opt-in per app, experimental): the same, plus `runc
    checkpoint` on the source, the CRIU image and the writable rootfs layer in
    the payload, and `runc restore` on the target. If CRIU refuses at dump
    time, the source is still running (CRIU resumes it on failure) and the
    migration continues as **cold**. If restore fails on the target, it
    cold-starts there with the volumes already moved.
- Rootful runc only.

**Out, explicitly:**

- Iterative pre-dump, lazy pages, page server (0.3 or later, section 11).
- Keeping established TCP connections. They're closed at dump time; the
  instance gets a new address on the target; clients reconnect.
- GPUs, host-path volumes, rootless runc, Apple Container, ProcessGrill:
  `migrate` refuses them up front, and drain reports them as blocking.
- Automatic triggers: rebalancing, spot/preemption notices, autoscaling.
  Rebalancing is a scheduler policy question we shouldn't answer in the same
  release as the mechanism.
- Jobs. A job attempt has its own durable attempt ledger (`src/bun/jobs.rs`)
  and the batch tracker in Raft, so moving one without consuming a retry is
  its own design. It's the first follow-up, because batch is where the demand
  evidence is (open question Q3).
- Binary upgrades. They don't restart workloads (section 2).

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
5. Checkpoint mode only: the app opted in, both nodes report the same CPU
   model and a CRIU at or above the pinned minimum, and the source's CRIU is
   no newer than the target's.

### 5.2 What travels

| State | Where it lives on the source | Cold | Checkpoint |
|---|---|---|---|
| Managed volumes | `volumes/<ns>/<app>/...` (plain, loop ext4, Btrfs) | tar stream | tar stream |
| Writable rootfs layer | `<bundle>/rootfs-upper` | not moved (a cold restart starts fresh today too) | tar stream (restored files reference it) |
| Process memory, fds, namespaces | kernel | none | CRIU image |
| Image layers | image store | target pulls through Pickle **before** the source stops | same |
| Captured stdout/stderr | `output.stdout`/`output.stderr` | stays; Ketchup has already ingested it | stays, see 5.4 |
| Network identity | `/23` per node | new address and netns on the target | same; netns is external to CRIU |
| Workload certificate | per instance, key generated on the node | new instance, new certificate | new certificate; old one revoked (section 7) |

The copy of each volume stays on the source, renamed out of the way
(`volumes/.migrated/<migration-id>/...`), until the migration completes and a
retention period passes. That's what makes the fallbacks below possible, and
it's also the one-writer rule from PR #267 in another form: the source copy
is never mounted again unless the migration resolves *back* to the source.

A plain `tar` of a stopped volume is consistent for every backend, so 0.2.0
doesn't need `btrfs send`. Btrfs incremental send (or repeated rsync passes)
is how 0.3 cuts downtime: pre-sync while the app runs, then send only the
last delta after the stop, as Incus 7.4 does.

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
- **TCP.** Drop, don't hand off. `--tcp-established` needs the same IP on the
  target, and our per-node `/23` pools make that a network redesign
  (routed per-instance `/32`s, or an overlay). Google made the same call for
  Borg. `runc checkpoint` has no `--tcp-close` flag, so we pass CRIU's
  `tcp-close` through the `org.criu.config` annotation (a small CRIU config
  file Reliaburger writes per checkpoint). Without it, CRIU refuses to dump a
  process with an established connection.
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
  have that much free memory, checkpoint mode isn't eligible and the
  migration runs cold. The encrypted stream may spool to disk on either side;
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
  volume is 90 seconds or more on 1 Gbit/s. That's why volume pre-sync, not
  memory pre-dump, is the first 0.3 item.

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
otherwise), and anything ineligible is listed as blocking with the reason.
`relish drain --stop-blocking` stops those instead, with their data left in
place. The drain finishes when the node runs nothing but system services.

## 6. Compatibility against the post-0.1.0 policy

The policy (PR #254's `CLAUDE.md` and `docs/releasing.md`) has two kinds of
change: additive (optional JSON/TOML fields old nodes tolerate, no bump) and
incompatible (bump `protocol` or `state` and ship a migration). PR #266
proposes a third, **gated**: every node learns to decode the new entries, but
the leader writes none of them until the cluster has run
`RaftRequest::FinaliseClusterFeatures { level }`, after which rollback to
0.1.x is refused. Migration can't be done additively, so it should ride the
same gate, at the same feature level as task arrays if both land in 0.2.0.
One finalisation per release is plenty for operators.

| Change | Kind under the policy | Handling |
|---|---|---|
| `RaftRequest::MigrationStart`, `MigrationAdvance`, `MigrationFinish` | New enum variants: incompatible | Gated. `relish migrate` answers 409 "finalise the cluster upgrade first" until finalised |
| `RaftRequest::NodeCordon`, `NodeUncordon` | New variants: incompatible | Gated with the rest. The upgrade cordon stays as it is |
| `CouncilResponse` variants for the above, if any | New variants: incompatible | Gated; prefer reusing `Ok`/`Applied`/`Refused` |
| `DesiredState.migrations`, `DesiredState.cordoned_nodes` | New optional JSON fields; `DesiredState` has no `deny_unknown_fields` | Additive on paper, meaningful only after the gate. `#[serde(default)]`, skip when empty |
| `[app.migration]` in `AppSpec` (the opt-in) | `AppSpec` has `deny_unknown_fields`: incompatible even as an `Option` | Gated: apply refuses the field until finalised, because an old follower would refuse the whole `AppSpec` entry |
| `NodeAssignments.migrations` ("migrating in/out" instructions) | New optional field; no `deny_unknown_fields` | Additive, and only ever non-empty after the gate |
| Node routes: prepare, payload, report | New HTTP routes | Additive; new entries in the `authz.rs` route matrix |
| `OciSpec` annotations and time offsets, time namespace entry | `OciSpec` has no `deny_unknown_fields`; namespace type is a string | Additive with `#[serde(default, skip_serializing_if = ...)]`. Worth a test that an old binary reads back a new intent journal |
| Node-local migration journal (payload, tombstones, transfer state) | A new file no old binary reads | Its own file, so `IntentConfiguration` and the owner records (both `deny_unknown_fields`) don't change |
| `StateReport` and reporting frames | bincode: any change is incompatible | Not touched. Progress goes over the new node route |
| Metrics, events | New names | Additive |
| CRIU image format | Not ours | Record the source's CRIU version in the record; refuse checkpoint mode when the target's is older |

If the maintainer rejects the gate, the fallback is a `protocol`/`state`
bump with a designed migration, which strands every 0.1.0 cluster that
can't do a blue-green move. For a feature that's opt-in and experimental,
that's the wrong trade.

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
  we add AppArmor. Nothing to do in 0.2.0 beyond a test that keeps the
  checkpoint path honest when a profile appears.
- **Egress.** A restored process must not run, even briefly, without its
  egress policy. Section 5.3's ordering (spike S4) is a security requirement,
  not a nicety.

## 8. Observability and UX

**Commands.**

```text
relish migrate default/cache --to node-3                 # cold
relish migrate default/cache --to node-3 --mode checkpoint
relish migrate default/cache --to node-3 --dry-run        # eligibility report
relish migrate status [<migration-id>]
relish migrate cancel <migration-id>
relish drain node-2 [--timeout 30m] [--stop-blocking]
relish drain status node-2
relish uncordon node-2
```

The opt-in in the app file:

```toml
[app.cache.migration]
mode = "checkpoint"   # "checkpoint", "cold" (default for managed volumes) or "never"
```

**Status** shows the phase, the mode (and whether it fell back, with CRIU's
reason), bytes moved per kind, and the frozen time so far.

**Events** (Bun's event log and `relish status`): requested, prepared, source
stopped (dump duration, payload size), transferred, restored or cold-started,
fell back (reason), aborted (reason), completed (downtime).

**Metrics** (Mayo):

- `reliaburger_migration_total{mode, outcome}`
- `reliaburger_migration_downtime_seconds` (source frozen to target healthy)
- `reliaburger_migration_payload_bytes{kind="memory|rootfs|volume"}`
- `reliaburger_migration_phase_seconds{phase}`
- `reliaburger_migration_fallback_total{reason}`

**`relish wtf` checks:** CRIU missing or below the minimum on a runc node
(`criu check`); an app opted into checkpoint that uses a host-path volume, a
GPU or rootless runc; nodes with different CPU models hosting a
checkpoint-enabled app; a migration past its deadline; orphaned payloads or
tombstoned volumes past retention; a cordoned node that's been cordoned for
days.

**`relish lint`** rejects `mode = "checkpoint"` together with a host-path
volume or a GPU.

**Docs.** A manual chapter ("Moving workloads"), a `docs/design/` section in
`deployments.md` (drain) and `agent-bun.md` (checkpoint), and a book section.
The book should tell the honest story: why stop-and-copy, why we drop TCP,
what CRIU refuses, and why the fallback is the real feature.

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

And the drain, with the soak's append-only writer on a managed volume:

```sh
relish drain node-2            # writer moves cold, cache moves with its memory
relish drain status node-2     # empty; nothing blocking
```

**Success criteria.**

- `DBSIZE` and a sample of values match before and after; `uptime_in_seconds`
  keeps counting.
- Frozen time for the 256 MiB cache under 10 seconds on the Apple-silicon
  quickstart (to be confirmed by spike S7; this is a target, not a
  measurement).
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
- The gate: every new request refused before finalisation.
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
drain a node with a mix of stateless, cold and checkpoint apps.

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
style: owners, fences, receipts, crash tests at every step.

| Piece | Weeks |
|---|---|
| Spikes S1-S8 (after the soak) | 1-1.5 |
| Feature-level gate, if PR #266's M5 lands first; 1.5 weeks if we build it | 0.5-1.5 |
| Cordon, `relish drain` for stateless apps, `uncordon`, status | 2 |
| Cold move: Raft record and state machine, node instructions, payload route, volume tar and tombstones, crash recovery, drain integration | 4-5 |
| Checkpoint mode: runc checkpoint/restore under owners, stdio, cgroup and egress ordering, tmpfs and encryption, time namespace, CRIU packaging, certificate revocation | 3-4 |
| Observability, `wtf`, lint, manual, design docs, book, soak loop, qualification | 1.5-2 |
| **Total** | **12-16** |

The CRIU-specific work is about a quarter of it. The rest is what any
stateful move needs, and it's where the crash matrix lives.

**0.2.0:** everything in section 5.1. Checkpoint mode labelled experimental;
cold moves and drain are the supported feature. If the spikes sink
checkpoint mode, ship cold moves and drain alone and say so.

**0.3:**

1. Pre-sync volumes while the app runs (Btrfs incremental send, or rsync
   passes elsewhere), then stop and send the last delta. The biggest
   downtime win, and it helps cold moves too.
2. Jobs: move a running attempt without spending a retry.
3. Memory pre-dump (`--pre-dump`, `--parent-path`), **x86-64 only** until
   arm64 has soft-dirty tracking.
4. Lazy pages, only if S7-style measurements show memory transfer dominating
   after (1). userfaultfd restores are the part Google called "very, very
   difficult" to make incremental.

**Later, each its own design:** GPU warm starts (a snapshot store with
invalidation rules, driver and CPU matching, and hardware to test on);
keeping TCP connections (needs addresses that can move between nodes);
automatic rebalancing and preemption-driven moves; rootless and non-runc
runtimes.

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
- *"Iterative pre-dump in 0.3"*: pre-dump needs soft-dirty tracking, which
  arm64 mainline doesn't have, and our quickstart is Apple silicon. Volume
  pre-sync is the better 0.3 downtime work.
- *GPU as a headline use*: every shipping LLM warm-start product we could
  verify restores a pre-warmed snapshot on the same class of machine, mostly
  via gVisor. That's a snapshot product, not migration, and we have no GPUs.
  Lead with batch, where there's evidence; use a memory-only server for the
  demo.
- *Upgrades as a trigger*: they don't restart workloads. Reboots do.
- *Effort*: the brief reads as a CRIU feature. It's a stateful-move feature
  with a CRIU option, at 12 to 16 weeks, and three spikes could still
  cut the CRIU part.
- *Unmentioned*: a checkpoint carries workload private keys off the node,
  which contradicts the workload-identity design and needs revocation.

## 13. Open questions for the maintainer

1. **Framing.** Do you accept "move first, CRIU as an opt-in mode" for 0.2.0,
   including shipping cold moves and drain alone if the spikes sink
   checkpoint mode?
2. **Gate.** Can migration share PR #266's feature-level finalisation (one
   level for 0.2.0)? Without a gate it's a protocol bump.
3. **Jobs.** Apps only in 0.2.0 with jobs first in 0.3, or jobs in 0.2.0
   given batch is the best-evidenced use?
4. **Opt-in shape.** `[app.<name>.migration] mode = "checkpoint" | "cold" |
   "never"`, with managed-volume apps defaulting to `cold`? Or should drain
   refuse to move stateful apps unless they opt in at all?
5. **Host-path apps during drain.** Block the drain (default here) or stop
   them with `--stop-blocking`?
6. **Plaintext on disk.** When tmpfs is too small, refuse checkpoint mode
   (proposed) or allow disk with a warning?
7. **CRIU distribution.** Depend on the upstream PPA in guest images and
   document it for other installs, or vendor a static `criu`? Minimum version
   4.1 (pidfd) or 4.2?
8. **Identity.** Is revoking the source instance's workload certificate on
   completion acceptable, given apps that cache their identity must reload
   it?
9. **CPU policy.** Require identical CPU models for checkpoint mode (from a
   node label), or run CRIU's `--cpu-cap` check and let it refuse?
10. **Book.** Which chapter gets the story: Chapter 7 ("Ship It", where
    deploys and draining live) or a section in Chapter 12?
