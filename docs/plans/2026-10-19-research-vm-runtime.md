# Stronger walls: a `vm` runtime on KVM

Research note, 19 October 2026, written against `origin/main` at `ef971e6a`. It answers the maintainer's question: could an app get stronger isolation by changing one field, `runtime`, and run as a Linux VM on the same hosts as containers, from the same OCI image (and from an ISO or disk image), and could that help move workloads between hosts? Nothing here is implemented, and no spike has run. Every claim about outside software links to its source.

## The short version

Yes, and it fits the code better than you'd expect. Reliaburger already has three runtime backends behind one trait (`Grill` in `src/grill/mod.rs`), a per-workload router between two of them (`MixedGrill` in `src/grill/mixed.rs`), a scheduler that keeps workloads off nodes whose runtime can't run them (`NodeCapabilities::containers`, #914), and a precedent for "every container is a VM": the Apple runtime (`src/grill/apple.rs`), where Apple's `container` runs each container in its own lightweight VM ([apple/containerization](https://github.com/apple/containerization)).

The recommendation:

1. **Add a fourth backend, `VmGrill`, with `RuntimeKind::Vm`, selected per app by `runtime = "vm"`.** Jobs already have a `runtime` field (`JobRuntime` in `src/config/job.rs`); apps get the same field. The generalised `MixedGrill` routes each instance to runc, process or VM and journals the choice, as it does today for runc and process.
2. **Drive [Cloud Hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor) as a pinned external binary**, over its REST API on a Unix socket, the same posture as `runc` today. It's Rust, runs on x86_64 and arm64, has virtio-fs, vsock, hotplug, UEFI firmware, snapshot/restore and live migration with pre-copy and post-copy over mutual TLS. Firecracker, QEMU, libkrun and a home-grown rust-vmm VMM are rejected below, with reasons.
3. **Boot every OCI app the same way**: a release-built guest kernel and a small static Rust guest agent (in an initramfs), the image's already verified, unpacked generation turned once into a read-only block image, and a per-instance scratch disk for writes. The agent speaks to Bun over vsock for start, stop, exec, logs, secrets and identity certificates. This is what Fly.io, Kata and Apple each converged on.
4. **Give the VM the same network shape as a runc container.** The VMM runs inside the per-instance network namespace that runc workloads already get, with a tap device joined to the existing veth. Port maps, the gateway DNS resolver and the nftables isolation table then work unchanged. What doesn't work unchanged is Onion's `connect()` rewrite, because it's a cgroup socket hook and a guest kernel's sockets never reach it. That's the biggest rough edge, and it has a host-side fix.
5. **Plug VMs into the move plan rather than building a second one.** [The container migration plan](2026-10-09-plan-container-migration.md) already defines `MoveRecord`, fencing, drain, modes and volume transfer. For a VM, `checkpoint` mode is a VMM snapshot with none of CRIU's refusal list, and `live` mode is the VMM's own live migration, which needs exactly the movable `/32` that spike S11 tests for 0.5.0. VMs may well be the easiest first customer for live moves.
6. **Ship it as three 0.4.x releases**: VM apps (0.4.3), VMs that move (0.4.4), and bring-your-own disk and ISO (0.4.5). The ISO and disk path is a different product (a guest that owns its OS, with no app semantics) and shouldn't hold back the OCI path.

What we don't do: a containerd shim, Kata as a dependency, a VMM written by us, Windows guests (sketched below, filed in Later), GPU passthrough, nested containers inside the guest, or memory overcommit by default.

## Who it's for

The same small team the move plan describes, with one more worry: a workload they don't fully trust. A customer-supplied plugin, a build that runs untrusted code, a vendor appliance, a CI runner, an AI agent's sandbox. Today the answer is "give it its own machine". With this, it's `runtime = "vm"` and the scheduler finds room on the machines they've got.

It isn't for dense, short-lived functions at Lambda scale (that's Firecracker's niche and a different control plane), or for running Windows desktops.

**If we did Windows guests.** Cloud Hypervisor runs Windows Server 2019+ and Windows 11 on UEFI only, with `kvm_hyperv=on`, no VGA device, and QEMU still needed to install the OS ([windows.md](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/windows.md)). Windows rides the disk-image path, not the OCI path, and the hypervisor is the easy part; what changes is below.

| Area | What changes | Size |
| --- | --- | --- |
| Images | Golden images built once in CI under QEMU (install + [virtio-win](https://github.com/virtio-win/virtio-win-pkg-scripts) drivers + SAC enabled, per [windows.md](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/windows.md)), then shipped through Pickle by digest; QEMU stays out of the runtime | Medium |
| Guest agent | The Rust agent runs as a Windows service over vsock (exec, logs, secrets, identity); [cloudbase-init](https://cloudbase.it/cloudbase-init/) replaces cloud-init for first boot | Large |
| Console | `relish console` speaks SAC over serial, since Cloud Hypervisor has no VGA adaptor ([windows.md](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/windows.md)); RDP for anything graphical. Desktops stay out of scope | Small |
| Windows 11 | TPM 2.0 under Cloud Hypervisor "was not proven successful" ([windows.md](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/windows.md)), so Server first, client later | Blocked upstream |
| Licensing | Placement on licensed node pools (Datacenter is licensed by core, with unlimited VMs ([Microsoft](https://www.microsoft.com/en-us/windows-server/pricing))), plus KMS or customer keys for activation | Medium |
| Migration | Live migration and snapshot of Windows guests aren't covered upstream; spike before promising vMotion parity | Unknown |
| CI | Evaluation-licensed images in `make test-vm`, slower boots | Small |

To displace vSphere, the gap is the ecosystem rather than the hypervisor: [VSS](https://learn.microsoft.com/en-us/windows-server/storage/file-server/volume-shadow-copy-service)-quiesced snapshots and a backup-vendor integration, VMDK import with driver injection (as [virt-v2v](https://libguestfs.org/virt-v2v.1.html) does), templates and clones, and a console UX. Without the first two, most vSphere shops won't move.

## What the user sees

The app config barely changes. Here's the Redis from `examples/phase-1/container-volumes.toml`, moved into a VM:

```toml
[app.redis]
image = "redis:7-alpine"
runtime = "vm"            # was implicitly "runc"
port = 6379
cpu = "500m-1"
memory = "256Mi-512Mi"

[[app.redis.volumes]]
path = "/data"
size = "10Gi"
```

Health checks, `[app.x.env]` (including `ENC[AGE:...]` secrets), `config_file`, ingress, autoscaling, `relish logs`, `relish exec` and `relish move` all keep their meaning. A small optional table holds what only a VM needs:

```toml
[app.redis.vm]
scratch = "2Gi"       # writable root filesystem disk (default 1Gi)
```

Memory has one new rule, which the manual has to say plainly: the guest sees the *upper* bound of `memory` as its RAM, because a kernel can't grow into memory it didn't boot with (until virtio-mem, below). The scheduler reserves that bound plus a measured per-VM overhead.

The node opts in, in `node.toml`, next to the existing `[runtime]` section:

```toml
[runtime.vm]
enabled = true
hypervisor = "/usr/lib/reliaburger/cloud-hypervisor@sha256:..."   # pinned by digest, same ref@sha256 format as images
```

And when it can't run here, the operator gets a reason, not a pending replica:

```text
$ relish status redis
redis  0/1  unplaced: no ready node can run VMs (needs /dev/kvm and [runtime.vm])
```

0.4.5 adds disk and ISO sources (see "The disk and ISO path").

## How it fits the code

| Concern | Where it lives today | What changes |
| --- | --- | --- |
| Runtime trait | `Grill` (`src/grill/mod.rs`), `impl Future` methods, so not `dyn`-safe | New `VmGrill` implementing `Grill`; `AnyGrill` gains a variant |
| Per-workload routing | `MixedGrill<C, H>` with a durable route journal (`src/grill/mixed.rs`) | Third backend; the `Route` records `RuntimeKind::Vm` |
| Runtime kinds | `RuntimeKind { Process, Runc, Apple }` (`src/grill/records.rs`) | Add `Vm`; it's serialised in instance records, so `state` in `CURRENT` (`src/compatibility.rs`) bumps |
| Node selection | `select_runtime` in `src/bin/bun.rs` (`auto`, `process`, `runc`, `mixed`; `apple` refused for now) | `mixed` gains the VM backend when `[runtime.vm]` is on and `/dev/kvm` opens |
| Surviving Bun restarts | Durable owners (`src/grill/process_owner.rs`, `src/grill/runc/owned.rs`), `adopt`, `launch_inventory` | The VMM is started under the same owner, so it outlives Bun and is adopted |
| Scheduling | `WorkloadRequirements` and `node_can_run` (`src/meat/filter.rs`), `NodeCapabilities` (`src/meat/cluster_state.rs`) | `requires_vm`, `NodeCapabilities::vm`, `UnplacedReason::NoVmRuntime`; overhead in `meat::admission::app_requests` |
| Config | `AppSpec` (`src/config/app.rs`), `JobSpec.runtime` | `AppSpec.runtime`, `[app.x.vm]`, validation refusals; gossip format changes, so `protocol` bumps |
| Image trust | Digest binding at apply, signature checks before deploy (`docs/design/security-sesame.md`) | Unchanged; the VM consumes the same verified generation |

Two trait methods already anticipate this. `pid()` documents `Ok(None)` "for runtimes where the PID isn't directly visible (e.g. containers running inside VMs)", and `honours_cgroup_path` says "process/VM runtimes run workloads elsewhere". The VM backend should honour the cgroup path, though: the VMM process lives in the workload's cgroup, which is what gives us `cpu.max`, `memory.max` and the I/O controller for free.

Rust note for the book: `Grill` returns `impl Future`, which can't go into a `Box<dyn Grill>`, so `AnyGrill` is an enum that matches on every call. Adding a variant forces every `match` to handle it at compile time. That's the type system doing our checklist for us, and a good place to explain it.

## Choosing a VMM

| VMM | Language | Arches | Rootfs options | Snapshot | Live migration | Verdict |
| --- | --- | --- | --- | --- | --- | --- |
| [Cloud Hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor) | Rust | x86_64, AArch64 (riscv64 experimental) | virtio-blk, virtio-fs, pmem | yes | yes: pre-copy, post-copy, mTLS ([docs](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/live_migration.md)) | **Recommended** |
| [Firecracker](https://github.com/firecracker-microvm/firecracker/blob/main/README.md) | Rust | x86_64, aarch64 | virtio-blk, pmem; no virtio-fs | yes, identical hardware and software ([docs](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md)) | no upstream | Rejected |
| QEMU | C | all | everything | yes | yes, including storage | Fallback for ISO edge cases only |
| [libkrun](https://github.com/containers/libkrun) | Rust, a library | x86_64, arm64, macOS HVF | virtio-fs | not documented | not documented | Rejected |
| Our own on [rust-vmm](https://github.com/rust-vmm/community) | Rust | ours | ours | ours | ours | Rejected |

**Why Cloud Hypervisor.** It covers both paths we need with one binary: direct kernel boot for OCI apps, and UEFI firmware (`CLOUDHV.fd` on x86_64, `CLOUDHV_EFI.fd` on AArch64) for disk images ([README](https://github.com/cloud-hypervisor/cloud-hypervisor)). It has hotpluggable virtio-net, block, pmem, fs and vsock devices and VFIO. Memory can be hugepage-backed, shared, prefaulted or hot-added with virtio-mem ([memory](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/memory.md)), and the balloon has `free_page_reporting`, which hands memory the guest freed back to the host ([balloon](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/balloon.md)). It sandboxes itself with Landlock ([landlock](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/landlock.md)). Snapshot/restore writes `config.json`, `memory-ranges` and `state.json`, and restore accepts fresh network file descriptors ([snapshot_restore](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/snapshot_restore.md)), which is exactly what a restore on another node needs. Live migration runs over a Unix socket (for in-place VMM upgrades) or TCP with optional mutual TLS, has `downtime_ms` and `timeout_s` budgets, and a failed or cancelled migration leaves the VM running on the source ([live migration](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/live_migration.md)). Mutual-TLS migration arrived in v53 (July 2026) and v54 (14 October 2026) added migration cancellation and a policy of accepting migrations from the two previous releases ([v54 notes](https://github.com/cloud-hypervisor/cloud-hypervisor/releases), [Phoronix on v54](https://phoronix.com/news/Cloud-Hypervisor-54), [Phoronix on v53](https://phoronix.com/news/Cloud-Hypervisor-53)). Kata Containers, Apple's Containerization (on Linux) and Virtink all build on it ([Kata hypervisors](https://github.com/kata-containers/kata-containers/blob/main/docs/hypervisors.md), [apple/containerization](https://github.com/apple/containerization), [Virtink](https://github.com/smartxworks/virtink)), so it's well exercised in exactly our shape.

Two cautions. Its README still says snapshot/restore and live migration aren't supported "across different versions", while v54 now promises n-2 for migration; we treat compatibility as directional and qualified, as the move plan already does for CRIU (S20). And arm64 live migration was reported working in v18 back in 2021 ([Phoronix](https://www.phoronix.com/news/Cloud-Hypervisor-18)); we found no current statement either way, so a spike proves it on both architectures.

**Why not Firecracker.** It's superb at what it's for: a VMM process with `<= 5 MiB` overhead for a 1 vCPU, 128 MiB guest and `<= 125 ms` to guest init ([specification](https://github.com/firecracker-microvm/firecracker/blob/main/SPECIFICATION.md)), and a [jailer](https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md) that chroots, namespaces, cgroups and drops privileges. But it has no virtio-fs (so host-directory volumes need another route), no UEFI boot (so no disk or ISO path), and no live migration upstream; people point at snapshot-and-restore or third-party forks instead ([OpenNebula forum](https://forum.opennebula.io/t/is-it-possible-to-live-migrate-firecracker-vms/8542), [Fly community](https://community.fly.io/t/live-migration-using-firecracker/16323)). Its snapshots "must be resumed on an software and hardware configuration which is identical to what they were generated on", and diff snapshots are still a developer preview ([snapshot support](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md)). For a feature whose second selling point is moving workloads, that's disqualifying. We should copy the jailer's ideas, though.

**Why not QEMU as the main backend.** QEMU does everything, including post-copy with recovery after a network failure ([postcopy](https://www.qemu.org/docs/master/devel/migration/postcopy.html)) and a Firecracker-like `microvm` machine type, which has no PCI and no hotplug and excludes migration across QEMU versions ([microvm](https://www.qemu.org/docs/master/system/i386/microvm.html)). It's also a large C codebase with a much bigger device surface. Nomad's QEMU driver shows the cost of the generic route: no exec, no network isolation, no volume mounts, and an `args_allowlist` operators must set or submitters can reach host devices ([Nomad QEMU driver](https://developer.hashicorp.com/nomad/docs/drivers/qemu)). Keep QEMU in reserve for the ISO path if Cloud Hypervisor can't boot an installer ISO (see rough edges).

**Why not libkrun.** It's a library, which tempts us to embed it, and it powers `crun --runtime=krun`, krunkit and containerd's experimental [nerdbox](https://github.com/containerd/nerdbox). Its default networking, TSI, impersonates guest sockets using the VMM's own host network access, and its README warns that the VM boundary doesn't limit host resources exposed through devices ([libkrun](https://github.com/containers/libkrun)). Its model is "a process with a kernel", not "a workload we can move", and it documents neither snapshot nor migration.

**Why not write our own.** The rust-vmm crates (`kvm-ioctls`, `vm-memory`, `virtio-queue`, `vhost`, `linux-loader`) make it possible ([rust-vmm](https://github.com/rust-vmm/community)), and it would make a great book chapter. It would also make us a VMM vendor with a VMM's CVE stream. "Use well-established crates, don't reinvent what exists" (`CLAUDE.md`) applies with extra force to the thing that stands between a hostile guest and the host.

**Embed or shell out?** Shell out. Bun drives `runc` as a CLI and `container` as a CLI and dropped containerd to avoid a gRPC socket and a second daemon (`docs/design/agent-bun.md`, decision log). Cloud Hypervisor's REST API over a Unix socket, called with a few hand-written serde types, keeps that posture. A VMM crash then kills one VM, never Bun.

**And gVisor?** It's the other answer to "stronger isolation": a user-space kernel, with `systrap` the default platform since mid-2023 and a KVM platform that's feasible but often slower under nested virtualisation ([gVisor platforms](https://gvisor.dev/docs/architecture_guide/platforms/)). It needs no `/dev/kvm`, which matters on clouds without nested virtualisation. It doesn't give us a guest kernel, ISO boot or VM migration, so it's a separate, smaller feature (`runtime = "gvisor"` via `runsc` as an OCI runtime) worth an issue of its own, not a substitute.

## The OCI path

Every serious "OCI image to microVM" system ends up with the same four parts: a kernel we ship, an init we ship, the image as a filesystem, and a control channel.

**Prior art.** Fly.io runs containerd on an LVM2 thin pool, takes a lease on a snapshot so containerd "synthesizes a new block device based on the image", and boots Firecracker with a Rust `init` that finishes the root filesystem, reads a config with user, network and entrypoint, rewrites `resolv.conf`, and spawns and monitors the entrypoint ([Docker without Docker](https://fly.io/blog/docker-without-docker/)). Kata maps a guest image in through DAX/pmem, shares the container bundle over virtio-fs, and runs `kata-agent` speaking ttRPC over vsock ([Kata architecture](https://github.com/kata-containers/kata-containers/blob/main/docs/design/architecture/README.md)). Apple's Containerization runs each container in its own VM with `vminitd`, a gRPC API over vsock, and ext4 filesystems it creates itself; it wants macOS 26, and its tested kernels start at 6.14.9 ([apple/containerization](https://github.com/apple/containerization)). firecracker-containerd puts an agent in the VM that runs `runc` inside it ([firecracker-containerd](https://github.com/firecracker-microvm/firecracker-containerd)), and Weave Ignite, the "VMs from OCI images with a container workflow" project, was archived in December 2023 in favour of Flintlock ([Ignite](https://github.com/weaveworks/ignite)).

**What we build.**

- **Guest kernel.** No new pipeline: it comes out of 0.3.0's weekly OS build, the one that already builds the netboot kernel and image, built and signed once per architecture. Ideally it's the same kernel artefact, with virtio, KVM clock and vsock built in rather than as modules; a separate minimal guest config only if the spike shows the netboot kernel costs too much boot time or memory. It's verified like a self-upgrade binary before use.
- **Guest agent.** A static musl Rust binary, `grill-guest`, packed with the kernel's initramfs. It mounts the image disk read-only, mounts the scratch disk, builds an overlay, `pivot_root`s, then starts the entrypoint with the image config's user, working directory and environment (the same resolution `src/grill/image_config.rs` does for runc). It's PID 1, reaps zombies, and reports exits. No runc inside the guest: one app per VM.
- **Control channel.** Cloud Hypervisor exposes guest vsock as a host Unix socket; the host connects and sends `CONNECT <port>` once per connection, and guest-initiated connections land on `<socket>_<port>` ([vsock](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/vsock.md)). So Bun needs no `AF_VSOCK` at all. The protocol is length-prefixed `serde_json` frames with a hard size cap: no gRPC, consistent with the containerd decision.
- **Root filesystem.** `ImageStore` already unpacks each verified image into a content-addressed `gen-*` directory (`docs/design/agent-bun.md` §1.1). The VM backend turns that directory, once per generation, into a read-only block image beside it (ext4 via `mkfs.ext4 -d`, or EROFS, which containerd now recommends for passing layers to VMs with less overhead than virtio-fs or 9p ([EROFS snapshotter](https://containerd.io/docs/2.3/snapshotters/erofs/))). Many VMs share it read-only, as many runc containers share the lower layer today. Each instance gets a sparse scratch disk as its overlay upper, kept across a restart of the same instance and deleted with it, matching runc's `rootfs-upper` semantics. A dm-verity root hash on the kernel command line is a later hardening step.
- **Why not virtio-fs for the root?** It needs an external `virtiofsd` per share and `--memory shared=on`, and Cloud Hypervisor doesn't support DAX ([fs](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/fs.md)). That's an extra process per VM, extra host attack surface, and a device whose migration story we haven't verified. A block image is one file.

**Secrets, config files and identity.** The runc path writes decrypted secrets into a 0600 `config.json` and mounts identity certificates from a per-instance tmpfs at `/var/run/reliaburger/identity/` (`docs/design/security-sesame.md`). For a VM, Bun pushes them over vsock into a guest tmpfs at the same path, never onto a disk image or the kernel command line (visible in host `ps` and guest `/proc/cmdline`). Rotation every 30 minutes becomes "push the new files, agent renames atomically".

## Networking

This is where most of the work hides.

**The shape.** A runc instance gets its own network namespace, a veth pair with a node-side gateway, an address lease (`src/grill/network_leases.rs`), host-port DNAT (`src/grill/portmap.rs`) and a resolver on the gateway (`docs/design/discovery-onion.md`). The VM backend keeps all of that and runs the VMM *inside* that namespace, with a tap device joined to the container-side veth by TC redirect. That's Kata's default model ([Kata networking](https://github.com/kata-containers/kata-containers/blob/main/docs/design/architecture/networking.md)). The guest gets the lease's address statically (kernel `ip=` argument or the agent). From the host's side, a VM instance is indistinguishable from a runc instance: same veth, same lease, same port map, same per-namespace nftables isolation sets (`src/firewall/isolation.rs`), same health-check target, same `container_ip()`.

**What breaks: service VIPs.** Onion answers `redis.internal` with a VIP from `127.128.0.0/16` and rewrites it in a `BPF_CGROUP_INET4_CONNECT` hook attached to the root cgroup (`docs/design/discovery-onion.md` §3). Both halves fail inside a guest. The guest kernel treats `127.128.x.x` as its own loopback, so packets never leave the VM; and the guest's `connect()` happens in the guest kernel, so the host's cgroup hook never runs. Packets leave the VM as frames on a tap, not as host sockets.

The fix is host-side, because the guest is untrusted and enforcement must stay on the host:

- Bun's resolver already knows which instance asked (by source address), so for a VM client it answers with a *mirror* VIP from a small routable range reserved for this, mapped one-to-one onto the real VIP.
- A TC eBPF program on the host-side veth's ingress rewrites mirror VIP packets to a backend using the same `backend_map`, the same health view and the same view lease as `connect4`, with conntrack handling the reply. An nftables DNAT map maintained by Bun is the simpler fallback and the first thing to try in the spike.

**What breaks: policy.** Egress allowlists and `allow_from` are enforced by cgroup socket hooks keyed on the caller's cgroup id. A VM's traffic has no such socket. The forward-path nftables isolation table already catches "traffic that never meets a socket hook" between namespaces, so namespace isolation holds. Per-app egress allowlists need a per-veth TC or nftables equivalent. Until it exists, a VM app with egress or `allow_from` rules is refused at apply with a named reason, just as nodes that can't enforce an allowlist refuse them today.

**Ingress and mTLS.** Wrapper proxies to backends by address and host port, so a VM backend looks like any other. Workload mTLS identity reaches the guest through the agent (above), so an app inside a VM presents the same SPIFFE identity as it would in runc.

## Storage and volumes

Reliaburger has three managed-volume backends: plain directories, Btrfs subvolumes with quotas, and a loop-mounted ext4 file elsewhere (`docs/manual/11_images-and-volumes.md`). For a VM:

- **Loop-ext4 is already a disk image.** Hand the same file to the guest as virtio-blk instead of loop-mounting it on the host. Quotas hold by construction, and a block device is the easiest thing to snapshot and move. This should be the default backend for VM volumes, so the VM path may want a node to provision loop-ext4 even where it would otherwise pick Btrfs.
- **Btrfs and plain directories** need virtio-fs (one `virtiofsd` per volume, shared guest memory) or a per-volume image file. Start with image files; add virtio-fs only if someone needs host-visible files.
- **Host-path volumes** keep the double gate (node allowlist plus `host-exec`) and need virtio-fs. libkrun's warning applies: virtio-fs exposes the whole tree to the guest kernel ([libkrun](https://github.com/containers/libkrun)). Refuse them in the first milestone.
- **Snapshots** (`relish snapshot`) of a VM volume need the guest to flush. The agent does `fsfreeze` on request, the host snapshots, the agent thaws.

## Resources and scheduling

**One pool.** VMs and containers share the node's allocatable CPU and memory, and the scheduler stays a pair of plain functions (`node_can_run`, `score_nodes`). What changes is what one replica costs:

- **Memory.** A guest boots with fixed RAM, the upper bound of `memory`. The VMM's cgroup is charged only for pages the guest has touched, but without help that charge only grows. So a VM replica requests `memory.max + overhead`, not the lower bound, and the cgroup's `memory.max` is set to the same. Balloon `free_page_reporting` gives freed guest pages back ([balloon](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/balloon.md)). No overcommit by default: if the host OOM-kills a VMM, the whole guest dies at once, which is a much bigger blast radius than one process.
- **Overhead.** VMM threads, the guest kernel, page tables and the agent. Firecracker claims `<= 5 MiB` for the VMM alone ([specification](https://github.com/firecracker-microvm/firecracker/blob/main/SPECIFICATION.md)); the guest side will dominate. A spike measures it per architecture; nodes report the measured figure in `NodeCapabilities::vm`, and the qualification record prints it.
- **CPU.** vCPUs are `ceil(cpu.max)` threads; `cpu.max` on the VMM cgroup enforces the millicores, so the guest sees steal time rather than extra cores. CPU pinning, NUMA and hugepages stay out until someone asks; Cloud Hypervisor supports hugepage backing when they do ([memory](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/memory.md)).
- **Autoscaling.** Horizontal scaling works unchanged. Vertical changes need virtio-mem hot-add or a restart; that's a later item.
- **Capabilities.** `NodeCapabilities` gains `vm: Option<VmCapability>` (architecture, VMM version, kernel artefact version, CPU model, measured overhead, whether volumes can be virtio-fs). `WorkloadRequirements` gains `requires_vm`, checked in `runs_its_kind` beside `requires_containers`, and an unplaceable app gets `UnplacedReason::NoVmRuntime`.

## Observability, exec and faults

- **Logs.** The agent captures the entrypoint's stdout and stderr and streams them over vsock; Bun writes them to the instance's existing capture files (`log_stem`), so Ketchup, resume offsets and `relish logs` don't change. The serial console goes to a third capture file, because a kernel panic is the one log line nobody can afford to lose.
- **Metrics.** Bun keeps reading the VMM cgroup for host-side CPU and memory (what scheduling and autoscaling care about); the agent reports guest-side memory (`MemAvailable`) as a separate series, and the dashboard labels which is which. Scraping an app's metrics endpoint works by address.
- **Health.** HTTP and TCP checks go to the guest address exactly as they do to a container's. Exec-style checks go through the agent.
- **Exec.** `Grill::exec` becomes an agent call that spawns a process in the guest and returns its output. `relish exec` works unchanged.
- **Smoker.** Faults that act on cgroups work on the VMM cgroup: `cpu`, `memory` (with different semantics: the guest slows down rather than OOMs), `disk-io`. `kill` on the VMM is a power cut, which is honest; `pause` and `resume` become the VMM's pause API. `delay` via tc netem on the caller's interface works on the veth. `drop`, `partition` and `dns` work through connect hooks that a guest never hits, so they'd silently do nothing. The 12b.6 lesson (#117, a DNS fault that was a no-op while its tests passed) says: refuse them for VM targets by name until TC versions exist.

## Security

- **The VMM runs unprivileged.** Bun (root) creates the namespace, tap and cgroup, opens `/dev/kvm` and the disk files, and hands file descriptors to a Cloud Hypervisor process running as a per-node `burger-vm` user in its own mount namespace with Landlock on. That's the Firecracker jailer's recipe ([jailer](https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md)) without needing the jailer.
- **The guest is hostile.** Everything from vsock is untrusted input: bounded frames, bounded log rates, no host path or command ever taken from the guest. A compromised guest can lie about its health and its metrics; it mustn't be able to do anything else.
- **Image trust is unchanged.** Images are bound to a digest at apply and signatures are verified before deploy; the VM uses the same verified generation. The kernel, agent and VMM binary are release artefacts, verified by the same signature path as self-upgrade.
- **Honest limits.** A VM boundary is only as good as the host's CPU mitigations and the VMM's device code. The manual should say what it does and doesn't protect against, in one paragraph, rather than imply "unbreakable".

## Moving VMs

The [migration plan](2026-10-09-plan-container-migration.md) and its [research](2026-09-28-research-container-migration.md) already did the hard part: the continuity contract, `MoveRecord` with ownership epochs, target activation as the one-way recovery boundary, encrypted transfers bound to a manifest, drain, and the three modes. Its decision 1 says ProcessGrill and the Apple runtime refuse checkpoint and live moves. VMs should be added as a second supported runtime, not a separate system.

| Mode | Container (0.4.0 / 0.5.0 plan) | VM |
| --- | --- | --- |
| `cold` | Stop, carry volumes, start | Same; plus the scratch disk if the policy keeps it |
| `checkpoint` | `runc checkpoint` / CRIU, with S8's refusal list (io_uring, tracers, packet pipes, corked UDP…) | Pause, snapshot memory and device state, transfer, restore with fresh net FDs ([snapshot_restore](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/snapshot_restore.md)). No refusal list: the guest kernel's state is just memory |
| `live` | CRIU pre-dump plus `TCP_REPAIR`, packet locking, needs S11's movable address; no soft-dirty pre-copy on arm64 | VMM live migration: pre-copy with a `downtime_ms` budget, optional post-copy, mTLS ([live migration](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/live_migration.md)); guest TCP state moves with guest memory; needs S11's movable address |

That's the interesting finding. The hardest CRIU problems (what a process holds that can't be dumped, time namespaces, egress ordering, TCP repair) are inside the guest for a VM, and the guest carries them along. What remains is shared with containers:

- **The address has to move.** Today a client's socket names the source node's IP and host port, because Onion rewrites at `connect()` and DNAT forwards. A migrated guest keeps its IP, but nobody routes to it on the target. That's S11's movable `/32`, unchanged. Before 0.5.0, a VM `checkpoint` move re-addresses the guest through the agent after restore, and connections close, which is precisely the checkpoint contract.
- **Disks don't move by themselves.** Cloud Hypervisor's migration needs kernels and disk images "at the same paths on both machines" and doesn't copy them ([live migration](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/live_migration.md)). The read-only image disk is content-addressed and Pickle can pre-place it. Volumes and the scratch disk use the move plan's volume transfer (Btrfs send, chunked copy), with a final copy inside the pause. Fly.io's answer for big disks is lazy hydration with dm-clone, booting on a volume that fills in from the source ([Making Machines Move](https://fly.io/blog/machine-migrations/)); it leaves the target depending on the source, as post-copy does, so it waits for evidence. KubeVirt, for comparison, requires ReadWriteMany shared storage for live migration of PVC-backed VMs, refuses bridge binding, and warns that with post-copy "if either the target or guest VMs crash the state cannot be recovered" ([KubeVirt live migration](https://kubevirt.io/user-guide/compute/live_migration/)). QEMU's post-copy docs say the same: a failure of either side loses the guest ([postcopy](https://www.qemu.org/docs/master/devel/migration/postcopy.html)). So: pre-copy only, post-copy off, as the move plan already decided for lazy pages.
- **Compatibility is directional.** The pool fingerprint gains the VMM version and CPU model. v54's n-2 policy and "backward migrations are not supported" match the plan's S20 rules for kernels and CRIU. Heterogeneous CPUs need a common CPU model or a refusal.
- **arm64 isn't second-class.** CRIU on arm64 has no soft-dirty tracking, so container checkpoints there are a full stop-and-copy (move plan, "The laptop cluster comes first"). VM pre-copy uses the VMM's own dirty-page tracking instead, which Cloud Hypervisor added in v16 ([Phoronix](https://www.phoronix.com/news/Cloud-Hypervisor-16.0)). The spike must confirm arm64 live migration on the current release.
- **Nested moves are fine.** In the quickstart, nodes are VMs, so a moved VM is an L2 guest. The kernel docs expect L2 migration to work on Intel and AMD alike; it's migrating an L1 *with* running L2s that's undefined on AMD ([running nested guests](https://docs.kernel.org/virt/kvm/x86/running-nested-guests.html)). We never do that.

The VM path also gives us a bonus the move plan wanted later: Cloud Hypervisor's local migration with `memory_mode=memfds` hands guest memory to a new VMM process for "VMM live upgrades with minimal VM downtime" ([live migration](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/live_migration.md)), which makes upgrading the VMM binary under running VMs a solved problem.

## The disk and ISO path

This is a different product from the OCI path, and it shouldn't share a milestone with it.

|  | OCI image | Disk or ISO |
| --- | --- | --- |
| Who owns the OS | We do (our kernel, our init) | The guest |
| Boot | Direct kernel boot | UEFI firmware |
| Root filesystem | Read-only image plus scratch, rebuildable | One persistent disk that *is* the workload's state |
| Configuration | Env, secrets, config files via agent | cloud-init NoCloud seed (`user-data`, `meta-data`, `network-config` on a `CIDATA` volume) ([NoCloud](https://docs.cloud-init.io/en/latest/reference/datasources/nocloud.html)) |
| Logs, exec | Agent | Serial console only, unless the image runs our agent |
| Rolling deploy | Replace the instance | Means "reinstall"; usually you don't |

**Distribution.** KubeVirt's `containerDisk` ships raw or qcow2 disks in a registry image under `/disk`, explicitly ephemeral ([disks and volumes](https://kubevirt.io/user-guide/storage/disks_and_volumes/)). We can do the same through Pickle, so disks and ISOs get digests, signatures, quotas and P2P distribution for free. On first boot the image is copied into a managed volume that becomes the persistent root disk, and from then on it moves like any other volume.

**Config sketch** (field names are proposals):

```toml
[app.crm]
runtime = "vm"
port = 8080
cpu = "2"
memory = "4Gi"

[app.crm.vm]
boot = "uefi"
disk = "pickle.internal/vms/crm@sha256:..."   # copied to a 40Gi root volume
root_size = "40Gi"
cloud_init = "crm-user-data.yaml"
```

An `iso` field attaches a read-only installer or live ISO. Installers need a console (`relish console`), and OpenStack users learnt early that an ISO attached as a disk rather than a CD-ROM often won't boot ([OpenStack thread](https://lists.openstack.org/pipermail/openstack/2013-November/003495.html)). Cloud Hypervisor's devices are virtio-only and its docs don't describe a CD-ROM device, so whether its EDK2 build boots an ISO attached as read-only virtio-blk is unverified. Spike it; if it fails, ISOs either run under a small QEMU backend or wait.

Recommendation: start with cloud images (qcow2/raw with cloud-init), which cover appliances and "lift this old VM over"; add live and installer ISOs only when someone asks for one.

## Host requirements

| Where | `/dev/kvm` for us | Notes |
| --- | --- | --- |
| Bare metal x86_64 and arm64 | Yes | The release tarball carries the VMM, kernel and agent; the 0.3.0 appliance image includes them too, since its weekly OS build already makes the kernel |
| Linux quickstart (Lima on QEMU) | Yes, nested | Nested KVM is on by default for Intel and AMD since Linux 4.20 ([kernel docs](https://docs.kernel.org/virt/kvm/x86/running-nested-guests.html)) |
| macOS quickstart (Lima on VZ) | M3 or later with macOS 15 or later | Virtualization.framework gained nested virtualisation in macOS 15 on M3+ ([Apple](https://developer.apple.com/documentation/virtualization/vzgenericplatformconfiguration/isnestedvirtualizationenabled), [GeekNews summary](https://news.hada.io/topic?id=15318)); Lima 1.0 added `nestedVirtualization` for VZ on M3 ([Lima 1.0](https://newreleases.io/project/github/lima-vm/lima/release/v1.0.0)). **M1 and M2 Macs can't** |
| AWS | Since February 2026 on C8i, M8i, R8i virtual instances ([AWS](https://aws.amazon.com/about-aws/whats-new/2026/02/amazon-ec2-nested-virtualization-on-virtual)), widened in June 2026 to C7i, M7i, R7i and more ([AWS](https://aws.amazon.com/about-aws/whats-new/2026/06/nested-virtualization-intel-us-gov-cloud/)) | Graviton only on metal |
| GCP | Intel machines except E2 and memory-optimised; AMD only on N4D; never Arm ([GCP](https://docs.cloud.google.com/compute/docs/instances/nested-virtualization/overview)) | Expect 10% or more CPU loss |
| Azure | Size-dependent: Dv5/Ev5 and Dasv5 list it ([Dasv5](https://docs.azure.cn/en-us/virtual-machines/sizes/general-purpose/dasv5-series)), some older AMD sizes don't ([Q&A](https://learn.microsoft.com/en-us/answers/a/1164354)) | Check the size page |
| GitHub-hosted Linux runners | Yes, after a udev rule, since April 2024 on 2-vCPU runners ([changelog](https://github.blog/changelog/2024-04-02-github-actions-hardware-accelerated-android-virtualization-now-available/)) | `soak.yml` already opens `/dev/kvm` this way (`docs/releasing.md`) |
| macOS host-native nodes | No KVM | The Apple runtime returns in 0.4.3 (F10, #367). Every Apple container already is a VM, so `runtime = "vm"` is satisfied by it, with cold moves only |

The M1/M2 gap matters for the laptop-first rule. The move plan says every stage's demo "must pass on the default quickstart cluster" on Apple silicon. For VMs, that's only true on M3 or newer, and the manual and `relish wtf` must say so plainly.

## Testing strategy

Tests first, as always. The order:

**Portable (in `make ci`).**

- Config: `runtime = "vm"` parses and round-trips; each refusal has a test (egress rules, `allow_from`, host-path volumes, init containers, `exec` or `script` with `vm`, GPUs) and a snapshot of the error text.
- Scheduler: `requires_vm` keeps VM apps off nodes without the capability across leader change; `UnplacedReason::NoVmRuntime` is reported; a proptest bin-packs mixed VM and container replicas and never exceeds allocatable memory once overhead is counted.
- Agent protocol: frame encode/decode round-trips; proptest feeds hostile frames (oversize, truncated, wrong types, floods) and the host side never panics or allocates past its cap.
- VMM client: the REST client against a fake Cloud Hypervisor on a Unix socket in a temp directory, including timeouts and a VMM that dies mid-call.
- Routing: `tests/suite/` gains a mock VM backend behind `MixedGrill`, covering adoption after a Bun restart, a route journal that names `Vm`, and a restore that runs and then reports an error (the move plan's case).

**Gated: a new `make test-vm`.** `RELIABURGER_VM_TESTS=1`, needs `/dev/kvm`, the pinned VMM and the kernel artefact. It refuses, rather than skips, when a prerequisite is missing (the move plan's rule). It boots a VM from the pinned test images, checks health, logs, exec, secrets and identity rotation, restarts Bun underneath it, kills the VMM, and asserts no leaked taps, mounts or cgroups. It runs on a GitHub-hosted Ubuntu runner directly (L1 is the runner, our VMs are L2), not inside Lima, which would make them L3. `make test-linux` gains the VM networking cases (mirror VIPs, isolation sets, port maps).

**Cluster.** `make test-cluster` gains VM scenarios on the Lima quickstart with nested virtualisation: Linux x86_64 in CI, Apple M3+ as a release qualification lane recorded in `docs/qualification/`.

**Smoker.** A fault matrix for VM targets: each supported fault has an effect assertion (not just "the command succeeded"), and each unsupported one refuses by name.

**Soak.** VM apps join the V02 workload mix, and VM moves join the move fault kinds the move plan adds, rather than getting a soak of their own.

**Built-in.** `relish test --filter vm` runs the Redis demo inside a VM, and `--filter move` gains a VM leg once moves exist.

## Rough edges and their solutions

| Problem | Impact | Solution | Milestone |
| --- | --- | --- | --- |
| Service VIPs are `127.128.0.0/16` (guest loopback) and rewritten by a host cgroup `connect4` hook the guest never reaches | VM apps can't call other services | Mirror VIPs answered by Bun's resolver to VM clients; TC eBPF (or nftables DNAT) on the host veth using `backend_map` | 0.4.3, spike V3 |
| Egress allowlists and `allow_from` rely on cgroup socket hooks | Policy silently unenforced for VMs | Refuse such VM apps at apply; per-veth TC/nftables enforcement later | Refuse in 0.4.3, enforce in 0.4.4 |
| Smoker `drop`, `partition`, `dns` act in connect hooks | Faults would be no-ops (the #117 trap) | Refuse for VM targets by name; TC versions later | 0.4.3 |
| No nested virtualisation on M1/M2 Macs, Graviton virtual instances, GCP Arm and most AMD | Laptop demo and some clouds can't run VMs | Capability-gated placement, `NoVmRuntime`, `relish wtf` explains; manual lists hosts | 0.4.3 |
| Guest RAM is fixed at boot and host charge only grows | Over-commit OOM kills whole VMs | Schedule at `memory.max` + overhead; balloon free-page reporting; no overcommit by default | 0.4.3 |
| Per-VM overhead (guest kernel, agent, VMM) | Density drops, packing misjudged | Measure per arch in spike V2; report in capabilities; print in the manual | 0.4.3 |
| Boot time and image conversion | Slower deploys and first start | Convert once per image generation, content-addressed; `health.initial_delay` default for VMs | 0.4.3 |
| Writable root filesystem must have a size | Runc has no such limit; surprises users | `[app.x.vm] scratch` with a default and a clear "disk full" event | 0.4.3 |
| We now ship a kernel | CVEs, two architectures, rebuild cadence | Pinned config, signed artefact, rebuilt with 0.3.0's weekly OS build | 0.4.3 |
| Guest agent is a new trust boundary | A hostile guest attacks Bun through vsock | Capped frames, rate limits, proptest/fuzz; host never acts on guest-supplied paths | 0.4.3 |
| Init containers share a filesystem with the main container | No obvious VM equivalent | Refuse first; later run them in the same guest before the entrypoint | Refuse 0.4.3 |
| virtio-fs needs a `virtiofsd` per share and shared memory; migration unverified | Extra processes; moves may fail | Volumes as virtio-blk (loop-ext4 files); virtio-fs only for host paths, which refuse moves | 0.4.3 / 0.4.4 |
| Disks aren't migrated by the VMM | Live moves need the disk on the target | Pre-placed content-addressed image disk; move plan's volume transfer; final copy in the pause | 0.4.4 |
| Snapshot and migration compatibility is version- and CPU-bound | Restore fails mid-move | Fingerprint VMM version and CPU model; directional admission (as S20) | 0.4.4 |
| Guest clock after restore | Timers jump, leases misfire | KVM clock in the guest; agent steps the clock after restore; S6's timer checks | 0.4.4 |
| Guest address is node-local before S11 | Checkpoint moves drop connections | Agent re-addresses after restore; `live` waits for 0.5.0's movable `/32` | 0.4.4 / 0.5.0 |
| ISO boot under Cloud Hypervisor is unverified | ISO path may not work | Spike V5; QEMU backend for ISOs only if needed | 0.4.5 |
| VMM upgrades under running VMs | Restart all VMs to patch the VMM | Local migration with `memory_mode=memfds` | 0.4.4 or later |
| CI nesting depth | L3 guests are slow or impossible | Run `make test-vm` on the runner, not inside Lima | 0.4.3 |

## Milestones within 0.4.x

0.4.0 and 0.4.1 are container migration, and 0.4.2 is the AI SRE. The VM runtime follows them, so its moves can reuse what 0.4.0 builds. As with the move plan: a milestone is something an operator can use, built from week-sized steps, and a failed spike removes its feature rather than holding the release.

### Spikes (can start alongside 0.4.x, one week each unless noted)

| Spike | "Yes" means | "No" means |
| --- | --- | --- |
| V1: nested KVM where people are | Cloud Hypervisor boots a guest inside the Lima quickstart on Linux x86_64 and Apple M3+, and on a GitHub-hosted runner | The laptop demo is Linux-only; the manual says so |
| V2: an OCI image as a VM | `redis:7-alpine` boots from a converted read-only image plus scratch disk with our kernel and agent, on both arches; boot time and overhead recorded | Revisit virtio-fs for the root |
| V3: the network shape | VMM in the per-instance netns with tap/TC redirect; port map, isolation sets and a mirror VIP all work, with the VMM unprivileged | VM apps can't use services; the feature waits |
| V4: snapshot and migrate between two nodes | Snapshot/restore with new net FDs and `send-migration` over mTLS between two quickstart nodes, on both arches | `checkpoint` and `live` are refused for VMs; cold moves only |
| V5: ISO and UEFI | EDK2 under Cloud Hypervisor boots a cloud image with a NoCloud seed, and an ISO attached read-only | Disk path only, or a QEMU backend for ISOs |

### 0.4.3: "Stronger walls" (VM apps)

Exit test: the same Redis config deploys with `runtime = "runc"` and then `runtime = "vm"` on the quickstart cluster (Linux x86_64 and Apple M3+), keeps its volume data across the switch, is reachable by name from a runc client and reaches a runc service by name, and `relish logs`, `relish exec`, health checks and a rolling deploy all work. Roughly 10 to 12 developer-weeks.

Draft issues:

- Add `runtime = "vm"` to apps, with validation refusals
- Guest kernel and `grill-guest` agent as signed release artefacts
- `VmGrill`: Cloud Hypervisor lifecycle under a durable owner, with adoption
- Read-only image disks from image generations, and scratch disks
- vsock agent protocol: start, stop, exit codes, logs, exec
- Secrets, config files and identity certificates over vsock
- VM networking: tap in the instance namespace, port maps, isolation
- Mirror VIPs: service discovery for VM clients
- Scheduler: `NodeCapabilities::vm`, `requires_vm`, overhead accounting
- VM volumes as virtio-blk on loop-ext4
- Smoker: VM fault matrix and named refusals
- `make test-vm` gate and CI job with `/dev/kvm`
- Apple runtime returns (F10, #367) and satisfies runtime = "vm" on macOS host-native nodes, cold moves only
- Manual: host OS support matrix (runtimes, move modes, nested virtualisation per host), showing macOS gets cold moves only
- Quickstart: Lima nested virtualisation on by default on M3+; on M1/M2, bun refuses to start with the vm runtime enabled
- Manual chapter section, `vm-*` examples, book chapter

### 0.4.4: "VMs that move"

Exit test: drain a node running a VM Redis with `mode = "checkpoint"` and a VM volume app with `mode = "cold"`; both come back on another node with every acknowledged write, and the drained node holds nothing. Egress allowlists work for VM apps. Roughly 5 to 6 developer-weeks, given 0.4.0.

Draft issues:

- VM cold moves through `MoveRecord` and the volume transfer
- VM checkpoint moves: snapshot, encrypted transfer, restore with new net FDs
- Pool fingerprint: VMM version, CPU model, directional admission
- Egress allowlists and `allow_from` for VMs on the host veth
- Smoker `drop`, `partition` and `dns` for VM targets
- VMM upgrade under running VMs (local migration)

### 0.4.5: "Bring your own disk"

Exit test: a stock Ubuntu cloud image boots from Pickle with a cloud-init seed, serves HTTP behind Wrapper, keeps its root disk across a restart and a cold move. Roughly 4 to 5 developer-weeks.

Draft issues:

- Disks and ISOs as signed OCI artifacts in Pickle
- UEFI boot and persistent root disks as managed volumes
- cloud-init NoCloud seeds from app config
- `relish console` for serial access
- ISO attach (gated on spike V5)

### 0.5.0: live moves, VMs included

If S11 says yes, live VM moves are the VMM's pre-copy migration over mTLS with the movable `/32`, inside the interruption budget. They're 0.5.0's first target, ahead of live container moves, since they skip CRIU's refusal list and TCP repair, and they work on arm64 with pre-copy. Draft issue: "Live moves for VMs with movable addresses".

### Later

No date. Draft issues:

- `runtime = "gvisor"` via `runsc` as an OCI runtime, for hosts without `/dev/kvm`
- Windows Server guests on the disk-image path (see "If we did Windows guests"), with VSS backup and VMDK import as the research questions

## On GitHub

The milestones and issues created from this plan on 19 October 2026. #367 (F10, the Apple runtime's return) moved from Later into 0.4.3.

| Milestone | Issues |
|---|---|
| [0.4.3](https://github.com/reliaburger/reliaburger/milestone/20) | #1153–#1171, #367 |
| [0.4.4](https://github.com/reliaburger/reliaburger/milestone/21) | #1172–#1178 |
| [0.4.5](https://github.com/reliaburger/reliaburger/milestone/22) | #1179–#1184 |
| [0.5.0](https://github.com/reliaburger/reliaburger/milestone/14) | #1185 |
| [Later](https://github.com/reliaburger/reliaburger/milestone/9) | #1186, #1187 |

## Decisions

1. **The field.** `runtime = "vm"` on apps, mirroring jobs, rather than `isolation = "vm"` (which would say why rather than how). Decided for now: `runtime`.
2. **The single binary.** The VMM binary, guest kernel and agent are three new artefacts per architecture. Ship them in the release tarball for now, the easiest start. Distributing them through Pickle or embedding them in `bun` (tens of megabytes) can come later.
3. **Release slots.** Three 0.4.x releases after the AI SRE. Decided: no separate 0.6.0 minor release of its own.
4. **ISO and disk.** In 0.4.x. Decided: not deferred to "Later".
5. **Overcommit.** Decided: schedule VMs at their memory limit, with no overcommit for now. A per-node opt-in can come later.
6. **macOS.** When the Apple runtime returns (F10) in 0.4.3, it satisfies `runtime = "vm"` on macOS nodes, with cold moves only. Checkpoint moves stay out, because Virtualization.framework save files are hardware-encrypted and "no other Mac or user account can … restore the virtual machine" ([WWDC23 session 10007](https://developer.apple.com/videos/play/wwdc2023/10007/)). 0.4.3 also ships a manual page comparing what each host OS supports (runtimes, move modes, nested virtualisation), stating plainly that macOS nodes get cold moves only.
7. **Live moves.** Decided: VMs are the first workload 0.5.0 targets, if spike V4 passes before the CRIU spikes.
8. **Quickstart.** Decided: turn on Lima's nested virtualisation by default on M3+ Macs (it costs nothing when unused). On M1 and M2, bun refuses to start with the vm runtime enabled and names the chip as the reason. Both in 0.4.3.
9. **gVisor.** Decided: a separate small issue for `runtime = "gvisor"` on hosts without KVM, in the Later milestone.
10. **Examples.** Decided: a new `vm-*` prefix in `examples/`, plus a line in `CLAUDE.md`.

## Sources

Reliaburger (read on `origin/main` at `ef971e6a`): `docs/roadmap.md`, `docs/plans/2026-10-09-plan-container-migration.md`, `docs/plans/2026-09-28-research-container-migration.md`, `docs/plans/2026-10-19-research-extension-points.md`, `docs/design/agent-bun.md`, `docs/design/discovery-onion.md`, `docs/design/scheduler-meat.md`, `docs/design/security-sesame.md`, `docs/design/chaos-smoker.md`, `docs/manual/05_chaos.md`, `docs/manual/11_images-and-volumes.md`, `docs/book/STYLE.md`, `src/grill/mod.rs`, `src/grill/mixed.rs`, `src/grill/apple.rs`, `src/grill/records.rs`, `src/grill/process_owner.rs`, `src/grill/runc/owned.rs`, `src/bin/bun.rs`, `src/config/app.rs`, `src/config/job.rs`, `src/config/node.rs`, `src/meat/filter.rs`, `src/meat/cluster_state.rs`, `Makefile`, `examples/phase-1/container-volumes.toml`.

External:

- Cloud Hypervisor README: https://github.com/cloud-hypervisor/cloud-hypervisor
- Cloud Hypervisor live migration: https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/live_migration.md
- Cloud Hypervisor releases (v54): https://github.com/cloud-hypervisor/cloud-hypervisor/releases
- Phoronix, Cloud Hypervisor 54: https://phoronix.com/news/Cloud-Hypervisor-54
- Phoronix, Cloud Hypervisor 53: https://phoronix.com/news/Cloud-Hypervisor-53
- Phoronix, Cloud Hypervisor 18 (AArch64 live migration): https://www.phoronix.com/news/Cloud-Hypervisor-18
- Phoronix, Cloud Hypervisor 16 (dirty-page tracking, AArch64 UEFI): https://www.phoronix.com/news/Cloud-Hypervisor-16.0
- Cloud Hypervisor memory: https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/memory.md
- Cloud Hypervisor balloon: https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/balloon.md
- Cloud Hypervisor snapshot/restore: https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/snapshot_restore.md
- Cloud Hypervisor Landlock: https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/landlock.md
- Cloud Hypervisor vsock: https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/vsock.md
- Cloud Hypervisor virtio-fs: https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/fs.md
- Firecracker README: https://github.com/firecracker-microvm/firecracker/blob/main/README.md
- Firecracker specification: https://github.com/firecracker-microvm/firecracker/blob/main/SPECIFICATION.md
- Firecracker snapshot support: https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md
- Firecracker jailer: https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md
- OpenNebula forum on Firecracker live migration: https://forum.opennebula.io/t/is-it-possible-to-live-migrate-firecracker-vms/8542
- Fly community on Firecracker live migration: https://community.fly.io/t/live-migration-using-firecracker/16323
- firecracker-containerd: https://github.com/firecracker-microvm/firecracker-containerd
- Weave Ignite (archived): https://github.com/weaveworks/ignite
- Kata Containers hypervisors: https://github.com/kata-containers/kata-containers/blob/main/docs/hypervisors.md
- Kata Containers architecture: https://github.com/kata-containers/kata-containers/blob/main/docs/design/architecture/README.md
- Kata Containers networking: https://github.com/kata-containers/kata-containers/blob/main/docs/design/architecture/networking.md
- Kata Containers limitations: https://github.com/kata-containers/kata-containers/blob/main/docs/Limitations.md
- Apple Containerization: https://github.com/apple/containerization
- libkrun: https://github.com/containers/libkrun
- containerd nerdbox: https://github.com/containerd/nerdbox
- Fly.io, Docker without Docker: https://fly.io/blog/docker-without-docker/
- Fly.io, Making Machines Move: https://fly.io/blog/machine-migrations/
- KubeVirt live migration: https://kubevirt.io/user-guide/compute/live_migration/
- KubeVirt disks and volumes: https://kubevirt.io/user-guide/storage/disks_and_volumes/
- Virtink: https://github.com/smartxworks/virtink
- Nomad QEMU driver: https://developer.hashicorp.com/nomad/docs/drivers/qemu
- QEMU post-copy migration: https://www.qemu.org/docs/master/devel/migration/postcopy.html
- QEMU microvm machine type: https://www.qemu.org/docs/master/system/i386/microvm.html
- virtio-fs: https://virtio-fs.gitlab.io/
- containerd EROFS snapshotter: https://containerd.io/docs/2.3/snapshotters/erofs/
- gVisor platforms: https://gvisor.dev/docs/architecture_guide/platforms/
- rust-vmm: https://github.com/rust-vmm/community
- Linux kernel, running nested guests: https://docs.kernel.org/virt/kvm/x86/running-nested-guests.html
- cloud-init NoCloud: https://docs.cloud-init.io/en/latest/reference/datasources/nocloud.html
- OpenStack thread on ISO as disk vs CD-ROM: https://lists.openstack.org/pipermail/openstack/2013-November/003495.html
- Apple, isNestedVirtualizationEnabled: https://developer.apple.com/documentation/virtualization/vzgenericplatformconfiguration/isnestedvirtualizationenabled
- GeekNews summary of macOS 15 nested virtualisation on M3: https://news.hada.io/topic?id=15318
- Lima 1.0 release notes: https://newreleases.io/project/github/lima-vm/lima/release/v1.0.0
- Cloud Hypervisor Windows support: https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/windows.md
- virtio-win drivers: https://github.com/virtio-win/virtio-win-pkg-scripts
- cloudbase-init: https://cloudbase.it/cloudbase-init/
- Windows Server pricing and licensing: https://www.microsoft.com/en-us/windows-server/pricing
- Volume Shadow Copy Service: https://learn.microsoft.com/en-us/windows-server/storage/file-server/volume-shadow-copy-service
- virt-v2v: https://libguestfs.org/virt-v2v.1.html
- Apple WWDC23 session 10007, Create seamless experiences with Virtualization: https://developer.apple.com/videos/play/wwdc2023/10007/
- AWS, nested virtualisation on virtual instances (February 2026): https://aws.amazon.com/about-aws/whats-new/2026/02/amazon-ec2-nested-virtualization-on-virtual
- AWS, more Intel platforms (June 2026): https://aws.amazon.com/about-aws/whats-new/2026/06/nested-virtualization-intel-us-gov-cloud/
- Google Cloud nested virtualisation: https://docs.cloud.google.com/compute/docs/instances/nested-virtualization/overview
- Azure Dasv5 series: https://docs.azure.cn/en-us/virtual-machines/sizes/general-purpose/dasv5-series
- Microsoft Q&A on Azure nested virtualisation sizes: https://learn.microsoft.com/en-us/answers/a/1164354
- GitHub changelog, KVM on 2-vCPU Linux runners: https://github.blog/changelog/2024-04-02-github-actions-hardware-accelerated-android-virtualization-now-available/
