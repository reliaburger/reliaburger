# Reliaburger as the OS: Talos, alternatives, and a five-mini-PC join flow

*Research note, 26 September 2026. The repo facts come from reading `main` at `0a5dfc6`. External facts come from primary sources fetched today; the URLs are in the Sources section. **[unverified]** marks a claim nobody has tested or confirmed from a primary source. **[inference]** marks my own reading of code or docs.*

*Revised the same day with four follow-up questions: forking Talos (§2.10), Ubuntu versus Debian as the mkosi base (§3.1), whether mkosi ties us to the distro kernel (§3.2), and network boot shipped by us (§4.7).*

---

## 0. Summary

- **Talos can now host `bun`, but it's the wrong base for our appliance.**
  - Talos v1.14.0 (3 Sep 2026) added the two things we'd need:
    - an **experimental Kubernetes-less, etcd-less mode**;
    - **host-mode extension services** (`runnerMode: host`), which run in the root namespaces and signal only the main PID when they stop.
  - The costs:
    - The k8s-less mode is experimental, and upstream says "only controlplane mode will be supported/tested".
    - The rootfs is musl-based and has no `ip`, `tc`, `ss`, `mount`, `fallocate` or `losetup`.
    - User namespaces are off by default (`user.max_user_namespaces=0`).
    - A custom extension can't come from the public Image Factory, so we'd run our own `imager` pipeline anyway.
    - The operator would manage two APIs and two PKIs: `talosctl` plus machine secrets, and `relish` plus the cluster CA.
    - Sidero announced its own "beyond Kubernetes" container scheduling for December 2026.
- **Don't fork Talos either** (§2.10). A fork would keep the parts we'd most like to replace (apid, the Talos PKI, machine config), and it would saddle us with a musl toolchain, bldr and a kernel that upstream bumps about three times a month. Upstream's own core is about 4–5 people. Sidero is now building Kubernetes-less edge containers itself, so a fork would chase a moving target that also competes with us.
- **Recommendation: build our own appliance image** with **mkosi** (UKI, verity `/usr`, `systemd-repart`, `systemd-sysupdate` A/B). This is the design Incus OS ships.
  - **Base: Ubuntu 26.04 LTS, not Debian 13** (changed in this revision, §3.1). Ubuntu gives us a supported runc 1.4 in `main` (trixie's runc is the EOL 1.1 line with an open CVE), Linux 7.0, standard support to May 2031, a kernel update roughly every week, and the same distro as the quickstart guest image. Debian stays a close second, and the mkosi recipe should keep it buildable.
  - **Kernel: ride the distro's** (§3.2). mkosi doesn't tie us to it, but every kernel option bun needs is already on in both distros' stock configs, and owning a kernel means owning ~5,500 CVEs a year.
  - **Fallback:** a Kairos "core" image via `kairos-init` if we want A/B ISO/PXE with the least tooling of our own.
  - **Later:** offer Talos as a "bring your own Talos" system extension once k8s-less mode leaves experimental.
- **Network boot is a first-class install path** (§4.7). `relish image serve` runs a ProxyDHCP, TFTP and HTTP boot server on the operator's laptop, or on the first installed node, so the other machines install with no USB stick. It coexists with the home router's DHCP. USB stays the fallback.
- **Four repo gaps block unattended bare-metal joins, whatever OS we pick:**
  1. **Every node needs the 32-byte `master.key`, and nothing delivers it except out-of-band copying.** The quickstart copies it with `limactl`. Its derived service token is Admin-equivalent for everything except user-management routes. I found no master-key rotation.
  2. **The perimeter firewall drops the management port (9117) and cluster ports** from anything that isn't a member, loopback, or an exact `bootstrap_peers` IP. `PerimeterConfig.admin_cidrs` exists but isn't wired to node config. So a DHCP joiner, or the operator's laptop on the LAN, can't reach the join API today (from the code; not tested on a LAN).
  3. **`advertise_address` falls back to `127.0.0.1`, and node names default to `node-<gossip_port>`.** An appliance needs both detected automatically.
  4. **Join tokens are single-use, node-bound and at most 1 h.** They're good primitives, but a "fleet stick" needs something on top.
- **Recommended join flow: "claim over the LAN".** Machines boot a generic signed image, show an "unclaimed" screen and announce themselves over mDNS. The operator runs `relish machines claim`, which pushes each machine an existing single-use, node-bound join token and the pinned CA fingerprint over TLS. No secrets ever live on a USB stick or go out over network boot. A seed-file mode (`relish image create`) covers headless installs.
- **First spike (about 4 days):** boot the mkosi Ubuntu 26.04 image and a Talos 1.14 k8s-less image with a host-mode `bun` extension side by side in QEMU, and run the existing tour on each. Then network-boot the mkosi UKI through a ProxyDHCP next to an ordinary DHCP server (details in §6).

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
| G2 | The perimeter drops 9117 for the laptop and for DHCP joiners | Wire `admin_cidrs` into `[security]` or `[firewall]` config. Add a **time-boxed `join_window` CIDR** (for example the LAN /24 for 60 minutes after `relish machines claim`) that opens only 9117. That exposes `/v1/cluster/ca` and `/v1/cluster/join`, which are token-gated anyway. |
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

Effort is in engineer-weeks, including tests and book and manual updates per `CLAUDE.md`.

### Phase 0: spike (~3 days, §6)

### Phase 1: MVP appliance, seed mode (~4–6 weeks)

**Security and bun:**
- G1: master-key delivery over mTLS after join, with tests for refusal without a node cert and refusal for retired nodes.
- G2: `admin_cidrs` wired, plus a time-boxed `join_window`.
- G3/G4: advertise-address detection and appliance naming.
- G6: seeded join tokens in the initial `SecurityState`.

**New `bun --appliance` mode** (a module such as `src/appliance/`), as a pure state machine with unit tests:
- read the seed (`RELIABURGER-SEED` partition);
- lay out state under `/var/lib/reliaburger`;
- write `node.toml` from the seed;
- enrol by calling the existing join code in-process, not by shelling out to `relish join`;
- zero the create-seed after first boot;
- run the tty1 status screen.

**Self-upgrade:** a launcher in the image execs `/var/lib/reliaburger/bin/bun`. The existing `binary_dir` and symlink swap stay unchanged.

**`relish`:**
- `relish cluster create --bare-metal` (generalised bare-metal context);
- `relish image download | write | seed`.

**Image:**
- an mkosi recipe (for example `image/mkosi.conf`), Ubuntu 26.04 LTS, x86_64, with the distro's generic kernel and `linux-firmware` (§3.2), and `[Match]` blocks that keep Debian 13 buildable (§3.1);
- package list shared with `guest-images.json` so the guest image and the appliance can't drift, and the guest moved to 26.04 in the same release;
- `Snapshot=` pinned to `snapshot.ubuntu.com`, with every package version in the build record;
- systemd unit reused from `provision.rs::SERVICE`;
- no SSH;
- `systemd-repart` for data;
- installer mode ("boot from USB, install to disk") via a small repart-based first-boot installer or mkosi's ISO output.

**CI** (`.github/workflows/build.yml`):
- a `build-appliance-images` job next to `build-guest-images`, on native runners;
- `SourceDateEpoch`;
- the build record as a JSON artefact;
- the image digest signed into release metadata with the existing Ed25519 release keys, the way `guest-image-metadata.json` already is, and verified by `relish image download`;
- a scheduled rebuild when Ubuntu publishes a new kernel or a USN touching the package list (about weekly for kernels from 28 Sep 2026), so riding the distro kernel doesn't mean shipping a stale one.

**Tests:**
- unit tests for the seed parser and appliance state machine;
- a **QEMU+OVMF boot test** in CI on KVM runners: boot the ISO headless, install to a virtio disk, reboot, assert `/v1/health`.

### Phase 2: claim over the LAN (~3–4 weeks)

- A claim server in appliance mode: self-signed key, console fingerprint and QR.
- mDNS announce and browse.
- `relish machines [claim]`, `relish join-token list/revoke`.
- A **5-node virtual lab:** 5 QEMU VMs on a bridge with dnsmasq DHCP, driven by a script (for example `scripts/lab/five-node.sh`) that:
  1. claims all nodes;
  2. runs `scripts/demo/tour.sh`;
  3. kills a VM;
  4. checks rescheduling;
  5. writes a qualification record under `docs/qualification/`.

### Phase 2b: network boot (~2–3 weeks)

Before this revision, PXE sat in Phase 4 polish. §4.7 makes it a first-class path, so it follows the claim flow directly, since it's only useful once machines can be claimed without a seed.
- **Installer UKI** that pulls and verifies the full image from its boot origin (`rd.systemd.pull=` with `bootorigin`, systemd ≥ 258; Ubuntu 26.04 ships 259) and writes it to disk.
- **`relish image serve`** on macOS and Linux: ProxyDHCP (`dhcproto`), a minimal TFTP for iPXE, HTTP via axum, a refusal when another proxy or `bootpd` is already answering, `--mac` allow-list, and a time-boxed window. It serves only verified release artefacts.
- **`relish image serve --node <name>`**: the same server as a bun subsystem, opened through the admin API and closed after 60 minutes.
- **Vendored iPXE** (pinned, the Microsoft-signed shim build) with an embedded chain script.
- **Tests:** unit tests for the proxy-offer builder (PXE and HTTPClient variants, no `yiaddr`, option 43/60/67) as table-driven cases; property tests that we never answer a non-PXE DISCOVER; a **CI lab** with QEMU+OVMF on a bridge, a dnsmasq handing out addresses *only* (no boot options), and `relish image serve` as the proxy, covering both OVMF PXE and OVMF HTTP Boot, ending in five `unclaimed` machines.
- **Hardware qualification:** at least one Dell or Lenovo business mini PC (HTTP Boot) and one consumer AMI box (PXE only), with a Mac and a Linux laptop as the server.

### Phase 3: immutable A/B (~3–5 weeks)

- verity `/usr`, UKI, `systemd-sysupdate` slots.
- An `UpgradeManager` backend: `Symlink` (today) or `ImageSlot`. The orchestrator keeps its council-aware order and health gates. For OS releases each node step becomes "stage slot, drain, reboot, verify rejoin, else roll back".
- Open question: sysupdate verifies with GPG and SHA256SUMS, so either bun verifies Ed25519 first and feeds sysupdate a local verified source, or we also publish GPG signatures. **[design decision]**
- Secure Boot: our own db key, documented enrolment or disable instructions, and TPM2-sealed data-partition encryption as an option. That later unlocks `AttestationMode::Tpm`.

### Phase 4: polish (~2–3 weeks)

- aarch64 images (for Raspberry Pi-class boards through an overlay); G5 master-key rotation.
- Book chapter and manual "Bare metal" chapter.
- A physical-hardware qualification on real mini PCs.

### Optional: Talos extension (~2–3 weeks, 2027)

- A `siderolabs/extensions`-style repo with a host-mode service, a static musl `bun`, and bundled runc, iproute2 and util-linux.
- `relish image create --base talos` generates a k8s-less machine config with `SysctlConfig` and embedded config.

### Minimal first version

- x86_64 only, Ubuntu 26.04 via mkosi, distro kernel, mutable root (A/B deferred).
- `bun` self-upgrade as today.
- Seed-mode join with node-bound tokens, plus G1 and G2.
- QEMU boot test in CI.
- **Proof point:** 5 VMs join from one seeded image and pass the tour.

That's roughly the Phase 1 scope, and it ships value before A/B. Network boot (Phase 2b) is the first thing to add after it, since it's what turns "five sticks" into "five power buttons".

---

## 6. Recommendation, risks and the first spike

**Recommendation:**
1. Build a **Reliaburger appliance image on Ubuntu 26.04 LTS with mkosi**, following the Incus OS design, with `bun` as the only service. This revision changes the base from Debian 13 (§3.1): runc 1.4 in `main`, a longer support tail, a faster kernel cadence and one distro shared with the quickstart guest outweigh Debian's reproducibility record. Keep the recipe buildable on Debian 13. Keep bun's own symlink self-upgrade for the agent, and add sysupdate A/B for the OS in Phase 3.
2. **Ride the distro kernel** (§3.2). Sign only the UKI; don't build or sign kernels.
3. Make the join flow **claim over the LAN (option d)**, with **seed mode (option c)** as the MVP and the headless path.
4. Make **network boot a first-class install path** (§4.7, Phase 2b), served by `relish image serve` from the laptop or the first node, with USB as the fallback.
5. Fix gaps G1 and G2 first. They block *every* bare-metal install, not just the appliance.
6. **Don't fork Talos** (§2.10). Treat upstream Talos as a later "bring your own Talos" target, not the base.
7. If effort matters more than control, **Kairos core on an Ubuntu base** is the fallback. It gives ISO/PXE/A/B out of the box, at the cost of persistence bind-mounts and upstream churn.

**Key risks:**
- **Master-key blast radius.** Any enrolment path that hands out `master.key` turns a token leak into a cluster compromise. G1 plus approval-gated enrolment keep that bounded, and G5 (rotation) makes it recoverable.
- **Secure Boot on consumer mini PCs.** Enrolling our own key is fiddly, on USB and network boot alike. The v1 docs must cover "disable Secure Boot" honestly. The Microsoft UEFI CA 2011 expiry (27 June 2026) makes shim-based chains less dependable on older firmware, which is one more reason to prefer our own db key.
- **Owning an OS.** Canonical fixes the CVEs, but we have to rebuild and publish images promptly: a CI schedule plus a USN feed check. With Ubuntu's new kernel cadence that's roughly a weekly image.
- **LAN realities.** mDNS across VLANs, DHCP address changes, and consumer routers without reservations. Network boot adds firmware that ignores ProxyDHCP, AP isolation and a second proxy on the same LAN.
- **Talos direction.** If Talos Containers GA (Dec 2026) makes k8s-less first-class, the Talos option gets cheaper. Re-check then.

**First spike (≈4 days, QEMU on a Linux box with KVM, plus an hour on a Mac):**
1. **Talos track.** Build a Talos 1.14.1 image with `imager`:
   - a custom host-mode extension (bun, relish, a launcher, pinned runc, static iproute2 and util-linux);
   - embedded k8s-less config with `SysctlConfig user.max_user_namespaces`.

   Boot three VMs, form a cluster by hand, and run `scripts/demo/tour.sh`. Record:
   - eBPF attach to the root cgroup;
   - userns containers at uid 2e9;
   - whether owners survive `talosctl service ext-reliaburger restart`;
   - netem and `ss -K`;
   - an upgrade with the `/var` launcher.
2. **mkosi track.** Build an Ubuntu 26.04 mkosi disk image with the `guest-images.json` packages, the generic kernel and the quickstart unit. Boot three VMs and run the same tour. Build the same recipe with `Distribution=debian` to prove the switch is a config change, and record image size for both.
3. **Join track.** Prototype G1 (master-key fetch after join) and G2 (`admin_cidrs` and join window) against the mkosi VMs, and measure one seed-mode join end to end.
4. **Network-boot track.** On a bridge with a dnsmasq that hands out addresses only, run dnsmasq in proxy mode as a stand-in for `relish image serve`. HTTP-boot the mkosi UKI in OVMF, and PXE-boot it through iPXE. Check that `rd.systemd.pull=` with `bootorigin` fetches and verifies the image and the installer writes it. Then, on a Mac, check that a non-root process can bind UDP 67, 69 and 4011 on the wildcard address and receives the broadcasts, and note the firewall prompt.

**Exit criteria:** the tour passes on both tracks, owners-survive-restart holds on Talos, a VM installs over both PXE and HTTP Boot next to an unmodified DHCP server, and a written go/no-go for Talos versus mkosi.

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
- That the perimeter blocks a LAN laptop in practice (derived from `src/firewall/rules.rs` and `src/bun/agent.rs`, not tested on hardware).
