# Plan: spike S5, the Dell Wyse 3040s

*Written 1 October 2026, before the hardware run, and rewritten on 3 October around the 0.3.0 train (`release-1-3-0`): `relish netboot` from the Mac, the Raspberry Pi as the lab's router, and claiming over the network instead of `netboot-server.sh` and `seed-fleet.sh`. The checklist is research §9.7; the lab and the reasons behind it are the [Wyse lab plan](2026-10-02-plan-appliance-wyse-lab.md); the commands are the [manual's appliance chapter](../manual/15_appliance.md). What to record at each step is in **Record**; it all goes into `docs/qualification/<date>-wyse-3040.md`.*

> **7 October 2026:** the exit test now gates on **three** Wyses, run on a lab build of the train, and gates the v0.3.0 tag; the go/no-go thresholds are in the [lab plan's decisions](2026-10-02-plan-appliance-wyse-lab.md#decisions-7-october-2026). The steps below still work for ten, and the lab can run them all; the gate counts three. The "formal run" on a signed channel no longer comes before the tag, since the weekly publish stays off until v0.3.0 is promoted.

## The lab

```
home Wi-Fi ── wlan0  Raspberry Pi  eth0 10.77.0.1 ── Netgear JGS524E ─┬─ M2 MacBook Pro, USB-C Ethernet (en7) 10.77.0.2, relish netboot
                     DHCP, DNS, NTP, NAT                              ├─ wyse-1  10.77.0.11
                                                                      ├─ …
                                                                      └─ wyse-10 10.77.0.20
```

- **The Pi** (a Pi 5 or Pi 4 with 4 GB) is the lab's router: DHCP with a reservation per Wyse and one for the Mac's adapter, DNS, NTP, and NAT out through its Wi-Fi. Set it up with [`image/lab/pi/README.md`](../../image/lab/pi/README.md). It answers nothing about booting.
- **The Mac** runs `relish netboot`, a ProxyDHCP beside the Pi's DHCP: it adds the boot file to the conversation and never hands out an address. That's the topology CI tests (`image/tests/relish-netboot-install.sh`). There's no `--dhcp` mode, and the lab doesn't need one (maintainer, 3 October 2026).
- **The switch**, a Netgear JGS524E, with spanning tree off. STP's listening delay outlasts PXE's DHCP timeout. Turn "green Ethernet" (EEE) off too if links flap.
- **Ten Wyse 3040s**: x86_64, 2 GB, 8 GB eMMC, UEFI PXE on a Realtek RTL8111/8168. Some still hold ThinOS, some are blank.

## Two kinds of run

| | Lab runs | The formal S5 run |
|---|---|---|
| Images | A CI lab build of the train (throwaway key per run) | The signed channel, once 0.3.0 publishes an OS release |
| relish on the Mac | Built from the lab build's commit | The 0.3.0 release |
| `relish netboot` | `art --key art/x86_64/spike-signing-key.pub.pem` | `os`, no `--key` |
| Claim keys | `--trust-lan` | All ten compared on the monitor |
| sshd, so `fleet-measure.sh` and the fallback test | Yes (lab profile) | No: published images have no sshd |
| OS update | To the run's own next version, over a lab channel | To the next published release, if one exists by then |

Do the lab runs first. Each step below gives both where they differ.

## Before the day

- **The Pi**, set up and checked as its README says, with all ten reservations and the Mac's filled in. Label each Wyse with its MAC, its address and its node number.
- **The Mac:**
  - `relish`. For a lab run, build it from the commit the lab build came from, because the image's bun is built from that commit (the appliance workflow's `bun` job): `git checkout <sha>` then `cargo build --release --bin relish`, and use `./target/release/relish`. For the formal run, the 0.3.0 release.
  - `gh`, and `python3` for serving the next version.
  - The adapter's name: `networksetup -listallhardwareports`, the `Device:` under the USB Ethernet port, say `en7`. `ipconfig getifaddr en7` should print `10.77.0.2` once the Pi is up, and `route -n get default` should still name the Wi-Fi.
  - The application firewall: `/usr/libexec/ApplicationFirewall/socketfilterfw --getglobalstate`. If it's on, allow relish (manual, "Serving from a Mac"), and `python3` for step 7.
  - Internet Sharing off, and no VM with shared or host networking running: either may hold UDP 67. `sudo lsof -nP -iUDP:67` should print nothing.
  - An SSH key for the lab runs: `~/.ssh/id_ed25519.pub`.
- **A lab build** (lab runs only): 2026.40.46 or later, from a green run of the train. Dispatching the appliance workflow needs it on `main`, so until 0.3.0 merges, take the newest green pull request run into `release-1-3-0` and download it the same day: a pull request's artefacts last one day, a dispatched run's seven. Note the run ID and the image version (`IMAGE_VERSION`, the run's summary). Every x86_64 lab build also uploads `appliance-x86_64-next`: the next version, one build number on, laid out as a GitHub release beside a lab `os-channel.json`, all signed with the run's key. Step 7 serves it.
- **A broken version** (lab runs only), for step 7's fallback, comes with the lab build. `appliance-x86_64-next` also holds `os-<broken version>-x86_64`, two build numbers on from the lab build (`2026.41.7` → `2026.41.9`), whose bun never starts. The run signs it with its own throwaway key, the one the fleet trusts, so no second run is needed. The run's summary names it ("Broken version for the fallback test"), and its "OS fallback" step is the CI run of step 7, with the time it took. Lab builds from before 7 October have no broken version.
- **Disk size:** CI tests on a 7.25 GiB disk (`image/tests/disk.sh`), a little under the 3040's ~7.3 GiB eMMC, so the data partition the tests see is the one the Wyses get.
- A DisplayPort monitor and a USB keyboard: the 3040 has no serial port, and its monitor is where the installer's progress and the claim key show.

## 1. BIOS, all ten units

F2 at power-on (default password `Fireport`):
- update to BIOS 1.2.5 if it's older;
- UEFI boot, legacy (CSM) off. Once off, it can't be turned back on;
- the network stack and UEFI PXE (IPv4) on;
- Secure Boot off (it ships off);
- the eMMC first, the network second;
- optionally, power on after AC loss.

A blank eMMC falls through to PXE. A unit that still holds ThinOS boots ThinOS instead, so for its first install press F12 and pick the UEFI IPv4 Realtek entry, or put the network first until it has installed. After the install, the installer puts the disk first by itself.

**Record:** the BIOS version found, the exact menu names, the PXE boot entry's name, which units held ThinOS, and the minutes per unit.

## 2. Serve

Lab run:

```sh
gh run download <run> -R reliaburger/reliaburger -n appliance-x86_64 -D art/x86_64
gh run download <run> -R reliaburger/reliaburger -n appliance-x86_64-next -D next
caffeinate -i sudo ./target/release/relish netboot art \
  --key art/x86_64/spike-signing-key.pub.pem --interface en7 --for 3h \
  --mac <mac-1> --mac <mac-2> … --mac <mac-10>
```

Formal run:

```sh
relish image download --dir os
caffeinate -i sudo relish netboot os --interface en7 --for 3h \
  --mac <mac-1> … --mac <mac-10> \
  --wipe <mac-1> … --wipe <mac-10>
```

`relish image download` saves every architecture the release has (`os/x86_64/` and `os/aarch64/`); `--arch x86_64` saves only the Wyses'. `--mac` keeps relish away from anything else on the switch that network-boots.

Attended or not: without `--wipe`, each unit that still holds ThinOS reports its disk and waits for a `y` at this terminal (manual, "A disk that isn't blank"). Answer the first lab run's questions by hand, to see each disk report; pass `--wipe <mac>` for every unit, as in the formal run, to install unattended. A blank disk never asks.

**Record:** the start-up lines: the signature checks, the probe's verdict (`10.77.0.1 hands out addresses on en7, and no other netboot server answers`), and what it serves. If the probe says nothing hands out addresses, the Pi is down or on the wrong port: stop and fix that first.

## 3. Netboot all ten at once

Power them all on together. Watch relish's log and one monitor.

**Record:** the time from power-on to the last `installed … in N s`; each unit's disk report and decision; the installer's peak memory line; whether `target` is `/dev/mmcblk0` (never `mmcblk0boot0`/`boot1`); the option 93 architecture the firmware sent; and the NIC name and driver (`r8169`). If the SNP iPXE misbehaves on the Realtek, stop relish and start it again with `--ipxe full`, and record that.

## 4. Claim the cluster

Each installed unit reboots into the appliance, finds no seed and becomes unclaimed: its monitor shows its address, MAC and claim key, and it announces itself over mDNS as `_rb-unclaimed._tcp`.

```sh
relish machines                  # ten rows, ARCH x86_64
```

Lab run, trusting the isolated switch:

```sh
relish machines claim ~/wyse --create --name wyse \
  --operator 10.77.0.2 --network 10.77.0.0/24 \
  --ssh-key ~/.ssh/id_ed25519.pub --trust-lan \
  <mac-1> <mac-2> … <mac-10>
```

Formal run: the same without `--trust-lan` and `--ssh-key`. relish asks, for each unit in turn, whether its monitor shows the claim key it got. Move the monitor from unit to unit and compare all ten. Answer anything but `y` and nothing is claimed.

The units become `wyse-1` to `wyse-10` in the order given, so list them in label order, `<mac-1>` first. The council grows to five voters (the appliance default; `relish machines claim --create --council-size` changes it). If `relish machines` misses a unit, give its address instead of its MAC; `dns-sd -B _rb-unclaimed._tcp` shows what the Mac hears.

Then:

```sh
relish nodes
relish council          # Size: up to 5 voters
relish wtf
```

**Record:** whether all ten showed in `relish machines`, and how long that took; for the formal run, the minutes it took to compare ten keys; the time from the claim to `relish nodes` showing 10/10; who is in the council; `relish wtf`.

## 5. Tour, then pull a cord

The [five-minute tour](../manual/08_five-minute-tour.md) from `relish apply`, with the manual's two differences (ingress on port 80 of every node; pull a power cord instead of `relish local stop`). Pull one worker's cord, wait, plug it back.

**Record:** the time per tour step against the laptop quickstart (deploy and image pull especially); how long the pulled node took to come back and rejoin; whether the reboot hung (`dw_dmac`: a clean `systemctl reboot` on two units counts).

## 6. Measure for 24 hours (lab run)

```sh
image/lab/fleet-measure.sh ~/wyse 300 288    # every 5 minutes, 24 hours
```

`~/wyse` is the claim directory, whose `fleet.json` names the nodes (`wyse-1` to `wyse-10`) and their addresses. `fleet-measure.sh --relish ~/wyse 300 288` takes them from `relish nodes --output json` instead. The script logs in as root over SSH, so it needs a lab image and the key from the claim's `--ssh-key`. Published images have no sshd, so the formal run doesn't repeat this.

Leave the tour's apps running for the first 12 hours, then remove them. One CSV per node lands in `~/wyse/measure/`.

**Record, per node and for the fleet:**
- `MemAvailable`: the minimum, and the median idle and loaded. The research's budget is 1.0–1.3 GB left for workloads.
- Swap and zram use: whether it was used at all, and the compression ratio (`zram_orig_bytes / zram_compr_bytes`).
- bun's RSS over time, council members against workers.
- eMMC bytes written per day (the difference in `disk_written_bytes`), idle against loaded, and the years to 300 × 8 GB at that rate.
- Data partition use at the end.
- `temp_max_mc`: whether a fanless unit throttles under the tour.

## 7. OS update and fallback

**The update.** Lab run: serve the next version from the Mac and roll it out, as the manual's "Updating a CI build" says:

```sh
(cd next && python3 -m http.server 8000)
channel=http://10.77.0.2:8000/releases/download/os-channel/os-channel.json
relish os list --channel "$channel" --key next/lab-signing-key.pub.pem
relish os upgrade --channel "$channel" --key next/lab-signing-key.pub.pem
relish os status
```

The lab channel is signed with the run's key, so relish reads it only with `--key`, and warns that it's not checking against the release keys. `relish os list` must show the next version as the newest release; if it doesn't, the `next/` directory is from another run. Formal run: `relish os list`, then `relish os upgrade` with no version, if a release newer than the installed one exists by then. The leader takes one node at a time, workers first and itself last.

**The fallback** (lab run only). The broken version sits in `next/` beside the next one, signed with the same key, so the server from the update serves it too. Stage it by hand on one worker that isn't in the council (`relish council`), say wyse-9, with the image's `os-stage`. It checks the signature against the key the node's own image carries, which is this run's, so it needs no key argument:

```sh
ssh root@10.77.0.19 /usr/lib/reliaburger/os-stage \
  http://10.77.0.2:8000/releases/download/os-<broken version>-x86_64 <broken version>
ssh root@10.77.0.19 systemctl reboot
```

Then leave it. Each of the three tries waits for bun before the boot check reboots it, and after the third systemd-boot falls back to the version it ran before. (`relish os upgrade <broken version> --channel "$channel"` works too, with no `--key` since it names the version, and it's how CI does it, but then the leader picks the node rather than you, and the rollout pauses once that node falls back; `relish os abort` ends it.)

Until 7 October this step needed a second CI run built with `broken_bun`. That run signed with its own throwaway key, which the fleet doesn't trust, so `relish os upgrade` couldn't reach it and `os-stage` had to be handed the other run's `spike-signing-key.pub.pem`. And a run numbered one after the lab build carried the same version as the lab build's next. The broken version now comes from the lab build's own run, which removes all three traps. `broken_bun` stays for the Mac lab's arm64 builds (`image/lab/README.md`).

**Record:** the time per node from reboot to blessed; that the cluster stayed quorate; for the broken one, the time to fall back on its own, from the reboot to bun healthy on the old version (about 16 minutes in VMs with three 300 s checks; with the 120 s check a counted boot now gets, CI's KVM VM measures it on every lab build, in the "OS fallback" summary; the go/no-go needs 10 minutes or less on the Wyse) and whether the Wyse firmware kept counting tries.

## 8. Write it up (S6)

`docs/qualification/<date>-wyse-3040.md` with the numbers above, then tick S5 and S6 in the [lab plan](2026-09-28-plan-appliance-lab.md), fold the numbers into research §9, and give the go/no-go against the [7 October thresholds](2026-10-02-plan-appliance-wyse-lab.md#decisions-7-october-2026): 3 of 3 claimed, power-on to a working cluster in 60 minutes or less, at least 1.0 GB `MemAvailable` per node under the workload, at least 5 years of eMMC life at the measured rate, and a fallback within 10 minutes.

Stop and write down what happened if a step fails: the failure is the result.
