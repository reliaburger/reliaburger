# Reliaburger as the OS: Talos, alternatives, and a five-mini-PC join flow

*Research note, 26 September 2026. The repo facts come from reading `main` at `0a5dfc6`. External facts come from primary sources fetched today; the URLs are in the Sources section. **[unverified]** marks a claim nobody has tested or confirmed from a primary source. **[inference]** marks my own reading of code or docs.*

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
- **Recommendation: build our own appliance image** from Debian 13 with **mkosi** (UKI, verity `/usr`, `systemd-repart`, `systemd-sysupdate` A/B). This is the design Incus OS ships.
  - **Fallback:** a Kairos "core" image via `kairos-init` if we want A/B ISO/PXE with the least tooling of our own.
  - **Later:** offer Talos as a "bring your own Talos" system extension once k8s-less mode leaves experimental.
- **Four repo gaps block unattended bare-metal joins, whatever OS we pick:**
  1. **Every node needs the 32-byte `master.key`, and nothing delivers it except out-of-band copying.** The quickstart copies it with `limactl`. Its derived service token is Admin-equivalent for everything except user-management routes. I found no master-key rotation.
  2. **The perimeter firewall drops the management port (9117) and cluster ports** from anything that isn't a member, loopback, or an exact `bootstrap_peers` IP. `PerimeterConfig.admin_cidrs` exists but isn't wired to node config. So a DHCP joiner, or the operator's laptop on the LAN, can't reach the join API today (from the code; not tested on a LAN).
  3. **`advertise_address` falls back to `127.0.0.1`, and node names default to `node-<gossip_port>`.** An appliance needs both detected automatically.
  4. **Join tokens are single-use, node-bound and at most 1 h.** They're good primitives, but a "fleet stick" needs something on top.
- **Recommended join flow: "claim over the LAN".** Machines boot a generic signed image, show an "unclaimed" screen and announce themselves over mDNS. The operator runs `relish machines claim`, which pushes each machine an existing single-use, node-bound join token and the pinned CA fingerprint over TLS. No secrets ever live on a USB stick. A seed-file mode (`relish image create`) covers headless and PXE installs.
- **First spike (about 3 days):** boot the mkosi Debian image and a Talos 1.14 k8s-less image with a host-mode `bun` extension side by side in QEMU, and run the existing tour on each (details in §6).

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

---

## 3. Comparison of candidate bases

Legend: ✅ good, ⚠️ workable with effort, ❌ poor.

| | **Talos 1.14** | **Kairos 4.3 (core, Debian/Ubuntu base)** | **Flatcar 4757 (stable)** | **Fedora CoreOS 44** | **Bottlerocket 1.66** | **Own mkosi image (Debian 13)** | **Buildroot / Alpine** | **Ubuntu 26.04 autoinstall** |
|---|---|---|---|---|---|---|---|---|
| **Fit with bun's needs** | ⚠️ All kernel needs met. Userns sysctl=0 by default. No iproute2 or util-linux. musl. | ✅ Stock distro kernel and apt packages. `/var/lib/reliaburger` needs `install.bind_mounts`. | ✅ BTF, userns, nft, netem, btrfs, iproute2, e2fsprogs, bpftool all in base. runc via default-enabled sysext. | ⚠️ All tools present, but **SELinux enforcing** with no `semanage`. Ships Docker/podman. | ❌ Bun would run as a "superpowered" host container. Every package has to be cross-built into a kit. | ✅ We choose every package: the `guest-images.json` list. | ⚠️ We own the kernel config. Alpine lts has BTF=y. | ✅ Same as today's guest image |
| **Immutability** | ✅ squashfs, API-only | ✅ A/B/recovery images | ✅ Read-only `/usr` A/B | ✅ ostree/bootc | ✅ dm-verity | ✅ verity `/usr`, UKI | ⚠️ Diskless mode, or our own layout | ❌ Mutable |
| **Upgrades** | ✅ A/B, auto-rollback, kexec | ✅ `kairos-agent upgrade --source oci:` | ✅ update_engine + Nebraska. Sysexts via sysupdate. | ✅ Zincati/rpm-ostree. Derived images unofficial. | ✅ TUF A/B, but we'd host the TUF repo | ✅ systemd-sysupdate A/B, verified | ⚠️ RAUC/SWUpdate (Buildroot) or our symlink | ⚠️ apt plus our symlink |
| **Image tooling** | ✅ `imager` offline: ISO, raw, PXE, UKI, embedded config | ✅ AuroraBoot: ISO, raw, netboot, ProxyDHCP "pixie", UKI | ⚠️ Ignition/Butane. **ISO has no UEFI boot** per docs. PXE good. | ✅ `coreos-installer iso customize` gives unattended USB (BIOS and UEFI) | ❌ No ISO or PXE on metal. Metal variants dropped after K8s 1.29. | ✅ mkosi v27: disk, UKI, ISO (new), sysext. `mkosi burn`. | ⚠️ Bespoke | ✅ ISO remaster (livefs-editor, xorriso), NoCloud CIDATA |
| **Auto-join support** | ⚠️ Embedded config, maintenance-mode apply, SideroLink (BUSL Omni) | ⚠️ cloud-config. QR/p2p exist but are k3s-oriented and experimental. | ⚠️ Ignition config URL | ⚠️ Ignition embedded in ISO | ⚠️ `user-data.toml` | Ours to build (seed partition plus claim) | Ours | cloud-init user-data |
| **Licence** | MPL-2.0 (Omni and discovery BUSL) | Apache-2.0 | Mostly Apache-2.0 plus GPL kernel **[unverified per component]** | Mixed FOSS | Apache-2.0/MIT | Ours, over Debian packages | GPL/MIT mix | Mixed FOSS |
| **Maturity** | High overall. **k8s-less and host mode are brand new.** | CNCF Sandbox (Apr 2024). v4.3 monorepo (Sep 2026). Hadron init confusion. | CNCF Incubating (2024). 18-month LTS. | High. bootc transition still open. | High, but not for metal | mkosi mature. Our image would be new. **Incus OS ships this design (GA Nov 2025).** | Buildroot mature. Our image new. | Very high |
| **Effort for us** | M–L: extension, musl build, bundled tools, Talos config generation, `imager` CI | **S–M** | M: sysext trivial, installer UX weak on UEFI USB | M: SELinux labelling for runc and bun | L–XL | **M–L**: image recipe, repart, sysupdate, CI, Secure Boot | L–XL | **S** |
| **Key risks** | Experimental mode; vendor roadmap overlap; two PKIs; upgrade coupling | Upstream churn (monorepo, Hadron); persistence gotchas; Spectro-driven roadmap | UEFI ISO gap; `locksmithd` reboot coordination vs our council | SELinux denials; derived-image support | Metal abandoned | We own CVE cadence (Debian security feed) and the Secure Boot key story | We own kernel and CVEs | Drift returns; no A/B; slow apt install |

**Reading the table:**
- **Ubuntu autoinstall** is the cheapest way to *prove the join UX*.
- **Kairos core** is the cheapest way to get *A/B plus ISO/PXE*.
- **Own mkosi image** is the best *long-term product*: full control, no second API, and a published precedent in Incus OS. Incus OS uses Debian 13, mkosi, UKI plus Secure Boot, sysupdate A/B, the payload as a sysext, a daemon-only API with no shell, and a `SEED_DATA` seed partition.

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

| # | Step | Time |
|---|---|---|
| 1 | `relish image download` fetches and verifies the signed x86_64 appliance image (~0.5–0.8 GB **[estimate]**) | 3–6 min |
| 2 | `relish image write /dev/diskN` writes the image. Two sticks let two machines run at once. | 3–4 min |
| 3 | `relish cluster create home --bare-metal` generates the PKI, admin context and master-key backup prompt on the laptop | 1 min |
| 4 | For each of 5 machines: plug in, set the boot menu (F11/F12), boot the installer, auto-install to disk, reboot into `unclaimed`. About 5 min each; with 2 sticks, 3 rounds. Mini PCs often need Secure Boot turned off or our key enrolled the first time. | 15–20 min |
| 5 | `relish machines` lists 5 unclaimed machines. Compare fingerprints. | 2 min |
| 6 | `relish machines claim <first> --create`, wait for ready | 2–3 min |
| 7 | `relish machines claim --all` enrols 4 joiners concurrently. The council grows (up to 7 voters). | 3–4 min |
| 8 | `relish status`, `relish nodes`, then the five-minute tour (`docs/manual/08_five-minute-tour.md`) | 5–8 min |
| | **Total** | **≈ 34–48 min** |

- **PXE variant:** `relish image serve --pxe` runs a ProxyDHCP and HTTP boot server on the laptop, in the spirit of Kairos "pixie". It removes steps 2 and 4's stick shuffling and brings step 4 to about 8 min, but home routers and firmware vary. It comes after v1.

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
- an mkosi recipe (for example `image/mkosi.conf`), Debian 13, x86_64;
- package list shared with `guest-images.json` so the guest image and the appliance can't drift;
- systemd unit reused from `provision.rs::SERVICE`;
- no SSH;
- `systemd-repart` for data;
- installer mode ("boot from USB, install to disk") via a small repart-based first-boot installer or mkosi's ISO output.

**CI** (`.github/workflows/build.yml`):
- a `build-appliance-images` job next to `build-guest-images`, on native runners;
- `SourceDateEpoch`;
- the build record as a JSON artefact;
- the image digest signed into release metadata with the existing Ed25519 release keys, the way `guest-image-metadata.json` already is, and verified by `relish image download`.

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

### Phase 3: immutable A/B (~3–5 weeks)

- verity `/usr`, UKI, `systemd-sysupdate` slots.
- An `UpgradeManager` backend: `Symlink` (today) or `ImageSlot`. The orchestrator keeps its council-aware order and health gates. For OS releases each node step becomes "stage slot, drain, reboot, verify rejoin, else roll back".
- Open question: sysupdate verifies with GPG and SHA256SUMS, so either bun verifies Ed25519 first and feeds sysupdate a local verified source, or we also publish GPG signatures. **[design decision]**
- Secure Boot: our own db key, documented enrolment or disable instructions, and TPM2-sealed data-partition encryption as an option. That later unlocks `AttestationMode::Tpm`.

### Phase 4: polish (~2–3 weeks)

- aarch64 images (for Raspberry Pi-class boards through an overlay); PXE/HTTP boot server (`relish image serve`); G5 master-key rotation.
- Book chapter and manual "Bare metal" chapter.
- A physical-hardware qualification on real mini PCs.

### Optional: Talos extension (~2–3 weeks, 2027)

- A `siderolabs/extensions`-style repo with a host-mode service, a static musl `bun`, and bundled runc, iproute2 and util-linux.
- `relish image create --base talos` generates a k8s-less machine config with `SysctlConfig` and embedded config.

### Minimal first version

- x86_64 only, Debian 13 via mkosi, mutable root (A/B deferred).
- `bun` self-upgrade as today.
- Seed-mode join with node-bound tokens, plus G1 and G2.
- QEMU boot test in CI.
- **Proof point:** 5 VMs join from one seeded image and pass the tour.

That's roughly the Phase 1 scope, and it ships value before A/B.

---

## 6. Recommendation, risks and the first spike

**Recommendation:**
1. Build a **Reliaburger appliance image on Debian 13 with mkosi**, following the Incus OS design, with `bun` as the only service. Keep bun's own symlink self-upgrade for the agent, and add sysupdate A/B for the OS in Phase 3.
2. Make the join flow **claim over the LAN (option d)**, with **seed mode (option c)** as the MVP and the headless/PXE path.
3. Fix gaps G1 and G2 first. They block *every* bare-metal install, not just the appliance.
4. Treat Talos as a later "bring your own Talos" target, not the base.
5. If effort matters more than control, **Kairos core on a Debian base** is the fallback. It gives ISO/PXE/A/B out of the box, at the cost of persistence bind-mounts and upstream churn.

**Key risks:**
- **Master-key blast radius.** Any enrolment path that hands out `master.key` turns a token leak into a cluster compromise. G1 plus approval-gated enrolment keep that bounded, and G5 (rotation) makes it recoverable.
- **Secure Boot on consumer mini PCs.** Enrolling our own key is fiddly. The v1 docs must cover "disable Secure Boot" honestly.
- **Owning an OS.** We inherit Debian security-update cadence and have to rebuild and publish images promptly: a CI schedule plus a CVE feed check.
- **LAN realities.** mDNS across VLANs, DHCP address changes, and consumer routers without reservations.
- **Talos direction.** If Talos Containers GA (Dec 2026) makes k8s-less first-class, the Talos option gets cheaper. Re-check then.

**First spike (≈3 days, QEMU on a Linux box with KVM):**
1. **Talos track.** Build a Talos 1.14.1 image with `imager`:
   - a custom host-mode extension (bun, relish, a launcher, pinned runc, static iproute2 and util-linux);
   - embedded k8s-less config with `SysctlConfig user.max_user_namespaces`.

   Boot three VMs, form a cluster by hand, and run `scripts/demo/tour.sh`. Record:
   - eBPF attach to the root cgroup;
   - userns containers at uid 2e9;
   - whether owners survive `talosctl service ext-reliaburger restart`;
   - netem and `ss -K`;
   - an upgrade with the `/var` launcher.
2. **mkosi track.** Build a Debian 13 mkosi disk image with the `guest-images.json` packages plus the quickstart unit. Boot three VMs and run the same tour.
3. **Join track.** Prototype G1 (master-key fetch after join) and G2 (`admin_cidrs` and join window) against the mkosi VMs, and measure one seed-mode join end to end.

**Exit criteria:** tour passes on both tracks, owners-survive-restart holds on Talos, and a written go/no-go for Talos versus mkosi.

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
- That the perimeter blocks a LAN laptop in practice (derived from `src/firewall/rules.rs` and `src/bun/agent.rs`, not tested on hardware).
