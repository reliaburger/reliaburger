# Plan: the Wyse lab, from today's code to ten netbooted nodes

*Written 2 October 2026 on the `appliance-train` branch (the spike, W1–W6 and the fleet research in one PR). It answers one question: what's left between the code on that branch and the maintainer's lab? It reads the code rather than the plans, cites files and functions, and marks anything not checked on real hardware or on macOS as **[unverified]**. It doesn't replace the [product plan](2026-10-01-plan-appliance-product.md) or the [S5 runbook](2026-10-01-plan-appliance-s5-wyse.md); it feeds both.*

## The lab

- An **M2 MacBook Pro** (macOS, arm64) with a **USB-C Ethernet adapter**, running `relish`.
- A **16-port gigabit switch** off eBay.
- **Ten Dell Wyse 3040s**: x86_64 (Atom x5-Z8350), 2 GB RAM, 8 GB eMMC, UEFI PXE through a Realtek RTL8111/8168, no serial port (research §9.1).
- A **Raspberry Pi**.

Everything on the switch, isolated from the home LAN. The ten Wyses netboot into the appliance and form one cluster with the real commands: `relish image download`, `relish netboot`, `relish machines claim`.

## The short answer

1. **Make the Pi the lab's router**: DHCP with a reservation per Wyse, DNS, NTP and NAT out through its Wi-Fi. Then `relish netboot` on the Mac is a ProxyDHCP beside an ordinary DHCP server, which is exactly the topology CI already tests (`image/tests/relish-netboot-install.sh`). No DHCP code is needed for the lab.
2. **Don't build `relish netboot --dhcp` for the lab.** The cluster needs DHCP for its whole life, not for the hour netboot runs, and it needs a default route (`appliance::address::detect`). A DHCP server inside a one-hour CLI is the wrong home for that. The design is below in case we want it later for a Pi-less setup.
3. **Four small fixes block the lab**, about 4–6 days in all: `relish netboot --wipe` (used Wyses aren't blank), `relish image download` picking aarch64 on a Mac, a pre-flight in `relish netboot` that says when nothing hands out addresses, and a macOS qualification of `relish netboot` and `relish machines`. Two more make the S5 measurements possible: CI uploading the next OS version, and `fleet-measure.sh` reading a claimed fleet.
4. Most of it can be done **before the switch arrives**: the code, the CI changes, and a dry run with the Pi and one Wyse on any spare switch or directly cabled.

## 1. DHCP on an isolated switch

### What the code does today

`relish netboot` (`src/relish/netboot/`) runs three servers for `--for` (default 1 h, `DEFAULT_DURATION`):
- `dhcp::answer` builds ProxyDHCP replies with `yiaddr` always zero (`build_reply`), and a property test (`no_reply_ever_assigns_an_address`) pins that down;
- `tftp` serves iPXE and a chain script;
- `http` serves the installer and the image.

Before it starts, `server::refuse_if_another_server_answers` broadcasts a PXE-looking DHCPDISCOVER and refuses if another *boot* server answers (`dhcp::competing_server` ignores a router's plain offer).

So on a switch with only the Mac and the Wyses, PXE firmware broadcasts, gets a boot file from relish but no address from anyone, and times out. iPXE then does its own DHCP and fails the same way.

Two more facts shape the choice:
- **The nodes need DHCP for ever.** `image/mkosi.extra/usr/lib/systemd/network/80-wired.network` is `DHCP=yes` on every wired link, and nothing static is written by the seed or the claim. Leases have to be renewed long after netboot exits.
- **The nodes need a default route and stable addresses.** `appliance::address::detect` finds the advertise address from the default-route interface in `/proc/net/route`. No router option, no default route, no address. And the claim uses each machine's current address as its node address (manual, "Or claim them over the network"), so the DHCP server has to give each MAC the same address every time.

### The options

| | Pi as router (dnsmasq) | macOS Internet Sharing | `relish netboot --dhcp` |
|---|---|---|---|
| Addresses for the cluster's life | Yes | Yes, while the Mac shares | Only while netboot runs |
| Stable address per MAC | `dhcp-host=` reservations | No reservations in the UI; `bootpd.plist` by hand **[unverified]** | Needs a lease file and sticky allocation |
| Default route and internet for image pulls and OS updates | NAT through the Pi's Wi-Fi | NAT through the Mac | Only if we also do NAT on the Mac (`pfctl`), outside relish |
| Works with `relish netboot` on the same Mac | Yes | **No**: bootpd holds UDP 67, so `udp_socket("DHCP", …:67)` fails with `PortInUse` (`interface::bind_error` even names bootpd) | Yes, it's the same process |
| Code | None | A ProxyDHCP that can share port 67, which BSD sockets make hard | ~1.5–2 weeks |
| What CI already tests | Exactly this (dnsmasq router in a netns, relish beside it) | Nothing | Nothing |

**Recommendation: the Pi as the router.** It's what the product targets (a home router does DHCP, relish does the boot part), it's what CI tests, and it keeps the cluster up when the Mac sleeps or leaves.

The Pi's setup goes in `image/lab/pi/` (PR 4 below): Raspberry Pi OS Lite 64-bit, `eth0` on the switch at `10.77.0.1/24`, `wlan0` on the home Wi-Fi, and dnsmasq with:

```
interface=eth0
bind-interfaces
dhcp-range=10.77.0.100,10.77.0.199,255.255.255.0,12h
dhcp-option=option:router,10.77.0.1
dhcp-option=option:dns-server,10.77.0.1
dhcp-option=option:ntp-server,10.77.0.1
dhcp-authoritative
# One line per Wyse: the MAC from its label, a fixed address.
dhcp-host=<mac-1>,10.77.0.11,wyse-1
...
dhcp-host=<mac-10>,10.77.0.20,wyse-10
# The Mac: a fixed address, and no default route, so its internet stays on Wi-Fi.
dhcp-host=<mac-adapter-mac>,10.77.0.2,set:operator
dhcp-option=tag:operator,option:router
```

plus `net.ipv4.ip_forward=1`, an nftables masquerade on `wlan0`, and chrony serving NTP on `eth0`. No `dhcp-boot`, no `enable-tftp`, no `pxe-service`: if the Pi answered PXE, `relish netboot` would rightly refuse to start (`NetbootError::CompetingServer`).

NTP matters more than it looks. Second-hand Wyses may have flat RTC batteries **[unverified per unit]**, and a node whose clock is years out rejects certificates. The image runs `systemd-timesyncd` (`image/mkosi.conf`), which takes the server from DHCP option 42.

### If we want `--dhcp` later

For a lab with no Pi: a real DHCP server inside `relish netboot`, for the lifetime of the run. Not for this lab. Sketch:

- **CLI** (`src/bin/relish.rs`, `Command::Netboot`): `--dhcp <first>-<last>` (a range), `--gateway <addr>` (defaults to none, with a warning that nodes won't find their advertise address without one), `--lease 12h`.
- **Code:**
  - a new `src/relish/netboot/leases.rs`: a pure `LeaseTable` (MAC → address, expiry), sticky by MAC, persisted to `<dir>/netboot-leases.json` as `installed.rs` persists `netboot-installed.json`;
  - `dhcp::answer` grows a mode: in `--dhcp` mode, DISCOVER from *any* client gets an OFFER with `yiaddr` from the table, REQUEST gets an ACK or a NAK, RELEASE and DECLINE update the table; PXE clients get the boot options in the same reply instead of a separate proxy offer. The ProxyDHCP path stays unchanged and stays the default.
  - `server::run` keeps the 4011 listener for firmware that confirms there.
- **Safety rails:**
  - refuse to start if *any* DHCP server answers the probe (today only boot servers count): two DHCP servers on one LAN is how home networks break;
  - refuse unless `--interface` is named explicitly, never the default-route interface, and the range lies inside that interface's subnet and excludes its own address;
  - refuse ranges larger than a /24;
  - `--for` still applies, and the summary says in capitals that leases stop being renewed when it exits.
- **Tests:** unit tests for `LeaseTable` (sticky allocation, expiry, a full range, a declined address), property tests that a `--dhcp` OFFER's `yiaddr` is always inside the range and never the server's, a loopback exchange (DISCOVER, OFFER, REQUEST, ACK), and a CI variant of `relish-netboot-install.sh` with the netns router's dnsmasq removed.

## 2. `relish netboot` on macOS

What W4 already handles:
- **Root:** binding 67, 69 and 4011 needs root on macOS too. `interface::bind_error` maps `PermissionDenied` to `NetbootError::NeedsRoot` ("run it with sudo").
- **The interface:** `--interface en7` (or `--address`). `interface::udp_socket` ties the DHCP sockets to it with `bind_device_by_index_v4`, which socket2 implements as `IP_BOUND_IF` on macOS (the `Cargo.toml` comment says so).
- **bootpd:** if Internet Sharing's bootpd holds port 67, the bind fails with `PortInUse` and a hint naming it.
- **TFTP and HTTP** bind the interface's own address, not 0.0.0.0.

What's never been run on macOS (the W4 PR says the Mac-lab run wasn't done; the spike's Mac lab ran dnsmasq inside a Linux VM, `image/lab/`):
- **The default route picks Wi-Fi.** `InterfaceChoice::DefaultRoute` uses the route to 192.0.2.1, which on the Mac is Wi-Fi. The lab must pass `--interface` with the adapter's name (`networksetup -listallhardwareports`). Worth a warning when the chosen interface has no PXE answer path, but the real fix is the docs.
- **A self-assigned address.** If the adapter came up before the Pi's DHCP, macOS gives it `169.254.x.x`, and relish would happily serve as `siaddr 169.254…`. **Fix:** refuse a link-local server address in `interface::pick` (PR 3).
- **The competing-server probe binds UDP 68**, which macOS's own DHCP client (configd's IPConfiguration) may hold **[unverified]**. If it does, the bind fails, `refuse_if_another_server_answers` prints a warning and skips the check. Safe, but the check silently does nothing on every Mac. **Fix:** on macOS, set `SO_REUSEPORT` on the probe socket, or listen on the ProxyDHCP's own port-67 socket for the probe's replies instead; test which on the Mac (PR 3).
- **Broadcasts out of the right interface.** Replies to clients without an address go to 255.255.255.255 (`dhcp::destination`). With `IP_BOUND_IF` they should leave through the adapter **[unverified]**; the qualification run shows it in a packet capture (`sudo tcpdump -ni en7 port 67 or port 68`).
- **The application firewall.** With it on, macOS may drop incoming UDP for an unsigned, ad-hoc-signed binary run under sudo, without a prompt **[unverified]**. Check `socketfilterfw --getglobalstate`; if it's on, `socketfilterfw --add $(which relish)` and `--unblockapp`. The manual says so.
- **vmnet's DHCP.** UTM, Lima and socket_vmnet's shared and host modes run a DHCP server on the bridge (`bridge100`) **[unverified that it binds port 67 on all interfaces]**. If it does, `relish netboot` can't start while any such VM is running. The qualification checks `sudo lsof -nP -iUDP:67` with a VM up.
- **`relish machines` and mDNS.** The claim discovery uses `mdns-sd` on UDP 5353, which mDNSResponder also holds. The CI claim test runs on Linux, and on loopback without mDNS (#405's `bb1cb595`). On macOS it's untested **[unverified]**. Fallback: claim by address, which the manual already documents.
- **Sleep.** A Mac that sleeps mid-install stops TFTP and HTTP. `caffeinate -i sudo relish netboot …` in the docs.

## 3. Architecture: x86_64 Wyses, an arm64 Mac

- The Mac only *serves* files. It never runs the x86_64 iPXE, installer or image, so arm64 relish serving x86_64 artefacts is fine. `dhcp::boot_file` picks the architecture from option 93: 6, 7 and 9 (and 16 for HTTP Boot) are x86_64. The Wyse sends 7 (x64 UEFI) or 9 (EFI BC) **[unverified which]**; both map to `ipxe-x86_64.efi`.
- **Where the x86_64 files come from.** Today there's no published OS release: `gh release view os-channel` finds nothing, because W1's `appliance.yml` publishes only from `main` (`plan` job: `GITHUB_REF = refs/heads/main` and a schedule or `publish`). So until 0.3.0 merges, the lab takes a CI lab build: `gh run download <run> -n appliance-x86_64 -D art/x86_64`, served with `--key art/x86_64/spike-signing-key.pub.pem`. Its artefacts are kept **one day** (`retention-days`), so download them the day of the run, and keep the directory.
- **The bun inside a lab image** is built from the commit under test (the `bun` job), so the Mac's relish must come from the same commit: `cargo build --release --bin relish` on the train branch. A released relish won't do.
- **`relish image download` gets the wrong architecture on a Mac.** `ImageAction::Download`'s `--arch` defaults to `std::env::consts::ARCH`, which is `aarch64` on an M2. For the Wyses you must pass `--arch x86_64`. That's a trap: the operator's laptop is rarely the machines' architecture. **Fix (PR 2):** download every architecture the channel has unless `--arch` narrows it. `relish netboot` already serves whichever directories exist (`artefacts::load`).

## 4. From power-on to a cluster

### The sequence with today's commands

| Step | Command | State |
|---|---|---|
| Pi up as router | dnsmasq, nftables, chrony (`image/lab/pi/`, PR 4) | To write |
| BIOS, each Wyse | F2, password `Fireport`: BIOS 1.2.5, UEFI with CSM off, UEFI network stack and PXE on, Secure Boot off, power on after AC loss | Manual, ~5 min a unit (S5 runbook step 1) |
| relish for the Mac | `cargo build --release --bin relish` on `appliance-train` | Real |
| Images | `gh run download … -n appliance-x86_64` (lab build) | Real, preview signing until 0.3.0 publishes |
| Serve | `caffeinate -i sudo relish netboot art --key … --interface en7 --mac <10 MACs> --wipe --for 2h` | Real; `--wipe` is PR 1 |
| Install | Power on; F12 → the UEFI IPv4 Realtek entry, or network first in the boot order | Real in VMs; never on a Wyse |
| Claim | `relish machines`, then `relish machines claim ~/wyse --create --name wyse --operator 10.77.0.2 --network 10.77.0.0/24 <mac-1> <mac-2> <mac-3>`, then `relish machines claim ~/wyse <mac-4> … <mac-10>` | Real; CI covers two VMs (`image/tests/claimed-pair.sh`) |
| Check | `relish nodes`, `relish council`, `relish wtf` | Real |
| Tour | `relish apply …`, ingress on port 80 of each node | Real |
| Measure | `image/tools/fleet-measure.sh` | Preview script; reads a `seed-fleet.sh` directory, not a claim directory (PR 6) |
| OS update | `relish os upgrade <next> --channel http://10.77.0.2:8000/…/os-channel.json` | Real; needs a next version from the same run (PR 5) |

The claim asks you to compare each machine's claim key with its monitor. With one DisplayPort monitor and ten machines, that's ten cable swaps; `--trust-lan` skips it, which is fine on an isolated switch with nothing else on it.

### What's still preview or script-only

- **The images' signing.** Lab builds carry a throwaway key per run, so a node can only update to a build from the same run (`os::slot` trusts the release keys and its own image's `os-signing-key.pub.pem`).
- **`image/tools/`**: `netboot-server.sh`, `seed-fleet.sh`, `node-toml.py`, `fleet-measure.sh` and `seed-admin`. W7 retires them; only `fleet-measure.sh` is still needed for S5, and it needs SSH, which only lab images have (`mkosi.conf.d/30-lab.conf`). `relish machines claim --ssh-key` puts the key in the seed (`ClaimOptions::ssh_key` in `src/relish/machines.rs`).
- **The manual** (`docs/manual/14_appliance.md`) is still titled "preview" and starts from CI runs; the S5 runbook still uses `netboot-server.sh` on "a Linux machine" and `seed-fleet.sh`. Both need rewriting around `relish netboot` and `relish machines claim` (W7, PR 7 here).
- **`relish netboot --node`** (a bun node serving netboot) isn't built. The lab doesn't need it.

### Wyse risks, and what covers each

| Risk | Source | Covered? |
|---|---|---|
| **The eMMC isn't blank.** A used Wyse ships with ThinOS or ThinLinux, and the installer refuses a disk that isn't empty unless the kernel command line has `reliaburger.wipe=1` (`image/mkosi.images/installer/.../install`). `relish netboot`'s chain script (`installed::chain_script`) has no way to pass it. | Code | **No: PR 1** |
| A non-blank eMMC also boots before the network, so the first install needs F12 or network first in the boot order. | Runbook says "a blank eMMC falls through" | Docs (PR 7) |
| **8 GB isn't 8 GiB.** CI and the lab test on 8 GiB disks (`truncate -s 8G`), about 1.3 GB more than an 8 GB eMMC's ~7.3 GiB **[unverified exact size]**. The fixed partitions take ~2.8 GiB (ESP 512 MiB, two `/usr` slots of 1100 MiB, two verity of 64 MiB), so the data partition gets ~4.3 GiB, not the ~5.6 the tests see. | `image/mkosi.repart/`, `image/tests/*.sh` | **PR 5**: test on a 7.25 GiB disk |
| The installer picks `mmcblk0boot0/1`. | research §9.3 | Yes: the installer's `lsblk` filter skips read-only disks, and eMMC boot partitions are read-only. Record the `target` line. |
| 2 GB RAM: installer buffering | research §9.2 | Yes: it streams; VMs peaked at 32 MiB anonymous memory |
| 2 GB RAM: seven council voters on ten 2 GB nodes | the reconciler caps voters at seven | Open question 3 |
| PXE entries vanish after a CMOS reset | research §9.4 | Docs |
| No UEFI HTTP Boot on the 3040 | research §9.4 (no evidence found) | Not needed: PXE → iPXE over TFTP → HTTP |
| **iPXE's SNP build on the Realtek's UEFI driver.** relish serves only `ipxe-snp-*.efi` under both names (`server::tftp_files`). If it misbehaves on the Wyse, there's no switch to the full-driver build the spike lab used (`IPXE=full`). | Code | **PR 3**: `--ipxe full` (or `snp`, the default). The artefacts carry both builds (`ipxe-x86_64.efi` and `ipxe-snp-x86_64.efi`, `image::wanted` fetches every `ipxe-*`), so it's a choice of which file `tftp_files` serves |
| Reboot hangs on Cherry Trail (`dw_dmac`) | research §9.6 | Blacklisted in `modprobe.d/reliaburger-wyse3040.conf`; S5 confirms |
| Secure Boot | product plan | Off (the 3040's default) |
| Fanless throttling | research §9.6 | S5 measures `temp_max_mc` |
| Flat RTC battery | inference | NTP from the Pi |

## 5. Order of work

Each is a small PR on top of the train (or on main once it merges), with tests. Estimates are engineer-days including docs.

| # | PR | Tests | Estimate | Needs hardware? |
|---|---|---|---|---|
| 1 | **`relish netboot --wipe`**: adds `reliaburger.wipe=1` to the chain script for machines not yet installed; requires `--mac` (no wiping whatever network-boots on the LAN); the summary lists the MACs it will wipe. `installed::chain_script`, `NetbootOptions`, `Command::Netboot`. | Unit tests on the chain script with and without it; CLI parse tests (refused without `--mac`); `relish-netboot-install.sh` writes a GPT and a filesystem onto the blank disk first, so CI installs over a used disk | 1–1.5 | No |
| 2 | **`relish image download` fetches every architecture** unless `--arch` narrows it, and prints which it saved. `src/relish/image.rs::download`, `ImageAction::Download`. | Unit test on the architecture selection; snapshot of the output | 0.5 | No |
| 3 | **macOS hardening and qualification of `relish netboot`**: refuse a link-local server address (`interface::pick`); make the probe work while configd holds port 68; `--ipxe full` (or `snp`, the default); warn when the probe sees no DHCP offer at all ("nothing hands out addresses here"), which also catches a Pi that's down; manual notes on `--interface`, the firewall, vmnet and `caffeinate`. A Mac run against one x86_64 VM, recorded in `docs/qualification/`. | Unit tests for the link-local refusal and the no-offer warning (`competing_server`'s sibling); loopback probe test; the recorded run | 2–3 | Pi and a cable; a Wyse helps but a VM will do |
| 4 | **The Pi router**: `image/lab/pi/` with `dnsmasq.conf`, `nftables.conf`, `chrony.conf` and a README; the manual's "What you need" mentions a router you control. | `shellcheck`/`dnsmasq --test` in CI on the config | 0.5–1 | Pi |
| 5 | **CI for S5**: upload the next-version image (`appliance-x86_64-next`) and a lab `os-channel.json` signed with the run's key, keep dispatch-run artefacts 7 days, and test on a 7.25 GiB disk. `.github/workflows/appliance.yml`, `image/tests/*.sh`. | The appliance workflow itself | 1 | No |
| 6 | **`fleet-measure.sh` reads a claim directory** (node names and addresses from `relish nodes --output json`, not seed-fleet's `fleet` file). Or, better, a `relish` command that samples the same numbers from bun's metrics, which also works on published images without SSH **[larger: ~3 days]**. | `image/tests/test_*` style unit test on the parser | 0.5 (script) | No |
| 7 | **The S5 runbook and manual rewritten** around `relish netboot` and `relish machines claim`, with the Pi topology (the part of W7 the lab needs). | Docs | 0.5–1 | No |
| — | Optional: **`relish netboot --dhcp`** as designed above | as above | 7–10 | No |

Total for the lab path: **about 6–8 days**, then the S5 day itself (the runbook's estimate is a day of hands-on plus the 24-hour measurement).

### Before the hardware

Most of this needs no Wyse:
- PRs 1, 2, 5, 6 and 7 are pure code, CI and docs.
- PR 3 needs the Mac on a wired segment with something that PXE-boots. The cheapest rig is the Pi (as router) cabled straight to the USB adapter through any switch, and one x86_64 VM bridged onto the adapter with socket_vmnet (`--vmnet-mode=bridged --vmnet-interface=en7`) **[unverified that vmnet bridged mode works on a USB adapter, and that the host sees its own bridged guest]**. The VM runs under TCG, so the install is slow but real (`image/lab/wyse.sh` already boots OVMF x86_64 under TCG). Use `-cpu Westmere` to keep AVX out (research §8.2).
- Then one real Wyse before all ten: BIOS, netboot, install over ThinOS, claim as a one-node cluster. That shakes out PXE and firmware surprises for the cost of one unit.

### Hardware and setup checklist

- **Switch:** an unmanaged gigabit switch is simplest. If the eBay one is managed, turn spanning tree off or set every port to edge/portfast: STP's 30-second listening delay makes PXE's DHCP time out. Turn off "green Ethernet" (EEE) if links flap.
- **Cables:** 12 patch leads (ten Wyses, the Mac, the Pi), Cat5e or better.
- **Power:** ten Wyse supplies. Batches differ, 5 V or 12 V barrel (research §9.1), so use each unit's own supply and don't mix them up. Two 6-way strips or one 12-way. Ten 3040s draw well under 100 W in total **[estimate]**. A switched strip makes the S5 cord-pull and power-loss tests repeatable.
- **Console:** a DisplayPort monitor (or a DP-to-HDMI adapter) and a USB keyboard. The 3040 has no serial port, so this is the only console, and it's where the claim key shows.
- **The Mac's adapter:** gigabit, on a chipset macOS drives natively (Realtek RTL8153 or ASIX AX88179A are the common ones **[unverified for your adapter: check it shows up in `networksetup -listallhardwareports` without a driver]**). It doesn't need PXE support itself: the Mac never netboots. Plug it straight into the Mac, not through a USB hub dock that might sleep it.
- **The Pi:** any model with wired Ethernet and Wi-Fi (3B+, 4 or 5). The 3B+'s Ethernet is USB 2-bound (~300 Mbit/s), which is fine: installs flow Mac → switch → Wyse, and only internet traffic crosses the Pi. Raspberry Pi OS Lite 64-bit on a decent SD card.
- **Labels:** each Wyse's MAC (on the label underneath), its reservation and its node number, on the unit.
- **A USB stick** for the BIOS 1.2.5 update, if any unit is older **[unverified: how Dell ships the 3040 BIOS update outside ThinOS]**.

## The Raspberry Pi's role

| Role | Verdict |
|---|---|
| **Router, DHCP, DNS, NTP** | **Recommended.** Keeps the lab isolated from the home LAN, gives the cluster addresses and internet for its whole life, and matches what CI tests. |
| Fallback netboot server | **Yes, as a backup.** `relish-linux-aarch64` is a release asset, and the Linux path of `relish netboot` is what CI runs. If the macOS run hits a wall (PR 3), run `relish netboot` on the Pi instead. It then needs the artefacts (~1 GB) on the Pi. |
| Eleventh cluster member | No. The appliance image is UEFI-only; a Pi 4 needs community UEFI firmware to boot it, there's no CI for that, and a mixed-architecture cluster is a different test **[unverified that the aarch64 image boots on Pi UEFI firmware at all]**. |
| Out-of-band console | No. The Wyses have no serial port; the console is DisplayPort. |

## Open questions for the maintainer

1. **Which Pi model** is it, and is its Wi-Fi in range of the home router? (The plan assumes Wi-Fi uplink. If not, a second USB Ethernet adapter on the Pi does the same.)
2. **Are the Wyses' eMMCs blank?** If any still runs ThinOS or ThinLinux, PR 1 (`--wipe`) blocks the lab; if they're all blank, it's still needed for reinstalls, but not urgently.
3. **How many council voters** on 2 GB nodes? The reconciler caps the council at seven; research §9.2 suggests three or five. Should claim or `cluster create --bare-metal` set a smaller cap for appliance clusters?
4. **Is the switch managed?** (Spanning tree; see the checklist.)
5. **Do we want `relish netboot --dhcp` at all?** The plan says not for this lab. If the product should work on a bare switch with only a laptop, it's 1.5–2 weeks and belongs after 0.3.0.
6. **Run S5 before or after 0.3.0 publishes a signed OS release?** Before means lab builds and `--key` (and the OS-update step needs PR 5); after means `relish image download --arch x86_64` and the real channel, which is the product path and what the exit test should prove.
7. **Trust on the isolated switch:** is `--trust-lan` acceptable for the lab's claims, or should the run compare all ten claim keys on the monitor, as a user would?
