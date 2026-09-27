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

- [ ] Branch `feat/appliance-image` from `origin/main`, this plan, and a draft PR
- [ ] `image/`: mkosi configuration for Ubuntu 26.04 (`resolute`), x86_64 and aarch64
  - generic kernel, `linux-firmware-minimal` plus only the Wyse's firmware (`-realtek`, `-intel-graphics`)
  - the `guest-images.json` packages
  - bun and relish from the latest (staging) release, checked against its `SHA256SUMS`
  - `reliaburger.service`, and a boot-check unit that reports bun's health on the serial console
  - the Wyse 3040 `dw_dmac` blacklist, and DHCP on wired links
- [ ] `.github/workflows/appliance.yml`:
  - mkosi v27 action on `ubuntu-24.04` and `ubuntu-24.04-arm`, unprivileged, default tools tree, no cache
  - sizes in the step summary
  - throwaway Ed25519 signature over `SHA256SUMS`
  - 1-day artefacts
- [ ] x86_64 boot test: QEMU + OVMF under KVM on the hosted runner, pass only on bun's health marker on the serial console
- [ ] Iteration 2: `/usr` as EROFS with dm-verity (the A/B-ready layout, research §7.2) instead of a writable root
- [ ] Record sizes and timings here, and tick S1 in the spike plan
- [ ] Deferred to S2 preparation, not S1: the installer UKI (streaming `/usr` to disk), the ISO, the iPXE binaries

## Log

(Filled in as CI runs.)

## Picking up S2 on the lab Mac (M1, 64 GB, from Monday 28 Sep)

Research §8.4, level 1:
1. **Get the artefacts.** On the lab Mac run `brew install qemu gh`, then download the latest `appliance-aarch64` artefact from a green `appliance.yml` run: `gh run download <run-id> -n appliance-aarch64`. Artefacts expire after 1 day, so re-run the workflow if needed.
2. **Before the installer UKI exists** (S2's own work item), boot the raw disk directly to check the image on HVF:

   ```sh
   Q="$(brew --prefix)/share/qemu"
   zstd -d reliaburger-os_*_aarch64.raw.zst -o disk.raw
   cp "$Q/edk2-aarch64-vars.fd" vars.fd
   qemu-system-aarch64 -machine virt -accel hvf -cpu host -smp 4 -m 2048 \
     -drive if=pflash,format=raw,readonly=on,file="$Q/edk2-aarch64-code.fd" \
     -drive if=pflash,format=raw,file=vars.fd \
     -drive if=virtio,format=raw,file=disk.raw \
     -device virtio-net-pci,netdev=n0 -netdev user,id=n0 -nographic
   ```

   Expect `reliaburger: bun healthy` on the console.
3. Then carry on with S2 in the spike plan: iPXE via `-netdev user,tftp=…,bootfile=…` and the installer UKI.
