# Plan: spike S5, the ten Dell Wyse 3040s

*Written 1 October 2026, before the hardware run. The checklist is research §9.7; the commands are [`docs/manual/14_appliance.md`](../manual/14_appliance.md), which ran word for word on two virtual Wyses (see the [lab plan's log](2026-09-28-plan-appliance-lab.md)). What to record at each step is in **Record**; it all goes into `docs/qualification/<date>-wyse-3040.md`.*

## Before the day

- **An image with the appliance profile:** 2026.40.46 or later (zram, `noatime,compress=zstd:1`, journald at 32 MB, one old bun kept). Start a fresh build so the artefacts haven't expired: Actions → Appliance image → Run workflow, on `feat/appliance-image` (or `main` once merged). Note the run ID.
- **A second build to update to**, started after the first: the OS update in step 7 needs a newer version. Run it once more with `broken_bun` ticked for the fallback test.
- **A Linux machine on the Wyse network** for `netboot-server.sh` (`apt install dnsmasq-base python3`), on wired Ethernet. The lab Mac can't bridge a VM onto a physical LAN without root.
- **The laptop** with `relish` 0.1.1, `gh`, `rustup`, `mtools` and a USB stick for the seeds.
- **A router** with a DHCP reservation per Wyse. The MAC is on the label under each unit, or in the BIOS.
- A DisplayPort monitor and a USB keyboard: the 3040 has no serial port.

## 1. BIOS, all ten units

F2 at power-on (default password `Fireport`):
- update to BIOS 1.2.5 if it's older;
- UEFI boot, legacy (CSM) off. Once off, it can't be turned back on;
- the network stack and UEFI PXE (IPv4) on;
- Secure Boot off (it ships off);
- the eMMC first, the network second. A blank eMMC falls through to PXE;
- optionally, power on after AC loss.

**Record:** the BIOS version found, the exact menu names, the PXE boot entry's name, and the minutes per unit.

## 2. Serve and seed

```sh
# Linux box: the first build's x86_64 artefact, unpacked
gh run download <run> -R reliaburger/reliaburger -n appliance-x86_64 -D art/x86_64
sudo image/tools/netboot-server.sh "$PWD/art" eth0

# Laptop: all ten nodes now (released buns can't take --network yet)
image/tools/seed-fleet.sh init ~/wyse --cluster wyse --operator <laptop IP> \
  --ssh-key ~/.ssh/id_ed25519.pub <MAC1>@<IP1> ... <MAC10>@<IP10>
```

Then write the stick as the manual's "Create the cluster" says, once node 1's seed exists.

## 3. Netboot all ten at once

Power them all on together. Watch the netboot server's log and one console.

**Record:** the time from power-on to the last `installed … in N s`; the installer's peak memory line; whether `target` is `/dev/mmcblk0` (never `mmcblk0boot0`/`boot1`); and the NIC name and driver (`r8169`).

## 4. Form the cluster

Follow the manual's "Start node 1" and "Add the others" (`seed-fleet.sh join ~/wyse`).

**Record:** the time from node 1 up to `relish nodes` showing 10/10; who is in the council (the reconciler caps it at seven voters); `relish wtf`.

## 5. Tour, then pull a cord

The [five-minute tour](../manual/08_five-minute-tour.md) from `relish apply`, with the manual's two differences (ingress on port 80 of every node; pull a power cord instead of `relish local stop`). Pull one worker's cord, wait, plug it back.

**Record:** the time per tour step against the laptop quickstart (deploy and image pull especially); how long the pulled node took to come back and rejoin; whether the reboot hung (`dw_dmac`: a clean `systemctl reboot` on two units counts).

## 6. Measure for 24 hours

```sh
image/tools/fleet-measure.sh ~/wyse 300 288    # every 5 minutes, 24 hours
```

`~/wyse` is either the claim directory (`relish machines claim ~/wyse …`), whose `fleet.json` names the nodes (`wyse-1` to `wyse-10`), or the `seed-fleet.sh` directory from step 2 (nodes `node-01` to `node-10`). `fleet-measure.sh --relish ~/wyse 300 288` takes the nodes from `relish nodes --output json` instead, and only writes into `~/wyse`. The script logs in as root over SSH, so the machines need the key in their seeds: `relish machines claim --ssh-key ~/.ssh/id_ed25519.pub`, or `seed-fleet.sh init --ssh-key`, and a lab image (only lab images start sshd).

Leave the tour's apps running for the first 12 hours, then remove them. One CSV per node lands in `~/wyse/measure/`.

**Record, per node and for the fleet:**
- `MemAvailable`: the minimum, and the median idle and loaded. The research's budget is 1.0–1.3 GB left for workloads.
- Swap and zram use: whether it was used at all, and the compression ratio (`zram_orig_bytes / zram_compr_bytes`).
- bun's RSS over time, council members against workers.
- eMMC bytes written per day (the difference in `disk_written_bytes`), idle against loaded, and the years to 300 × 8 GB at that rate.
- Data partition use at the end.
- `temp_max_mc`: whether a fanless unit throttles under the tour.

## 7. OS update and fallback

Stage the second build across the fleet as the manual's "Updating the OS" says, one node at a time, node 1 last. Then stage the `broken_bun` build on one worker and leave it.

**Record:** the time per node from `systemctl reboot` to blessed; that the cluster stayed quorate; for the broken one, the time to fall back on its own (about 16 minutes in VMs: three 300 s health checks) and whether the Wyse firmware kept counting tries.

## 8. Write it up (S6)

`docs/qualification/<date>-wyse-3040.md` with the numbers above, then tick S5 and S6 in the [lab plan](2026-09-28-plan-appliance-lab.md), fold the numbers into research §9, and give the go/no-go for Phase 1.

Stop and write down what happened if a step fails: the failure is the result.
