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

Nobody has run these steps on the lab Mac yet. Commands marked
**[unverified]** are the ones most likely to need a change.

## What you need

- dnsmasq: `brew install dnsmasq`. Don't start its Homebrew service; it runs
  in a terminal of its own for the run.
- The adapter's name and network service:
  `networksetup -listallhardwareports` gives the device (say `en7`), and
  `networksetup -listnetworkserviceorder` the service it belongs to (say
  `USB 10/100/1G/2.5G LAN`). If the device isn't `en7`, change
  `interface=` in `dnsmasq.conf`.
- The Wi-Fi's device, usually `en0`: `route -n get default` names it. If
  it's another, change `wifi =` in `pf-lab.conf`.
- No Internet Sharing, and no VM with shared or host networking running:
  either holds UDP 67. `sudo lsof -nP -iUDP:67` should print nothing yet.

## 1. A fixed address on the adapter

No router on it, so the Mac's own internet stays on the Wi-Fi:

```sh
sudo networksetup -setmanual "USB 10/100/1G/2.5G LAN" 10.77.0.1 255.255.255.0
```

With the adapter cabled to the switch, `ipconfig getifaddr en7` prints
`10.77.0.1` and `route -n get default` still names `en0`.

## 2. NAT out through the Wi-Fi

```sh
sudo sysctl -w net.inet.ip.forwarding=1
sudo pfctl -a com.apple/reliaburger-lab -f image/lab/mac/pf-lab.conf
sudo pfctl -E                       # prints a token; keep it for the end
sudo pfctl -a com.apple/reliaburger-lab -s nat
```

The last line should show the `nat on en0` rule **[unverified on the lab
Mac]**. `pfctl -E` turns pf on and counts a reference, so it doesn't fight
anything else on the Mac that uses pf.

## 3. DHCP

Fill in the reservations in `dnsmasq.conf` (each Wyse's MAC is on the label
underneath it) and uncomment them. Then, in a terminal of its own:

```sh
sudo "$(brew --prefix)/sbin/dnsmasq" --keep-in-foreground \
    --conf-file=image/lab/mac/dnsmasq.conf
```

It logs every DHCP exchange. `sudo lsof -nP -iUDP:67` should now name
dnsmasq. If the application firewall is on, allow dnsmasq as well as relish
(manual, "Serving from a Mac").

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
iPXE; dnsmasq's shows the address each one got.

Claim with the Mac's lab address:

```sh
relish machines claim ~/wyse --create --name wyse \
  --operator 10.77.0.1 --network 10.77.0.0/24 \
  <mac-1> <mac-2> <mac-3>
```

## If the Wyse firmware won't ask on 4011

Some PXE firmware ignores option 60 from a DHCP server **[unverified on the
3040]**: it takes the address and then gives up, with nothing in relish's
log. Then plug the switch into the home router instead, stop dnsmasq, and
run `relish netboot` without `--mode-dhcp-proxy` (the S5 runbook's home
router setup).

## Afterwards

```sh
# Ctrl-C dnsmasq and relish netboot, then:
sudo pfctl -a com.apple/reliaburger-lab -F all
sudo pfctl -X <token from pfctl -E>
sudo sysctl -w net.inet.ip.forwarding=0
sudo networksetup -setdhcp "USB 10/100/1G/2.5G LAN"
```

The cluster keeps its addresses with dnsmasq stopped, but it has no route
out until the Mac routes for it again.
