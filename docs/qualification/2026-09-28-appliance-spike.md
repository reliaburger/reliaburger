# Appliance spike, S1–S4 (virtualised): PASS, with one item open

27–28 September 2026. Spike stages S1 to S4 of the appliance OS (Ubuntu 26.04 built with mkosi, netbooted, A/B updates), run in GitHub Actions and on the lab Mac. **This is an interim record.** S5, the ten physical Dell Wyse 3040s, is the last step, and the go/no-go for Phase 1 waits for it.

## Verdict

PASS for everything that can be proven without the hardware, apart from one item.

- **The open item:** a rolling `relish upgrade` that swaps bun on the appliance. The appliance side is proven: an OS update carrying an older bun leaves the newer bun running (2026.40.41 → .42, bun 0.1.1 kept), and 0.1.0 refuses 0.1.1 the documented way. The swap itself waits for two release-signed versions with the same formats; 0.1.0 → 0.1.1 is a fresh-cluster upgrade by policy.
- The plan and the full log are in [`2026-09-28-plan-appliance-lab.md`](../plans/2026-09-28-plan-appliance-lab.md) (S2–S4) and [`2026-09-28-plan-appliance-image.md`](../plans/2026-09-28-plan-appliance-image.md) (S1).
- The lab is scripted in [`image/lab/`](../../image/lab/README.md).

| What the spike must prove (spike plan) | Result |
|---|---|
| 1. One mkosi configuration builds a signed x86_64 and aarch64 appliance in GitHub Actions, unprivileged | **Pass.** Both architectures, on hosted runners, in about 7 min per job, including the installer, iPXE and the boot tests. Both are boot-tested: x86_64 under KVM, aarch64 under TCG. |
| 2. Machines netboot and install next to an unmodified home-router DHCP, through a ProxyDHCP we control | **Pass in VMs.** Five aarch64 nodes at once, plus an x86_64 "virtual Wyse" through real firmware PXE. Real hardware is S5. |
| 3. The installer streams onto disk and never needs the image in RAM | **Pass.** At most 32 MiB anonymous memory; `MemAvailable` never below 1359 MiB of 2 GiB. |
| 4. An OS update rolls through sysupdate A/B slots, and a bad one falls back without hands | **Pass.** Rolled across five nodes with boot counting. A broken version fell back by itself after three tries. |
| 5. Ten Wyse 3040s form a cluster and run the tour within their RAM and eMMC budgets | **Not yet.** This is S5, on the hardware. |

## Builds and host

| | |
|---|---|
| Branch | `feat/appliance-image`, draft PR #259 |
| Workflow | `.github/workflows/appliance.yml`: mkosi v27, `ubuntu-24.04` and `ubuntu-24.04-arm` |
| Base | Ubuntu 26.04.1, `linux-image-7.0.0-34-generic`, systemd 259.5 |
| bun and relish | `staging-v0.1.0-36443508443-1` |
| Lab cluster versions | 2026.40.22 (install), 2026.40.23 (update), 2026.40.25 (broken on purpose) |
| iPXE | v2.0.0 (`12798ec2`), built in CI with `image/netboot/embed.ipxe` |
| Lab host | Apple M1 Max, 64 GB, macOS 15.7, QEMU 11.1.1 (Homebrew), no root |
| Signing | A throwaway Ed25519 key per CI run, as the spike plan says |

## Results

**S1: image in CI.**
- The UKI is 55 MB on aarch64 in 2026.40.23, after the stubble unwrap (61.5 MB in S1's last run). The `/usr` slot image is 320–350 MB zstd.
- The whole disk image is 400–460 MB zstd.
- The installer UKI is 99 MB (aarch64) and 135 MB (x86_64).
- x86_64 reaches `bun healthy` in 9–16 s under KVM, and aarch64 in 160–180 s under TCG.

**S2: one netbooted VM.**

| | x86_64 (CI, KVM) | aarch64 (lab Mac, HVF) |
|---|---|---|
| Netboot to installed | 14 s | 19 s (streaming took 4 s) |
| First boot to `bun healthy` | 8.7 s | 8.0 s |
| Installer anonymous memory, peak | 20 MiB | 30–32 MiB |
| Lowest `MemAvailable` during install | 1368 MiB | 1359 MiB |

**S3: five nodes on one LAN, and the virtual Wyse.**
- **Five aarch64 nodes netbooted and installed at once in 42 s**, next to an address-only DHCP server.
- **All five formed one cluster** from seeds passed as systemd credentials, and all five are council voters.
- **The tour passed:**
  - apply, ingress, `path` and `metrics`;
  - a 300 ms netem delay, observed at 300 ms;
  - `fault kill`, with a restart;
  - a node powered off: 4/4 alive, quorum healthy, replicas rescheduled;
  - `wtf`;
  - the node powered back on and rejoined: 5/5, 12 OK.
  - `relish dashboard` wasn't exercised, since `relish` ran headless.
- **The virtual Wyse** (x86_64, TCG, `-cpu Westmere`, 2 GiB, 8 GB):
  - OVMF PXE → ProxyDHCP → iPXE → installer: installed in 29 s;
  - `bun healthy` at 62 s into its first boot;
  - no AVX dependency showed up.

**S4: OS update and fallback.**
- **Staging** (`os-stage`, standing in for bun):
  - Ed25519 and SHA-256 checks, then `systemd-sysupdate` from a local directory, in 14 s per node;
  - the new `/usr` and its verity go into the inactive slot, carrying their partition UUIDs;
  - the UKI goes onto the ESP with `+3` tries.
- **The good update rolled across all five nodes**, followers first and the leader last:
  - each node was back about 20 s after rebooting, and `systemd-bless-boot` blessed it;
  - the cluster never dropped below 5 alive;
  - Raft continued (term 12, log 964 → 1008);
  - a volume's data survived on its node.
- **A second rolling update, to the newest image (2026.40.30), also went cleanly.** The new launcher moved bun onto `/var` on every node, and the cluster ended at 5/5 with `wtf` showing 12 OK.
- **An OS update never moves bun backwards.** 2026.40.41 shipped bun 0.1.1 and 2026.40.42 shipped 0.1.0. After the update to .42 the node still ran `bun-v0.1.1` from `/var/lib/reliaburger/bin`, and the boot check blessed it.
- **0.1.0 → 0.1.1 through `relish upgrade`:** refused, as `docs/releasing.md` says. The run paused on `incompatible cluster formats (state 46, required 44)`, `abort` ended it, and no node moved.
- **The broken version** (bun never starts):
  - three counted boots each failed the 300 s health gate;
  - systemd-boot then went back to the previous version on its own;
  - the node rejoined healthy about 15 min 40 s after the first try, with no hands.

## Defects found and fixed during the spike

| Defect | Fix |
|---|---|
| Ubuntu 26.04's arm64 `vmlinuz` is Canonical's stubble. systemd-stub refused it inside our UKIs ("Inner kernel image contains base relocations"). | A postinst script puts the inner zboot kernel in the UKI instead. |
| sysupdate's `Mode=0444` made the UKI read-only on FAT, so systemd-boot couldn't count down its tries and never blessed it. | `Mode=0644` |
| iPXE handed a leftover `autoexec.ipxe` to the installer UKI as an initrd, and systemd-stub then failed ("Already started"). | Free it unrun; chain with `--autofree`. |
| The ProxyDHCP answer races the router's offer inside iPXE. | Retry DHCP twice before falling back to `next-server`. |
| bun upgrades itself beside its own binary, which was on the read-only `/usr`. | Run bun from `/var/lib/reliaburger/bin` via a launcher that never downgrades it. |
| A machine that keeps network boot first would reinstall, or stop, on every boot (from Sidero Omni's provider). | The installer puts the disk first in `BootOrder` and sends already-installed machines back to their disk. iPXE gives up after about 30 s. |

## Deviations from the spike plan

- **No socket_vmnet or Lima.** The lab Mac had no root, so the shared L2 network is a QEMU hub inside the lab server VM. UDP multicast doesn't work between QEMU processes on macOS.
- **aarch64 clients under HVF start iPXE with `-kernel`.** Homebrew's EDK2 has no network boot under HVF, because it finds no RNG. Firmware PXE was exercised under TCG, on the virtual Wyse.
- **Nodes were configured with systemd credentials over SMBIOS** (`reliaburger.seed`, `ssh.authorized_keys.root`). There's no claim flow yet, and nodes have no login.
- **`openssh-server` is in the spike image**, started only by a credential, so S4 could stage by hand.

## Ready for S5

The appliance profile from research §9.2–9.3 is in the image since 2026.40.46: zram swap, the data partition mounted `noatime,compress=zstd:1`, journald capped at 32 MB, and the launcher and seeds keeping one old bun. Details are in the plan's log.

## Still to do

- **S5:** the ten Dell Wyse 3040s (research §9.7). It covers:
  - the BIOS setup;
  - PXE on the real Realtek NIC (the `snp` iPXE build is now the default);
  - whether firmware keeps the boot entry the installer creates;
  - `MemAvailable` under the tour, and whether zram gets used;
  - eMMC writes per day (`image/tools/fleet-measure.sh` samples both, and more, into CSVs);
  - the `dw_dmac` reboot hang;
  - one OS update across the fleet.
- **A rolling `relish upgrade` that swaps bun** on the appliance, once two release-signed versions share `protocol` and `state`.
- **Found on the way, to fix outside the spike:**
  - a seeded fleet couldn't grow past the addresses given to `seed-fleet.sh init`, since each seed fixes `bootstrap_peers` and the firewall drops anyone else before they join. Fixed on this branch: `bootstrap_peers` takes CIDRs and `seed-fleet.sh init --network` writes the LAN (needs a bun newer than 0.1.1);
  - a two-node cluster held `relish upgrade` in `UpgradingCouncil` with no reason shown, and `abort` refused it because it wasn't paused. Fixed on this branch: `start` and a cluster `rollback` refuse a two-voter council, and the leader logs a quorum hold.
- **S6:** fold the numbers into the research note, write `<date>-wyse-3040.md`, and give the go/no-go.
