# The Mac as the Wyse lab's router

The Wyse lab can run on the home network, with the home router handing out
addresses and `relish netboot` beside it. This is the other way: the lab on
a switch of its own, isolated from the home network, with the Mac as its
only server. The Mac does four jobs:

- **DHCP:** dnsmasq on the USB Ethernet adapter (`dnsmasq.conf`), with a
  reservation per Wyse. It owns UDP 67.
- **The boot part:** `relish netboot --mode-dhcp-proxy` answers PXE on UDP
  4011 only. dnsmasq's offers carry option 60 `PXEClient`, which tells PXE
  firmware and iPXE to ask the same machine on 4011 (PXE specification 2.1,
  section 2.2.4).
- **The route out:** pf NATs the lab out through the Mac's Wi-Fi
  (`pf-lab.conf`), so the nodes can pull images and reach NTP.
- **The operator:** `relish machines claim` and the rest, from 10.77.0.1.

```
home Wi-Fi ── en0  Mac  en7 10.77.0.1 ── switch ─┬─ wyse-1  10.77.0.11
                   NAT out through en0           ├─ wyse-2  10.77.0.12
                   dnsmasq: DHCP on UDP 67       └─ wyse-3  10.77.0.13
                   relish netboot: UDP 4011 and 69, HTTP
```

CI tests this split on every lab build: `image/tests/relish-netboot-install.sh`
runs `dnsmasq.conf` on the runner (with CI's interface and subnet) and
`relish netboot --mode-dhcp-proxy` beside it, and a VM installs from them.
`image/tests/test_mac_router.py` checks the files.

The catch: the cluster's route out is the Mac. Its addresses survive the Mac
sleeping (the reservations are infinite leases), but image pulls, OS updates
and NTP stop until it wakes. Keep it awake for the run (`caffeinate`), and
plugged in.

**First run, 15 November 2026:** an M2 MacBook Pro with an ASIX AX88179A
adapter (`en9`, network service "AX88179A") and a Wyse 3040 on BIOS 1.2.3.
Steps 1 to 3 worked as written below, and the 3040 follows option 60 to UDP
4011. Two bugs that run found are fixed:
- relish's reply on 4011 carried option 43, which the 3040's firmware
  takes as "boot locally" (#1970);
- the installer had no driver for the 3040's network PHY (#1969).

The copy-paste blocks here carry no `#` comments: macOS's zsh runs a pasted
comment as an argument unless `interactivecomments` is set.

## What you need

- dnsmasq: `brew install dnsmasq`. Don't start its Homebrew service; it runs
  in a terminal of its own for the run.
- The adapter's name and network service:
  `networksetup -listallhardwareports` gives the device (say `en7`), and
  `networksetup -listnetworkserviceorder` the service it belongs to (say
  `USB 10/100/1G/2.5G LAN`, or the chip's name, such as `AX88179A`). Other
  `Ethernet Adapter (en4)` rows with no cable are macOS's virtual ones.
- The Wi-Fi's device, usually `en0`: `route -n get default` names it. If
  it's another, change `wifi =` in `pf-lab.conf`.
- No Internet Sharing, and no VM with shared or host networking running:
  either holds UDP 67. `sudo lsof -nP -iUDP:67` should print nothing yet.
- Each Wyse's MAC from the label underneath it. The label writes it with
  dashes (`8c-ec-4b-57-72-6d`); dnsmasq and relish want colons.

## 1. A fixed address on the adapter

No router on it, so the Mac's own internet stays on the Wi-Fi. Use your
adapter's service name:

```sh
sudo networksetup -setmanual "USB 10/100/1G/2.5G LAN" 10.77.0.1 255.255.255.0
```

With the adapter cabled to the switch, `ifconfig en7 | grep status` says
`active`, `ipconfig getifaddr en7` prints `10.77.0.1`, and
`route -n get default` still names `en0`.

## 2. NAT out through the Wi-Fi

```sh
sudo sysctl -w net.inet.ip.forwarding=1
sudo pfctl -a com.apple/reliaburger-lab -f image/lab/mac/pf-lab.conf
sudo pfctl -E
sudo pfctl -a com.apple/reliaburger-lab -s nat
```

`pfctl -E` turns pf on, counts a reference so it doesn't fight anything else
on the Mac that uses pf, and prints a token: keep it for the end. The last
line should show `nat on en0 inet from 10.77.0.0/24 to any -> (en0)`. The
"No ALTQ support" lines and the warning about `-f` flushing rules are
harmless: the rules go into our own anchor.

## 3. DHCP

dnsmasq's `interface=` is a list, so `--interface` on the command line adds
to the file's `en7` instead of replacing it, and dnsmasq then stops with
"unknown interface en7". If your adapter isn't `en7`, run from a copy with
it swapped in (`en9` below). The reservations can go on the command line, one
`--dhcp-host` per Wyse, instead of being uncommented in the file. Then, in a
terminal of its own:

```sh
sed 's/^interface=en7$/interface=en9/' image/lab/mac/dnsmasq.conf > ~/wyse-lab/dnsmasq.conf
sudo "$(brew --prefix)/sbin/dnsmasq" --no-daemon --conf-file=$HOME/wyse-lab/dnsmasq.conf \
  --dhcp-host=<mac-1>,10.77.0.11,wyse-1,infinite \
  --dhcp-host=<mac-2>,10.77.0.12,wyse-2,infinite \
  --dhcp-host=<mac-3>,10.77.0.13,wyse-3,infinite
```

`--no-daemon` keeps it in the foreground, logs to the terminal, and stops on
Ctrl-C; `--keep-in-foreground` logs only to the system log and ignores
Ctrl-C. It prints `started, version …, DNS disabled` and the DHCP range,
then every DHCP exchange. If the application firewall is on, allow dnsmasq
as well as relish (manual, "Serving from a Mac").

## 4. Netboot

In another terminal, `relish netboot` as usual, plus `--mode-dhcp-proxy`:

```sh
caffeinate -i sudo relish netboot os --interface en7 --for 3h --mode-dhcp-proxy \
  --mac <mac-1> --mac <mac-2> --mac <mac-3>
```

Its start-up says it answers PXE on UDP 4011 only. It refuses to start if
anything else on the switch hands out addresses, and warns if nothing on
the Mac holds UDP 67 or if dnsmasq leaves out option 60. relish's log then
shows an `ack on 4011` for each machine's firmware and another for its
iPXE, then a `tftp:` line for each file; dnsmasq's shows the address each
one got.

Claim with the Mac's lab address:

```sh
relish machines claim ~/wyse --create --name wyse \
  --operator 10.77.0.1 --network 10.77.0.0/24 \
  <mac-1> <mac-2> <mac-3>
```

## If a machine won't boot from relish's reply

The symptom: dnsmasq gives the machine its address, relish logs
`ack on 4011`, and then nothing: no `tftp:` line, and the firmware moves on
to IPv6 or the next boot entry. That's how the 3040 behaved before #1970.
To take relish's DHCP reply out of the picture, let dnsmasq name the boot
file itself. relish still serves TFTP and HTTP:

```sh
grep -v '^dhcp-option-force=tag:pxe,60,PXEClient$' ~/wyse-lab/dnsmasq.conf > ~/wyse-lab/dnsmasq-direct.conf
printf '%s\n' 'dhcp-match=set:ipxe,175' 'dhcp-boot=tag:!ipxe,ipxe-x86_64.efi,,10.77.0.1' 'dhcp-boot=tag:ipxe,boot.ipxe,,10.77.0.1' >> ~/wyse-lab/dnsmasq-direct.conf
```

Restart dnsmasq with `--conf-file=$HOME/wyse-lab/dnsmasq-direct.conf`. With
it, the 3040 loaded iPXE, `boot.ipxe` and the installer. If instead dnsmasq
logs nothing at all from the machine, check the cable and the firmware's
network boot settings; if relish logs nothing after the DHCP exchange, the
firmware may ignore option 60, so plug the switch into the home router,
stop dnsmasq, and run `relish netboot` without `--mode-dhcp-proxy` (the S5
runbook's home router setup).

## Afterwards

Ctrl-C dnsmasq and relish netboot, then undo the router, with the token
`pfctl -E` printed:

```sh
sudo pfctl -a com.apple/reliaburger-lab -F all
sudo pfctl -X <token>
sudo sysctl -w net.inet.ip.forwarding=0
sudo networksetup -setdhcp "USB 10/100/1G/2.5G LAN"
```

`pfctl -X` may say a reference remains: that's macOS's own, so pf stays on
for the system's rules. The cluster keeps its addresses with dnsmasq
stopped, but it has no route out until the Mac routes for it again.
