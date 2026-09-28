# Plan: appliance image, spike stage S1 (image in CI)

*Started 27–28 September 2026. The spike is approved. This file is the working plan and log for **S1 only**: building the image in GitHub Actions. The spike as a whole (S1–S6), and the research behind it, live on the `research/appliance-os` branch (draft PR #218):*
- *[spike plan](https://github.com/reliaburger/reliaburger/blob/research/appliance-os/docs/plans/2026-09-27-plan-appliance-spike.md), the one source of truth for stages, success criteria and decisions;*
- *[research note](https://github.com/reliaburger/reliaburger/blob/research/appliance-os/docs/plans/2026-09-26-research-appliance-os.md): §7 covers weekly builds and updates, §8 the Mac lab, §9 the Wyse 3040.*

## Constraints

- **CI does all the building.** Nobody builds images or starts VMs on the maintainer's Mac while the release soak runs (until about 07:00 on Monday 28 Sep).
- **Respect CI storage** (PR #215):
  - no package caches;
  - artefacts kept for 1 day with `compression-level: 0`;
  - images kept small, with `/usr` under about 1 GiB compressed as the 8 GB Wyse budget requires.
- **Signing uses a throwaway key** generated in each run. It never touches the release key or the future OS key.
- Commit and push after every step. Never amend or squash; merge commits only.

## S1 checklist

- [x] Branch `feat/appliance-image` from `origin/main`, this plan, and a draft PR (#259)
- [x] `image/`: mkosi configuration for Ubuntu 26.04 (`resolute`), x86_64 and aarch64
  - generic kernel, `linux-firmware-minimal` plus only the Wyse's firmware (`-realtek`, `-intel-graphics`)
  - the `guest-images.json` packages
  - bun and relish from the latest (staging) release, checked against its `SHA256SUMS`
  - `reliaburger.service`, and a boot-check unit that reports bun's health on the serial console
  - the Wyse 3040 `dw_dmac` blacklist, and DHCP on wired links
- [x] `.github/workflows/appliance.yml`:
  - mkosi v27 action on `ubuntu-24.04` and `ubuntu-24.04-arm`, unprivileged, default tools tree, no cache
  - sizes in the step summary
  - throwaway Ed25519 signature over `SHA256SUMS`
  - 1-day artefacts
- [x] x86_64 boot test: QEMU + OVMF under KVM on the hosted runner, pass only on bun's health marker on the serial console
- [x] Iteration 2: `/usr` as EROFS with dm-verity (the A/B-ready layout, research §7.2) instead of a writable root
- [x] Record sizes and timings here, and tick S1 in the spike plan
- [ ] Deferred to S2 preparation, not S1: the installer UKI (streaming `/usr` to disk), the ISO, the iPXE binaries

## Log

**27 Sep, iteration 1 (writable ext4 root, the mkosi default layout).**
- Run 36347688705 failed while picking bun's release: the newest `v*` tag is an old `v0.0.1rc1` pre-release without `SHA256SUMS`. Now it takes published releases, else the newest staging candidate.
- Run 36347933911 failed because `systemd-repart` (which mkosi's default initrd installs) is in **universe** on 26.04. Enabled universe: it's built from the same systemd source as main. Also moved `dbus-broker` (universe) to `dbus` (main).
- Run 36348190616 built, but mkosi wrote to `image/`; set `OutputDirectory=`.
- **Run 36348503730: green on both architectures.**
  - Built from `staging-v0.1.0-36336992775-1` (bun 0.1.0).
  - Kernel `linux-image-7.0.0-34-generic`, Ubuntu 26.04.1.
  - Tools tree: Debian testing with systemd 261, **1.6–1.7 GB**, built in about a minute.
  - Whole job: about 4.5 min on x86_64 and 3 min on arm64.
  - **x86_64 boot under KVM with 2 GiB: `reliaburger: bun healthy (bun 0.1.0)` at 12.2 s kernel time.**

| | x86_64 | aarch64 |
|---|---|---|
| Disk image, zstd | 576.4 MB | 534.1 MB |
| Disk image, minimal raw size | 1.4 GB | 1.5 GB |
| UKI | **226.6 MB** | **213.4 MB** |
| Default initrd | 31.5 MB | 30.9 MB |
| Artefact (1 day) | 842 MB | 784 MB |

**Findings.**
- **The UKI is too big.** The kernel-modules initrd mkosi appends carries far more modules (and their firmware) than we need. That's two ESP slots' worth of the Wyse's 512 MiB, and slow over TFTP/HTTP. Next: an explicit `KernelInitrdModules=` list.
- Harmless build noise: tmpfiles can't resolve `kvm` and `tss` inside the build sandbox, and presets skip masked units.

**27 Sep, iteration 2a: a smaller UKI.** An explicit `KernelInitrdModules=` list covers virtio, NVMe, AHCI, MMC (the Wyse's eMMC), USB storage, dm-verity, and ext4/btrfs/erofs/vfat. **The UKI dropped from 226.6 to 73.0 MB (x86_64) and from 213.4 to 58.6 MB (aarch64).** Run 36348874813, still boots to healthy bun.

**27 Sep, iteration 2b: the A/B-ready layout.**
- **Build time** (`image/mkosi.repart/`):
  - the ESP, fixed at 512 MiB;
  - slot A of `/usr`, as EROFS with dm-verity, fixed at 1100 MiB, labelled `reliaburger_<version>`;
  - its verity hashes, 64 MiB;
  - the data partition: Btrfs holding `/etc` and `/var`, minimised, grown on boot.
- mkosi puts `usrhash=` on the UKI, so the UKI pins its own `/usr`, and the boot needs no `root=` (GPT auto-discovery).
- **First boot** (`image/mkosi.extra/usr/lib/repart.d/`): systemd-repart in the initrd grows the data partition and appends an empty slot B (`_empty`) for systemd-sysupdate.
- **Why the data partition carries `/etc`:** a `/usr`-only image would boot with an empty `/etc`, but Ubuntu keeps things in `/etc` that the system needs to work (linker paths, alternatives, certificates). The cost is that `/etc` changes in later images don't reach nodes that are already installed. That's a known gap to close before Phase 3's updates.
- Run 36349309427 built but picked a split partition for the boot test: mkosi now also splits out the ESP and data partitions, so the boot test takes the whole-disk file by name.
- Run 36349844223 booted, **but systemd-repart in the initrd said "No changes"**. The first-boot definitions were in the initrd (`mkosi.initrd.conf/`), yet repart printed only the four existing partitions, and I didn't find out why. Moving them to `/usr/lib/repart.d` in the image (the unit also looks in `/sysusr/usr/lib/repart.d`) fixed it.
- **Run 36350274906: green on both architectures, 8 GB disk, 2 GiB RAM.** On first boot repart grew the data partition from 109 MB to 5.2 GB and added both halves of slot B; the root filesystem grew too; bun was healthy at 11.6 s. Resulting layout (the Wyse budget, research §9.3):

```
vda1  512M vfat            esp                             /boot
vda2  1.1G erofs           reliaburger_2026.39.10          (/usr, verity)
vda3   64M DM_verity_hash  reliaburger_2026.39.10_verity
vda4  5.2G btrfs           reliaburger-data                /
vda5  1.1G                 _empty                          (slot B)
vda6   64M                 _empty                          (slot B verity)
```

| Run 36350274906 | x86_64 | aarch64 |
|---|---|---|
| Whole disk, zstd | 450.1 MB | 403.8 MB |
| `/usr` slot image, zstd (what an update ships) | 350.9 MB | 321.8 MB |
| `/usr` verity, zstd | 4.9 MB | 5.2 MB |
| UKI | 76.5 MB | 61.5 MB |
| Tools tree (not cached) | 1.6 GB | 1.7 GB |
| Job time | 5 min 8 s | 2 min 42 s |
| Artefact (1 day) | 883 MB | 792 MB |

**S1 is done**, apart from the items deferred to S2 preparation below. An update would ship roughly the UKI plus the `/usr` image: about 430 MB (x86_64) or 380 MB (aarch64) a week, well inside the ~1 GiB slot.

**28 Sep, first aarch64 boot on the lab Mac (HVF): the UKI didn't start.**
- Run 36350698283's `appliance-aarch64` (2026.39.12) verified: the throwaway signature and all four checksums were good.
- systemd-boot found the UKI, then systemd-stub refused it: `pe_kernel_check_no_relocation: Inner kernel image contains base relocations, which we do not support`.
- Cause: on 26.04 arm64, `linux-image`'s `vmlinuz` is Canonical's **stubble**, a devicetree-picking EFI stub with `.reloc` and 34 `.dtbauto` sections. The real kernel, an EFI zboot image with no relocations, sits in stubble's own `.linux`. mkosi v27 embedded the whole stubble binary as the UKI's `.linux`; it doesn't recognise stubble as a UKI because stubble has no `.sdmagic`. x86_64 is unaffected, and CI missed this because it has no aarch64 boot test.
- Check: I swapped the UKI's `.linux` for the inner zboot image by hand and put it on the ESP. **`reliaburger: bun healthy (bun 0.1.0)` came at 7.8 s**, and first boot grew the data partition to 5.2 GB and added both halves of slot B, as on x86_64.
- Fix: `image/mkosi.finalize` replaces any `usr/lib/modules/*/vmlinuz` that has a `.linux` section with that section's contents, before mkosi builds the UKI. Run against the real stubble `vmlinuz`, it produces the exact kernel that booted. The cost is stubble's devicetree matching, which only DT-only arm64 laptops need.
- Also: Homebrew's QEMU 11.1.1 has no `edk2-aarch64-vars.fd`. Its aarch64 firmware descriptor uses `edk2-arm-vars.fd` as the vars template, and the instructions below now use that too.

## Picking up S2 on the lab Mac (M1, 64 GB, from Monday 28 Sep)

Research §8.4, level 1:
1. **Get the artefacts.** On the lab Mac run `brew install qemu gh zstd`, then download the latest `appliance-aarch64` artefact from a green `appliance.yml` run: `gh run download <run-id> -n appliance-aarch64` (`gh workflow run appliance.yml --ref feat/appliance-image` starts a fresh one). Artefacts expire after 1 day. The artefact holds:
   - the whole disk, `reliaburger-os_<v>.raw.zst`;
   - the UKI, `.efi`;
   - the `/usr` slot image and its verity, `.usr.raw.zst` and `.usr-verity.raw.zst`;
   - the manifest;
   - `SHA256SUMS`, its `.sig`, and the throwaway public key.
2. **Before the installer UKI exists** (S2's own work item), boot the raw disk directly to check the image on HVF:

   ```sh
   Q="$(brew --prefix)/share/qemu"
   zstd -d "$(ls reliaburger-os_*.raw.zst | grep -v -e usr -e esp -e root)" -o disk.raw
   qemu-img resize -f raw disk.raw 8G      # the Wyse's eMMC size: first boot adds slot B and grows the data partition
   cp "$Q/edk2-arm-vars.fd" vars.fd && chmod u+w vars.fd   # QEMU's aarch64 vars template
   qemu-system-aarch64 -machine virt -accel hvf -cpu host -smp 4 -m 2048 \
     -drive if=pflash,format=raw,readonly=on,file="$Q/edk2-aarch64-code.fd" \
     -drive if=pflash,format=raw,file=vars.fd \
     -drive if=virtio,format=raw,file=disk.raw \
     -device virtio-net-pci,netdev=n0 -netdev user,id=n0 -nographic
   ```

   Expect `reliaburger: bun healthy`, then the disk layout (six partitions, slot B `_empty`) on the console. Quit QEMU with `Ctrl-a x`.
   Verify the signature first if you like: `openssl pkeyutl -verify -pubin -inkey spike-signing-key.pub.pem -rawin -in reliaburger-os_<v>.SHA256SUMS -sigfile reliaburger-os_<v>.SHA256SUMS.sig`, then `shasum -a 256 -c reliaburger-os_<v>.SHA256SUMS --ignore-missing`. macOS's LibreSSL may not do Ed25519 with `-rawin`; `brew install openssl@3` if it complains.
3. Then carry on with S2 in the spike plan: iPXE via `-netdev user,tftp=…,bootfile=…` and the installer UKI.
