# Wyse 3040 lab checklist

The fastest path from a CI lab build to Reliaburger on real Dell Wyse 3040s, written on 6 November 2026 for the first hands-on run. Take one machine all the way through first, including the OS fallback, then run the three-machine gated test. It's the [S5 runbook](2026-10-01-plan-appliance-s5-wyse.md) cut down to the commands, in the order you type them. The runbook has the reasoning, the record to keep and the go/no-go, and [manual chapter 15](../manual/15_appliance.md) explains each command. An interactive copy of this page, which fills in your MACs and addresses for you, is at https://claude.ai/artifact/1hMbg9hTE8CiHeifHDcir9.

The commands use these placeholders:

| Placeholder | What | Setup B (isolated) | Setup A (home router) |
|---|---|---|---|
| `<iface>` | The Mac's USB-C Ethernet adapter | `en7` | `en7` |
| `<lab-ip>` | The Mac's address on the lab network | `10.77.0.1` | its reservation, say `192.168.1.10` |
| `<net>` | The lab network | `10.77.0.0/24` | `192.168.1.0/24` |
| `<mac-1>` … `<mac-3>` | Each Wyse's MAC, from the label underneath | | |
| `<ip-1>` … `<ip-3>` | Each Wyse's address | `10.77.0.11` … `.13` | its reservations, say `192.168.1.11` … `.13` |
| `<run>` | The appliance lab build's run ID | | |
| `<sha>` | The commit the run says bun and relish were built from | | |
| `<broken>` | The broken OS version, two builds on from the image | | |

Start with setup B, the Mac as the lab's router. It's the open question: does the 3040's firmware follow DHCP option 60 to UDP 4011? If step 1.4 shows no `ack on 4011`, switch to A and carry on.

## Phase 0: before the hardware

Once. About 30 minutes, most of it a cargo build.

- [ ] **Gather the kit.**
  - A gigabit switch with spanning tree off (or PortFast on every port), and "green Ethernet" off if links flap.
  - The USB-C Ethernet adapter, plugged straight into the Mac, not a hub. It should appear in `networksetup -listallhardwareports`.
  - A DisplayPort monitor and a USB keyboard. The 3040 has no serial port, so the monitor is the only console.
  - Each Wyse's own power supply. Batches differ (5 V or 12 V barrel), so don't swap them between units.

  The Mac serves the netboot and, in B, routes the lab. If the laptop is about to be reinstalled, run Phase 1 first or wait until it has `gh`, Rust, Homebrew and dnsmasq again. Keep every `~/wyse*/secrets` backup off the laptop, and don't use the Mac Studio for this while it's gating a release.

- [ ] **Fetch the lab build.** Use a dispatched run of `release-1-3-0`: its files last 7 days, where a pull request's last one. Dispatch one if there isn't a recent green one, then download it:

  ```sh
  gh workflow run appliance.yml -R reliaburger/reliaburger --ref release-1-3-0 -f publish=false
  gh run list -R reliaburger/reliaburger --workflow appliance.yml --branch release-1-3-0 --limit 3
  mkdir -p ~/wyse-lab && cd ~/wyse-lab && rm -rf art next
  gh run download <run> -R reliaburger/reliaburger -n appliance-x86_64 -D art/x86_64
  gh run download <run> -R reliaburger/reliaburger -n appliance-x86_64-next -D next
  ```

  From the run's summary page (`https://github.com/reliaburger/reliaburger/actions/runs/<run>`), note:
  - the commit in "bun and relish built from …" (that's `<sha>`);
  - the image version and the next version;
  - the version in "Broken version for the fallback test" (that's `<broken>`).

- [ ] **Build the matching relish on the Mac.** No CI job builds a macOS relish from the train, so build it from the same commit as the image's bun:

  ```sh
  cd ~/github/reliaburger && git fetch origin <sha>
  git worktree add ~/github/rb-wt/wyse <sha>
  cd ~/github/rb-wt/wyse && cargo build --release --bin relish
  export R=~/github/rb-wt/wyse/target/release/relish L=~/github/rb-wt/wyse/image/lab
  $R --version
  ```

  Every later command uses `$R` and `$L`, so run that `export` line in each new terminal.

- [ ] **Check the Mac.**

  ```sh
  networksetup -listallhardwareports                        # the adapter's Device:, e.g. en7
  /usr/libexec/ApplicationFirewall/socketfilterfw --getglobalstate
  sudo lsof -nP -iUDP:67                                    # must print nothing
  ls ~/.ssh/id_ed25519.pub
  ```

  If the firewall is on, allow relish (and dnsmasq for B, `python3` for the update step):

  ```sh
  sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add "$R"
  sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblockapp "$R"
  ```

  Turn off Internet Sharing, and stop any VM with shared networking: both hold UDP 67.

## Phase 1: one Wyse, end to end

The fast loop: aim for under 30 minutes a pass, and use the shortcuts freely.

- [ ] **1.1 Firmware setup on Wyse 1.** F2 at power-on (default password `Fireport`):
  - BIOS 1.2.5 or newer;
  - UEFI boot, with legacy (CSM) off; once off, it can't be turned back on;
  - the network stack and UEFI PXE (IPv4) on;
  - Secure Boot off;
  - eMMC first, network second.

  Note the BIOS version, the PXE entry's exact name, and whether the unit still holds ThinOS.

- [ ] **1.2 B only: make the Mac the lab's router.** Cable the adapter to the switch, with nothing else on the switch but the Wyses. Give the adapter a fixed address, NAT out through the Wi-Fi, and run dnsmasq with the reservations in a terminal of its own. The network service name comes from `networksetup -listnetworkserviceorder`.

  ```sh
  sudo networksetup -setmanual "USB 10/100/1G/2.5G LAN" 10.77.0.1 255.255.255.0
  sudo sysctl -w net.inet.ip.forwarding=1
  sudo pfctl -a com.apple/reliaburger-lab -f $L/mac/pf-lab.conf
  sudo pfctl -E          # keep the token it prints for the clean-up
  sudo pfctl -a com.apple/reliaburger-lab -s nat
  ```

  ```sh
  sudo "$(brew --prefix)/sbin/dnsmasq" --keep-in-foreground --conf-file=$L/mac/dnsmasq.conf \
    --interface=<iface> \
    --dhcp-host=<mac-1>,<ip-1>,wyse-1,infinite \
    --dhcp-host=<mac-2>,<ip-2>,wyse-2,infinite \
    --dhcp-host=<mac-3>,<ip-3>,wyse-3,infinite
  ```

  These flags save editing `dnsmasq.conf`: dnsmasq takes any config line as a long option. Nobody has run these steps on a real Mac yet; the [README](../../image/lab/mac/README.md) marks them unverified.

  **A only, instead:** plug the switch into the home router. In its admin page, reserve an address for each Wyse and for the Mac's adapter. The router must not answer PXE. Turn the Mac's Wi-Fi off, so the adapter is its only way onto the LAN. Then `ipconfig getifaddr <iface>` should print `<lab-ip>`, and `route -n get default` should name `<iface>`.

- [ ] **1.3 Serve Wyse 1 only.** `--wipe` skips the "wipe ThinOS?" question; leave it out once to see the disk report, and answer `y`. In A, drop `--mode-dhcp-proxy`.

  ```sh
  cd ~/wyse-lab && caffeinate -i sudo "$R" netboot art --key art/x86_64/spike-signing-key.pub.pem \
    --interface <iface> --for 3h --mode-dhcp-proxy \
    --mac <mac-1> --wipe <mac-1>
  ```

  The start-up lines should show the signature checks passing, and the probe's verdict:
  - **B:** "this machine's DHCP server … hands out addresses", or "a DHCP server on this machine holds UDP 67";
  - **A:** "&lt;router&gt; hands out addresses on `<iface>`, and no other netboot server answers".

- [ ] **1.4 Power on and watch it install.** A blank eMMC falls through to the network by itself. A unit with ThinOS boots ThinOS, so press F12 and pick the UEFI IPv4 Realtek entry.

  In relish's log, within a minute or so, you should see:
  1. `ack on 4011` twice (B only): once for the firmware and once for iPXE;
  2. the TFTP fetch;
  3. the installer over HTTP;
  4. the disk decision;
  5. `installed … in N s`.

  The target must be `/dev/mmcblk0`, never a `boot0` or `boot1` partition. The monitor shows the installer's `reliaburger-install:` lines too (#1611); on a build without them, go by relish's log.

  - **If B stalls**, dnsmasq hands out the address but relish logs nothing: the firmware ignores option 60. Stop relish and dnsmasq, plug the switch into the home router, and repeat from 1.2 as A.
  - **If iPXE misbehaves**, hanging or losing the link after TFTP: restart relish with `--ipxe full`.

- [ ] **1.5 Claim it as a one-node cluster.** After the reboot, the monitor shows its address, MAC and claim key. For the dry run, skip the questions with `--trust-lan --yes`, and add your SSH key so the lab image lets you in.

  ```sh
  $R machines --wait 10
  $R machines claim ~/wyse-dry --create --name dry --operator <lab-ip> --network <net> \
    --ssh-key ~/.ssh/id_ed25519.pub --trust-lan --yes <mac-1>
  $R nodes && $R council && $R wtf
  ```

  You want one node alive, `Size: up to 5 voters`, and nothing for `wtf` to fix. If `relish machines` sees nothing, run `dns-sd -B _rb-unclaimed._tcp`, or claim by address (`<ip-1>`) instead of MAC.

- [ ] **1.6 Run a workload.** The ingress is on port 80 of the node, not `localhost:18080`. Note how long the image pull took compared with the laptop quickstart.

  ```sh
  cd ~/wyse-lab && curl -fsSL https://reliaburger.com/demo/burger.tar.gz | tar xz
  $R apply burger/burger.toml
  curl -H 'Host: burger.localhost' http://<ip-1>/
  ```

- [ ] **1.7 Test the fallback on this one node.** This is the riskiest Wyse-specific part: whether the firmware keeps counting boot tries. Serve `next/` in a terminal of its own, stage the broken version and reboot. The node is down while it tries, which is fine for the dry run.

  ```sh
  cd ~/wyse-lab/next && python3 -m http.server 8000
  ```

  ```sh
  S="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null root@<ip-1>"
  $S /usr/lib/reliaburger/os-stage http://<lab-ip>:8000/releases/download/os-<broken>-x86_64 <broken>
  $S 'date -u +%FT%TZ; systemctl reboot'; date -u +%FT%TZ          # T2
  ```

  ```sh
  $S 'journalctl -b 0 -u reliaburger-boot-check -o short-iso --no-pager | grep "bun healthy"'   # T3
  ```

  It gets three tries of up to 120 s each, then boots the version it ran before, by itself. T3 − T2 must be 10 minutes or less. Count the reboots you see on the monitor.

- [ ] **1.8 Reset it to go again.** Serve again with `--reinstall` (and the `--wipe` you already have), then make the Wyse boot from the network: F12, then the UEFI IPv4 Realtek entry. It reinstalls once, then boots the new install as a fresh unclaimed machine (#1615). Start a new claim directory each pass.

  ```sh
  rm -rf ~/wyse-dry
  cd ~/wyse-lab && caffeinate -i sudo "$R" netboot art --key art/x86_64/spike-signing-key.pub.pem \
    --interface <iface> --for 3h --mode-dhcp-proxy \
    --mac <mac-1> --wipe <mac-1> --reinstall
  ```

## Phase 2: three Wyses, the gated run

This run counts for the v0.3.0 tag. No shortcuts. About a day, plus 24 hours of measuring.

- [ ] **2.1 Prepare all three.**
  - Do 1.1 on Wyse 2 and 3, and reset Wyse 1 (1.8).
  - Label each unit with its MAC, address and node name (`wyse-1`, not `wyse-01`).
  - Use a fresh lab build (Phase 0), and note the run ID, image version, next and broken versions, and the built-from commit.
  - Turn spanning tree off on the switch, and check `sudo lsof -nP -iUDP:67` again.

- [ ] **2.2 Serve all three, then start the clock.**

  ```sh
  cd ~/wyse-lab && caffeinate -i sudo "$R" netboot art --key art/x86_64/spike-signing-key.pub.pem \
    --interface <iface> --for 3h --mode-dhcp-proxy --reinstall \
    --mac <mac-1> --mac <mac-2> --mac <mac-3> \
    --wipe <mac-1> --wipe <mac-2> --wipe <mac-3>
  ```

  **T0** is when the first Wyse gets power. Power all three on together and write the time down.

- [ ] **2.3 Claim, comparing every key.** No `--trust-lan` or `--yes` this time. Move the monitor from unit to unit and check each claim key matches. At the backup prompt, copy `~/wyse/secrets` off the Mac and type `yes`.

  ```sh
  $R machines --wait 10      # three rows, ARCH x86_64
  $R machines claim ~/wyse --create --name wyse --operator <lab-ip> --network <net> \
    --ssh-key ~/.ssh/id_ed25519.pub <mac-1> <mac-2> <mac-3>
  $R nodes && $R council && $R wtf
  ```

  **T1** is the first time `nodes` shows three alive and `wtf` has nothing to fix. **T1 − T0 must be 60 minutes or less.** All three are voters, so note which one leads.

- [ ] **2.4 Tour, then pull a cord.** Run the burger tour from 1.6 against `<ip-1>`. Then pull the power on a voter that isn't the leader, wait a minute, plug it back in, and wait for `relish nodes` to show three alive. Note how long it took to rejoin, and whether any reboot hung.

- [ ] **2.5 Measure for 24 hours, with no reboots.** It samples every 5 minutes, 288 times. Leave the tour's apps running for the first 12 hours, then `relish delete` them and note the time. A reboot anywhere in the window spoils that node's eMMC figure. The CSVs land in `~/wyse/measure/`.

  ```sh
  cd ~/github/rb-wt/wyse && caffeinate -i image/lab/fleet-measure.sh ~/wyse 300 288
  ```

- [ ] **2.6 Update the OS, then test the fallback.** Only once the 24 hours are over. Serve `next/` as in 1.7, then roll out the next version: the two followers first, the leader last.

  ```sh
  channel=http://<lab-ip>:8000/releases/download/os-channel/os-channel.json
  $R os list --channel "$channel" --key ~/wyse-lab/next/lab-signing-key.pub.pem
  $R os upgrade --channel "$channel" --key ~/wyse-lab/next/lab-signing-key.pub.pem
  $R os status
  ```

  Then repeat the fallback from 1.7 on a voter that isn't the leader, with that node's address in `$S`. `relish council` names the leader, which may have moved during the update. The node should come back on the *next* version. **T3 − T2 must be 10 minutes or less.** Keep `relish council` open in another terminal, to check the cluster keeps quorum throughout.

- [ ] **2.7 Record the results.** Collect the relish netboot log, T0 to T3, the BIOS notes, the measure CSVs and anything that went wrong. They go into `docs/qualification/<date>-wyse-3040.md`, with the go/no-go below. A failed step is still a result: write down what happened and stop.

## Go/no-go

All five must hold (maintainer decisions of 7 October 2026, in the [Wyse lab plan](2026-10-02-plan-appliance-wyse-lab.md#decisions-7-october-2026)):

| Criterion | Threshold | From |
|---|---|---|
| Machines claimed | 3 of 3 | 2.3 |
| Power-on to a working cluster | ≤ 60 min | T1 − T0 |
| `MemAvailable` on every node under the workload | ≥ 1.0 GB | first 12 h of 2.5 |
| eMMC life at the measured write rate, worst node | ≥ 5 years (about 1.3 GB a day) | 24 h of 2.5 |
| A broken update falls back by itself | ≤ 10 min | T3 − T2 in 2.6 |
