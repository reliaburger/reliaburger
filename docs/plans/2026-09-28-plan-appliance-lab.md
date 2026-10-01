# Plan: appliance spike S2–S6 (the Mac lab, then the Wyse fleet)

*Started 28 September 2026, on the lab Mac (M1 Max, 64 GB, macOS 15.7). S1's plan and log are in [`2026-09-28-plan-appliance-image.md`](2026-09-28-plan-appliance-image.md). The spike plan and research note are on the `research/appliance-os` branch (draft PR #218). The lab's scripts and runbook are in [`image/lab/`](../../image/lab/README.md).*

## Constraints

- **Everything runs virtualised until the very last step.** The ten physical Wyse 3040s (S5) are the final check. Until then a "virtual Wyse" stands in: x86_64 under TCG with `-cpu Westmere` (SSE4.2 and AES-NI, no AVX), 2 GiB, an 8 GB disk.
- CI still builds every image. The lab Mac only runs QEMU, the lab server VM and `relish`.
- No root on the lab Mac. That rules out socket_vmnet, so the lab network is QEMU's own (see Findings).
- Signing stays a per-run throwaway key. The installer carries that build's public key.
- Commit and push after every step. Never amend or squash.

## Checklist

- [x] **S2, image side** (CI): the installer UKI (`image/mkosi.images/installer/`), iPXE v2.0.0 pinned and built in CI with `image/netboot/embed.ipxe`, `image/netboot/boot.ipxe`, and an x86_64 netboot install test under KVM.
- [x] **S2, one VM, Level 1** (lab Mac): aarch64 under HVF with 2 GiB and an 8 GB disk, QEMU's DHCP and TFTP, iPXE, the installer over HTTP from the Mac, then a reboot into bun. The installer's peak memory is recorded, and no image is held in RAM.
- [x] **S3, five VMs on one L2 network**: an address-only "home router", a separate ProxyDHCP and TFTP and HTTP server, and five aarch64 clients netbooting at once. Then the cluster is formed by hand (`relish init`, join tokens) and the tour runs.
- [x] **S3, the x86_64 smoke run**: the virtual Wyse installs through the same path with real firmware PXE, and starts bun.
- [x] **S4, good update**: a second CI version is staged by hand the way bun would (`os-stage`: Ed25519, SHA-256, `systemd-sysupdate` from a local directory) and rolled across the cluster. Boot counting blesses each node, and Raft, images and volumes stay intact.
- [x] **S4, bad update**: a deliberately broken version (bun won't start) falls back to the previous slot within three boots, with no hands.
- [ ] **S4, bun upgrade on top**: an OS update never moves bun backwards (passed, 1 Oct), and `relish upgrade` refuses 0.1.0 → 0.1.1 on the appliance as `docs/releasing.md` says it must (passed). A rolling `relish upgrade` that actually swaps bun waits for two release-signed versions with the same formats. 0.1.0 and 0.1.1 differ, and main has bumped them again.
- [ ] **S5, ten Dell Wyse 3040s**: the last step, on the hardware (spike plan, research §9.7).
- [ ] **S6, write-up**: `docs/qualification/<date>-appliance-spike.md` and `<date>-wyse-3040.md`.

## Log

**S2 in CI (28 Sep).**
- The installer is a `Format=uki` mkosi subimage: a small Ubuntu 26.04 that runs from RAM.
  - It reads `reliaburger.url=` from its command line, which iPXE passes (systemd-stub lets load options replace the built-in command line when Secure Boot is off). Failing that, it reads `LoaderDeviceURL`, which systemd-stub sets for UEFI HTTP Boot.
  - It checks this version's `SHA256SUMS` against the Ed25519 key baked into it, then streams `curl | zstd -d | dd oflag=direct` onto the largest fixed disk, hashing the download on the way. On a mismatch it wipes the disk.
  - It moves the backup GPT to the end of the disk, puts a boot entry for the disk first in `BootOrder`, and reboots.
- Sizes: the installer UKI is 99 MB on aarch64 and 135 MB on x86_64.
- **Run 36476145097, x86_64 under KVM: installed in 14 s, then `bun healthy` at 8.7 s on the second boot.**
- **Installer memory (run 36477886026): at most 20 MiB anonymous; the rest of the cgroup's 780 MiB peak is page cache; `MemAvailable` never fell below 1368 MiB of 2 GiB.** The image is never held in RAM.

**S2 on the lab Mac (28 Sep).** aarch64, HVF, 2 GiB, an 8 GB qcow2, QEMU's user network with `tftp=` and our iPXE, HTTP from `python3 -m http.server` on the Mac.
- Homebrew's EDK2 can't network-boot under HVF (see Findings), so iPXE starts through `-kernel`, like an iPXE USB stick.
- **Netboot to installed in 19 s (streaming took 4 s). The installed system was `bun healthy` 8.0 s into its first boot**, with slot B added and the data partition grown.

**S3 (28 Sep).**
- **Lab network:**
  - A server VM (Ubuntu 26.04 arm64) holds a QEMU hub.
  - Its bridge `br0` at 192.168.105.1 plays the home router: dnsmasq with addresses only, reservations .101–.108 by MAC, and NAT out through QEMU's user network.
  - A network namespace at .2 is the netboot server: dnsmasq as ProxyDHCP and TFTP (`port=0`, `dhcp-range=…,proxy`), plus HTTP on port 8080.
  - `relish` runs on the server VM, which is in every node's `operator_cidrs`.
- **Five aarch64 clients netbooted and installed at once in 42 s.** Each took its address from the router and its boot script from the proxy. Each installer used at most 32 MiB of anonymous memory, and `MemAvailable` stayed above 1359 MiB.
- **Cluster by hand, with no bun change:**
  - `relish init` runs on the Mac.
  - A helper adds a hashed admin token to the security bootstrap, as quickstart does.
  - Node 1's seed (node.toml, master key, identity, bootstrap) is a tarball passed as the systemd credential `reliaburger.seed` over `-smbios type=11`. `reliaburger-seed.service` unpacks it once, and `start` then runs bun clustered.
  - Joiners get their identity from `relish join` on the lab server, which mints a token and pins the root CA fingerprint.
- **All five nodes joined as council voters.** node-01 was healthy 7.7 s into its seeded boot, and the joiners at 17–23 s.
- **The tour passes:**
  - `apply` of podinfo, then ingress on two nodes;
  - `path` (eBPF service map, 1/1 connects);
  - `metrics`;
  - a 300 ms netem delay, where `path` showed 3/3 probes at 300 ms;
  - `fault kill`, after which the frontend restarted;
  - node-03 "powered off" (QEMU killed): 4/4 nodes alive, council healthy, replicas rescheduled;
  - `wtf` was clean apart from the tour's own faults;
  - node-03 powered back on rejoined: 5/5, and `wtf` showed 12 OK, 0 warnings.
  - `relish dashboard` was skipped: it opens a browser, and `relish` runs headless on the server VM.
- **The virtual Wyse** (x86_64, TCG, Westmere, 2 GiB, 8 GB):
  - real OVMF PXE → ProxyDHCP → our iPXE → installer: **installed in 29 s**;
  - the same QEMU process then booted the disk and reached **`bun healthy` at 62 s kernel time**, with slot B added.
  - No AVX problems.

**S4 (28 Sep).** v1 was 2026.40.22 (the cluster above), v2 2026.40.23 and the broken build 2026.40.25, all from one commit via `workflow_dispatch`.
- **`os-stage` on node-05:** a signed `SHA256SUMS` and three verified downloads, then `systemd-sysupdate`. It took 14 s:
  - `/usr` and its verity went into slot B;
  - the new UKI went onto the ESP as `reliaburger-os_2026.40.23+3-0.efi`;
  - `@u` carried the partition UUIDs over, so the new UKI's `usrhash=` found its slot.
- **The first reboot ran 2026.40.23, but nothing was blessed.** sysupdate's `Mode=0444` sets the FAT read-only attribute on the UKI. systemd-boot then can't rename it to count down its tries, so it never set `LoaderBootCountPath`. Fixed with `Mode=0644`. With the file made writable, the next boot ran `+2-1`, reached `boot-complete.target`, and `systemd-bless-boot` renamed it to `reliaburger-os_2026.40.23.efi`.
- **Rolled across all five nodes** (followers first, the leader last). Each was back, blessed, about 20 s after its reboot, and the cluster never dropped below 5 alive. Raft continued (term 12, log 964 → 1008, 5 apps), and `wtf` showed 12 OK.
- **Volumes:** the `keeper` app's volume on node-01 kept its data through the update. But while node-01 rebooted, the scheduler moved `keeper` to node-03 with a fresh volume. Managed volumes don't pin an app to its node; that's a bun question for Phase 3's drain-before-reboot, not an OS one.

- **Bad update, node-05 (28 Sep, 21:31–21:47 UTC).** `os-stage` put the broken 2026.40.25 into the oldest slot (2026.40.22's), keeping the running 2026.40.23. Then:
  - three counted boots of 2026.40.25 each ended with `reliaburger: bun not healthy after 300 s` and a reboot from the boot check;
  - the UKI ran out of tries (`reliaburger-os_2026.40.25+0-3.efi`), and systemd-boot picked 2026.40.23 by itself;
  - node-05 was `bun healthy` 7.6 s into that boot and back in the cluster (5/5, council healthy).
  - **From the first try to the fallback: about 15 min 40 s, with no hands.** Nearly all of it is the boot check's 300 s timeout, three times. A shorter timeout on counted boots would bring it down.
- **bun upgrade on top.** `relish upgrade start --binary` reached every node and was refused, as it should be, for lack of an operator countersignature (`[upgrades] external_signing_key`, docs/manual/12_operations.md). Two things stood out:
  - **bun upgrades itself by writing `bun-vX.Y.Z` beside its own binary**, and on the appliance that was the read-only `/usr`. The `start` launcher now runs bun from `/var/lib/reliaburger/bin` and installs the image's bun there only when it's newer. Tested on Linux with stand-in binaries: first boot; the same version; a newer image; an older image; and a bun that upgraded itself past the image. Neither path moves bun backwards.
  - **Every staging candidate is v0.1.0**, and `start` refuses the same version with different bytes. So the end-to-end run waits for the next release-signed bun. The lab's `node-toml.py` takes `OPERATOR_KEY` for it.

- **A second rolling update, to 2026.40.30 (run 36488707344), the newest image.** It rolled across all five nodes, followers first and the leader last:
  - each node came back blessed, and the exhausted 2026.40.25 left node-05's ESP;
  - every node's `BootOrder` starts with the installer's "Reliaburger OS" entry;
  - **the launcher's first run moved bun to `/var/lib/reliaburger/bin/bun-v0.1.0`** (it logged "bun 0.1.0 from the image is now active");
  - the cluster ended at term 18, log 2007, 5/5, and `wtf` showed 12 OK, 0 warnings.

**bun upgrade on top, with real releases (30 Sep – 1 Oct).** Cluster `upg` on 2026.40.38 (bun 0.1.0), seeded from an RBSEED stick, with relish 0.1.0 on the lab server and the v0.1.1 release countersigned with the lab operator key.
- **Two nodes held forever.** `relish upgrade start --binary bun-v0.1.1` recorded the run, and it stayed in `UpgradingCouncil` with both nodes `Pending`. `live_quorum_headroom_ok` won't take one of two voters down, which is right, but nothing told the operator so. `status` showed no reason, and `abort` refused because the run wasn't paused. Only adding a third node moved it.
- **A third node couldn't join.** Its bun waited for replicated API credentials and timed out, again and again. The seeds from `seed-fleet.sh init` carry `bootstrap_peers` for the addresses listed then (.106 and .107), and node-01's perimeter firewall dropped .108 before it had joined. After adding .108 to `bootstrap_peers` on nodes 1 and 2 and restarting their buns, node-03 joined (3/3 in the council).
- **Then 0.1.0 refused 0.1.1, as documented.** node-02 answered the directive with `409 incompatible cluster formats: received protocol 27, state 46, required protocol 27, state 44`. The run paused, `abort` ended it, and all three nodes stayed on `bun-v0.1.0`. `docs/releasing.md` ("Upgrading from 0.1.0") says exactly this.
- **An OS update doesn't downgrade bun.** Runs 36795751727 (#41, bun 0.1.1) and 36795766209 (#42, bun 0.1.0) built a pair where the newer OS carries the older bun. Node 9:
  - netboot-installed 2026.40.41, and the launcher logged `bun 0.1.1 from the image is now active (was none)`;
  - `os-stage` put 2026.40.42 in the spare slot, and the node rebooted into it, blessed, 25 s later;
  - `/usr/lib/reliaburger/bin/bun` was 0.1.0, but the launcher left `bun -> bun-v0.1.1` and logged nothing, and bun ran from `/var/lib/reliaburger/bin/bun-v0.1.1`;
  - the boot check passed: `bun healthy (bun 0.1.1 (77bace5)) on OS 2026.40.42`.
- **Still open:** a rolling upgrade that actually swaps bun. It needs two consecutive release-signed versions with the same `protocol` and `state`. main is already at 28/47 against 0.1.1's 27/46, so 0.1.1 → 0.1.2 won't roll either.

**The appliance profile, ahead of S5 (1 Oct).** Research §9.2–9.3 asks for it on a 2 GB, 8 GB eMMC machine. Build 2026.40.46 (run 36808394596) has it, checked on a lab VM:
- **zram swap:** `systemd-zram-generator`, half of RAM with zstd (973 MB on a 2 GiB VM, priority 100).
- **The data partition** mounts `noatime,compress=zstd:1` (`rootflags=` on the command line; Btrfs adds `discard=async` itself).
- **journald** is capped at 32 MB on disk and 16 MB in RAM.
- **Old buns are pruned.** The launcher keeps the active bun and the one it replaced, and deletes the rest with their `.sig`, so a 100 MB binary per OS update can't fill the eMMC. Tested with stand-in binaries through seven image versions and a bun that upgraded itself: it never downgraded, and it never touched a bun newer than the image's.
- **The seeds' `node.toml`** keeps one old bun (`[upgrades] retain_versions = 1`) and two days of unreferenced images (`[images] gc_retain_days = 2`). `[metrics]` and `[logs] max_storage_mb` were left alone, because they only prune files already exported; the V02 soak's 7–11 MB per 12 h means a week of each fits.
- **`image/tools/fleet-measure.sh`** samples every node over SSH into one CSV per node, for S5's 24-hour run: `MemAvailable`, swap and zram, bun's RSS, bytes written to the system disk (`/sys/block/<disk>/stat`), data partition use, load, the hottest thermal zone, and the OS and bun versions. Checked against the lab VM.

**The bare-metal preview (29–30 Sep).** It covers the manual chapter `docs/manual/14_appliance.md`, the book chapter `docs/book/15a-becoming-the-os.md`, and the tools in `image/tools/`.
- **Seeds for real machines:** real hardware has no SMBIOS channel for the lab's seed credential. So `reliaburger-seed` now also reads a USB stick labelled `RBSEED`, with `seeds/<mac>.seed` for one of the machine's NICs, so one stick serves a whole fleet. Without it, a physical node could install but never join a cluster, and S5 would have been blocked.
- **The guide, followed end to end in the lab:**
  1. `image/tools/netboot-server.sh` ran in the netboot namespace in place of the lab's own services, and served two netboot installs (16 s each). The installer prints each NIC's MAC and address.
  2. `seed-fleet.sh init` ran on the Mac. The first run compiles the seed-admin helper, which took 8 min in release, so the helper now builds in debug.
  3. The stick seeded node 1.
  4. `seed-fleet.sh join` ran on the lab server.
  5. The same stick, now holding both seeds, seeded node 2. The `home` cluster came up 2/2, and `wtf` showed 12 OK.
- **SSH from a seed** (`seed-fleet.sh init --ssh-key`, spike only, so OS updates can be staged on real machines):
  - Node 8 booted with no SSH credential. The seed installed root's key, `ssh.socket` started, and root SSH worked on 2026.40.33.
  - CI's six boots, with no key and no seed, started sshd zero times.
- **Bugs found and fixed:**
  - dnsmasq serves TFTP as an unprivileged user, and couldn't read `netboot-server.sh`'s 0700 temp directory.
  - The lab server VM has no NTP, and its clock fell behind whenever the Mac slept. The nodes use timesyncd and stayed right.

**The guide on two virtual Wyses (30 Sep).** Both are x86_64 under TCG, `-cpu Westmere`, with 2 GiB and an 8 GB disk. Everything came from `netboot-server.sh` and `seed-fleet.sh --ssh-key`, with no SMBIOS credentials at all.
- **Wyse 1, disk first.**
  - Real OVMF PXE → ProxyDHCP → **iPXE `snp.efi`** (its first real-firmware run) → installer, which took 25 s.
  - The installer's `efibootmgr` entry (Boot0008, "Reliaburger OS") came first, and the firmware booted it.
  - The stick seeded node-01 with root's SSH key. bun was healthy at 67 s.
- **Wyse 2, network first.**
  - It installed (30 s), but OVMF still put the network first, because QEMU's boot order overrides the one the OS writes.
  - So it netbooted again. The installer found the existing install and logged `/dev/vda already holds Reliaburger OS; booting it (boot entry 0008, now first)`. It then set `BootNext` to the disk and rebooted.
  - The stick seeded node-02. bun was healthy at 82 s.
  - The `wyse` cluster came up 2/2, and `wtf` showed 12 OK.
- **Wyse 2, network first, with the netboot server off.** The firmware tried PXE over IPv4 and IPv6, then HTTP Boot over IPv4 and IPv6, and each failed. It then booted Boot0008, and bun was healthy. Under TCG this took about 5 minutes, mostly firmware timeouts.
- **The manual's "Updating the OS" section, run word for word on both Wyses.** It moved them from 2026.40.33 to 2026.40.34 (run 36664269208), node-02 first, with `os-stage` over the seed's SSH key and the new run's spike key. Both booted `+3-0`, both were blessed, and the cluster stayed 2/2 with 12 OK. Following it caught one error in the manual, now fixed: its URLs include `/x86_64/`, but it said to serve the artefact directory itself.
- **What this means for S5:** if a Wyse keeps network first, every boot either goes through the installer (about 30 s, then back to the disk) or waits for the firmware's network timeouts. So put the disk first in the BIOS.

## Findings

- **Stubble**: see S1's log. The unwrap has to be a *postinst* script, because for `Format=uki` mkosi saves the kernel aside before finalize scripts run. Without it, the aarch64 installer couldn't start (`Error 0x7f048281`).
- **Homebrew's `edk2-aarch64-code.fd` (edk2-stable202408) has no network boot under HVF on M1.** It finds no RNG, so its network stack doesn't load. Firmware PXE works under TCG. Under HVF, the lab starts iPXE with `-kernel`.
- **UDP multicast (`-netdev dgram`) doesn't work between QEMU processes on macOS** (`EADDRNOTAVAIL` on every send). The lab uses a hub in the server VM's QEMU with a unix-socket stream port per client.
- **QEMU's stream server doesn't deliver frames to a second client on a reused socket.** Symptom: the node's `DHCPDISCOVER` gets an offer that never arrives, and ARP fails. Every QEMU start takes a fresh slot (40 per server start).
- **iPXE hands every image it still holds to the next EFI binary as an initrd.** systemd-stub then fails with `Error registering initrd: Already started`. It hit on x86_64, where iPXE had fetched an `autoexec.ipxe` from the TFTP server and never ran it. `embed.ipxe` now frees it, and both scripts chain with `--autofree`.
- **The ProxyDHCP answer races the router's offer.** iPXE only waits for proxy offers when the router's offer says `PXEClient`, which a home router's doesn't. `embed.ipxe` retries DHCP twice for it before settling for `next-server`.
- **`network-online` waited for every link.** Both images now wait for any link, for mini PCs with a spare NIC.

### From Sidero Omni's bare-metal provider

`siderolabs/omni-infra-provider-bare-metal` was read for ideas only; no code was copied. It encodes few vendor quirks: its main defence against boot loops is server-side state plus BMC one-shot PXE, which the Wyse fleet doesn't have. Taken so far:
- **After installing, the disk comes first in the boot order.** A machine that network-boots onto an existing install goes back to its disk instead of failing.
- **`embed.ipxe` gives up after about 30 s** and exits to the next boot option.
- **CI builds iPXE's `snp.efi`** (the firmware's NIC drivers, the default) as well as `ipxe.efi`.
- **`boot.ipxe` works out its own server**, for firmware with iPXE built in.
- The ideas for `relish netboot`'s Rust ProxyDHCP are in the list below.

For `relish netboot` (Phase 2b):
- **Ports:** answer on UDP 67 and on 4011. Needs root or `CAP_NET_BIND_SERVICE` on Linux; unverified on macOS.
- **The reply:**
  - `yiaddr` 0, option 54 and `siaddr` set to the server;
  - echo option 97 when it's present;
  - option 60 set to `PXEClient` or `HTTPClient`;
  - the file name in both option 67 and the BOOTP `file` field;
  - option 43 with PXE discovery control 8.
- **Architecture from option 93:** 6, 7 and 9 are x86_64 EFI; 11 is arm64; 16 and 19 are HTTP Boot and get a URL.
- **Remember installed machines by MAC and SMBIOS UUID.** Serve `boot.ipxe` over HTTP with those as query parameters, so installed machines get `exit`.
- **TFTP:** RFC 2347–2349, blksize capped by the MTU, and a client aborting right after OACK is normal.
- **HTTP:** serve `.efi` as `application/efi` with a `Content-Length`.

## Known gaps

- `/etc` lives on the data partition, so a later image's `/etc` doesn't reach installed nodes (S1). The seed and SSH credentials land in `/etc` too.
- A rolling bun upgrade on the appliance hasn't swapped a binary yet: no two release-signed versions share formats (see S4).
- A seeded fleet couldn't grow past the addresses given to `seed-fleet.sh init`, because each seed fixes `bootstrap_peers`. Now `bootstrap_peers` takes CIDRs (cluster and API ports only, all mTLS), and `seed-fleet.sh init --network` writes the LAN; `join` enrols nodes appended to `fleet` later. The released buns (≤ 0.1.1) take bare addresses only, so `--network` waits for the next release.
- ~~A two-node cluster holds a bun upgrade in `UpgradingCouncil` indefinitely.~~ Fixed: `start` and a cluster `rollback` now refuse a two-voter council, and the leader logs a quorum hold once when it begins.
- The fallback takes about 16 minutes, because the boot check waits 300 s each try.
- The lab's SSH (`openssh-server`, started only with an `ssh.authorized_keys.root` credential) exists so S4 can stage by hand. Phase 1 drops it, once bun stages OS updates itself.
- Every spike build signs with its own throwaway key, so staging a later build needs that build's public key passed to `os-stage` explicitly.
- Under Secure Boot, systemd-stub drops the command line iPXE passes, so the installer would need another way to find its server.
