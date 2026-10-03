# A Raspberry Pi as the Wyse lab's router

The Wyse lab (`docs/plans/2026-10-02-plan-appliance-wyse-lab.md`) is an
isolated switch with ten Dell Wyse 3040s, a Mac running `relish netboot`, and
a Raspberry Pi. The Pi is the lab's home router. It hands out addresses, with
a reservation per Wyse, and answers DNS and NTP. It also routes the lab to the
internet through its Wi-Fi, so nodes can pull images and OS updates.

It does nothing about booting. `relish netboot` on the Mac is a ProxyDHCP: it
adds a boot file to the conversation and never hands out an address, and it
refuses to start if another boot server answers. That's the topology CI
tests in `image/tests/relish-netboot-install.sh`, with dnsmasq in a network
namespace playing the router.

```
home Wi-Fi ── wlan0  Raspberry Pi  eth0 10.77.0.1 ── switch ─┬─ Mac (USB Ethernet) 10.77.0.2, relish netboot
                     NAT, DHCP, DNS, NTP                     ├─ wyse-01 10.77.0.11
                                                             ├─ …
                                                             └─ wyse-10 10.77.0.20
```

| File | On the Pi |
|---|---|
| `dnsmasq.conf` | `/etc/dnsmasq.d/lab.conf` |
| `nftables.conf` | `/etc/nftables.conf` |
| `chrony.conf` | `/etc/chrony/chrony.conf` |
| `sysctl-lab-router.conf` | `/etc/sysctl.d/90-lab-router.conf` |

`image/tests/test_pi_router.py` checks the files: `dnsmasq --test` and
`nft -c` parse them, and dnsmasq has no boot options. The appliance workflow
runs it.

Nobody has run these steps on a real Pi yet. Commands marked
**[unverified]** are the ones most likely to differ on your Pi OS release.

## What you need

- A Raspberry Pi 5 or Pi 4 with 4 GB, its power supply, and a 32 GB microSD
  card.
- Raspberry Pi Imager on the Mac (`brew install --cask raspberry-pi-imager`).
- A home Wi-Fi network that isn't `10.77.0.0/24`. If it is, pick another
  subnet and change it in every file here.

## 1. Flash the card

In Raspberry Pi Imager, choose **Raspberry Pi OS Lite (64-bit)**. Before
writing, edit the OS customisation settings:
- hostname `pi`;
- a user and password;
- your Wi-Fi's name, password and country (the Wi-Fi stays off without a
  country);
- SSH on, with your public key.

Boot the Pi with eth0 *not* yet on the lab switch, and SSH in over Wi-Fi:
`ssh <user>@pi.local`.

## 2. Bring it up to date and install the services

```sh
sudo apt update && sudo apt full-upgrade -y
sudo apt install -y dnsmasq nftables chrony
```

Installing chrony removes `systemd-timesyncd` **[unverified on Trixie]**.
dnsmasq starts straight away with Debian's default configuration (DNS on every
interface, no DHCP). Step 4 replaces that.

## 3. A static address on eth0

Raspberry Pi OS manages the network with NetworkManager. Find the profile it
made for eth0 and replace it with a static one that has no gateway, so the
default route stays on Wi-Fi:

```sh
nmcli -f NAME,DEVICE,TYPE connection show       # e.g. "Wired connection 1" [unverified name]
sudo nmcli connection delete "Wired connection 1"
sudo nmcli connection add type ethernet ifname eth0 con-name lab \
    ipv4.method manual ipv4.addresses 10.77.0.1/24 ipv4.never-default yes \
    ipv6.method disabled
sudo nmcli connection up lab
```

NetworkManager only applies the address once eth0 has a link, so plug it into
the switch now. `ip -4 addr show eth0` should show `10.77.0.1/24`, and
`ip route` a default route through `wlan0` only.

## 4. Copy the files

From the Mac, in a checkout of the repository:

```sh
scp image/lab/pi/{dnsmasq.conf,nftables.conf,chrony.conf,sysctl-lab-router.conf} <user>@pi.local:
```

Then, on the Pi:

```sh
sudo install -m 0644 dnsmasq.conf /etc/dnsmasq.d/lab.conf
sudo install -m 0755 nftables.conf /etc/nftables.conf
sudo install -m 0644 chrony.conf /etc/chrony/chrony.conf
sudo install -m 0644 sysctl-lab-router.conf /etc/sysctl.d/90-lab-router.conf
```

Debian's dnsmasq reads every file in `/etc/dnsmasq.d/`. `sudo dnsmasq --test`
reads the whole configuration and should say `syntax check OK`.

## 5. Fill in the reservations

Each Wyse has its MAC on the label underneath. Edit
`/etc/dnsmasq.d/lab.conf`, put each MAC into its line, and remove the `#`:

```
dhcp-host=6c:4b:90:12:34:56,10.77.0.11,wyse-01
```

Write the node number on the unit as well, so wyse-03 is the third box on the
shelf and not just the third line in a file. The claim records each machine's
current address as its node address, which is why the addresses must never
move.

Do the same for the Mac's USB Ethernet adapter (`ifconfig en7 | grep ether`,
with the adapter's name from `networksetup -listallhardwareports`). Its line
gives it 10.77.0.2 and no default route, so the Mac keeps using its Wi-Fi for
the internet.

Lost a label? Boot the Wyse from the network once: it gets an address from
the pool, and its MAC shows in `/var/lib/misc/dnsmasq.leases` and in the
dnsmasq log. `relish machines` lists them too.

## 6. Turn it on

The firewall drops everything that arrives on wlan0, SSH included. From here
on, manage the Pi from the lab side (`ssh <user>@10.77.0.1` from the Mac) or
from its own keyboard and screen. Then:

```sh
sudo sysctl --system                  # IP forwarding, now and at every boot
sudo systemctl enable --now nftables
sudo systemctl restart dnsmasq chrony
```

## 7. Check it

On the Pi:

```sh
sudo dnsmasq --test                       # syntax check OK
sudo nft list ruleset                     # the rules from nftables.conf
sysctl net.ipv4.ip_forward                # = 1
chronyc tracking                          # Leap status: Normal, once synchronised
journalctl -u dnsmasq -f                  # DHCPDISCOVER, DHCPOFFER, DHCPACK as machines ask
cat /var/lib/misc/dnsmasq.leases          # one line per lease
```

On the Mac, with the adapter on the switch:

```sh
ipconfig getifaddr en7                    # 10.77.0.2
ipconfig getpacket en7                    # router should be absent, domain_name_server 10.77.0.1
route -n get default                      # still the Wi-Fi interface
ping -c 1 10.77.0.1
```

Then power on a Wyse. Its DHCPACK should show in the log with its reserved
address, and once it runs the appliance it should reach the internet through
the Pi. `relish netboot` starting without complaint is the last check: it
refuses to run if the Pi answers PXE.

## If the Wi-Fi doesn't reach

A second USB Ethernet adapter on the Pi, cabled to the home router, does the
same job:

1. Plug it in. It shows as `eth1` in `ip link` **[unverified: the name on your
   Pi OS release]**, and NetworkManager gives it a DHCP address from the home
   router on its own.
2. In `/etc/nftables.conf`, change `define WAN = "wlan0"` to `"eth1"`, and run
   `sudo nft -f /etc/nftables.conf`.
3. Optionally turn the Wi-Fi off: `sudo nmcli radio wifi off`.

Check which port is which before cabling: the lab must stay on the built-in
port, `eth0`, because that's the only interface dnsmasq serves. The adapter
plugs into the home router, never into the lab switch.
