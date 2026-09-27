# Reliaburger as the OS: an mkosi appliance on Ubuntu, netboot from relish, and bare-metal joins

*Research note, 26 September 2026. The repo facts come from reading `main` at `0a5dfc6`. External facts come from primary sources fetched today; the URLs are in the Sources section. **[unverified]** marks a claim nobody has tested or confirmed from a primary source. **[inference]** marks my own reading of code or docs.*

*Revised the same day with four follow-up questions: forking Talos (§2.10), Ubuntu versus Debian as the mkosi base (§3.1), whether mkosi ties us to the distro kernel (§3.2), and network boot shipped by us (§4.7).*

*Decided on 27 September 2026: the maintainer chose the own mkosi image on Ubuntu with netboot built into `relish`. §0, §5 and §6 are rewritten for that decision. §7 (weekly builds and OS updates), §8 (iterating in VMs on a Mac) and §9 (the Dell Wyse 3040 final stage) are new. The Kairos alternative written earlier the same day is condensed into §10.1, and Talos is parked (§2, §10.2).*

---

## 0. Summary

**Decision (maintainer, 27 Sep 2026):** build our own appliance image with mkosi on Ubuntu, and put the netboot server inside `relish`. Kairos was considered and not chosen (§10.1). Talos is parked, to revisit later (§10.2).

- **The image: our own mkosi build on Ubuntu 26.04 LTS** (§3.1), riding **Ubuntu's generic kernel**, which Canonical patches (§3.2). `bun` is the only service. There's no SSH and no package manager, and the image carries only the packages we list (the same list as the quickstart guest).
  - We sign the UKI and never build or sign kernels.
  - We build both x86_64 (the real hardware) and aarch64 (fast VMs on Apple silicon).
- **Netboot lives in `relish`** (`relish image serve`, §4.7):
  - a ProxyDHCP answer next to the home router's DHCP;
  - TFTP for a pinned iPXE, and HTTP for iPXE chains and UEFI HTTP Boot;
  - it serves only our signed release artefacts, from the laptop or from the first installed node.

  USB stays the fallback.
- **Weekly builds** (§7): GitHub Actions rebuilds the appliance weekly, and on demand, with the latest Canonical kernel and packages.
  - Outputs per architecture: a UKI, a verity `/usr` image, an installer UKI for netboot, an ISO and a raw disk.
  - Everything is signed with Ed25519 and published as its own release channel.
  - Nodes never update themselves. The cluster pins a target OS version, and bun's orchestrator rolls it out with **`systemd-sysupdate` A/B slots and systemd-boot boot counting**. If the new version doesn't come back healthy, the node falls back to the old slot on its own.
- **Iterate in VMs on a Mac, then test on real hardware** (§8, §9).
  - The maintainer has no KVM box, so iteration uses raw QEMU on the second Mac: aarch64 guests at native speed under HVF, plus an x86_64 smoke run under TCG emulation.
  - Clients netboot over a shared L2 network (socket_vmnet), with the netboot server in a small Lima VM.
  - The final stage is **ten Dell Wyse 3040 thin clients**: 2 GB RAM, 8 or 16 GB eMMC, UEFI PXE only. 2 GB is enough for bun plus a few small workloads, but only just. The image and the retention settings need an appliance profile.
- **Join flow: claim over the LAN** (§4.2 d). Seed mode on USB covers headless installs. Network boot never serves secrets.
- **Repo gaps that still block unattended joins** (§4.3):
  - G1: `master.key` is copied by hand;
  - G2: the time-boxed join window. The static half landed on `main` as `[security] operator_cidrs`;
  - G3/G4: address and name detection;
  - G5: no master-key rotation;
  - G6: no token list or revoke.
- **Spike:** about 9–11 engineer-days in five stages, the last on the Wyse 3040s. It's in [`2026-09-27-plan-appliance-spike.md`](2026-09-27-plan-appliance-spike.md). It **awaits maintainer approval**, and it needs the second Mac.

---

## 1. Problem statement

### What removing the OS solves

| Pain today | What a Reliaburger appliance image changes |
|---|---|
| **Host prerequisites.** The manual demands Linux 5.8+, cgroup v2, bpffs, rootful runc and eBPF. The guest image pins `runc uidmap btrfs-progs nftables iptables iproute2` (`scripts/release/guest-images.json`). Bun shells out to `ip`, `nft`, `iptables`, `tc`, `ss`, `mount`, `mkfs.ext4`, `fallocate`, `btrfs` and `runc` (grep of `Command::new` in `src/grill`, `src/firewall`, `src/bun/agent.rs`). | The image *is* the prerequisite list, tested together as one artefact. |
| **Drift.** Every hand-built node is a snowflake: kernel version, sysctls, nft backend, systemd unit edits. | One image digest per release across the whole fleet. `relish nodes` can show it. |
| **Patching.** Today the OS is the user's problem. Reliaburger self-upgrades only `bun` (a symlink swap plus `execv`, `src/upgrade/store.rs`). | Kernel, userland and `bun` ship as one signed A/B image. The existing rolling orchestrator drives it. |
| **SSH hardening and attack surface.** A general-purpose distro has SSH, a package manager and a login shell. | No SSH, no shell and no package manager by default. The `relish` API (mTLS plus bearer tokens) is the only management plane, which matches Talos's pitch. |
| **Reproducibility.** | mkosi supports `SourceDateEpoch=` and reproducible output. The build record lists every package version, as `build_guest_image.sh` already does. |
| **Time to cluster.** The whitepaper targets "bare metal to first deploy < 5 minutes" (`docs/whitepaper.md` §2), but that assumes a prepared Linux host. | "Five mini PCs to a working cluster in under an hour", including writing the USB stick. |

### What it doesn't solve

- **Hardware.** Firmware, Secure Boot keys, BIOS boot order, flaky NICs and dead disks are still the operator's problem.
- **Disaster recovery.** You still have to back up `master.key`, and `relish council recover` still exists. An immutable OS doesn't protect Raft state.
- **Debuggability.** Without a shell you need an escape hatch: a debug build, a console or a `relish node shell` API. Talos solves this with `talosctl` debug containers.
- **Multi-tenant host use.** People who want Reliaburger on hosts that also run other things keep the "install on your distro" path. The appliance is an additional delivery channel, not a replacement.
- **Laptops.** The quickstart stays on Lima.

---

## 2. How it could work with Talos

*Background, not pursued (maintainer decision, 27 Sep 2026): Talos is parked, to revisit later (§10.2). This section is kept as the research record; its spike and plan items are gone.*

### 2.1 What Talos is today

- **Current release:** v1.14.1 (15 Sep 2026), with Linux 6.18.x, runc 1.5.1 and containerd 2.3.x. Minor releases come about every 4 months, and three branches are maintained.
- **Architecture:** `machined` is PID 1 and there is no systemd. `apid` is the gRPC API on :50000 and `trustd` handles certificates. There's no SSH and no shell. The rootfs is a read-only squashfs and `/var` is XFS on the EPHEMERAL partition. There are two containerds: a system one for extensions and a CRI one for Kubernetes pods and Talos Containers.
- **Licences:** Talos is MPL-2.0. Omni and the Discovery Service are **BUSL-1.1**. Omni is free only for non-production and home-lab use, and converts to MPL-2.0 on 9 Sep 2030.
- **Ownership:** Yardi acquired Sidero Labs. On 14 Sep 2026 Sidero announced "native hypervisor and edge container support", with an alpha at TalosCon (15–16 Oct) and GA planned for December 2026.

### 2.2 Can Talos run without Kubernetes?

- **Until recently, no.** In 2022 smira wrote that "Talos unconditionally bootstraps and runs Kubernetes" (#6473). In May 2025 Sidero said standalone mode "isn't planned" (discussion #8343).
- **Since v1.14.0, experimentally.** PR #13892 ("feat: support experimental k8s-less and etcd-less mode", merged 4 Aug 2026) says: "If no K8s & etcd configs are present, continue running Talos as normal. **Only controlplane mode will be supported/tested.**"
  - The v1.14.0 release notes confirm "support for experimental k8s-less and etcd-less mode". I checked those notes myself.
  - It's generated with a hidden flag, `talosctl gen config --skip-k8s-etcd`.
  - The integration test checks that no kubelet or etcd runs, and that reboot, reset and upgrade work.
- **Consequence for us:** every Reliaburger node would be a Talos "controlplane" machine type in k8s-less mode. That's fine semantically, since we don't use Talos's control plane, but it's the supported path in name only. **[unverified]** whether worker-type k8s-less configs behave.

### 2.3 Running `bun` as an extension service

Talos offers three ways to run our code:

| Mechanism | Namespaces | Survives a `bun` restart? | Verdict |
|---|---|---|---|
| **Extension service, `runnerMode: container`** (the default) | Host network and IPC. **Private PID, mount and UTS.** All capabilities and devices. Runs on system containerd with `KillAll` on stop. | **No.** When the PID-namespace init exits, everything in that namespace dies, including runc containers and the process-owner helpers. `provision.rs` sets `KillMode=process` precisely so owners outlive Bun. | Unfit |
| **`ContainerConfig`** ("Talos Containers", new in v1.14) | CRI containerd, namespace `taloscontainers`. No host-PID option. Talos docs: "There are no user namespaces, so uid 0 is root on the host". | No, for the same PID-namespace reason. It also sits inside the `sandboxd` namespace when workload isolation is on **[inference]**. | Unfit |
| **Extension service, `runnerMode: host`** (new in v1.14, used by the official libvirtd extension) | Runs an absolute host path in machined's (root) namespaces, in cgroup `/system/extensions/<name>`. Stop sends SIGTERM and then SIGKILL **to the main PID only**, then deletes the cgroup. I verified the signalling in `process.go`. | **Probably yes.** Owners and containers that bun moves into `/sys/fs/cgroup/reliaburger/...` are outside the service cgroup **[inference, untested]**. libvirtd has the same requirement: QEMU survives a daemon restart. | **The only viable route** |

**Host-mode restrictions.** Validation in `services.go@v1.14.1`, which I checked:
- "container mounts are not supported in host runner mode"
- security options are not supported in host runner mode
- the entrypoint must be an absolute host path
- `preShutdown` hooks exist only in host mode

The agent report adds that `ExtensionServiceConfig` files are rejected in host mode. So config can't come through Talos's config-file mechanism. It has to live in `/var/lib/reliaburger` or be baked into the extension.

A sketch of the manifest. It is **[untested]**; the field names come from the v1.14.1 spec:

```yaml
# /usr/local/etc/containers/reliaburger.yaml (inside our system extension)
name: reliaburger
runnerMode: host
depends:
  - network: [addresses, connectivity, etcfiles]
  - time: true
restart: always
preShutdown:            # drain before node shutdown, like libvirtd's guest shutdown
  entrypoint: /usr/local/lib/reliaburger/bin/relish
  args: [node, drain, --local]
  timeout: 60s
container:
  entrypoint: /usr/local/lib/reliaburger/bin/bun-launcher   # execs /var/lib/reliaburger/bin/bun
  args: [--cluster, --runtime, runc, --config, /var/lib/reliaburger/node.toml, --listen, "0.0.0.0:9117"]
  environment:
    - PATH=/usr/local/lib/reliaburger/bin:/usr/bin:/usr/sbin:/bin:/sbin
```

### 2.4 bun's host requirements against Talos

| Requirement (source in repo) | Talos 1.14 | Action |
|---|---|---|
| Kernel ≥ 5.8, cgroup v2, `CONFIG_DEBUG_INFO_BTF` (`docs/design/discovery-onion.md` §2) | 6.18, cgroup v2 only, BTF=y | none |
| cgroup/connect4, connect6, sendmsg4, sendmsg6 attached to **root** cgroup `/sys/fs/cgroup` (`src/onion/ebpf/loader.rs`) | `CGROUP_BPF=y`. Host mode is in the root cgroup namespace **[inference]**. `unprivileged_bpf_disabled=1` doesn't matter for root. Secure Boot lockdown moved to `integrity` "for eBPF compatibility" (v1.14 notes). | test in spike |
| bpffs at `/sys/fs/bpf`, with pins under `/sys/fs/bpf/reliaburger-*` (`src/bin/bun.rs:1187`) | Talos always mounts bpffs | none |
| User namespaces mapping to host uid 2,000,000,000+ (`src/grill/userns.rs`) | **`user.max_user_namespaces=0`** by KSPP default (I checked `kspp.go`) | Set with `SysctlConfig` in the embedded machine config |
| `runc` on `PATH` (`src/grill/runc.rs:102`) | runc 1.5.1 is in the rootfs. **[unverified]** exact path. | Ship our own pinned runc in the extension so we don't couple to Talos's version |
| `ip`, `tc`, `ss`, `mount`, `umount`, `fallocate`, `sysctl`, `sh` | **Absent** from the rootfs (Dockerfile@v1.14.1, per the agent) | Ship static builds in the extension, or replace shell-outs with netlink and syscalls. That's worth doing anyway. |
| `nft`, `iptables`, `mkfs.ext4` | Present. `nft` was added in 1.12. **[unverified]** whether `iptables` uses the nft or legacy backend. | Ship our own anyway, for version control |
| Btrfs volumes (optional; loop ext4 fallback, `src/grill/volume.rs`) | `BTRFS_FS=m`, available only through the official `btrfs` extension. `/var` is XFS, so the fallback needs loop devices plus ext4: `BLK_DEV_LOOP=y`, `EXT4_FS=y`. | Use loop ext4, or add the btrfs extension plus a `UserVolumeConfig` |
| netem for `relish fault delay` (Z6.3); `ss -K` for Z6.2 | `NET_SCH_NETEM=y`, `INET_DIAG_DESTROY=y` | none |
| glibc binary (release builds are `*-unknown-linux-gnu`, `.github/workflows/build.yml`) | musl rootfs. A `glibc` extension exists. | Build `bun` for `*-linux-musl`. rustls/ring/aya make that plausible, but the Arrow/Parquet fork needs checking. |
| Writable state `/var/lib/reliaburger/{data,images,logs,metrics,volumes}` (`src/config/node.rs:493`) | `/var` persists across reboots and upgrades (preserve is always on since 1.8). Wiped on `talosctl reset`. | none |
| Own nftables tables `reliaburger`, `reliaburger_fw` (`src/firewall/rules.rs`) | Talos's optional ingress firewall is also nftables | Check chain priorities; don't enable both |
| Supervision: systemd `Restart=on-failure`, `KillMode=process` (`provision.rs::SERVICE`) | `restart: always`, main-PID signalling | Equivalent **[inference]** |

### 2.5 Coexistence with Talos's containerd

Bun drives runc directly, with its own state directory, its own cgroup subtree (`/sys/fs/cgroup/reliaburger/...`, `src/grill/cgroup.rs`) and its own netns names. Nothing in Talos should conflict **[inference]**.

In k8s-less mode, CRI containerd still runs because `ContainerConfig` needs it **[inference]**. That's wasted memory, but harmless.

One open question: does Talos sweep unknown root-level cgroups at boot or shutdown? At machined shutdown it kills only the `kubepods`, `podruntime`, `system` and `taloscontainers` cgroups (`startup/cgroups.go`, per the agent). Libvirt's `/machine` precedent in PR #14454 suggests root-level cgroups are left alone.

### 2.6 Self-upgrade vs Talos A/B

- **Talos upgrades are image-level.** `talosctl upgrade --image <installer>` writes the other slot, kexecs, and rolls back automatically if boot fails. **Extensions are part of the image**, so a new `bun` in the extension means a Talos upgrade and a reboot.
- **We have two choices:**
  1. **Defer to Talos:** bun's orchestrator (`src/upgrade/orchestrator.rs`) keeps the council-aware rolling order and health gates, but each node step becomes "call the Talos LifecycleService / upgrade API, then wait for rejoin". That needs Talos API credentials inside bun, meaning the Talos machine PKI with an `os:admin` role, which widens bun's blast radius.
  2. **Keep our symlink upgrade:** the extension ships a tiny launcher that execs `/var/lib/reliaburger/bin/bun`. The existing `UpgradeManager` supports `upgrades.binary_dir` (`src/upgrade/manager.rs`), so bun can live in `/var` with the existing Ed25519-signed swap. Host mode only requires an absolute entrypoint, and `/var` is executable. This gives two cadences: bun via our upgrade, and kernel/OS via Talos upgrades. It partly defeats immutability, but it's honest and proven. **[untested]**
- **Recommendation for any base:** option 2 for bun, with an image-level upgrade for the OS driven by the same orchestrator. This is the same design proposed for mkosi in §5.

### 2.7 Identity and PKI

- **Talos has its own PKI:** a machine CA for apid and trustd, with separate Kubernetes and etcd CAs. There's no documented way for an extension to reuse it.
- **Reliaburger keeps its own:** root CA, node CA and CSR-based join (`src/sesame/{ca,join}.rs`).
- **Cost:** the operator now holds Talos `secrets.yaml` and `talosconfig` *and* the Reliaburger admin token and `master.key`. To "remove the OS from the equation", `relish image create` would have to generate the Talos machine config (k8s-less, sysctls, embedded config via `imager --embedded-config-path`, added in v1.12). Then either:
  - `relish` keeps `talosconfig` hidden, or
  - we throw the Talos secrets away and accept that disk and network changes need a re-image.

  Both are awkward.

### 2.8 Logs and metrics

- `talosctl logs ext-reliaburger` shows bun's stdout. The operator would use `relish logs` and `relish wtf` anyway.
- Ketchup and Mayo read from bun's own paths, so nothing changes there.
- Host metrics (CPU and memory from `/proc`, `/sys`) work in host mode **[inference]**.

### 2.9 Verdict on Talos

**Where Talos fits:**
- It has the most hardened, best-maintained kernel and userland of any candidate, and ticks every kernel box we need.
- A/B upgrades, UKI and Secure Boot, the ISO/PXE/disk outputs and unattended installs (`UnattendedInstallConfig` with a CEL disk selector, new in 1.14) are all done for us.
- Maintenance mode with a console fingerprint (`apply-config --insecure --cert-fingerprint`) is exactly the claim UX we want.

**Where it doesn't:**
1. The k8s-less mode is experimental and labelled controlplane-only.
2. Host-mode services are three weeks old and not in the user docs.
3. We'd still own an `imager` pipeline, because custom extensions aren't on the public Image Factory.
4. There are two management planes and two PKIs.
5. We'd have to ship our own iproute2, util-linux and runc, and probably a musl build.
6. Each bun extension change means an OS upgrade unless we launch from `/var`.
7. Discovery Service and Omni are BUSL.
8. The vendor's roadmap now overlaps ours: native container scheduling for December 2026.

**Conclusion:** Talos is not a poor *technical* fit any more, but it's a poor *product* fit for "the OS disappears and you only see Reliaburger". Revisit a Talos extension as a secondary target in 2027, after k8s-less goes GA.

### 2.10 Could we fork Talos instead?

Most of §2.9's objections are about *upstream* Talos: the second API, the second PKI, the experimental mode, the missing tools. A fork could fix all of them. So why not take the best-engineered immutable OS on the list, rip Kubernetes out and put `bun` in?

**What we'd fork.** It isn't one repo; it's five, all MPL-2.0 (licence fields checked through the GitHub API):

| Repo | What it holds | Why we'd need it |
|---|---|---|
| `siderolabs/talos` | machined (PID 1), apid, trustd, installer, `talosctl`, the imager, the COSI controllers. Go 1.26.x (`go.mod` pins 1.26.5 at v1.14.1). Built by a `Dockerfile` under buildx that pulls one `PKG_*` image per package. | The OS itself |
| `siderolabs/pkgs` | About 100 packages built by `bldr`: kernel, linux-firmware, runc, containerd, util-linux, nftables, systemd-udevd, sd-boot | The kernel and every userland binary |
| `siderolabs/tools` | A musl bootstrap toolchain (gcc, llvm, perl, python3, meson, rustc) | Everything in `pkgs` builds against it |
| `siderolabs/bldr` | A BuildKit frontend that turns `Pkgfile` and `pkg.yaml` into LLB | The build system for `pkgs` and `tools` |
| `siderolabs/extensions` | System extensions | Only if we keep the extension mechanism |

**How deep the Kubernetes coupling runs.** Shallower than I expected. I counted files in v1.14.1:
- **Controllers** (`internal/app/machined/pkg/controllers`): 21 packages, about 261 non-test files. The Kubernetes-specific ones (`k8s` 33, `cri` 10, `etcd` 6, `kubeaccess` 4) come to about 53 files, roughly 20%. `secrets` is mixed: part Kubernetes PKI, part OS PKI. The biggest packages (`network` 60, `runtime` 43, `block` 20) have nothing to do with Kubernetes.
- **COSI resources** (`pkg/machinery/resources`): about 50 of roughly 230 resource types are Kubernetes or etcd (`k8s` 34, `cri` 8, `etcd` 5, and 6 of the 13 `secrets` types).
- **Services:** kubelet, etcd and CRI containerd are separate services. apid, trustd, machined, udevd and the system containerd are not Kubernetes-specific.
- **Machine config:** v1.14 is moving from one `v1alpha1` document to many typed documents (`SysctlConfig`, `KernelModuleConfig` and so on), so the `cluster.*` section is increasingly optional **[inference]**.
- The best evidence is upstream's own: PR #13892 made k8s-less mode work by *not starting* those services when their config is absent. That's a sign the coupling is at the service and controller layer, not woven through machined.

**What a fork would keep, strip and add** **[inference]**:
- **Keep:** machined and the COSI runtime, the network, block, storage and time controllers, the installer, A/B upgrades with rollback, maintenance mode, UKI and Secure Boot, `imager`, udevd.
- **Strip:** kubelet, etcd, the `k8s`/`etcd`/`kubeaccess`/`cri` controllers and resources, KubeSpan and SideroLink. CRI containerd goes (bun drives runc). The system containerd could go too if we drop extensions. trustd goes if we keep apid's PKI; apid is the hard one (below).
- **Add:** `bun` as a first-class machined service (not an extension), iproute2, util-linux and a static `sh` in the rootfs, `user.max_user_namespaces` raised in the KSPP defaults, and a Reliaburger config document.

**The catch is apid.** Talos is managed through apid's gRPC API with its own PKI. To get "only Reliaburger", we'd either:
1. keep apid and hide `talosconfig` inside `relish`, which is the two-PKI problem again, just in our own code; or
2. replace apid's role with bun's API, which means re-plumbing how upgrades, reset, maintenance mode and config apply are *triggered*. Those are the parts of Talos we wanted to reuse.

Either way we'd be maintaining Go code in a codebase whose conventions and review culture aren't ours, in a project that's otherwise Rust.

**Licence.** MPL-2.0 is file-level copyleft. Files we modify stay MPL-2.0 and we must publish their source. New files and the Rust `bun` binary can carry any licence, because MPL allows a "Larger Work" to combine them. That's compatible with anything we'd do. BUSL-1.1 covers only Omni and the Discovery Service, which we wouldn't use. We'd have to rename it: I found no published Sidero trademark policy, but Cisco's Talos trademark already forced Talos Systems to become Sidero Labs, so shipping a fork called "Talos" is asking for trouble **[inference]**.

**Maintenance burden.** This is what kills it:
- **Cadence.** Talos ships a minor every ~4 months (v1.8.0 Sep 2024, 1.9.0 Dec 2024, 1.10.0 Apr 2025, 1.11.0 Sep 2025, 1.12.0 Dec 2025, 1.13.0 Apr 2026, 1.14.0 Sep 2026) and patch releases every 1–2 weeks across three maintained branches.
- **Kernel.** Between 26 Mar and 26 Sep 2026, `pkgs` main had 18 kernel bumps (6.18.24 to 6.18.49, about three a month), about 10 kernel config or patch commits, and 5 Go bumps. A fork owns every one, plus its own module-signing key (Talos signs modules with an ephemeral per-build key, so any out-of-tree module has to be built in the same pipeline).
- **Rebase load.** 1,154 commits landed on `talos` main in the last 12 months. A fork that deletes 20% of the controllers conflicts with a steady share of them.
- **Who does it upstream.** 39 authors in 12 months, but concentrated: one maintainer wrote 545 of those commits, the next three 191, 141 and 77, and nobody else passed 20. `pkgs` is similar (149 of 285 commits from one person). The core is about 4–5 engineers inside a company of roughly 26–29 **[unverified: third-party headcount data]**. A fork needs a meaningful fraction of that, forever.
- **Precedent.** I found no maintained public derivative of Talos that drops Kubernetes, only personal forks and a Radxa board-port fork. In December 2024 a maintainer wrote that "Talos will always stay a single-purpose Kubernetes distribution". Upstream then reversed itself: Sidero's 14 September 2026 announcement (the same day as the Yardi acquisition) promises containers "directly on Talos Linux at edge and single-node sites", managed through Omni, alpha in October and GA in December. A fork would diverge from an upstream that is walking towards the same place from the other side.

**Compared with an mkosi image:**

| | Talos fork | mkosi image (Ubuntu 26.04) |
|---|---|---|
| Up-front effort | L–XL: strip, re-plumb apid, add bun as a service, rename, rebuild the CI for bldr | M–L: recipe, repart, sysupdate, CI |
| Ongoing effort | Rebase every ~4 months, ~3 kernel bumps a month, Go and musl toolchain bumps | Rebuild when Ubuntu publishes a kernel or USN; mkosi version bumps |
| Languages and tools we must know | Go, bldr, BuildKit LLB, musl, kernel config | mkosi config, systemd units, shell |
| Who fixes kernel CVEs | Us | Canonical |
| What we get for free | Polished installer, maintenance mode, A/B, network controllers | systemd-repart, sysupdate, networkd, timesyncd, apt's security feed |
| Main risk | Falling behind upstream; a product that competes with its upstream | Assembling the boot and upgrade pieces ourselves (Incus OS shows it's done) |

**Verdict: no fork.** Talos's k8s coupling is shallow enough that forking is *possible*, but the fork buys us machined's lifecycle plumbing at the cost of owning a kernel, a musl toolchain, a Go codebase and a rename, with no community to share the load. The pieces worth having (A/B, UKI, repart-style installs, maintenance mode) exist in systemd and mkosi, which we can consume without forking anything. If Talos's December edge-container GA turns out to host an arbitrary daemon cleanly, the right move is still to *consume* upstream as an extension (§2.3), not to fork it.

---

## 3. Comparison of candidate bases

Legend: ✅ good, ⚠️ workable with effort, ❌ poor.

| | **Talos 1.14** | **Kairos 4.3 (core, Debian/Ubuntu base)** | **Flatcar 4757 (stable)** | **Fedora CoreOS 44** | **Bottlerocket 1.66** | **Own mkosi image (Ubuntu 26.04 or Debian 13)** | **Buildroot / Alpine** | **Ubuntu 26.04 autoinstall** |
|---|---|---|---|---|---|---|---|---|
| **Fit with bun's needs** | ⚠️ All kernel needs met. Userns sysctl=0 by default. No iproute2 or util-linux. musl. | ✅ Stock distro kernel and apt packages. `/var/lib/reliaburger` needs `install.bind_mounts`. | ✅ BTF, userns, nft, netem, btrfs, iproute2, e2fsprogs, bpftool all in base. runc via default-enabled sysext. | ⚠️ All tools present, but **SELinux enforcing** with no `semanage`. Ships Docker/podman. | ❌ Bun would run as a "superpowered" host container. Every package has to be cross-built into a kit. | ✅ We choose every package: the `guest-images.json` list. Stock kernel config has everything (§3.2). | ⚠️ We own the kernel config. Alpine lts has BTF=y. | ✅ Same as today's guest image |
| **Immutability** | ✅ squashfs, API-only | ✅ A/B/recovery images | ✅ Read-only `/usr` A/B | ✅ ostree/bootc | ✅ dm-verity | ✅ verity `/usr`, UKI | ⚠️ Diskless mode, or our own layout | ❌ Mutable |
| **Upgrades** | ✅ A/B, auto-rollback, kexec | ✅ `kairos-agent upgrade --source oci:` | ✅ update_engine + Nebraska. Sysexts via sysupdate. | ✅ Zincati/rpm-ostree. Derived images unofficial. | ✅ TUF A/B, but we'd host the TUF repo | ✅ systemd-sysupdate A/B, verified | ⚠️ RAUC/SWUpdate (Buildroot) or our symlink | ⚠️ apt plus our symlink |
| **Image tooling** | ✅ `imager` offline: ISO, raw, PXE, UKI, embedded config | ✅ AuroraBoot: ISO, raw, netboot, ProxyDHCP "pixie", UKI | ⚠️ Ignition/Butane. **ISO has no UEFI boot** per docs. PXE good. | ✅ `coreos-installer iso customize` gives unattended USB (BIOS and UEFI) | ❌ No ISO or PXE on metal. Metal variants dropped after K8s 1.29. | ✅ mkosi v27: disk, UKI, ISO (new), sysext. `mkosi burn`. | ⚠️ Bespoke | ✅ ISO remaster (livefs-editor, xorriso), NoCloud CIDATA |
| **Auto-join support** | ⚠️ Embedded config, maintenance-mode apply, SideroLink (BUSL Omni) | ⚠️ cloud-config. QR/p2p exist but are k3s-oriented and experimental. | ⚠️ Ignition config URL | ⚠️ Ignition embedded in ISO | ⚠️ `user-data.toml` | Ours to build (seed partition plus claim) | Ours | cloud-init user-data |
| **Licence** | MPL-2.0 (Omni and discovery BUSL) | Apache-2.0 | Mostly Apache-2.0 plus GPL kernel **[unverified per component]** | Mixed FOSS | Apache-2.0/MIT | Ours, over Ubuntu or Debian packages | GPL/MIT mix | Mixed FOSS |
| **Maturity** | High overall. **k8s-less and host mode are brand new.** | CNCF Sandbox (Apr 2024). v4.3 monorepo (Sep 2026). Hadron init confusion. | CNCF Incubating (2024). 18-month LTS. | High. bootc transition still open. | High, but not for metal | mkosi mature. Our image would be new. **Incus OS ships this design (GA Nov 2025).** | Buildroot mature. Our image new. | Very high |
| **Effort for us** | M–L: extension, musl build, bundled tools, Talos config generation, `imager` CI | **S–M** | M: sysext trivial, installer UX weak on UEFI USB | M: SELinux labelling for runc and bun | L–XL | **M–L**: image recipe, repart, sysupdate, CI, Secure Boot | L–XL | **S** |
| **Key risks** | Experimental mode; vendor roadmap overlap; two PKIs; upgrade coupling | Upstream churn (monorepo, Hadron); persistence gotchas; Spectro-driven roadmap | UEFI ISO gap; `locksmithd` reboot coordination vs our council | SELinux denials; derived-image support | Metal abandoned | We own the rebuild cadence (the distro fixes the CVEs) and the Secure Boot key story | We own kernel and CVEs | Drift returns; no A/B; slow apt install |

**Reading the table:**
- **Ubuntu autoinstall** is the cheapest way to *prove the join UX*.
- **Kairos core** is the cheapest way to get *A/B plus ISO/PXE*.
- **Own mkosi image** is the best *long-term product*: full control, no second API, and a published precedent in Incus OS. Incus OS uses Debian 13, mkosi, UKI plus Secure Boot, sysupdate A/B, the payload as a sysext, a daemon-only API with no shell, and a `SEED_DATA` seed partition.
- **Decision (27 Sep 2026): own mkosi image on Ubuntu 26.04.** Kairos was evaluated in depth and not chosen (§10.1).
- A **Talos fork** isn't in the table because §2.10 rules it out: it would score like Talos on fit and tooling, but with XL effort and the kernel CVE feed on us.

### 3.1 Which distro under mkosi: Ubuntu 26.04 or Debian 13?

The first draft picked Debian 13 because Incus OS did. mkosi treats both as first-class (`Distribution=ubuntu` or `debian`; mkosi v27, 27 Aug 2026), so the recipe barely changes between them. The question is which archive we'd rather ride for five years.

| | **Ubuntu 26.04 LTS "Resolute Raccoon"** | **Debian 13 "trixie"** |
|---|---|---|
| Released | 23 Apr 2026. 26.04.1 on 27 Aug 2026. | 9 Aug 2025. 13.7 on 12 Sep 2026. |
| Kernel | 7.0 (GA). HWE kernels from 26.04.2, after 26.10 ships; opt-in on servers. | 6.12 LTS (6.12.107 today). trixie-backports carries 7.1.8. |
| Support | Standard to May 2031. Ubuntu Pro/ESM to May 2036, Legacy add-on to May 2041. | Security to 9 Aug 2028, then LTS to 30 Jun 2030. |
| Kernel updates | 4-week SRU plus a security respin until now. From 28 Sep 2026, a two-week cycle with overlapping cycles, so a kernel lands about weekly. Livepatch. | DSAs as needed, and point releases about every two months. |
| runc | **1.4.0**, binary `runc` from source `runc-app` in **main**. (A different `runc` source, 1.3.3, sits in universe: depend on the main one.) | **1.1.15**, the upstream-EOL line. The Debian security tracker lists CVE-2025-31133 as still vulnerable in trixie. We'd ship our own runc. |
| Rest of `guest-images.json` | All in main (btrfs-progs 6.17.1, nftables 1.1.6; uidmap, iptables, iproute2 **[unverified per package]**) | All in main (btrfs-progs 6.14, nftables 1.1.3, iproute2 6.15, iptables 1.8.11, shadow 4.17.4) |
| Other changes that touch us | cgroup v1 removed (fine: we need v2). rust-coreutils and sudo-rs are the defaults; bun's shell-outs are util-linux, iproute2 and nft, not coreutils **[inference]**. AppArmor restricts unprivileged userns by sysctl, which doesn't affect root. | None |
| Signed boot pieces | shim and GRUB only. No signed `systemd-boot` in resolute. | Debian-signed `systemd-boot-efi-amd64-signed` 257.13. |
| Reproducibility and pinning | `snapshot.ubuntu.com` back to 1 Mar 2023, retention "at least 2 years". No published reproducibility statistics. | ~96.9% of trixie/amd64 reproducible. `snapshot.debian.org` keeps everything. |
| mkosi precedent | Supported, less exercised. ParticleOS supports Arch, Fedora and Debian, not Ubuntu. | Incus OS (with Zabbly kernels), ParticleOS |
| Shared with the quickstart guest | **Yes.** The guest image is the Ubuntu 24.04 cloud image plus the pinned package list (`guest-images.json`, `build_guest_image.sh`). Moving the guest to 26.04 gives both one archive, one package list and one kernel source tree. | No: two distros to track, two sets of package names and versions. |

**What tips it.**
1. **runc.** Bun's containers run on it. On Debian we'd have to ship and patch our own runc in the first release, which is exactly the "own a component the distro should own" problem we're trying to avoid.
2. **One distro across both delivery channels.** Today's tested combination (bun plus the Ubuntu package set) is what CI builds into the guest image and what every quickstart runs. An Ubuntu appliance inherits that testing; a Debian one starts again. The kernel flavour will differ (the appliance needs the generic kernel and `linux-firmware` for real hardware; the guest keeps the cloud image's kernel **[unverified which flavour]**), but it's the same source tree and SRU stream.
3. **Support tail and kernel cadence.** Five years of standard support from April 2026 against three from August 2025, and a kernel update roughly every week.

**What we give up.** Debian's reproducibility record and indefinite snapshots, and a signed systemd-boot. Neither matters much for us:
- We'll sign our own UKI with our own db key (§5 Phase 3), so the distro's boot signatures are irrelevant. That's also the safer route now that the Microsoft UEFI CA 2011 expired on 27 June 2026: new shims are signed by the 2023 CA and won't boot on firmware that doesn't have it in db.
- We pin with `Snapshot=` and record every package version in the build record anyway. Two years of Ubuntu snapshot retention covers any release we'd still need to rebuild **[inference]**.

**Decision: Ubuntu 26.04 LTS.** Keep the recipe distro-neutral (mkosi `[Match] Distribution=` blocks for the package names), and build Debian 13 in the spike too, so switching back costs a config change. Move the quickstart guest to 26.04 in the same release so `guest-images.json` stays the single package list.

### 3.2 Does mkosi tie us to the distro's kernel?

No. mkosi doesn't care where the kernel comes from. It builds the UKI from whatever sits in `/usr/lib/modules/<version>/vmlinuz` in the image tree (mkosi(1), v27). The distro package is just the default way to get it there. The options, cheapest first:

| Kernel source | How in mkosi | Who owns security updates |
|---|---|---|
| **Distro GA kernel** (`linux-image-generic` on Ubuntu, `linux-image-amd64` on Debian) | `Packages=` | The distro |
| **Newer distro kernel** (Ubuntu HWE `linux-generic-hwe-26.04` from 26.04.2; Debian `trixie-backports`) | `Repositories=` plus `Packages=` | The distro (backports get less attention than stable **[inference]**) |
| **Vendor or third-party kernel .deb** (like Incus OS's Zabbly builds) | `Repositories=`, or `PackageDirectories=` for local .debs | The vendor |
| **Our own kernel .deb** | Build it in CI, feed it through `PackageDirectories=` or `LocalMirror=` | **Us** |
| **Built from source inside the mkosi build** | `BuildSources=` plus a `mkosi.build` script, or drop a prebuilt tree with `ExtraTrees=`. `mkosi-kernel` does this, but it's a kernel-hacking harness, not a shipping tool. | **Us** |

The module options are `KernelModules=`, `KernelInitrdModules=` and `KernelModulesInitrd=` (v26 removed `KernelModulesInclude=`/`Exclude=`). The UKI bundles kernel, initrd and command line into one PE binary, which `SecureBoot=`/`SecureBootKey=` sign as a unit. So any kernel change means a new UKI, and with sysupdate that means a new image version. That's what we want anyway: one digest per release.

**Signing is where custom kernels hurt.** Both distros build with `CONFIG_MODULE_SIG=y` and `SECURITY_LOCKDOWN_LSM=y`. Debian also sets `LOCK_DOWN_IN_EFI_SECURE_BOOT=y`, and Ubuntu enforces lockdown under Secure Boot through its LSM list. With Secure Boot on, only modules signed by a key built into the kernel load. Distro modules come pre-signed by the distro's key, so riding the distro kernel means **we sign only the UKI**. Our own kernel means we also own a module-signing key and must build every module in the same pipeline, which is what Talos does with an ephemeral per-build key.

**Would bun ever need a custom kernel?** I checked the actual config files:

| Option (why bun needs it) | Debian 13 (6.12) | Ubuntu 24.04 (6.8.0-146) | Ubuntu 26.04 (7.0.0-38) |
|---|---|---|---|
| `DEBUG_INFO_BTF` (aya/CO-RE eBPF) | y | y | y |
| `CGROUP_BPF`, `BPF_SYSCALL` (Onion's connect/sendmsg hooks) | y | y | y |
| `NET_SCH_NETEM` (`relish fault delay`) | m | m | m |
| `USER_NS` (userns containers) | y | y | y |
| `INET_DIAG_DESTROY` (`ss -K` for Z6.2) | y | y | y |
| `BTRFS_FS` (volumes) | m | m | m |
| `NF_TABLES` (firewall, service map) | m | m | m |
| `BLK_DEV_LOOP` (ext4 volume fallback) | m | y | y |

Every row passes, so no. `INET_DIAG_DESTROY` is the one kernels do sometimes lack, but the known gaps are OrbStack, Docker Desktop's LinuxKit and Azure Linux, not these distros. One difference to note: Ubuntu 7.0's default `CONFIG_LSM` doesn't include `bpf`, while Debian's does. Bun doesn't use BPF LSM today, so it doesn't matter yet.

**The trade-off in numbers.** The kernel became its own CVE Numbering Authority in February 2024 and is now the largest CNA by volume: roughly 5,500 CVEs in 2025, 8–9 a day **[unverified: secondary source for the exact total]**. Upstream stable ships about weekly. Owning a kernel means triaging that feed, rebuilding, testing on real hardware, and handling hardware enablement and firmware ourselves: a job Talos staffs with its own kernel pipeline. Riding Ubuntu's kernel, the job shrinks to "rebuild the image when a new kernel package lands", which a scheduled CI job can do.

**Decision:** ride the distro GA kernel, move to HWE only if hardware support demands it, sign only the UKI, and keep `BuildSources=` for debugging kernel issues, not for shipping.

---

## 4. The join UX: five old mini PCs to a cluster in under an hour

### 4.1 The mechanics we already have (from `src/sesame`, the quickstart and the manual)

- **Cluster creation.** `initialize_cluster()` (`src/sesame/init.rs`) generates the root CA, node CA, the first node's identity, the 32-byte master secret and the initial `SecurityState`. The quickstart runs this **on the laptop** (`src/relish/quickstart/security.rs::prepare`) and adds an Admin API token straight into the initial state. The admin token and CA stay in the laptop's context (`LocalContext`).
- **Join tokens** (`src/sesame/join.rs`, `types.rs::JoinToken`):
  - 32 random bytes, stored only as a SHA-256 hash.
  - TTL between 1 s and **1 h** (default 15 min).
  - **Single use**, consumed atomically in the Raft state machine (PKI5).
  - **Bound to one `node_id`** (M4).
  - Refused for ids in `crl.retired_nodes`.
  - Created only by an Admin user token, never the service principal (tests in `src/bun/api.rs`).
- **Join ceremony:**
  1. `GET /v1/cluster/ca` over trust-on-first-use.
  2. Verify the root CA against a pinned `sha256:` fingerprint *before* sending the token.
  3. Send a CSR to `POST /v1/cluster/join` over the CA-pinned client.
  4. Get back a leaf, 365-day lifetime, with **no private key in the bundle** (PKI4).
- **Master key.** Every clustered node must load `master_key_path` (`src/sesame/bootstrap.rs`). It unwraps CA keys and secrets and derives the internal service token (`src/sesame/token.rs::derive_service_token`). That `__system` principal is Admin-role for everything except user-management routes (`src/sesame/auth.rs::authorize_user`). The quickstart copies `master.key` to every VM (`runner.rs::node_files`). **Leaking it is close to leaking the cluster, and I found no rotation code.**
- **Firewall.** In cluster mode the perimeter (`src/firewall/rules.rs`) accepts loopback, member IPs, and `bootstrap_peers` (exact IPs, cluster ports plus 9117). Then it **drops** 9117 and the gossip, Raft and reporting ports. `admin_cidrs` exists in `PerimeterConfig` but no node-config field sets it.
- **Revocation.** `relish decommission-node` retires an identity permanently (`crl.retired_nodes`). CRL entries revoke serials. There's no list or revoke command for outstanding join tokens; they expire or get consumed.
- **Attestation.** `AttestationMode::Tpm` is a placeholder with no code behind it.

### 4.2 Design options

**(a) First node prints or shows a QR join blob, and you paste or scan it into each other node**
- *Flow:* node 1 boots and runs create. Its console shows a QR code of `{ca_fingerprint, endpoint, token}`. On each other machine the operator types or scans it at a console prompt.
- *Security:* the token is short-lived, but a node-bound token needs a node id chosen in advance, so you mint one per node. Typing a 64-hex token on a mini PC keyboard is miserable, and QR scanning needs a camera on the *joiner*, which is backwards. A variant is to scan node 1's QR with the laptop, which is really option (d).
- *Verdict:* poor ergonomics; there's no keyboard-free path.

**(b) A pre-baked per-cluster ISO with the CA fingerprint, a long-lived credential and a seed address or mDNS**
- *Flow:* `relish image create --cluster home` produces an ISO. Every machine booted from it joins automatically.
- *Security:* the credential must be **multi-use and long-lived**, which our tokens deliberately aren't. Anyone holding the ISO can enrol a machine. Worse, **enrolment would hand out `master.key`**, so a leaked ISO means full cluster compromise until the grant expires or is revoked. Decommissioning the rogue node doesn't help, because the master key is already gone and can't be rotated. CA pinning protects the *joiner* from a fake cluster, but it doesn't protect the *cluster* from a fake joiner.
- *Verdict:* acceptable only with approval-gated grants (see (d)) and master-key delivery that requires approval.

**(c) A generic signed ISO plus a small seed written by `relish`**
- *Flow:* one generic image per release, signed and cacheable. `relish image seed --cluster home --nodes 5 /dev/diskX` writes a `RELIABURGER-SEED` FAT partition or file containing:
  - cluster name, root CA fingerprint, seed endpoint(s)
  - N **node-bound, single-use** join tokens for `home-2` to `home-5`
  - network overrides and the disk policy
- A joiner tries tokens in order; a consumed one fails with `TokenConsumed`, so it moves on. That works with today's primitives.
- *Security:*
  - The stick holds secrets for ≤ 1 h (`MAX_JOIN_TOKEN_TTL`), each usable once, each for one fixed id.
  - A leaked stick after expiry is inert. Before expiry, an attacker can enrol at most the unused ids, and you'd see unknown machines in `relish nodes`.
  - The seed must **not** contain `master.key` (see gap G1 below).
  - The stick for node 1 is special: it carries the bootstrap bundle (master key, `security-bootstrap.json`, identity). The appliance must copy it to `/var` and **zero the seed partition** after first boot, so it has to be writable FAT, not the ISO.
- *Verdict:* **the right MVP** (§5 phase 1). It reuses existing security semantics almost unchanged. It's also the only mode that works for PXE and headless boxes.

**(d) Claim over the LAN (recommended v1)**
- *Prior art:* Talos maintenance mode with `--cert-fingerprint`, Proxmox automated install, Tailscale device approval, Kubernetes CSR approval.
- *Flow:*
  1. Every machine boots the **generic** image with no seed and enters `unclaimed`.
  2. It generates a self-signed claim key and serves a claim API on its own port. The console (tty1) shows IP, MAC, disks, a 6-word or 8-hex **claim fingerprint** and a QR code of `rb-claim://<ip>:<port>#<fingerprint>`.
  3. It announces `_reliaburger-unclaimed._tcp` over mDNS (no secrets in the TXT record).
  4. The operator runs `relish machines` on the laptop, which browses mDNS and lists unclaimed machines with their fingerprints.
  5. `relish machines claim <mac-or-ip> --create` pushes the bootstrap bundle to the first machine.
  6. `relish machines claim --all --cluster home` mints one **existing** node-bound, single-use join token per machine through the admin API and pushes `{token, node_id, ca_fingerprint, seed endpoint, advertise address}` to each over TLS pinned to that machine's claim fingerprint.
- *Security:*
  - No secret ever sits on removable media.
  - Every credential is single-use, short-lived and node-bound: today's semantics.
  - The residual risk is a LAN attacker impersonating an unclaimed machine (TOFU). It would receive one node-bound token and enrol a rogue node, which then receives `master.key`.
  - Mitigations: compare fingerprints (`--confirm` shows each one and asks, and the default is interactive), or scan the QR from the laptop's camera or phone. For headless homelabs, `--trust-lan` explicitly accepts TOFU.
  - Every enrolment shows up in `relish nodes`. A rogue node can be decommissioned, but because it saw the master key, **master-key rotation is a prerequisite for recovering properly** (gap G5).
- *Verdict:* the best UX, and no new credential types. It needs a claim server in bun, mDNS and the laptop commands.

### 4.3 Gaps to close first (from the code)

| # | Gap | Fix |
|---|---|---|
| G1 | `master.key` has to be copied by hand to every node | After a successful join, the joiner fetches the key over the CA-pinned, **node-cert-authenticated** mTLS connection. Either add an endpoint such as `GET /v1/cluster/master-key` (System route, node identity required, audited) or extend `JoinBundle`. Optionally seal it to the CSR's P-256 key with HPKE for defence in depth. |
| G2 | The perimeter drops 9117 for the laptop and for DHCP joiners. *(27 Sep: the static half landed on `main` as `[security] operator_cidrs`, API port only; the time-boxed join window is still open.)* | Wire `admin_cidrs` into `[security]` or `[firewall]` config. Add a **time-boxed `join_window` CIDR** (for example the LAN /24 for 60 minutes after `relish machines claim`) that opens only 9117. That exposes `/v1/cluster/ca` and `/v1/cluster/join`, which are token-gated anyway. |
| G3 | `advertise_address` defaults to 127.0.0.1 | Appliance mode detects the address from the default-route interface, and warns if DHCP changes it. Recommend DHCP reservations. |
| G4 | Node name defaults to `node-<gossip_port>` | Appliance names come from the cluster plus an ordinal assigned at claim time (`home-3`), recorded against DMI serial and MAC. |
| G5 | No master-key rotation | Needed so we can recover from a rogue enrolment or a lost stick. It's a bigger security-design item: re-wrap CA keys and secrets, re-derive the service token, and roll it out cluster-wide. |
| G6 | Join-token TTL is at most 1 h and there's no revoke | Keep the TTL for claim mode. Add `relish join-token list/revoke`. For seed mode, allow pre-seeded tokens in the initial `SecurityState` (like the admin token in `security.rs::prepare`). |

### 4.4 First-node bootstrap

- **Where the PKI is generated.** Generate it **on the laptop**, exactly as the quickstart does (`initialize_cluster` plus an Admin token). The laptop keeps:
  - the admin token and CA in a new *bare-metal* context (generalise `LocalContext`, which is quickstart-specific today);
  - a backup of `master.key`, with the prompt "store this in your password manager", since `relish council recover --from` needs it.
- **Handing it to node 1.** Node 1 receives the bundle through a claim (`--create`) or a create-seed. It writes the bundle to `/var/lib/reliaburger/secure` (0600) and starts bun with `bootstrap_path`. It then deletes `security-bootstrap.json` once Raft has committed it, which it doesn't need afterwards.
- **Alternative, not recommended:** node 1 generates the PKI itself and pairs with the laptop by console code. That keeps the master key on one box but loses the laptop-side backup and makes `relish` context creation a separate ceremony.

### 4.5 Disks, partitions and network

- **Disk selection.** Install to the only non-removable disk. If there are several, pick the largest SSD over HDD; the rule can be overridden in the seed or at the claim with a CEL-style selector like Talos's `UnattendedInstallConfig`. **Never wipe a disk that has partitions** unless the operator confirms (claim `--wipe`, or seed `wipe = true`).
- **Layout** (mkosi and `systemd-repart`):

  | Partition | Size |
  |---|---|
  | ESP | 1 GiB |
  | `usr-A`/`usr-B` (verity) | 2 × ~2 GiB |
  | Data | rest |

  On an 8 GB eMMC (the Wyse 3040) the `/usr` slots shrink to ~1.1 GiB each and the ESP to 512 MiB (§9.3).

  The data partition (`/var`) should be **btrfs**, since bun prefers Btrfs for volumes (`docs/design/agent-bun.md`). The loop-mounted ext4 fallback stays in place.
- **Network.** DHCP on all links by default. Static configuration goes in the seed or claim payload (address, gateway, DNS). mDNS is embedded in bun (for example the `mdns-sd` crate), because Flatcar and FCOS ship no Avahi and we don't want OS mDNS. Seeds may be `host:port`, and bun already accepts hostname seeds (`src/bin/bun.rs:634`). NTP comes from `systemd-timesyncd`, since certificates need sane clocks.
- **Console.** A small tty1 status screen shows state, IP, fingerprint, cluster, and a QR code. It's the only on-box UI.

### 4.6 What the operator does, and how long it takes

There are two install paths: USB sticks, or network boot from the laptop (§4.7). Steps 3 and 5–8 are the same on both.

| # | Step | USB | Network boot |
|---|---|---|---|
| 1 | `relish image download` fetches and verifies the signed x86_64 appliance image (~0.5–0.8 GB **[estimate]**) | 3–6 min | 3–6 min |
| 2 | USB: `relish image write /dev/diskN` (two sticks let two machines run at once). Network: `relish image serve` starts ProxyDHCP, TFTP and HTTP on the laptop, with one firewall prompt on macOS. | 3–4 min | 1 min |
| 3 | `relish cluster create home --bare-metal` generates the PKI, admin context and master-key backup prompt on the laptop | 1 min | 1 min |
| 4 | USB: for each machine, plug in, pick the stick in the boot menu (F11/F12), auto-install, reboot into `unclaimed`. About 5 min each; with 2 sticks, 3 rounds. Network: power all five on and pick "IPv4 PXE" or "IPv4 HTTP" once in each boot menu, and they install in parallel. Five × 0.6 GB is ~3 GB: about 30 s on wired gigabit, 3–5 min from a laptop on Wi-Fi. On either path, mini PCs often need Secure Boot turned off or our key enrolled the first time. | 15–20 min | 6–10 min |
| 5 | `relish machines` lists 5 unclaimed machines. Compare fingerprints. | 2 min | 2 min |
| 6 | `relish machines claim <first> --create`, wait for ready | 2–3 min | 2–3 min |
| 7 | `relish machines claim --all` enrols 4 joiners concurrently. The council grows (up to 7 voters). | 3–4 min | 3–4 min |
| 8 | `relish status`, `relish nodes`, then the five-minute tour (`docs/manual/08_five-minute-tour.md`) | 5–8 min | 5–8 min |
| | **Total** | **≈ 34–48 min** | **≈ 23–35 min** |

- Network boot saves its time in step 4: no stick shuffling, and all five machines install at once. Step 4 is also where it can go wrong: firmware that ignores ProxyDHCP offers, or a boot menu that hides network boot until you enable "Network Stack". When that happens, that one machine falls back to a stick and the other four carry on.
- Seed-mode installs (option c) stay USB-only on purpose, because network boot never serves secrets (§4.7).

### 4.7 Network boot: `relish image serve`

USB sticks are the slowest, most hands-on part of §4.6. Every mini PC in the hardware table below can boot from the network instead, and Kairos AuroraBoot's "pixie" mode shows one binary can make that painless. So network boot should be a first-class path, served by `relish` itself, not a "bring your own dnsmasq" appendix.

**How a machine finds us without touching the router: ProxyDHCP.**
1. The machine's firmware broadcasts a DHCPDISCOVER with option 60 set to `PXEClient:Arch:...` (legacy PXE) or `HTTPClient:Arch:...` (UEFI HTTP Boot).
2. The home router answers as usual with an IP address.
3. `relish image serve` answers the same broadcast on UDP 67 with a **proxy offer**. It assigns no address (`yiaddr` 0.0.0.0), echoes option 60 (`PXEClient` or `HTTPClient`), puts PXE vendor options in option 43, and names the boot file. For PXE that's a TFTP path plus `next-server`. For HTTP Boot, option 67 carries a full URL. Some PXE clients then ask again on UDP 4011.
4. The firmware takes its address from the router and its boot instructions from us.

So we never hand out an address, and we can't break the router's DHCP. The limits:
- Only PXE and HTTP Boot clients listen to us. Everything else on the LAN ignores proxy offers.
- EDK2's `HttpBootDxe` explicitly pairs a router's address-only offer with a proxy offer carrying a URI (`HttpOfferTypeProxyIpUri`). A dnsmasq user confirmed proxy HTTP Boot works (June 2022). Whether AMI and Insyde firmware on consumer mini PCs honour proxy offers for **HTTP** Boot is **[unverified]**. For legacy PXE it's decades-old behaviour.
- We have to share a broadcast domain with the machines. Guest Wi-Fi, "AP isolation" and VLANs break it.
- Two ProxyDHCP servers on one LAN race each other. `relish image serve` should listen for a few seconds first and refuse to start if it hears another proxy answering **[inference]**.

**TFTP or HTTP?** HTTP wherever we can.
- **UEFI HTTP Boot** (UEFI 2.5, 2015) fetches the boot file by URL. EDK2 accepts an EFI binary *or* an ISO, which it mounts as a RAM disk. A UKI works directly as the boot file (kraxel, July 2024). It's fast and has no practical size limit.
- **TFTP** uses 16-bit block numbers, so without rollover a file tops out at about 32 MB with 512-byte blocks, or 94 MB with 1468-byte blocks. A 100–300 MB UKI over firmware TFTP would be slow and fragile. So on PXE-only firmware we serve a small network boot program over TFTP (**iPXE**), and iPXE fetches everything else over HTTP.

**What we boot: the installer UKI, not a diskless system.**
- The boot file is a signed mkosi UKI whose initrd is the installer. Since systemd 258 (September 2025), systemd-stub records the URL it booted from in `LoaderDeviceURL`, and `rd.systemd.pull=` can fetch a disk image relative to that origin (the `bootorigin` flag). So the UKI's command line stays generic and signable. The initrd pulls the full image from the same server, verifies it (a signed image with `verify=`, never the `verify=no` in the ParticleOS example), writes it to the chosen disk (§4.5), and reboots into `unclaimed`.
- ParticleOS issue #80 (August 2025) lists the rough edges: per-site command lines fighting Secure Boot, networkd config missing from the initrd, and import timeouts on slow links. We'd hit the same ones, which is why this is in the spike.
- **Diskless mode**, booting the UKI and running from RAM every time, looks tempting ("no install at all"). But bun keeps Raft, images, logs, metrics and volumes under `/var/lib/reliaburger`, and a cluster whose nodes can't boot while the laptop is shut is a fragile cluster. It's not in v1.

**Secure Boot is the same problem as on USB.** Firmware with only Microsoft keys in db loads only Microsoft-signed binaries.
- Direct HTTP Boot of our UKI needs our db key enrolled, or Secure Boot off. That's exactly the USB situation.
- The PXE path gained an option in November 2025, when Microsoft signed iPXE's shim, so stock iPXE runs under Secure Boot. But iPXE still has to hand over to our UKI, which must verify against a key the firmware or shim trusts **[unverified in detail]**. So it's our key in db or MOK either way.
- Getting our own shim signed by Microsoft needs an EV certificate and months of review. That isn't worth it for v1.

**Can a Mac serve it?** Mostly.
- Since macOS Mojave, a non-root process can bind ports below 1024 on the wildcard address. So `relish` can listen on `0.0.0.0` UDP 67, 69 and 4011 and TCP 80 without `sudo`, and learn the incoming interface of each packet with `IP_RECVIF`. Apple's statement covers ports generally; UDP is **[unverified]**, and the spike must check it on a real Mac.
- With Internet Sharing on, launchd's `bootpd` owns UDP 67. `relish` should detect that and say so, rather than fail with "address in use".
- The macOS application firewall asks once whether to allow incoming connections to `relish`.
- The laptop has to stay awake (`relish` can hold a power assertion while serving) and on the same LAN segment. Wired is better: five machines pulling an image over Wi-Fi turns step 4's transfer from seconds into minutes.
- On Linux, `relish` needs `CAP_NET_BIND_SERVICE` or root for the same ports.

**Or serve from the first node.** Once node 1 is installed (from a stick, or netbooted from the laptop) and claimed, it can do the serving: `relish image serve --node home-1`. That suits the rest of the fleet better:
- node 1 is wired, always on, and already holds the verified image (the running OS, or the next version sysupdate staged);
- the laptop can go to sleep;
- adding a sixth machine or replacing a dead one later is "plug it in and network-boot", with no laptop or stick at all.

It's a bun subsystem that an admin opens through the API; it serves, then stops. Because ProxyDHCP never leases addresses, leaving it on by mistake can't break the LAN. It does mean the LAN keeps an installer on offer, so the window closes after 60 minutes by default.

**Security model.**
- **Anyone on the LAN can boot the image.** That's fine. It's the same generic, signed image anyone can download from our releases, and booting it gives you an `unclaimed` machine, not a cluster member. Joining still needs a claim (§4.2 d), and a claim needs the admin token on the laptop.
- **Never serve secrets.** No seed files, no join tokens, no `master.key`, no node config. The served tree holds only the release's public artefacts, which `relish` verifies against the release signature before serving. A network seed would put tokens on an unauthenticated broadcast protocol, so seed mode stays on USB.
- **Integrity comes from signatures, not transport.** Plain HTTP and TFTP have no integrity, and a rogue proxy on the LAN can win the race. Secure Boot verifies the UKI, and the UKI verifies the image it pulls. With Secure Boot off, a LAN attacker can serve a malicious installer, just as they could swap a USB stick. The claim fingerprint and `--confirm` (§4.2 d) still stop that machine from joining silently. HTTPS Boot with our CA enrolled in firmware (EDK2's `TlsCaCertificate`, Dell's certificate import) would close the gap, but it's rarely practical on consumer boards.
- **Optional allow-list.** `--mac` limits the proxy to known machines, so it doesn't offer an installer to a colleague's laptop that happens to try network boot first.

**How it fits the claim flow.** Network boot only replaces "get the generic image onto the disk". After the reboot the machine sits in `unclaimed` and announces itself over mDNS, exactly as after a USB install. `relish image serve` can watch its own boot log and the mDNS browse together and print "5 booted, 5 unclaimed", which tells the operator step 4 is done.

**Which hardware supports what.**

| Hardware | UEFI PXE (IPv4) | UEFI HTTP Boot |
|---|---|---|
| Dell OptiPlex Micro and other Dell client BIOS | Yes | Yes: "HTTP(s) Boot", on by default, URL from DHCP in Auto mode, TLS with certificate import |
| Lenovo ThinkCentre Tiny | Yes | Yes: Network Stack, "IPv4 HTTP Support" |
| HP EliteDesk/ProDesk Mini | Yes | HP drove the HTTP Boot spec; the per-model menu is **[unverified]** |
| Consumer AMI Aptio boxes (Beelink, Minisforum, older Intel NUCs) | Almost always, after enabling "Network Stack" | Sometimes; model-specific **[unverified]** |
| Anything without network boot, or Wi-Fi-only boxes | No | No |

So the baseline is **UEFI PXE, then iPXE over TFTP, then HTTP**. Direct HTTP Boot is the faster path when the firmware offers it, and the USB stick covers everything else.

**Prior art to borrow from.**
- **Kairos AuroraBoot:** one binary that pulls an OCI image, extracts the kernel, initrd and squashfs, and serves them with built-in ProxyDHCP (`start-pixie`, which embeds Pixiecore). It also ships a generic iPXE ISO for NICs that can't PXE boot. It's the closest match to what we want.
- **pixiecore** (danderson/netboot): ProxyDHCP, PXE, TFTP and HTTP in one Go binary. Its README says it's no longer actively developed, but it's a compact reference for the protocol dance.
- **Tinkerbell smee:** DHCP, TFTP and iPXE, with a proxy mode.
- **Talos:** the Image Factory serves an iPXE script (`chain ... https://pxe.factory.talos.dev/pxe/...`) and a signed `secureboot-uki.efi` for Secure Boot PXE. Its issue tracker shows iPXE plus Secure Boot trouble in practice.
- **Matchbox** (still maintained): an HTTP profile matcher that leaves DHCP to dnsmasq. Right for datacentres, too much machinery for us.
- **dnsmasq** with `dhcp-range=...,proxy`, `pxe-service` and `dhcp-vendorclass=...,HTTPClient`: the reference behaviour to test against.
- **netboot.xyz:** a menu of iPXE installers. Not what we need, but a good record of firmware quirks.

**Building it in Rust.**
- `dhcproto` (DHCPv4/v6 encode and decode, safe Rust) for the proxy.
- `async-tftp` for TFTP. It implements RFC 1350/2347/2348/2349/7440 with block rollover, but it runs on smol, so we'd need a small adapter or a thin TFTP of our own on tokio. We only ever send one small file.
- axum for HTTP, which we already use.
- A vendored, pinned iPXE binary with a small embedded script that chains to our HTTP URL.

---

## 5. Implementation plan

Effort is in engineer-weeks, including tests and book and manual updates per `CLAUDE.md`. *Revised on 27 Sep 2026 for the maintainer's decision: the Talos extension is gone, the weekly build moves into Phase 1, aarch64 moves into Phase 1 (VM iteration needs it), and Phase 3's update design is decided (§7.6).*

### Phase 0: spike (~9–11 days, awaiting approval)

See [`2026-09-27-plan-appliance-spike.md`](2026-09-27-plan-appliance-spike.md) and the summary in §6.

### Phase 1: MVP appliance, seed mode (~5–7 weeks)

**Security and bun:**
- G1: master-key delivery over mTLS after join, with tests for refusal without a node cert and refusal for retired nodes.
- G2: the time-boxed `join_window` on top of the `operator_cidrs` that `main` already has.
- G3/G4: advertise-address detection and appliance naming.
- G6: seeded join tokens in the initial `SecurityState`.

**New `bun --appliance` mode** (a module such as `src/appliance/`), as a pure state machine with unit tests:
- read the seed (`RELIABURGER-SEED` partition);
- lay out state under `/var/lib/reliaburger`;
- write `node.toml` from the seed, with the **appliance profile**: bounded `[metrics]` and `[logs]` `max_storage_mb`, shorter `[images] gc_retain_days` (§9.3);
- enrol by calling the existing join code in-process, not by shelling out to `relish join`;
- zero the create-seed after first boot;
- run the tty1 status screen.

**Self-upgrade:** a launcher in the image execs the newest verified `bun` in `/var/lib/reliaburger/bin` (`[upgrades] binary_dir`), falling back to the image's copy. The existing symlink swap stays unchanged.

**`relish`:**
- `relish cluster create --bare-metal` (generalised bare-metal context);
- `relish image download | write | seed`.

**Image (`image/mkosi.conf` and friends):**
- Ubuntu 26.04 LTS, **x86_64 and aarch64**, with the distro's generic kernel (§3.2) and a pruned `linux-firmware` allow-list (§9.3);
- `[Match]` blocks that keep Debian 13 buildable (§3.1);
- the package list shared with `guest-images.json`;
- the systemd unit reused from `provision.rs::SERVICE`;
- no SSH;
- an EROFS or squashfs `/usr` with dm-verity, `systemd-repart` for the data partition (Btrfs), and a UKI;
- an installer UKI and ISO output (§4.7, §7.2).

**CI:** the weekly `appliance.yml` workflow (§7): build both architectures, run a boot test under KVM on x86_64, sign, and publish to the OS channel.

**Tests:**
- unit tests for the seed parser and the appliance state machine;
- a **QEMU+OVMF boot test** on the x86_64 hosted runner (KVM is available there): install from the raw disk, reboot, assert `/v1/health`.

### Phase 2: claim over the LAN (~3–4 weeks)

- A claim server in appliance mode: self-signed key, console fingerprint and QR.
- mDNS announce and browse.
- `relish machines [claim]`, `relish join-token list/revoke`.
- A **virtual lab** that runs on a Mac (§8), not only on Linux with KVM. It:
  1. netboots five aarch64 VMs;
  2. claims them;
  3. runs `scripts/demo/tour.sh`;
  4. kills a VM and checks rescheduling;
  5. writes a qualification record under `docs/qualification/`.

### Phase 2b: network boot in `relish` (~2–3 weeks)

Unchanged from §4.7:
- **Installer UKI** that pulls and verifies the image from its boot origin, and **streams** `/usr` onto the disk rather than into RAM. The 2 GB Wyse can't hold a full image in tmpfs (§9.2).
- **`relish image serve`** on macOS and Linux: ProxyDHCP (`dhcproto`), a minimal TFTP for iPXE, HTTP via axum, a refusal when another proxy or `bootpd` is already answering, a `--mac` allow-list, and a time-boxed window. It serves only verified release artefacts.
- **`relish image serve --node <name>`**: the same server as a bun subsystem.
- **Vendored iPXE** for x86_64 and arm64 EFI, pinned, with an embedded chain script.
- **Tests:** the proxy-offer builder and property tests from §4.7; the Mac lab (§8.4 level 2); and the x86_64 CI lab on a bridge, which hosted runners allow with `sudo`.

### Phase 3: OS updates with A/B slots (~3–4 weeks)

The design is decided in §7.6:
- `systemd-sysupdate` transfers for the UKI (with boot-counting tries) and for the verity `/usr` into the inactive slot.
- A **boot-check unit** ordered before `boot-complete.target` that waits for bun to come back healthy and rejoin, so `systemd-bless-boot` only blesses a slot that actually works.
- `UpgradeManager` gains an `OsSlot` backend next to `Symlink`. `relish os list | upgrade | status` and a cluster-wide pinned `os.target_version` in Raft.
- Tests:
  - unit tests for the version, channel and pin logic;
  - a CI boot test that upgrades 2026.40 → 2026.41 in QEMU;
  - a deliberately broken image that must fall back without hands.

### Phase 4: polish (~2–3 weeks)

- Secure Boot (our db key, enrolment docs) and TPM2-sealed data-partition encryption, which later unlocks `AttestationMode::Tpm`. Both are optional: the Wyse fleet runs with Secure Boot off.
- G5, master-key rotation.
- Book chapter and manual "Bare metal" chapter.
- Physical qualification on the Wyse 3040 fleet (§9), repeated on each major release.

### Minimal first version

- Ubuntu 26.04 via mkosi, distro kernel, x86_64 and aarch64.
- Seed-mode join with node-bound tokens, plus G1 and G2.
- `relish image serve` netboot, because it's what turns ten Wyse boxes from ten USB sessions into ten power buttons.
- `bun` self-upgrade as today. Weekly images are published, but a node takes a new OS by reinstalling until Phase 3 lands.
- **Proof point:** five aarch64 VMs on a Mac and ten Wyse 3040s netboot, join and pass the tour.

---

## 6. Recommendation, risks and the spike

**Recommendation (maintainer decision, 27 Sep 2026):**
1. **Own mkosi image on Ubuntu 26.04 LTS**, following the Incus OS design, with `bun` as the only service. **Ride Ubuntu's generic kernel** (Canonical-patched, §3.2), sign only the UKI, and keep the recipe buildable on Debian 13. Build **x86_64 and aarch64**.
2. **Netboot is built into `relish`** (§4.7, Phase 2b): ProxyDHCP next to the home router, TFTP for iPXE, HTTP for everything else, serving only our signed artefacts, from the laptop or node 1. USB is the fallback.
3. **Weekly signed builds in GitHub Actions** (§7). Nodes move between them only when the operator pins a new version. The rollout goes through `systemd-sysupdate` A/B slots with boot counting, driven by bun's council-aware orchestrator.
4. Make the join flow **claim over the LAN (option d)**, with **seed mode (option c)** as the MVP and the headless path.
5. Fix gaps **G1 and G2** first. They block every bare-metal install, not just the appliance.
6. **Iterate on aarch64 VMs on a Mac and finish on real hardware** (§8, §9): ten Dell Wyse 3040s.
7. **Not chosen:** Kairos (§10.1). **Parked:** Talos, to revisit later (§10.2). **Don't fork Talos** (§2.10).

**Key risks:**
- **2 GB of RAM on the Wyse 3040.** It's workable for bun plus a few small workloads (§9.2), but a leak like the one the V02 soak found (#220, bun RSS near 1 GB) would take a node down. The appliance profile, zram, and a per-node memory alert in `relish wtf` are the mitigations.
- **8 GB eMMC.** Two `/usr` slots, an ESP and a Btrfs data partition fit only with a pruned image. Metrics, logs, images and Raft need bounded retention (§9.3). eMMC endurance is unknown.
- **x86_64 only under emulation on the Mac.** Most iteration happens on aarch64. x86-specific bugs (firmware, iPXE, the Realtek NIC, Cherry Trail quirks) only show up in the TCG smoke run, in CI, or on the Wyse itself.
- **Signing on a schedule.** A weekly job that signs unattended puts a signing key on a runner (§7.5). A separate OS key in a protected environment keeps that away from the release key.
- **Master-key blast radius.** Any enrolment path that hands out `master.key` turns a token leak into a cluster compromise. G1 plus approval-gated enrolment keep that bounded, and G5 (rotation) makes it recoverable.
- **Owning an OS.** Canonical fixes the CVEs, but we must rebuild and publish promptly. The weekly job and "skip when nothing changed" make that cheap. Nodes stay on their pinned version until someone moves them.
- **LAN realities.** Firmware that ignores ProxyDHCP (the Wyse's PXE entries disappear after a CMOS reset, §9.4), AP isolation, a second proxy on the LAN, and consumer routers without DHCP reservations.

**The spike** is in [`2026-09-27-plan-appliance-spike.md`](2026-09-27-plan-appliance-spike.md). **It awaits maintainer approval and nobody should start building.** It has five stages:
1. build the image in CI for both architectures;
2. netboot one aarch64 VM on the Mac through QEMU's built-in TFTP;
3. five aarch64 VMs on a shared L2 with ProxyDHCP, plus an x86_64 TCG smoke run;
4. an A/B OS update with a forced fallback;
5. ten Wyse 3040s netbooted from the Mac on a wired LAN.

That's about 9–11 engineer-days, with stages 2–5 on the second Mac.

**Exit criteria:**
- one CI run yields signed artefacts for both architectures;
- VMs and Wyse boxes install over PXE next to an unmodified DHCP server;
- the tour passes on the Wyse cluster within the RAM and eMMC budgets;
- a bad OS update falls back on its own.

---

## 7. Weekly appliance builds and how nodes update

*Added 27 Sep 2026. Repo facts come from `main` at `0eb6071d` (`.github/workflows/build.yml`, `docs/releasing.md`, and PR #215, which cut artefact retention to days and trimmed the caches after both went over budget). External facts were fetched the same day. The workflow is a plan, not code.*

### 7.1 What changes every week, and what doesn't

Canonical now ships a kernel roughly weekly (§3.1), plus USNs for the packages on our list. The appliance should pick those up without anyone having to remember. So a **scheduled workflow rebuilds the image from the live Ubuntu archive every week**, and anyone can start it by hand. Each build:
- installs the **latest released `bun` and `relish`**, not `main`. The OS channel moves on its own cadence, and bun already has its own upgrade path;
- records every package version, the kernel version and the bun version in a build record, the way `build_guest_image.sh` does today;
- **publishes nothing when nothing changed**. If the package manifest and bun version equal last week's, the run ends green without a release. That keeps storage flat in quiet weeks.

Version scheme: `YYYY.WW.N` (for example `2026.40.0`), which sorts, reads as a date, and can't be confused with bun's semver.

### 7.2 What gets built

Per architecture (`x86_64`, `aarch64`), from one mkosi configuration:

| Artefact | What it is | Used by |
|---|---|---|
| `reliaburger-os_<v>_<arch>.efi` | UKI: kernel, initrd, and a command line with `usrhash=` pinning the verity root of this build's `/usr` | sysupdate (ESP), CI boot test |
| `reliaburger-os_<v>_<arch>.usr.raw.zst` and `.usr-verity.raw.zst` | the EROFS/squashfs `/usr` and its dm-verity hash tree | sysupdate (inactive `/usr` slot), the installer |
| `reliaburger-os-installer_<v>_<arch>.efi` | the installer UKI for netboot: small initrd that partitions the disk and streams `/usr` onto it (§4.7, §9.2) | `relish image serve` (UEFI HTTP Boot, or iPXE chain) |
| `reliaburger-os_<v>_<arch>.iso` | the installer on bootable media | USB sticks |
| `reliaburger-os_<v>_<arch>.raw.zst` | a complete installed disk | `dd` installs, QEMU tests, `relish image write` |
| `ipxe-<arch>.efi` (x86_64 and arm64) | pinned, vendored iPXE with an embedded chain script | TFTP stage of PXE |
| `SHA256SUMS`, `os-<v>.json` + `.sig` | digests, the build record and our Ed25519 statement | bun, `relish image`, the installer |

The UKI's `usrhash=` ties the kernel and the exact `/usr` together. So with Secure Boot on, verifying the UKI verifies the whole OS. With Secure Boot off (the Wyse fleet), integrity comes from the Ed25519 check before anything is staged (§7.5).

### 7.3 Runners, privileges and time

- **Runners:** `ubuntu-24.04` for x86_64 and `ubuntu-24.04-arm` for aarch64. They're native, like `build-guest-images` today, because package scripts run for the image's own architecture.
- **mkosi without root tricks.** mkosi ships a setup action (`systemd/mkosi@<pinned sha>`) whose steps:
  - lift Ubuntu's AppArmor userns restrictions (`kernel.apparmor_restrict_unprivileged_userns=0`);
  - remove AppArmor;
  - open `/dev/kvm`.

  mkosi then **builds unprivileged in a user namespace**. `RepartOffline=yes` is its default: "`systemd-repart` will not use loopback devices to build disk images". The mkosi man page says only `RepartOffline=no` needs root and loop devices, which we don't need unless we use `Subvolumes=`. So **no privileged container and no loop devices on the hosted runner**. The `build-guest-images` job needs `sudo` for its loop mounts; this one shouldn't.
- **Tools tree.** The runner's Ubuntu 24.04 has systemd 255. The UKI (`ukify`), `systemd-repart` verity options and systemd 258's `rd.systemd.pull=` `bootorigin` (§4.7) need newer tools. `ToolsTree=yes` makes mkosi build its own, pinned tools image, so the host's versions stop mattering. The build time and size of the tools tree are **[unmeasured]**, and the spike records them.
- **Boot test.** Standard 2-vCPU Linux hosted runners expose KVM (GitHub changelog, 2 Apr 2024), and the mkosi action makes `/dev/kvm` usable. So the x86_64 job boots the raw disk in QEMU+OVMF and waits for bun's `/v1/health`. It also runs a netboot test on a bridge (`sudo ip link add ... type bridge`, dnsmasq address-only, the proxy from `relish image serve`). **The arm64 hosted runner has no `/dev/kvm`**, so aarch64 gets a build-only check in CI plus the Mac lab (§8).
- **Estimated time per run:** 15–30 minutes per architecture, including the tools tree **[estimate]**.

### 7.4 Caching, storage and retention

PR #215 shows where the limits are: artefacts had reached 287 GiB, and the caches were at 11.2 GB against the repo's 10 GB cache limit, which causes eviction. So:
- **No package cache.** A weekly build wants fresh packages anyway, and a cache of `.deb`s would just push the Rust target caches out. The tools tree is the only candidate for `actions/cache`, keyed by the mkosi version and the ISO week, and only if the spike shows it saves real time.
- **Actions artefacts only for handing over between jobs** (build → sign → publish): `retention-days: 1`, `compression-level: 0` (the payloads are zstd already).
- **Publish to GitHub Releases, as a separate channel.**
  - Pre-release tags `os-2026.40.0` named "Reliaburger OS 2026.40.0", which keeps them apart from `v0.x` bun releases.
  - Release assets don't count against Actions artefact storage. Each file must stay under GitHub's 2 GiB per-asset limit, which is why the raw disk is zstd-compressed.
  - A prune step keeps the **last 8 weekly releases** plus any release a bun release names as its tested OS.
- **Estimated size per week:** per architecture the UKI is 60–120 MB, `/usr` 400–700 MB compressed, and the ISO and raw disk about the same again. That's **~2–3 GB per week for both architectures, and 16–24 GB for eight kept weeks [estimate; the spike measures it]**.
- **The channel pointer:** `os-channel.json`, signed, listing the newest version per architecture with its digests. It's published through GitHub Pages next to `install.sh` (`static.yml` already deploys Pages) and mirrored as a release asset. Bun reads the pointer and never lists releases.

### 7.5 Signing and trust

`docs/releasing.md` already warns against release signing on a scheduled runner. Its soak section rejects putting the release key in a job that "also runs third-party actions", because "a binary signed that way is a genuine release-signed bun". The same logic applies here, and it's sharper, because an OS image runs as root on every node. So:
- **A separate OS signing key**, `RELIABURGER_OS_KEY` (Ed25519, PKCS#8, like the release key). Bun trusts it **only** for OS artefacts, from a second trust list beside `src/upgrade/keys.rs`. A leak can't sign a bun binary, and rotation doesn't touch the release identity.
- **Sign in a job that runs nothing third-party.** The build jobs (mkosi, its setup action) upload digests and artefacts. A separate `sign` job:
  - has only `actions/checkout`, `actions/download-artifact` and our own `scripts/release/package.py`-style signer;
  - runs in a GitHub **environment `os-weekly`** restricted to `main`, holding the secret;
  - signs a statement per artefact: version, architecture, asset name, SHA-256, kernel version, bun version, build record digest.
- **A human in the loop to start with.** The environment requires one reviewer, so each week's build waits for a click before it's signed. Drop the reviewer once the pipeline has run cleanly for a while. That's a maintainer decision, and it's in the open questions.
- **Operator countersignature stays optional**, as it is for bun (`[upgrades] external_signing_key`): a cluster can require its own signature on OS images too.
- **Secure Boot keys** (a db key for the UKI) are a separate, later concern (Phase 4). The Wyse fleet runs with Secure Boot off.

### 7.6 How a node discovers and applies an update

**Decision: `systemd-sysupdate` with A/B `/usr` slots, UKIs with boot counting, and bun's orchestrator in charge.**

1. **Discovery.** The council leader fetches `os-channel.json` once a day, and whenever someone runs `relish os list`, and checks its signature. A newer version than the cluster's pin raises an `os-update-available` notice in `relish wtf` and Brioche. **Nothing applies automatically.** An opt-in `[os] auto = "window"` policy with a maintenance window can come later.
2. **Pinning.** The cluster's target is one value in Raft, `os.target_version`, set by `relish os upgrade 2026.41.0`. Nodes report their running version in their state reports, and `relish nodes` shows it. New nodes installed later from an older image get upgraded to the pin before they take workloads **[design]**.
3. **Staging on each node, in the orchestrator's order** (workers first, council members one at a time, leader last; `src/upgrade/orchestrator.rs` already does this for bun):
   1. download the artefacts, from the channel or from a peer that already has them, to save a home uplink ten times over;
   2. verify the Ed25519 statement and the SHA-256 values, and write them into a root-only `/var/lib/reliaburger/os-staging/`;
   3. run `systemd-sysupdate --definitions=/usr/lib/reliaburger/sysupdate.d update 2026.41.0`, whose transfers use **`Type=regular-file` sources in that staging directory**:
      - `/usr` and its verity go to the inactive partition slot;
      - the UKI goes to the ESP as `reliaburger-os_2026.41.0+3.efi`, three tries under systemd-boot's automatic boot assessment;
      - `InstancesMax=2`.
4. **Switch.** Drain the node, then reboot. systemd-boot picks the newest UKI, and its `usrhash=` selects the matching `/usr`.
5. **Health gate.** `reliaburger-boot-check.service` is ordered `Before=boot-complete.target`. It waits (with a timeout) for bun to be healthy and back in the cluster. Only then does `systemd-bless-boot` mark the entry good.
6. **Fallback.** If bun doesn't come back, the unit fails and the node reboots. After three failed tries, systemd-boot falls back to the previous UKI, which points at the previous `/usr` slot. The orchestrator sees the node rejoin on the old version, marks the rollout failed and stops it.
7. **bun versus the OS.** The launcher uses the newest verified bun in `binary_dir`, or the image's own copy if that's newer. Neither update path can downgrade bun.

**Why this design and not another:**

| Option | Why not (or why) |
|---|---|
| **`systemd-sysupdate` + UKI + boot counting** (chosen) | Part of systemd, which the image already runs. Atomic per partition. Boot assessment is built into systemd-boot. It's what Incus OS and ParticleOS ship. Only `/usr` is duplicated (2 × ~1 GB), which matters on an 8 GB eMMC (§9.3). |
| sysupdate with `url-file` sources | Would fetch straight from the channel, but sysupdate verifies downloads only with GPG (`SHA256SUMS.gpg` against a keyring). We'd run a second signature scheme next to Ed25519. With `regular-file` sources "no integrity or authentication verification is done", so bun verifies first and sysupdate only copies. |
| Whole-disk A/B images (Kairos-style) | Duplicates everything, including space the data partition needs; loop-mounted images. |
| RAUC or SWUpdate | Another daemon and bundle format beside systemd, for no feature we lack. |
| OSTree or bootc | A different OS model (Fedora-centric), and a large tool for an image that's only a few packages. |
| `apt upgrade` in place | No rollback, and drift comes back. That's the problem §1 set out to remove. |

---

## 8. Iterating in VMs on a Mac

*Added 27 Sep 2026. The maintainer has no KVM machine. Iteration has to run on macOS, on the second Mac that's free after the release, **not on the Mac running the soak**. Commands are sketches, and anything marked **[unverified]** hasn't been run.*

### 8.1 What Lima can and can't do here

- **Lima's VZ backend** (the quickstart's default) can't network-boot: Apple's Virtualization framework EFI loader has no PXE or HTTP Boot, and Lima's docs don't mention netboot at all.
- **Lima's QEMU backend** boots a disk image and waits for its guest agent and cloud-init. A netbooting machine has neither, so Lima would time out. **So the netboot *clients* are plain `qemu-system-*` processes.**
- **Lima is still useful for the server side.** A small Ubuntu VM on the same L2 network runs the netboot server: `relish image serve` (Linux build) once it exists, or `dnsmasq` in proxy mode as a stand-in during the spike. It can also build images locally with mkosi if CI is too slow a loop.

### 8.2 Architectures

- **Apple silicon runs aarch64 guests under HVF** at near-native speed, so everyday iteration happens on aarch64. That's why the image and CI build both architectures (§7.2).
- **x86_64 guests run only under TCG emulation**, which is much slower **[estimate: several times slower to boot, and an install could take tens of minutes]**. Use x86_64 for a **smoke run per milestone**. Pick `-cpu Westmere`, which has SSE4.2 and AES-NI but no AVX, like the Wyse's Atom **[inference]**, so an accidental AVX dependency shows up before the hardware stage.
- The real target, the Wyse 3040, is x86_64. It's the final stage (§9).

### 8.3 Firmware

Homebrew's QEMU (11.1.1 today) ships EDK2 builds in `$(brew --prefix)/share/qemu/`, for example `edk2-aarch64-code.fd`, `edk2-aarch64-vars.fd` and `edk2-x86_64-code.fd`. QEMU's `roms/edk2-build.config` builds all of them with `NETWORK_HTTP_BOOT_ENABLE`, `NETWORK_IP6_ENABLE`, `NETWORK_TLS_ENABLE` and `NETWORK_ALLOW_HTTP_CONNECTIONS`. So **both architectures get UEFI PXE and UEFI HTTP Boot on virtio-net** without building any firmware. QEMU no longer ships an x86_64 vars template. For x86_64, either copy `OVMF_CODE_4M.fd` and `OVMF_VARS_4M.fd` out of Ubuntu's `ovmf` package (from the Lima server VM), or try `-bios edk2-x86_64-code.fd` for throwaway runs **[unverified]**.

### 8.4 Networks: three levels

**Level 1: one VM, no root, no ProxyDHCP.** QEMU's user-mode network has a built-in DHCP and TFTP server, and `bootfile=` names the PXE boot file. That's enough to test the iPXE → installer UKI → stream `/usr` → reboot loop, with the Mac serving HTTP on the slirp host alias `10.0.2.2`:

```sh
# On the second Mac. Artefacts from a CI run in ./art; iPXE and its script in ./tftp.
Q="$(brew --prefix)/share/qemu"
cp "$Q/edk2-aarch64-vars.fd" vars-c1.fd
qemu-img create -f qcow2 c1.qcow2 8G            # the size of the smallest Wyse eMMC
(cd art && python3 -m http.server 8080) &       # stands in for relish's HTTP side
qemu-system-aarch64 -machine virt -accel hvf -cpu host -smp 4 -m 2048 \
  -drive if=pflash,format=raw,readonly=on,file="$Q/edk2-aarch64-code.fd" \
  -drive if=pflash,format=raw,file=vars-c1.fd \
  -device virtio-net-pci,netdev=n0,mac=52:54:00:00:01:01,bootindex=1 \
  -netdev user,id=n0,tftp="$PWD/tftp",bootfile=ipxe-arm64.efi \
  -drive if=virtio,file=c1.qcow2 -nographic
# tftp/boot.ipxe (embedded in or chained from ipxe-arm64.efi):
#   #!ipxe
#   dhcp
#   chain http://10.0.2.2:8080/reliaburger-os-installer_2026.40.0_aarch64.efi
```

`-m 2048` mimics the Wyse's RAM, so an installer that buffers the image in RAM fails here first. UEFI **HTTP Boot** through slirp probably won't work, because slirp's DHCP doesn't send the `HTTPClient` vendor class EDK2 looks for **[unverified]**. Level 1 is PXE only.

**Level 2: a shared L2 network with a real ProxyDHCP (the main lab).** `socket_vmnet` (Homebrew 1.2.2, or Lima's recommended `/opt/socket_vmnet` install) runs as root once and gives unprivileged QEMU processes a vmnet "shared" network: 192.168.105.0/24, gateway `192.168.105.1`, addresses from macOS's `bootpd`.
- **That `bootpd` plays the home router:** it hands out addresses and no boot options, which is exactly what a real router does.
- **The netboot server can't run on the Mac host in this mode**, because `bootpd` already owns UDP 67 there (the same clash §4.7 notes for Internet Sharing). So it runs in a **Lima VM on the same network**.

```sh
# Once, on the second Mac (asks for sudo).
brew install qemu socket_vmnet lima
sudo brew services start socket_vmnet      # shared mode, socket at $(brew --prefix)/var/run/socket_vmnet

# The server VM: Ubuntu 26.04 on the shared network.
cat > rb-lan.yaml <<'EOF'
vmType: qemu            # vz with socket_vmnet networks: [unverified], qemu is the safe choice
images:
  - location: "https://cloud-images.ubuntu.com/releases/resolute/release/ubuntu-26.04-server-cloudimg-arm64.img"
    arch: "aarch64"
networks:
  - socket: "/opt/homebrew/var/run/socket_vmnet"
EOF
limactl start --name rb-lan rb-lan.yaml     # [unverified: exact image URL and networks.socket key]

# Inside rb-lan, until relish image serve exists: dnsmasq as a pure ProxyDHCP + TFTP.
sudo dnsmasq --no-daemon --port=0 --interface=lima0 \
  --dhcp-range=192.168.105.0,proxy --enable-tftp --tftp-root=/srv/tftp \
  --dhcp-userclass=set:ipxe,iPXE \
  --pxe-service=tag:!ipxe,ARM64_EFI,"Reliaburger",ipxe-arm64.efi \
  --pxe-service=tag:!ipxe,X86-64_EFI,"Reliaburger",ipxe-x86_64.efi \
  --pxe-service=tag:ipxe,ARM64_EFI,"Reliaburger",http://192.168.105.2:8080/boot.ipxe

# Each client, on the Mac: socket_vmnet_client passes the connected socket as fd 3.
"$(brew --prefix)/opt/socket_vmnet/bin/socket_vmnet_client" \
  "$(brew --prefix)/var/run/socket_vmnet" \
  qemu-system-aarch64 -machine virt -accel hvf -cpu host -smp 4 -m 2048 \
    -drive if=pflash,format=raw,readonly=on,file="$Q/edk2-aarch64-code.fd" \
    -drive if=pflash,format=raw,file=vars-c2.fd \
    -device virtio-net-pci,netdev=n0,mac=52:54:00:00:01:02,bootindex=1 \
    -netdev socket,id=n0,fd=3 \
    -drive if=virtio,file=c2.qcow2 -nographic
```

Five of those, with different MACs and disks, is the claim lab from Phase 2. The same `socket_vmnet_client` line with `qemu-system-x86_64 -machine q35 -accel tcg -cpu Westmere` is the x86_64 smoke run. The dnsmasq flags, CSA names and interface name are **[unverified sketch]**, and the spike fixes them.

**Level 2b: no root at all.** QEMU's `-netdev dgram` joins processes into one L2 "hub" over UDP multicast:

```sh
-device virtio-net-pci,netdev=n0,mac=52:54:00:00:02:01 \
-netdev dgram,id=n0,remote.type=inet,remote.host=230.0.0.1,remote.port=1234
```

There's no DHCP on that hub, so one extra VM plays the router (dnsmasq handing out addresses only), and another runs the netboot server. Multicast over macOS loopback for this purpose is **[unverified]**. Use it only if socket_vmnet's root step is unwelcome.

**Level 3: bridged to the real LAN.** This is also the product topology. `relish image serve` runs **natively on macOS**, and the Wyse boxes (or VMs bridged with socket_vmnet `--vmnet-mode=bridged --vmnet-interface=en0`) sit on the wired LAN behind the home router. That's where the macOS questions from §4.7 get answered: non-root UDP 67/69/4011, the firewall prompt, and whether `bootpd` is running. Wi-Fi interfaces generally can't be bridged, so use Ethernet on the Mac **[unverified for vmnet]**.

### 8.5 What needs the second Mac

Everything in §8: levels 1–3, the x86_64 TCG smoke run, and serving the Wyse fleet. CI (§7) builds all the artefacts, so the second Mac only runs QEMU, Lima and `relish`. The soak Mac isn't touched.

---

## 9. Final hardware: ten Dell Wyse 3040 thin clients

*Added 27 Sep 2026. Hardware facts come from the Debian wiki, Parkytowers' 3040 pages and Dell's 3040 user guide, fetched that day. Repo numbers come from `main`: the V02 12-hour soak record, the binary-size record and `src/config/node.rs`.*

### 9.1 The machine

| | Dell Wyse 3040 (N10D) |
|---|---|
| CPU | Intel Atom x5-Z8350 ("Cherry Trail", Airmont), 4 cores, 1.44 GHz, x86_64, fanless |
| RAM | **2 GB DDR3L, soldered**, single channel. Not upgradeable. |
| Storage | **8 or 16 GB eMMC, soldered**. Linux sees `/dev/mmcblk0`, plus `mmcblk0boot0`, `mmcblk0boot1` and `mmcblk0rpmb`. |
| Network | Realtek RTL8111/8168 gigabit (`r8169`, needs `rtl_nic` firmware). Wi-Fi only via an SDIO M.2 card or a USB dongle. |
| Firmware | UEFI only once switched from the factory CSM mode ("it is not possible to reactivate the CSM"). F2 setup, F12 boot menu. Latest BIOS 1.2.5. Default BIOS password "Fireport". Secure Boot off by default (and it stays off for us). |
| Power | 5 V or 12 V barrel supplies, depending on the batch. Use the one each unit shipped with. |

### 9.2 Is 2 GB of RAM enough?

**Yes for the final test and small workloads, and only just.** The budget, per node:

| Consumer | Estimate | Source |
|---|---|---|
| Kernel, systemd, journald, networkd, timesyncd, udev | 150–250 MB | **[estimate]**, measured in the spike |
| bun | **200–330 MB** at the start of a run; 470–780 MB "warm" later in the V02 soak, with peaks near 1 GB **before** the metrics-collector leak fix (#220) | `docs/qualification/2026-09-26-v02-soak-12h.md` (3 nodes, `rss_kb`); post-#220 numbers **[unmeasured]** |
| bun's `reserved_memory` default | 512 MiB | `src/config/node.rs` |
| Page cache for images, Btrfs metadata | whatever's left | |
| **Left for workloads** | **~1.0–1.3 GB** | |

Some consequences:
- A tour-sized load fits, and so does Redis with a small dataset or a couple of small HTTP services. Anything JVM-sized doesn't. For scale, the tour transcript's frontend app reports `process_resident_memory_bytes` of 123.3M across its three replicas (the transcript doesn't say whether that's a sum or a mean).
- **Council members carry more**: Raft, the metrics rollup and the reporting tree. On ten nodes, use 3 or 5 voters and let the rest be workers.
- **Mitigations:**
  - zram swap (`systemd-zram-generator`, about half of RAM), which trades CPU the Atom has for RAM it hasn't;
  - the appliance profile (§9.3);
  - a `relish wtf` memory alert per node, which already exists as `memory_high`.
- **The installer must stream.** A netbooted installer runs from RAM. If it downloaded a 0.5–1 GB image into tmpfs before writing it, it would compete with the kernel and initrd for 2 GB. So the installer writes `/usr` straight from HTTP onto the eMMC partition. `systemd-sysupdate` with a `url-file` source into a partition target does exactly that, or `systemd-pull raw` does it to a file. Verification then covers the written partition before it's marked bootable **[design; verify in the spike]**. A diskless, run-from-RAM mode is out of the question at 2 GB.
- **What the spike measures:** idle and tour-loaded `MemAvailable` per node, bun RSS over a few hours, and whether zram gets used.

### 9.3 Is 8 GB of eMMC enough, and will it wear out?

**Disk selection.** The installer must pick `/dev/mmcblk0`: not removable, the largest disk. It must never touch `mmcblk0boot0`, `mmcblk0boot1` or `mmcblk0rpmb`, which are tiny hardware partitions of the same chip. §4.5's "largest non-removable disk" rule needs an explicit exclusion for those names. Some firmware also forgets NVRAM boot entries, so the installer writes both a boot entry and the removable-media fallback `\EFI\BOOT\BOOTX64.EFI` (the Debian wiki's install note says the same).

**Layout on an 8 GB part** (about 7.3 GiB usable **[unverified]**):

| Partition | Size |
|---|---|
| ESP (two or three UKIs of 60–120 MB each) | 512 MiB |
| `usr-A` and `usr-B`, each with verity | 2 × ~1.1 GiB |
| `RB_DATA` (Btrfs): Raft, images, logs, metrics, volumes | **~4.5 GiB** |

That fits only if `/usr` stays **under ~1 GiB compressed**. The biggest risk is `linux-firmware`, a very large package **[unverified size on 26.04]**. So the image keeps an allow-list of firmware:
- `rtl_nic` and `i915` for the Wyse;
- `intel` for the Atom's audio and ISP, if needed;
- nothing for Wi-Fi by default.

mkosi's `RemoveFiles=` (or a postinst script) prunes the rest, and drops docs, man pages and locales. The 16 GB units have plenty of room.

**What fills `RB_DATA`**, from the V02 soak's resource table (three nodes, 12 hours):
- the data directory: 10–780 MB, mostly Raft and the stores;
- images: 100–420 MB;
- logs and metrics: about 7–11 MB each;
- volumes: under 30 MB.

4.5 GiB covers that with room for a few small images, **provided retention is bounded**. The appliance profile sets:
- `[metrics] max_storage_mb` and `[logs] max_storage_mb` (0, meaning unlimited, today) to around 256 each;
- `[images] gc_retain_days` from 7 to 1 or 2;
- `[upgrades] retain_versions` to 1 old bun;
- journald to `SystemMaxUse=32M`.

**Wear.** Cheap eMMC has an endurance of the order of a few hundred times its capacity in writes **[unverified; vendor-specific]**. The write sources are Raft's log and snapshots, metrics, logs, image pulls and journald. Btrfs mount options for the data partition: `noatime,compress=zstd:1` (compression cuts bytes written), with the kernel's default async discard. The spike records bytes written per day from `/sys/block/mmcblk0/stat` on an idle node and a busy one. If the numbers point at a lifetime under a few years, lengthen `[metrics] collection_interval_secs` (10 s by default) and move logs to RAM with periodic export.

### 9.4 Network boot on the 3040

- The firmware offers **UEFI PXE through the Realtek NIC**, with a boot entry named along the lines of "UEFI: IP4 Realtek PCIe GBE Family Controller" (IPv6 too).
- Those entries **can disappear after a CMOS reset** until the network boot option is re-enabled in setup. The exact menu names aren't confirmed yet **[unverified]**, and the spike will write them down.
- I found **no evidence of UEFI HTTP Boot** on the 3040, so plan for **PXE → iPXE over TFTP → HTTP**. That's exactly the baseline in §4.7.
- **BIOS to-do per unit:**
  - update to 1.2.5;
  - UEFI mode with CSM off;
  - network boot enabled and PXE first in the order (or F12 once);
  - Secure Boot off (the default);
  - optionally, power on after AC loss, so a power cut doesn't leave the cluster off.

  Ten units makes this an hour of keyboard work on its own. Dell's BIOS settings can also be exported and imported with Dell's tools on some models **[unverified for the 3040]**.
- **Bandwidth:** ten installs at 0.5–1 GB each is 5–10 GB. That's 1–2 minutes over wired gigabit from the Mac, and far longer over Wi-Fi. The Mac goes on Ethernet.

### 9.5 CPU and our eBPF needs

- The Z8350 is plain x86_64 with SSE4.2, AES-NI and VT-x, and **no AVX** **[from Intel ARK memory, unverified]**. Our release binaries target the default `x86_64-unknown-linux-gnu` baseline: `main` sets no `target-cpu` in `.cargo` or the workflows. So they don't assume AVX, and the `-cpu Westmere` smoke run (§8.2) guards that.
- eBPF has **no CPU-model requirement**. The cgroup `connect4/6` and `sendmsg4/6` hooks, CO-RE with BTF, and the x86_64 BPF JIT all depend on the kernel config, and §3.2 showed Ubuntu 26.04's generic kernel has every option we need. User namespaces, netem, `INET_DIAG_DESTROY`, Btrfs and nftables are kernel features too.
- **Performance:** four slow cores. Image unpacking, TLS handshakes and zstd compression will be noticeably slower than on the quickstart's VMs. The spike times a deploy and an image pull.

### 9.6 Known quirks to bake into the image

- **Reboot and shutdown hang** on Cherry Trail: the HSUART DMA driver hangs. The Debian wiki's fix is to blacklist `dw_dmac` and `dw_dmac_core` (`install dw_dmac /bin/true`, and the same for `dw_dmac_core`). Ship that as a `modprobe.d` file in the image, and check in the spike whether Linux 7.0 still needs it. A node that can't reboot can't finish an A/B update.
- **Firmware files:** `rtl_nic/rtl8168*` for networking, `i915` for the console. Audio needs `firmware-intel-sound` on Debian, but we don't need audio.
- **Fanless:** watch for thermal throttling under sustained load **[unverified]**.
- **Only one USB 3 port** and no serial port, so a console means HDMI-to-DisplayPort and a USB keyboard. The tty1 status screen (§4.5) is how the operator reads the claim fingerprint.

### 9.7 How the final stage runs

1. Update and configure all ten BIOSes (§9.4). Record the menu names and time spent.
2. Put the second Mac on Ethernet on the same switch as the Wyse boxes, behind an ordinary home router. Run `relish image serve` natively, or dnsmasq-proxy in a bridged VM if `relish image serve` isn't ready.
3. Netboot all ten at once. They install and reboot into `unclaimed`.
4. Claim them: three or five council voters, the rest workers.
5. Run the tour. Pull a power cord.
6. Measure RAM (§9.2) and eMMC writes (§9.3) over 24 hours.
7. Pin a new weekly OS version and roll it out (§7.6). Then force one bad image and check the fallback.
8. Write `docs/qualification/<date>-wyse-3040.md`.

---

## 10. Alternatives considered

### 10.1 Kairos on Ubuntu (considered 27 Sep 2026, not chosen)

The alternative:
- build the image `FROM ubuntu:26.04` with `kairos-init` (Kairos 4.3.0, 8 Sep 2026, Apache-2.0, CNCF Sandbox);
- let **AuroraBoot** turn the one OCI image into netboot artefacts, an ISO and a raw disk, and serve them with its built-in ProxyDHCP (Pixiecore) next to the home router;
- get the installer, A/B plus recovery images and GRUB boot assessment for free.

It would have saved most of Phases 1, 2b and 3's image, installer and netboot work.

**Why it wasn't chosen:**
1. **It isn't a minimal appliance.** On Debian-family bases, `kairos-init`'s package map installs `openssh-server`, `fail2ban`, `neovim`, `snmpd`, `nfs-common`, `open-iscsi`, `isc-dhcp-server` and more. Trimming that fights upstream's own stages. On an 8 GB eMMC (§9.3) the extra weight hurts twice: once in each A/B image, and once in the recovery image.
2. **A second agent on every node.** `kairos-agent`, immucore and yip, plus AuroraBoot as a second tool on the laptop. Its v0.20+ fleet server would be a second management plane beside `relish`.
3. **Upstream has moved away from Ubuntu.** Since v4.0 (Feb 2026) the project publishes prebuilt artefacts only for its own Hadron distro, and says the old flavour repositories "are no longer actively updated". CI **builds** `ubuntu:26.04` in its `_build-flavors.yaml` smoke matrix but **boot-tests** only Hadron and one `ubuntu:20.04` cell. We'd be the only pipeline proving Ubuntu 26.04 boots.
4. **A small core.** About four people wrote almost all of the monorepo's last 12 months of commits, behind a single sponsor (Spectro Cloud).
5. **Persistence doesn't match bun.** `/var` and `/etc` are ephemeral, and `kairos-agent`'s partitioner can't format Btrfs (`agent/pkg/partitioner/mkfs.go` handles only ext2–4, xfs and fat). We'd work around both.
6. **Netboot is the part we want to own anyway.** `relish image serve` gives one binary, the operator's existing trust (the release key and the claim flow), and no Docker on the laptop. AuroraBoot's netboot doesn't work from its Docker image on macOS at all; the native binary needs `xorriso`.

What we keep from the exercise: the netboot flow (ProxyDHCP, iPXE, streamed install) matches AuroraBoot's design, which is reassuring. The rule "cloud-config and netboot carry no secrets" applies to our seed and claim design unchanged. The sources are in the Sources section.

### 10.2 Talos (parked, revisit later)

§2 is the full analysis, kept as background. In short, Talos 1.14 can host `bun` through an experimental, controlplane-only Kubernetes-less mode and brand-new host-mode extension services. But it brings a second API and PKI (`talosctl`), a musl rootfs without the tools bun shells out to, and user namespaces off by default. Its vendor also announced its own non-Kubernetes container scheduling for December 2026. Forking it is ruled out (§2.10). **There are no Talos spike steps or plan items any more.** Revisit once the k8s-less mode and host-mode services are GA, at the earliest in 2027, as a "bring your own Talos" extension for people already running it.

---

## Sources

**Talos**
- v1.14.0 release notes: https://github.com/siderolabs/talos/releases/tag/v1.14.0 (checked myself: k8s-less, ContainerConfig, sandboxd, lockdown=integrity, ExtensionServiceConfig change, installer image)
- v1.14.1: https://github.com/siderolabs/talos/releases/tag/v1.14.1
- k8s-less PR: https://github.com/siderolabs/talos/pull/13892 (checked myself)
- Earlier Kubernetes-only statements: https://github.com/siderolabs/talos/issues/6473 and https://github.com/siderolabs/talos/discussions/8343
- KSPP sysctls: https://raw.githubusercontent.com/siderolabs/talos/v1.14.1/pkg/kernel/kspp/kspp.go (checked myself)
- Extension spec: https://github.com/siderolabs/talos/blob/v1.14.1/pkg/machinery/extensions/services/services.go (checked myself)
- Host-mode process runner: https://github.com/siderolabs/talos/blob/v1.14.1/internal/app/machined/pkg/system/runner/process/process.go (checked myself)
- Extension services docs: https://docs.siderolabs.com/talos/v1.14/build-and-extend-talos/custom-images-and-development/extension-services
- libvirtd host-mode precedent: https://github.com/siderolabs/extensions/blob/main/hypervisors/libvirtd/virtqemud.yaml
- Root-level cgroups (libvirt): https://github.com/siderolabs/talos/pull/14454
- ContainerConfig: https://github.com/siderolabs/talos/blob/v1.14.1/website/content/v1.14/reference/configuration/container/containerconfig.md
- Kernel config: https://github.com/siderolabs/pkgs/blob/release-1.14/kernel/build/config-amd64
- Rootfs contents: https://github.com/siderolabs/talos/blob/v1.14.1/Dockerfile
- btrfs extension: https://github.com/siderolabs/extensions/tree/main/storage/btrfs
- Boot assets and imager: https://docs.siderolabs.com/talos/v1.14/platform-specific-installations/boot-assets
- Config acquisition: https://docs.siderolabs.com/talos/v1.14/configure-your-talos-cluster/system-configuration/acquire
- Maintenance mode: https://docs.siderolabs.com/talos/v1.14/configure-your-talos-cluster/system-configuration/insecure
- Unattended install: https://docs.siderolabs.com/talos/v1.14/configure-your-talos-cluster/lifecycle-management/unattended-install
- Upgrading: https://docs.siderolabs.com/talos/v1.14/configure-your-talos-cluster/lifecycle-management/upgrading-talos
- SideroLink: https://docs.siderolabs.com/talos/v1.14/networking/siderolink
- Omni licence: https://github.com/siderolabs/omni/blob/main/LICENSE
- Discovery Service licence: https://github.com/siderolabs/discovery-service/blob/main/LICENSE
- Sidero announcement: https://www.prnewswire.com/news-releases/talos-linux-adds-native-hypervisor-and-edge-container-support--see-it-live-at-taloscon-2026-302877040.html
- Talos Containers tracking issue: https://github.com/siderolabs/talos/issues/14220
- Yardi acquisition and licence statement (14 Sep 2026): https://www.siderolabs.com/blog/sidero-labs-joins-yardi
- "Single-purpose Kubernetes distribution" (Dec 2024): https://github.com/siderolabs/talos/discussions/10008
- Talos Systems renamed to Sidero Labs: https://www.siderolabs.com/blog/talos-systems-is-now-sidero-labs

**Forking Talos (§2.10; counts via the GitHub API on 26 Sep 2026)**
- Controllers at v1.14.1: https://github.com/siderolabs/talos/tree/v1.14.1/internal/app/machined/pkg/controllers
- COSI resources at v1.14.1: https://github.com/siderolabs/talos/tree/v1.14.1/pkg/machinery/resources
- Services at v1.14.1: https://github.com/siderolabs/talos/tree/v1.14.1/internal/app/machined/pkg/system/services
- Go version: https://github.com/siderolabs/talos/blob/v1.14.1/go.mod
- Releases (cadence): https://github.com/siderolabs/talos/releases
- Contributors: https://github.com/siderolabs/talos/graphs/contributors and https://github.com/siderolabs/pkgs/graphs/contributors
- pkgs (kernel bumps, Pkgfile, bldr version): https://github.com/siderolabs/pkgs/blob/main/Pkgfile and https://github.com/siderolabs/pkgs/commits/main
- tools: https://github.com/siderolabs/tools
- bldr: https://github.com/siderolabs/bldr
- Custom kernel modules and signing: https://docs.siderolabs.com/talos/v1.13/build-and-extend-talos/custom-images-and-development/kernel-module and https://github.com/siderolabs/talos/issues/7174
- MPL-2.0 FAQ: https://www.mozilla.org/en-US/MPL/2.0/FAQ/
- Headcount estimate **[unverified]**: https://profiles.crustdata.com/company/sidero-labs

**Kairos**
- Releases: https://github.com/kairos-io/kairos/releases
- Kairos factory: https://kairos.io/docs/reference/kairos-factory/
- Image matrix: https://kairos.io/docs/v4.3.0/reference/image_matrix/
- Immutability: https://kairos.io/docs/architecture/immutable/
- Extra persistent paths: https://kairos.io/docs/examples/extra_persistent_paths_after_install/
- AuroraBoot: https://kairos.io/docs/reference/auroraboot/
- p2p: https://kairos.io/docs/installation/p2p/
- CNCF: https://www.cncf.io/projects/kairos/

**Flatcar**
- Releases: https://www.flatcar.org/releases
- sysext: https://www.flatcar.org/docs/latest/sys-ext/
- sysext bakery: https://github.com/flatcar/sysext-bakery
- Booting with ISO: https://www.flatcar.org/docs/latest/deploy/bare-metal/booting-with-iso/
- CNCF incubation: https://www.cncf.io/blog/2024/10/29/flatcar-brings-container-linux-to-the-cncf-incubator/

**Fedora CoreOS**
- Customising installs: https://coreos.github.io/coreos-installer/customizing-install/
- bootc tracker: https://github.com/coreos/fedora-coreos-tracker/issues/1726
- SELinux: https://github.com/coreos/fedora-coreos-docs/blob/main/modules/ROOT/pages/selinux.adoc

**Bottlerocket**
- Metal provisioning: https://github.com/bottlerocket-os/bottlerocket/blob/develop/PROVISIONING-METAL.md
- Metal variants dropped: https://github.com/bottlerocket-os/bottlerocket/issues/3794

**mkosi, sysupdate, Incus OS, ParticleOS**
- mkosi news: https://github.com/systemd/mkosi/blob/main/mkosi/resources/man/mkosi.news.7.md
- systemd-sysupdate: https://man7.org/linux/man-pages/man8/systemd-sysupdate.8.html
- Incus OS announcement: https://stgraber.org/2025/11/07/introducing-incusos/
- Incus OS source: https://github.com/lxc/incus-os
- Incus OS seed: https://linuxcontainers.org/incus-os/docs/main/reference/seed/
- ParticleOS: https://github.com/systemd/particleos
- mkosi man page (v27 options: `Snapshot=`, `Bootloader=`, `ShimBootloader=`, `SecureBoot*`, `Kernel*`, `BuildSources=`, `PackageDirectories=`): https://github.com/systemd/mkosi/blob/main/mkosi/resources/man/mkosi.1.md
- mkosi releases: https://github.com/systemd/mkosi/releases
- mkosi distribution code: https://github.com/systemd/mkosi/blob/main/mkosi/distribution/ubuntu.py and https://github.com/systemd/mkosi/blob/main/mkosi/distribution/debian.py
- mkosi-kernel: https://github.com/DaanDeMeyer/mkosi-kernel
- Incus OS security (own PK/KEK/db, no shim): https://linuxcontainers.org/incus-os/docs/main/reference/security/

**Ubuntu 26.04 and Debian 13 (§3.1)**
- Ubuntu 26.04 release notes: https://documentation.ubuntu.com/release-notes/26.04/ and https://documentation.ubuntu.com/release-notes/26.04/summary-for-lts-users/
- Ubuntu 26.04 announcement: https://canonical.com/blog/canonical-releases-ubuntu-26-04-lts-resolute-raccoon
- 26.04.1: https://discourse.ubuntu.com/t/ubuntu-26-04-1-lts-released/86808
- Kernel lifecycle and support dates: https://ubuntu.com/kernel/lifecycle
- 15-year Legacy add-on: https://canonical.com/blog/canonical-expands-total-coverage-for-ubuntu-lts-releases-to-15-years-with-legacy-add-on
- Kernel SRU cadence: https://ubuntu.com/kernel/docs/explanation/stable-release-updates/ and the 23 Sep 2026 change https://canonical.com/blog/accelerating-delivery-of-cve-fixes-with-a-new-kernel-release-strategy
- runc in resolute: https://launchpad.net/ubuntu/resolute/+source/runc-app and https://packages.ubuntu.com/resolute/runc
- systemd-boot in resolute (unsigned only): https://packages.ubuntu.com/search?keywords=systemd-boot&suite=resolute
- Ubuntu snapshot service: https://ubuntu.com/server/docs/how-to/software/snapshot-service/
- Microsoft UEFI CA rotation: https://discourse.ubuntu.com/t/microsoft-uefi-ca-rotation-what-it-means-for-ubuntu-users-and-vendors/82652
- Debian trixie release and support: https://www.debian.org/releases/trixie/ and 13.7 https://www.debian.org/News/2026/20260912
- Debian package versions: https://api.ftp-master.debian.org/madison and https://packages.debian.org/search?keywords=linux-image-amd64&searchon=names&exact=1&suite=all&section=all
- runc CVE-2025-31133 in trixie: https://security-tracker.debian.org/tracker/CVE-2025-31133
- Debian signed systemd-boot: https://packages.debian.org/search?keywords=systemd-boot-efi-amd64-signed
- Debian reproducibility: https://tests.reproducible-builds.org/debian/trixie/index_suite_amd64_stats.html

**Kernel configs (§3.2)**
- Debian trixie: https://salsa.debian.org/kernel-team/linux/-/raw/debian/6.12/trixie/debian/config/config and https://salsa.debian.org/kernel-team/linux/-/raw/debian/6.12/trixie/debian/config/amd64/config
- Ubuntu 24.04 and 26.04: the `.config` in `linux-headers-*-generic` / `linux-modules-*-generic` debs under http://archive.ubuntu.com/ubuntu/pool/main/l/linux/ (6.8.0-146 and 7.0.0-38; the Launchpad annotations returned 403)
- Kernel CVE volume: https://lwn.net/Articles/1049963/ and, secondary, https://ciq.com/blog/linux-kernel-cves-2025-what-security-leaders-need-to-know-to-prepare-for-2026

**Network boot (§4.7)**
- ProxyDHCP: https://ipxe.org/appnote/proxydhcp
- EDK2 HTTP Boot proxy offers: https://github.com/tianocore/edk2/blob/master/NetworkPkg/HttpBootDxe/HttpBootDhcp4.c
- EDK2 HTTP Boot (EFI and ISO RAM disk): https://github.com/tianocore/tianocore.github.io/wiki/HTTP-Boot
- dnsmasq proxy HTTP Boot report (June 2022): https://www.mail-archive.com/dnsmasq-discuss@lists.thekelleys.org.uk/msg16282.html
- dnsmasq ProxyDHCP examples: https://wiki.fogproject.org/wiki/index.php?title=ProxyDHCP_with_dnsmasq
- UEFI network boot and UKIs (kraxel, July 2024): https://www.kraxel.org/blog/2024/07/uefi-network-boot/
- systemd 258 NEWS (`uki-url`, `LoaderDeviceURL`, `rd.systemd.pull=` `bootorigin`): https://github.com/systemd/systemd/blob/v258/NEWS
- ParticleOS netboot issue: https://github.com/systemd/particleos/issues/80
- iPXE Secure Boot (Microsoft-signed shim, Nov 2025): https://ipxe.org/secboot, https://github.com/ipxe/shim/releases and https://ipxe.org/appnote/etoken
- Kairos netboot and AuroraBoot: https://kairos.io/docs/installation/netboot/
- pixiecore: https://github.com/danderson/netboot
- Tinkerbell smee: https://github.com/tinkerbell/smee
- Matchbox: https://github.com/poseidon/matchbox
- Talos PXE: https://docs.siderolabs.com/talos/v1.7/platform-specific-installations/bare-metal-platforms/pxe and https://github.com/siderolabs/talos/issues/9947
- netboot.xyz: https://netboot.xyz
- macOS privileged ports since Mojave: https://news.ycombinator.com/item?id=18302380 and https://zameermanji.com/blog/2024/1/5/binding-to-privileged-ports-without-root-on-macos/
- macOS `bootpd` on port 67: https://canonical.com/multipass/docs/latest/how-to-guides/troubleshoot/troubleshoot-networking/
- Dell HTTPS Boot: https://www.dell.com/support/manuals/en-us/bios-connect/https_ug/introduction-to-https-boot
- Lenovo ThinkCentre network setup: https://docs.lenovocdrt.com/ref/bios/settings/thinkcentre/network_setup/
- HP and the HTTP Boot spec: https://uefi.org/sites/default/files/resources/FINAL%20Pres4%20UEFI%20HTTP%20Boot.pdf
- Rust crates: https://github.com/bluecatengineering/dhcproto and https://github.com/oblique/async-tftp-rs

**Other bases**
- Alpine linux-lts kernel config: https://github.com/alpinelinux/aports/blob/master/main/linux-lts/lts.x86_64.config
- LinuxKit: https://github.com/linuxkit/linuxkit
- NixOS image building: https://nixos.org/manual/nixos/stable/#sec-image-nixos-rebuild-build-image
- Ubuntu autoinstall: https://canonical-subiquity.readthedocs-hosted.com/en/latest/tutorial/providing-autoinstall.html

**Join UX prior art**
- Proxmox automated install: https://pve.proxmox.com/wiki/Automated_Installation
- Harvester configuration: https://docs.harvesterhci.io/v1.6/install/harvester-configuration/
- k3s tokens: https://docs.k3s.io/cli/token
- Tailscale auth keys: https://tailscale.com/kb/1085/auth-keys

**Kairos alternative (§10.1; all fetched 27 Sep 2026)**
- Docs source, used instead of the rendered site because it's current and unambiguous: https://github.com/kairos-io/kairos-docs (paths below are under `docs/`, `blog/` or `quickstart/` on `main`)
- Network booting and in-RAM mode: https://kairos.io/docs/installation/netboot/ (`docs/installation/netboot.md`)
- AuroraBoot reference (ProxyDHCP prerequisites, macOS, `build-iso`, `netboot`, `start-pixie`, raw disks, UKI over HTTP Boot): https://kairos.io/docs/reference/auroraboot/ (`docs/reference/auroraboot.md`)
- Configuration reference (`install`, `partitions`, `extra-partitions`, `bind_mounts`, `ssh_hardening`, `p2p`, kcrypt fields): https://kairos.io/docs/reference/configuration/ (`docs/reference/configuration.md`)
- Immutability and default persistent paths: https://kairos.io/docs/architecture/immutable/ (`docs/architecture/immutable.md`)
- Extra persistent paths after install: https://kairos.io/docs/examples/extra_persistent_paths_after_install/
- Manual upgrades (`kairos-agent upgrade --source oci:|ocifile:|dir:|file:`, `--recovery`): https://kairos.io/docs/upgrade/manual/ (`docs/upgrade/manual.md`)
- Boot assessment: https://kairos.io/docs/upgrade/boot_assessment/ and its tracking issue, closed as completed on 27 Nov 2024: https://github.com/kairos-io/kairos/issues/2864
- Trusted Boot architecture (USI, keys, TPM2): https://kairos.io/docs/architecture/trustedboot/
- P2P (experimental): https://kairos.io/docs/installation/p2p/
- Image support matrix (Hadron-only prebuilt artefacts, legacy flavours not updated): https://kairos.io/docs/reference/image_matrix/
- Kairos factory and `kairos-init` flags: https://kairos.io/docs/reference/kairos-factory/
- Extending the system with a Dockerfile: https://kairos.io/quickstart/extending-the-system-dockerfile/
- "Hadron-Only Artifacts with Ongoing Distro Support" (25 Feb 2026): https://kairos.io/blog/2026/02/25/kairos-v4-hadron-artifacts-and-distro-flexibility
- "Kairos v4.1.0: From Image Build to Managed Nodes with AuroraBoot" (15 May 2026; Ubuntu 26.04 in `kairos-init`, AuroraBoot fleet server): https://kairos.io/blog/2026/05/15/kairos-v4-1-0-hadron-ubuntu-boot-install-foundations
- Releases: https://github.com/kairos-io/kairos/releases (v4.3.0, 8 Sep 2026) and https://github.com/kairos-io/AuroraBoot/releases (v0.27.1, 11 Sep 2026)
- CI matrices (checked myself): https://github.com/kairos-io/kairos/blob/master/.github/workflows/_build-flavors.yaml, https://github.com/kairos-io/kairos/blob/master/.github/workflows/master.yaml and https://github.com/kairos-io/kairos/blob/master/.github/workflows/release.yaml
- `kairos-init` package map (kernel per Ubuntu release, shim and GRUB, the Debian-family base package set; checked myself): https://github.com/kairos-io/kairos/blob/master/kairos-init/pkg/values/packagemaps.go
- Partitioner filesystems (no Btrfs; checked myself): https://github.com/kairos-io/kairos/blob/master/agent/pkg/partitioner/mkfs.go
- Licences, stars and commit authors: the GitHub API for `kairos-io/kairos`, `kairos-io/AuroraBoot` and `kairos-io/hadron`
- CNCF project page: https://www.cncf.io/projects/kairos/ and Sandbox issue https://github.com/cncf/sandbox/issues/52
- Spectro Cloud and Hadron: https://www.spectrocloud.com/news/announcing-hadron-a-lightweight-security-first-linux-distribution
- iPXE bootstrap ISO for machines without PXE: https://github.com/kairos-io/ipxe-dhcp/releases
- Reliaburger `main` at `0eb6071d`: `docs/linux-servers.md` (unit file, `operator_cidrs`, master-key copy over SSH, `binary_dir` upgrades) and `scripts/release/guest-images.json` (still Ubuntu 24.04)

**Weekly builds and OS updates (§7; fetched 27 Sep 2026)**
- mkosi setup action (unprivileged userns, `/dev/kvm`): https://github.com/systemd/mkosi/blob/main/action.yaml
- mkosi man page (`RepartOffline=`, `ToolsTree=`, unprivileged user namespaces): https://github.com/systemd/mkosi/blob/main/mkosi/resources/man/mkosi.1.md
- KVM on standard 2-vCPU Linux hosted runners: https://github.blog/changelog/2024-04-02-github-actions-hardware-accelerated-android-virtualization-now-available/
- No `/dev/kvm` on `ubuntu-24.04-arm`: https://github.com/orgs/community/discussions/148648 and https://github.com/orgs/community/discussions/160591
- GitHub-hosted runners reference: https://docs.github.com/en/actions/reference/runners/github-hosted-runners
- `sysupdate.d(5)` (resource types, `Verify=`, `TriesLeft=`, `InstancesMax=`): https://man7.org/linux/man-pages/man5/sysupdate.d.5.html
- Automatic Boot Assessment: https://systemd.io/AUTOMATIC_BOOT_ASSESSMENT/
- Repo, `main` at `0eb6071d`: `.github/workflows/build.yml` (`build-guest-images`, `candidate`), `docs/releasing.md` (signing identity, guest images, the soak's signing caveat) and PR #215 (artefact retention and cache limits)

**VMs on a Mac (§8; fetched 27 Sep 2026)**
- QEMU EDK2 build options: https://gitlab.com/qemu-project/qemu/-/blob/master/roms/edk2-build.config and the shipped files in https://gitlab.com/qemu-project/qemu/-/tree/master/pc-bios
- Homebrew formulae (qemu 11.1.1, socket_vmnet 1.2.2): https://formulae.brew.sh/formula/qemu and https://formulae.brew.sh/formula/socket_vmnet
- socket_vmnet: https://github.com/lima-vm/socket_vmnet
- Lima vmnet networks (shared 192.168.105.0/24, `bootpd`, bridged): https://lima-vm.io/docs/config/network/vmnet/
- QEMU `-netdev dgram` multicast: https://www.mail-archive.com/qemu-devel@nongnu.org/msg1059033.html and https://john-millikin.com/improved-unix-socket-networking-in-qemu-7.2
- dnsmasq man page (`--dhcp-range=...,proxy`, `--pxe-service`): https://thekelleys.org.uk/dnsmasq/docs/dnsmasq-man.html
- iPXE on ARM64 EFI: https://ipxe.org/appnote/buildtargets

**Dell Wyse 3040 (§9; fetched 27 Sep 2026)**
- Debian wiki: https://wiki.debian.org/InstallingDebianOn/Dell/Wyse%203040
- Parkytowers hardware and firmware pages: https://www.parkytowers.me.uk/thin/wyse/3040/ and https://www.parkytowers.me.uk/thin/wyse/3040/firmware.shtml
- Dell 3040 user guide (boot sequence, BIOS access): https://www.dell.com/support/manuals/en-us/wyse-3040-thin-client/3040_ug/boot-sequence?guid=guid-569fa4de-9398-4878-ba44-a8e5b05ccff3&lang=en-us
- Install write-ups: https://nickcharlton.net/posts/installing-debian-12-dell-wyse-3040 and https://mcgarrah.org/dell-wyse-3040-debian12/
- Repo, `main`: `docs/qualification/2026-09-26-v02-soak-12h.md` (bun RSS, disk use), `docs/qualification/2026-09-24-binary-size.md` (bun 104.9 MB, relish 94.6 MB), `docs/qualification/2026-09-24-tour-transcript.md`, and `src/config/node.rs` (`reserved_memory`, `[metrics]`/`[logs]` `max_storage_mb`, `[images] gc_retain_days`)

**Still [unverified]**
- Talos runc path and `iptables` backend.
- Whether owners survive a restart under the host-mode runner on a real node.
- Whether worker-type k8s-less configs work.
- The Kairos Hadron init system.
- Flatcar and FCOS per-component licences.
- Appliance image size.
- Talos core-maintainer count beyond commit statistics, and Sidero headcount.
- Per-package Launchpad components for uidmap, iptables and iproute2 in resolute.
- Which kernel flavour the Ubuntu cloud image in `guest-images.json` ships.
- Whether consumer AMI/Insyde firmware honours ProxyDHCP offers for UEFI HTTP Boot.
- That a non-root process on macOS can bind UDP 67/69/4011 on the wildcard address.
- The exact Secure Boot hand-over from Microsoft-signed iPXE to our UKI.
- The appliance's image size, RAM use on 2 GB and eMMC writes per day (§9.2, §9.3), measured in the spike.
- The Wyse 3040's BIOS menu names for network boot, and whether it has UEFI HTTP Boot (§9.4).
- Whether the `dw_dmac` reboot-hang workaround is still needed on Linux 7.0 (§9.6).
- The mkosi tools-tree build time on hosted runners, and whether the arm64 runner can build unprivileged the same way (§7.3).
- The macOS lab details in §8.4: socket_vmnet with Lima's VZ backend, dnsmasq CSA names, slirp and UEFI HTTP Boot, multicast `-netdev dgram` on macOS, and an x86_64 vars file for QEMU 11.
- That the perimeter blocks a LAN laptop in practice (derived from `src/firewall/rules.rs` and `src/bun/agent.rs`, not tested on hardware).
