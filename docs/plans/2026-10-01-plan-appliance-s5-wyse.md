# Plan: spike S5, the Dell Wyse 3040s

*Written 1 October 2026, before the hardware run, and rewritten on 3 October around the 0.3.0 train (`release-1-3-0`): `relish netboot` from the Mac, the Raspberry Pi as the lab's router, and claiming over the network instead of `netboot-server.sh` and `seed-fleet.sh`; cut to the three gated Wyses on 7 October. The checklist is research §9.7; the lab and the reasons behind it are the [Wyse lab plan](2026-10-02-plan-appliance-wyse-lab.md); the commands are the [manual's appliance chapter](../manual/15_appliance.md). What to record at each step is in **Record**; it all goes into `docs/qualification/<date>-wyse-3040.md`.*

> **7 October 2026:** the exit test now gates on **three** Wyses, run on a lab build of the train, and gates the v0.3.0 tag; the go/no-go thresholds are in the [lab plan's decisions](2026-10-02-plan-appliance-wyse-lab.md#decisions-7-october-2026). The steps below are written for that gated run: three machines, wyse-1 to wyse-3 at 10.77.0.11 to .13. The lab owns ten and can run them all the same way (list ten MACs instead of three), but the gate counts three. The "formal run" on a signed channel no longer comes before the tag, since the weekly publish stays off until v0.3.0 is promoted.
>
> **Three machines, no workers.** The council grows to five voters by default, so with three machines all three are voters and none is a worker. Two of three keep a majority, so the run can lose any one node, but only one at a time. Wherever a step below takes a node down on purpose (the cord pull in §5, the fallback in §7), it picks a voter that isn't the leader, so the cluster doesn't hold an election on top of the test.

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
- **Ten Wyse 3040s** in the lab: x86_64, 2 GB, 8 GB eMMC, UEFI PXE on a Realtek RTL8111/8168. Some still hold ThinOS, some are blank. The gated run uses three of them, each with its own power supply (batches differ, 5 V or 12 V barrel).

## Two kinds of run

| | Lab runs, the gated run among them | The formal S5 run (after v0.3.0) |
|---|---|---|
| Images | A CI lab build of the train (throwaway key per run) | The signed channel, once 0.3.0 publishes an OS release |
| relish on the Mac | Built from the lab build's commit | The 0.3.0 release |
| `relish netboot` | `art --key art/x86_64/spike-signing-key.pub.pem` | `os`, no `--key` |
| Claim keys | The gated run compares all three on the monitor; ungated lab runs may take the `--trust-lan` shortcut | All compared on the monitor |
| sshd, so `fleet-measure.sh` and the fallback test | Yes (lab profile) | No: published images have no sshd |
| OS update | To the run's own next version, over a lab channel | To the next published release, if one exists by then |

The gated run is a lab run that compares claim keys: decision 7 of 3 October (compare every key on the monitor, as a user would) now covers the three gated machines. Each step below gives the formal run's commands where they differ.

## The clock

Two go/no-go criteria are times. Write each moment down as it happens, with the clock it came from.

- **T0**: you power on the first Wyse for its netboot install, with `relish netboot` already serving (§3). BIOS setup (§1) is done beforehand and isn't counted.
- **T1**: `relish nodes` shows all three nodes alive and `relish wtf` has nothing to fix (§4). **T1 − T0 must be 60 minutes or less.** It includes the installs, the reboots, `relish machines`, comparing three claim keys and the master-key backup prompt.
- **T2**: the fallback node reboots into the broken version (§7). Take it from the node's own clock, printed by the command that reboots it.
- **T3**: bun is healthy again on the version the node ran before: the boot check's `reliaburger: bun healthy … on OS <version>` line, from the node's journal. **T3 − T2 must be 10 minutes or less.**

T2 and T3 are what CI's fallback test measures (`image/tests/os-update.sh --fallback`, the "OS fallback" line in the lab build's summary): from the reboot into the broken version to bun healthy on the previous one. CI starts its clock at the broken version's first kernel banner on the serial console. The Wyse has no serial port, so T2 starts at `systemctl reboot` and also counts the shutdown and the firmware's start-up. That makes the Wyse figure a little stricter than CI's, never kinder.

## Before the day

- **The Pi**, set up and checked as its README says, with the reservations filled in (wyse-1 to wyse-3 for the gated run, all ten for a full lab run) and the Mac's. Label each Wyse with its MAC, its address and its node number. The Pi's names are the claim's: `wyse-1`, not `wyse-01`.
- **The Mac:**
  - `relish`. For a lab run, build it on the Mac from the commit the lab build's bun came from (the appliance workflow's `bun` job), so the two match. No run on the train builds a macOS relish: `build.yml` builds macOS binaries only on pushes to main and on dispatch, and the appliance run's `bun-source-*` artefacts hold Linux binaries. The run's summary names the commit ("bun and relish built from `<sha>`"). On a dispatch that's the train's head. On a pull request run it's GitHub's merge of the head into `main`, which has the same tree as the head as long as `main` hasn't moved since the train last merged it. If it has, build from the merge commit, not the head:

    ```sh
    git fetch origin <built-from sha> && git checkout <built-from sha>
    cargo build --release --bin relish
    ./target/release/relish --version
    ```

    Use `./target/release/relish` from here on. For the formal run, the 0.3.0 release.
  - `gh`, and `python3` for serving the next version.
  - The adapter's name: `networksetup -listallhardwareports`, the `Device:` under the USB Ethernet port, say `en7`. `ipconfig getifaddr en7` should print `10.77.0.2` once the Pi is up, and `route -n get default` should still name the Wi-Fi.
  - The application firewall: `/usr/libexec/ApplicationFirewall/socketfilterfw --getglobalstate`. If it's on, allow relish (manual, "Serving from a Mac"), and `python3` for step 7.
  - Internet Sharing off, and no VM with shared or host networking running: either may hold UDP 67. `sudo lsof -nP -iUDP:67` should print nothing.
  - An SSH key for the lab runs: `~/.ssh/id_ed25519.pub`.
- **A fresh lab build, on the day** (lab runs only), from a green run of the train. A pull request run's artefacts last one day (a dispatched run's seven), so on the morning of the run the maintainer asks for a new one: a dispatch of the appliance workflow (`appliance.yml`) on `release-1-3-0` once the workflow is on `main`, or until then a new run on the gate PR (#490). Download it the same day:

  ```sh
  gh run list -R reliaburger/reliaburger --workflow appliance.yml --branch release-1-3-0 --limit 3
  gh run download <run> -R reliaburger/reliaburger -n appliance-x86_64 -D art/x86_64
  gh run download <run> -R reliaburger/reliaburger -n appliance-x86_64-next -D next
  gh run view <run> -R reliaburger/reliaburger --json headSha
  ```

  Before anything boots, the maintainer records: the run ID; the image version (`IMAGE_VERSION`, the summary's "Appliance <version>" heading); the next version, one build number on; the broken version, two on ("Broken version for the fallback test"); the head SHA, and the commit the summary says bun was built from; and the summary's "OS fallback" line, CI's fallback time for this build. Every x86_64 lab build uploads `appliance-x86_64-next`: the next version laid out as a GitHub release beside a lab `os-channel.json`, all signed with the run's key. Step 7 serves it. The run's keys come with the artefacts: `art/x86_64/spike-signing-key.pub.pem` for `relish netboot`, `next/lab-signing-key.pub.pem` for `relish os`.
- **A broken version** (lab runs only), for step 7's fallback, comes with the lab build. `appliance-x86_64-next` also holds `os-<broken version>-x86_64`, two build numbers on from the lab build (`2026.41.7` → `2026.41.9`), whose bun never starts. The run signs it with its own throwaway key, the one the fleet trusts, so no second run is needed. The run's summary names it ("Broken version for the fallback test"), and its "OS fallback" step is the CI run of step 7, with the time it took. Lab builds from before 7 October have no broken version.
- **Disk size:** CI tests on a 7.25 GiB disk (`image/tests/disk.sh`), a little under the 3040's ~7.3 GiB eMMC, so the data partition the tests see is the one the Wyses get.
- A DisplayPort monitor and a USB keyboard: the 3040 has no serial port, and its monitor is where the installer's progress and the claim key show.

## 1. BIOS, each unit

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
caffeinate -i sudo ./target/release/relish netboot art \
  --key art/x86_64/spike-signing-key.pub.pem --interface en7 --for 3h \
  --mac <mac-1> --mac <mac-2> --mac <mac-3>
```

Formal run:

```sh
relish image download --dir os
caffeinate -i sudo relish netboot os --interface en7 --for 3h \
  --mac <mac-1> --mac <mac-2> --mac <mac-3> \
  --wipe <mac-1> --wipe <mac-2> --wipe <mac-3>
```

`relish image download` saves every architecture the release has (`os/x86_64/` and `os/aarch64/`); `--arch x86_64` saves only the Wyses'. `--mac` keeps relish away from anything else on the switch that network-boots.

Attended or not: without `--wipe`, each unit that still holds ThinOS reports its disk and waits for a `y` at this terminal (manual, "A disk that isn't blank"). Answer the first lab run's questions by hand, to see each disk report; pass `--wipe <mac>` for every unit, as in the formal run, to install unattended. A blank disk never asks. On the gated run a question waiting at this terminal counts against the 60 minutes, so answer promptly or pass `--wipe`.

**Record:** the start-up lines: the signature checks, the probe's verdict (`10.77.0.1 hands out addresses on en7, and no other netboot server answers`), and what it serves. If the probe says nothing hands out addresses, the Pi is down or on the wrong port: stop and fix that first.

## 3. Netboot the three at once

Power them all on together. **T0** is the moment the first one gets power. Watch relish's log and one monitor.

**Record:** T0; the time from power-on to the last `installed … in N s`; each unit's disk report and decision; the installer's peak memory line; whether `target` is `/dev/mmcblk0` (never `mmcblk0boot0`/`boot1`); the option 93 architecture the firmware sent; and the NIC name and driver (`r8169`). If the SNP iPXE misbehaves on the Realtek, stop relish and start it again with `--ipxe full`, and record that.

## 4. Claim the cluster

Each installed unit reboots into the appliance, finds no seed and becomes unclaimed: its monitor shows its address, MAC and claim key, and it announces itself over mDNS as `_rb-unclaimed._tcp`.

```sh
relish machines --wait 10        # three rows, ARCH x86_64
```

The gated run compares every claim key:

```sh
relish machines claim ~/wyse --create --name wyse \
  --operator 10.77.0.2 --network 10.77.0.0/24 \
  --ssh-key ~/.ssh/id_ed25519.pub \
  <mac-1> <mac-2> <mac-3>
```

relish asks, for each unit in turn, whether its monitor shows the claim key it got. Move the monitor from unit to unit and compare all three. Answer anything but `y` and nothing is claimed. Then it stops at the master-key backup prompt: copy `~/wyse/secrets` somewhere off the Mac and type `yes`. Ungated lab runs on a switch with nothing else on it may add `--trust-lan` to skip the key questions, and `--yes` to skip the backup prompt; the gated run doesn't. Formal run: the same command without `--ssh-key`, since published images have no sshd.

The units become `wyse-1` to `wyse-3` in the order given, so list them in label order, `<mac-1>` first. The council grows to five voters (the appliance default; `relish machines claim --create --council-size` changes it), so with three machines all three are voters. If `relish machines` misses a unit, give its address instead of its MAC; `dns-sd -B _rb-unclaimed._tcp` shows what the Mac hears.

Then:

```sh
relish nodes            # three alive
relish council          # Size: up to 5 voters; three members, and the leader
relish wtf              # nothing to fix
```

**T1** is the first time `relish nodes` shows all three alive and `relish wtf` has nothing to fix. Repeat both until it does.

**Record:** whether all three showed in `relish machines`, and how long that took; the minutes it took to compare three keys; the time from the claim to `relish nodes` showing 3/3; T1, and T1 − T0; who is in the council and who leads; `relish wtf`.

## 5. Tour, then pull a cord

The [five-minute tour](../manual/08_five-minute-tour.md) from `relish apply`, with the manual's two differences (ingress on port 80 of every node, say `curl -H 'Host: podinfo.localhost' http://10.77.0.11/`; pull a power cord instead of `relish local stop`). There are no workers to pull, so pull the cord of a voter that isn't the leader (`relish council` names the leader): two of three still make a majority, and the cluster carries on without an election. Wait a minute, plug it back, and wait until `relish nodes` shows three alive again before step 6 starts.

**Record:** the time per tour step against the laptop quickstart (deploy and image pull especially); which node you pulled; how long it took to come back and rejoin; whether the reboot hung (`dw_dmac`: a clean `systemctl reboot` on two units counts).

## 6. Measure for 24 hours (lab run)

From a checkout of the commit relish was built from:

```sh
caffeinate -i image/lab/fleet-measure.sh ~/wyse 300 288    # every 5 minutes, 24 hours
```

> **No reboots in the window.** `fleet-measure.sh` reads the bytes written from `/sys/block/<disk>/stat`, which the kernel resets at boot. A reboot anywhere in the 24 hours (a cord pull, an OS update, the fallback) spoils the eMMC figure for that node. So the order is fixed: the cord pull (§5) comes before the window starts, and the OS update and the fallback (§7) come after it ends. If a node's `disk_written_bytes` drops between two samples, it rebooted anyway: note it, and run the 24 hours again for that node. Don't let the Mac sleep either, hence `caffeinate`.

`~/wyse` is the claim directory, whose `fleet.json` names the nodes (`wyse-1` to `wyse-3`) and their addresses. `fleet-measure.sh --relish ~/wyse 300 288` takes them from `relish nodes --output json` instead. The script logs in as root over SSH, so it needs a lab image and the key from the claim's `--ssh-key`. Published images have no sshd, so the formal run doesn't repeat this.

Leave the tour's apps running for the first 12 hours, then remove them, and note the time. One CSV per node lands in `~/wyse/measure/`.

**Record, per node and for the fleet:**
- `MemAvailable`: the minimum, and the median idle and loaded. The research's budget is 1.0–1.3 GB left for workloads.
- Swap and zram use: whether it was used at all, and the compression ratio (`zram_orig_bytes / zram_compr_bytes`).
- bun's RSS over time (all three are council members; on a ten-node lab run, compare members with workers).
- eMMC bytes written per day (the last `disk_written_bytes` minus the first, over the 24 hours), idle against loaded, and the years to 300 × 8 GB at that rate. Five years is about 1.3 GB a day.
- Data partition use at the end.
- `temp_max_mc`: whether a fanless unit throttles under the tour.

## 7. OS update and fallback

Start only after the 24 hours of step 6 have ended: both halves reboot nodes.

**The update.** Lab run: serve the next version from the Mac and roll it out, as the manual's "Updating a CI build" says:

```sh
(cd next && python3 -m http.server 8000)
channel=http://10.77.0.2:8000/releases/download/os-channel/os-channel.json
relish os list --channel "$channel" --key next/lab-signing-key.pub.pem
relish os upgrade --channel "$channel" --key next/lab-signing-key.pub.pem
relish os status
```

The lab channel is signed with the run's key, so relish reads it only with `--key`, and warns that it's not checking against the release keys. `relish os list` must show the next version as the newest release; if it doesn't, the `next/` directory is from another run. Formal run: `relish os list`, then `relish os upgrade` with no version, if a release newer than the installed one exists by then. The leader takes one node at a time, workers first and itself last; with three voters and no workers, that's the two followers, then the leader.

**The fallback** (lab run only), once the update has finished. The broken version sits in `next/` beside the next one, signed with the same key, so the server from the update serves it too. Stage it by hand on a voter that isn't the leader (`relish council` names the leader, which may have moved during the update), say wyse-3 at 10.77.0.13. While it tries the broken version it's down for up to ten minutes, and the other two keep a majority. `os-stage` checks the signature against the key the node's own image carries; the next version carries this run's key too, so it needs no key argument. The second command prints the node's clock, which is **T2**, and reboots:

```sh
ssh root@10.77.0.13 /usr/lib/reliaburger/os-stage \
  http://10.77.0.2:8000/releases/download/os-<broken version>-x86_64 <broken version>
ssh root@10.77.0.13 'date -u +%FT%TZ; systemctl reboot'
```

`relish os upgrade <broken version> --channel "$channel"` works too, with no `--key` since it names the version, and it's how CI does it. But then the leader picks the node rather than you, the rollout pauses once that node falls back (`relish os abort` ends it), and T2 no longer comes from your own command.

Then leave it. Each of the three tries waits 120 s for bun before the boot check reboots it, and after the third systemd-boot falls back to the version it ran before. Because the update came first, that's the **next** version, not the one the node was installed with: the update put the next version in one slot, and staging the broken one overwrote the other. That's expected, and it's the case that matters, a node falling back from a bad update to the last good one. Once it's back, **T3** is in its journal:

```sh
ssh root@10.77.0.13 'journalctl -b 0 -u reliaburger-boot-check -o short-iso --no-pager | grep "bun healthy"'
```

The same line shows on its monitor. Note the Mac's clock (`date -u`) at both moments as well, in case the node's clock jumped at boot (a flat RTC battery).

Until 7 October this step needed a second CI run built with `broken_bun`. That run signed with its own throwaway key, which the fleet doesn't trust, so `relish os upgrade` couldn't reach it and `os-stage` had to be handed the other run's `spike-signing-key.pub.pem`. And a run numbered one after the lab build carried the same version as the lab build's next. The broken version now comes from the lab build's own run, which removes all three traps. `broken_bun` stays for the Mac lab's arm64 builds (`image/lab/README.md`).

**Record:** the time per node from reboot to blessed; that the cluster stayed quorate (`relish council` during the rollout and the fallback); for the broken one, T2, T3 and T3 − T2, the time to fall back on its own (about 16 minutes in VMs with three 300 s checks; with the 120 s check a counted boot now gets, CI's KVM VM measures it on every lab build, in the "OS fallback" summary; the go/no-go needs 10 minutes or less on the Wyse), the number of boots, whether the Wyse firmware kept counting tries, and `relish os list` afterwards (the fallback node back on the next version).

## 8. Write it up (S6)

`docs/qualification/<date>-wyse-3040.md` with the numbers above, then tick S5 and S6 in the [lab plan](2026-09-28-plan-appliance-lab.md), fold the numbers into research §9, and give the go/no-go against the [7 October thresholds](2026-10-02-plan-appliance-wyse-lab.md#decisions-7-october-2026): 3 of 3 claimed; power-on to a working cluster in 60 minutes or less (T1 − T0); at least 1.0 GB `MemAvailable` on every node under the workload (the minimum over the first 12 hours of step 6); at least 5 years of eMMC life at the rate measured over 24 hours (the worst node); and a fallback within 10 minutes (T3 − T2). The write-up names the run ID, the three versions and the commits recorded before the day.

Stop and write down what happened if a step fails: the failure is the result.
