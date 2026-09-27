# Plan: appliance spike (mkosi on Ubuntu, relish netboot, Wyse 3040 finale)

*27 September 2026. Companion to [the appliance OS research](2026-09-26-research-appliance-os.md) (draft PR #218). Docs only.*

> **Status: awaiting maintainer approval. Do not start building.** The spike runs after approval and after the release soak. It never runs on the Mac running the soak: stages S2–S5 need the **second Mac**, and S5 also needs the ten Wyse 3040s.

This file replaces `2026-09-27-plan-kairos-spike.md`. The maintainer chose the own mkosi image on Ubuntu with a netboot server built into `relish`. Kairos is now an alternative that was considered (research §10.1), and Talos is parked with no spike steps (§10.2).

## Write-up checklist (docs, done)

- [x] Rename this plan from the Kairos spike
- [x] Research note: new recommendation (mkosi, Ubuntu's kernel, `relish` netboot): §0, §5, §6
- [x] Research note: Kairos condensed to "considered, not chosen"; Talos spike and plan items dropped, one-paragraph note kept (§10)
- [x] Research note: weekly appliance builds and the node update path (§7)
- [x] Research note: netbooting VMs on a Mac (§8)
- [x] Research note: Dell Wyse 3040 constraints (§9)
- [x] This file: the spike stages
- [x] PR #218 title and body
- [ ] Maintainer approval of the spike (not started)

## What the spike must prove

1. One mkosi configuration builds a signed x86_64 **and** aarch64 appliance in GitHub Actions on hosted runners, unprivileged.
2. Machines netboot and install next to an unmodified home-router DHCP, through a ProxyDHCP we control. VMs on a Mac first, then real hardware.
3. The installer streams onto disk and never needs the image in RAM (2 GB machines).
4. An OS update rolls through sysupdate A/B slots, and a bad one falls back without hands.
5. Ten Dell Wyse 3040s form a cluster and run the tour within their RAM and eMMC budgets.

## Stages

| # | Stage | Where | Success criteria | Estimate |
|---|---|---|---|---|
| S1 | **Image in CI.** `image/mkosi.conf` for Ubuntu 26.04: generic kernel, the `guest-images.json` packages, bun and relish from the latest release, the unit and launcher, pruned firmware, EROFS `/usr` with verity, UKI, installer UKI, ISO, raw disk. A throwaway `appliance-spike.yml` workflow on `ubuntu-24.04` and `ubuntu-24.04-arm` with the mkosi action, `ToolsTree=yes`, `RepartOffline=yes`, no cache. On x86_64, boot the raw disk in QEMU+OVMF under KVM and wait for `/v1/health`. Sign with a **throwaway spike key**, never the release key. | GitHub Actions only | Both architectures build unprivileged; x86_64 boots to a healthy bun; build times, tools-tree size, and `/usr` and UKI sizes recorded; `/usr` under ~1 GiB compressed | 2 d |
| S2 | **One VM, Level 1** (research §8.4). On the second Mac: an aarch64 QEMU VM under HVF with 2 GB RAM and an 8 GB disk, PXE via QEMU's built-in TFTP into iPXE, chain the installer UKI over HTTP from the Mac, stream `/usr` to disk, reboot into bun. | second Mac | Installs and boots with 2 GB RAM; the installer's peak memory recorded; no image in tmpfs | 1.5 d |
| S3 | **Five VMs on a shared L2, Level 2, plus the x86_64 smoke run.** socket_vmnet shared network, macOS `bootpd` as the "router", a Lima VM running dnsmasq in proxy mode (stand-in for `relish image serve`) plus the HTTP side. Five aarch64 clients netboot at once. Form a cluster the manual way (`relish init`, join tokens as in `docs/linux-servers.md`, since the claim flow doesn't exist yet), then run the tour. Then one `qemu-system-x86_64 -accel tcg -cpu Westmere` client through the same path. | second Mac | Five installs next to an address-only DHCP; the tour passes; the x86_64 client installs and starts bun, however slowly; exact dnsmasq, Lima and QEMU flags written down | 2 d |
| S4 | **OS update and fallback.** Build a second weekly version in CI. On the S3 cluster, stage it by hand the way bun would (verify, `systemd-sysupdate` from a local directory, reboot), with a stand-in boot-check unit before `boot-complete.target`. Then ship a deliberately broken version (bun refuses to start) and watch boot counting fall back. | CI + second Mac | Good update: node rejoins on the new version, Raft, images and volumes intact. Bad update: node returns to the old slot within three boots with no manual action. bun `binary_dir` upgrade still works on top. | 1.5 d |
| S5 | **Ten Dell Wyse 3040s** (research §9.7). Update and configure all BIOSes (1.2.5, UEFI, PXE on, Secure Boot off). The Mac on wired Ethernet with the Wyse boxes behind an ordinary home router. Netboot all ten from the Mac: `relish image serve` natively if Phase 2b has started, otherwise dnsmasq in a bridged VM. Form the cluster (3 or 5 voters), run the tour, pull a power cord, measure for 24 h, roll one OS update. | second Mac + ten Wyse 3040s + switch and router | All ten install over PXE; the tour passes; per-node `MemAvailable` stays above ~300 MB under the tour load; eMMC bytes-written per day recorded; the reboot-hang quirk checked; one OS update rolls across the fleet | 2–3 d (BIOS work is ~1 h of it) |
| S6 | **Write-up:** `docs/qualification/<date>-appliance-spike.md` and `<date>-wyse-3040.md`, with measured numbers folded back into the research note, and a go/no-go for Phase 1. | anywhere | The maintainer can approve Phase 1 with numbers, not estimates | 0.5 d |

**Total: about 9.5–10.5 engineer-days.** S1 can start as soon as the spike is approved, because it needs only CI. S2–S5 wait for the second Mac, and S5 also needs the Wyse fleet set up on a wired LAN.

## Deliberately not in the spike

- The claim server, mDNS and `relish machines`. The spike joins by hand.
- `relish image serve` itself (Phase 2b). dnsmasq in proxy mode stands in for it, unless Phase 2b has started by S5.
- The weekly schedule, the OS signing environment and the channel pointer (research §7.4–§7.6). S1 uses a throwaway workflow and key.
- Secure Boot, TPM, and G1/G5.
- Talos and Kairos.

## Open questions for the maintainer

1. **OS signing:** is a separate Ed25519 OS key in a protected `os-weekly` environment acceptable? And should each weekly signing need a reviewer's click at first (research §7.5)?
2. **Channel:** GitHub Releases (`os-YYYY.WW.N` pre-releases, keep the last 8, ~16–24 GB) plus a signed `os-channel.json` on Pages. Or would you rather use a separate bucket?
3. **Command name:** `relish image serve` (as in the research) or `relish netboot`?
4. **Auto-updates:** notify-only by default with an explicit pin (research §7.6). Is that right, or do you want an opt-in maintenance window in v1?
5. **Wyse fleet:** how many units are 8 GB and how many 16 GB? Is there a spare DisplayPort monitor and keyboard for BIOS work, and a switch with ten free ports?
6. **Second Mac:** Apple silicon, and when is it free? Is installing `socket_vmnet` as a root service on it acceptable?
7. **Guest image:** should the quickstart guest move to Ubuntu 26.04 in the same release, so the guest and the appliance share one package list (research §3.1)?

## Constraints for whoever resumes this

- A release soak runs on the maintainer's Mac: no Lima VMs, no clusters, no local image builds there.
- Don't touch `.claude/worktrees/{dl,soak,train,research}`.
- Never squash or amend; push to `research/appliance-os`; PR #218 stays a draft.
