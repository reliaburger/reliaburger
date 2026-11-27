# Bare metal: the appliance and netboot

The five-minute tour builds its cluster in laptop VMs. This chapter builds one
on real machines: a handful of old mini PCs or thin clients, each erased and
turned into a Reliaburger appliance over the network. There's no distro to
install, nothing to log in to, and no USB stick to boot from. Each machine
boots from the network once, installs itself in under a minute, and from then
on it's a node.

Four commands do it all:
- `relish image download` fetches the newest signed OS release and checks it;
- `sudo relish netboot` installs it on every machine that network-boots;
- `relish machines claim` turns the installed machines into a cluster over
  the network;
- or `relish cluster create --bare-metal` writes a seed per machine for a USB
  stick, if you'd rather not claim over the network.

CI runs them against QEMU VMs on every change to the image. On x86_64 it
runs the lot: netboot installs, claimed and seeded clusters, an OS update
and a broken update falling back. On aarch64 it runs the boot test only.
The hardware it's built for is the Dell Wyse 3040, netbooted from a Mac.
The exit test took three of them from power-on to a working three-node
cluster in about 25 minutes
([the Wyse lab record](https://github.com/reliaburger/reliaburger/blob/main/docs/qualification/2026-11-20-wyse-3040.md)).
Other firmware may still have surprises; "What's missing" lists the ones
we know of.

## What you need

- **Two or more machines**, ideally three or more:
  - x86_64 or arm64, with UEFI and network boot (PXE);
  - a wired network card the installer has a driver for: Realtek RTL8111,
    RTL8168 or RTL8125, Intel I219, I225, I226, I210 or I350, Broadcom
    NetXtreme (`tg3`), or a USB Ethernet adapter;
  - at least 2 GiB of RAM and an 8 GB disk;
  - wired to the same network.

  Raspberry Pis don't work yet: they boot through their own firmware, not
  UEFI ("What's missing").

  **The installer erases the largest built-in disk.** A blank one goes
  without asking; one that holds anything else is wiped only after you say
  yes at the `relish netboot` terminal, or name the machine with `--wipe`.
- **A router you control**, with a DHCP reservation for each machine, so each
  keeps the same address. Nodes find each other by address, and they need the
  router's default route to work out their own. Your home router will do.
  Leave booting to `relish netboot`: the router must not answer PXE. On a
  switch of its own, the machine running `relish netboot` can be the router
  too ("The Wyse lab", below).
- **A machine on that network to serve the netboot**: your laptop (Linux or
  macOS), any spare box, or a VM bridged onto the LAN. It needs `relish` and
  root. A Mac on a USB-C Ethernet adapter does fine (see "Serving from a
  Mac").
- **Your laptop**, on the same network, with `relish` from the same release
  as the images' bun. For a CI build, that's relish built from the same
  commit (`cargo build --release --bin relish`), plus `gh` to download it and
  `python3` to serve its update.
- Optionally, a USB stick you can erase, if you'd rather carry seeds to the
  machines than claim them over the network.

## Get the images

`relish image download --dir os` fetches every architecture the newest
release has, whatever the machine you run it on, so an arm64 Mac serving
x86_64 machines gets the right image. It checks each one against the release
key relish carries, saves it as `os/x86_64/` and `os/aarch64/`, and ends by
naming what it saved:

```
Saved x86_64 and aarch64 under os (os/x86_64/, os/aarch64/)
```

To fetch only the architecture of your machines and save a few hundred
megabytes, name it with `--arch`; repeat the flag for more than one:

```sh
relish image download --dir os --arch x86_64
```

An architecture the release doesn't have is refused, and so is a name relish
doesn't know (`x86_64` and `aarch64` are the two it builds).

A slow link is fine: there's no limit on how long a download takes. A server
that stops answering isn't. The download gives up after 30 seconds without a
connection, or 60 seconds without a byte, and says which of the two timed
out. Run it again: files already saved and checked aren't fetched twice.

It saves what an install needs. `--all` also fetches the pieces only OS
updates use, the kernel image and the `/usr` images, for serving updates
yourself. `--channel URL` reads another release channel than the published
one, such as a CI lab build's, and `--key` names the public key that signs it.

### A CI lab build

To try a change to the image before it's released, take a CI lab build
instead. The appliance workflow builds both architectures on every change,
each run signed with a throwaway key of its own. For a lab day, start a run
by hand (Actions → Appliance image → Run workflow, with `publish` left off),
so its artefacts last a week rather than a day. A pull request's run keeps
them for a day, so download it the same day:

```sh
gh run list --workflow appliance.yml --status success
gh run download <run-id> -n appliance-x86_64 -D art/x86_64
gh run download <run-id> -n appliance-aarch64 -D art/aarch64
```

You only need the architecture of your machines. Each artefact holds:
- the installer, `reliaburger-os-installer_<version>.efi`;
- the whole disk image, `reliaburger-os_<version>.raw.zst`;
- its `SHA256SUMS`, and that run's Ed25519 signature over them;
- the pieces an update ships;
- iPXE and its boot script, in `netboot/`.

The installer checks that signature before it writes anything. It carries the
same run's public key. The x86_64 run also uploads `appliance-x86_64-next`,
the version to update to ("Updating a CI build").

## Serve them

On the machine that serves the netboot, point `relish netboot` at the
directory. For a release from `relish image download --dir os`:

```sh
sudo relish netboot os
```

For a CI run's artefacts, name the run's throwaway key too, since they aren't
signed with the release key:

```sh
sudo relish netboot art --key art/x86_64/spike-signing-key.pub.pem
```

Before it serves anything, it checks the signature on each architecture's
`SHA256SUMS` and the SHA-256 of every file it lists, and refuses to start if
any of them is wrong. Then it prints what it serves and logs every boot
request: each machine's MAC address, its firmware, and what it was told to
boot. Every line it logs while serving starts with the local time, to the
second, and so does the line that says it started serving, so you can time
each machine's stages from the log alone. It stops after an hour (`--for 3h` to change that) or at Ctrl-C.

It doesn't touch your router's DHCP. When a machine asks the network where to
boot from, your router still hands out the address, and relish answers only
the boot part: a *ProxyDHCP*. It never assigns an address, so it can't break
anything else on the LAN. The machine then fetches a small boot program
(iPXE) over TFTP, and iPXE fetches the installer over HTTP on port 8080. If
the serving machine has a firewall, open UDP 67, 69 and 4011 and TCP 8080.

A few options for real networks:
- It answers from the interface the default route uses. Name another with
  `--interface eth0` (or `--address 192.168.1.20`).
- `--mac d8:9e:f3:12:34:56`, once per machine, answers only those machines.
  Anything else on the LAN that network-boots is left alone.

Trust the install LAN. The installer checks every byte of the image it
writes, so nobody on the LAN can make a machine install something else. But
relish believes a machine's reports (its used disk, and that it installed)
only from the address that machine's own DHCP request came from or asked
for, and DHCP itself isn't authenticated: a host on the LAN that forges DHCP
requests can still stall an install. If the log says `no install ticket` for
a machine you're installing, its boot script came from another address than
its DHCP request; it installs, but relish can't record it or ask you about
its disk.
- `--http-port 8081` serves HTTP on another port than 8080, for a serving
  machine where 8080 is taken. The machines are told the port, so nothing
  else changes. Open that port instead of 8080 in its firewall.
- `--ipxe full` hands machines iPXE's full-driver build instead of the
  default `snp` one. `snp` drives the network card through the firmware's
  own driver, which suits odd cards; `full` brings iPXE's drivers, for
  firmware whose network stack misbehaves. It refuses to start if the
  release you serve doesn't have the full build.
- `--mode-dhcp-proxy` is for a network whose DHCP server runs on the
  serving machine itself, such as dnsmasq on the Mac in an isolated lab
  ("The Wyse lab" below). That server owns UDP 67 and puts option 60
  `PXEClient` in its offers, which sends network-booting machines to UDP
  4011 on the same address, and relish answers there only. It refuses to
  start if another machine on the network hands out addresses too, and
  warns if nothing on this machine holds UDP 67.

Once the files check out, it broadcasts a network-boot request of its own
and listens for five seconds:
- If another netboot server answers, it refuses to start. Two on one LAN
  race to boot every machine.
- If your router answers, it says so (`192.168.1.1 hands out addresses on
  eth0`) and starts.
- If nothing answers, it warns that nothing hands out addresses there and
  starts anyway. Machines that network-boot need an address before they ask
  relish anything, so check that the router is up and on the same switch. A
  DHCP server that only answers machines it knows (reservations with no
  range) stays quiet too, and then the warning is harmless.
- If it can't send the request at all, it warns (`can't check the network`)
  and starts anyway. This happens when your machine's own DHCP client holds
  UDP port 68, as many Linux ones do. relish can't tell then whether another
  netboot server is running, so check that yourself.

It also refuses to serve from a self-assigned address (`169.254.x.x`). An
interface gets one when no DHCP server answered it, and the machines you
boot couldn't reach it. Check the cable and the router, then reconnect the
interface, or give it a fixed address and pass that with `--address`.

It remembers each machine whose installer reports a finished install, by MAC
address and SMBIOS UUID, in `netboot-installed.json` in the directory it
serves. A machine whose install failed isn't remembered, so it gets the
installer again on its next network boot. When `--for` is up, relish waits
up to 20 more minutes for installs still running. Next time that machine
network-boots, it's told to boot its disk instead. Pass `--reinstall` to
install over them again. Their disks then hold Reliaburger OS, which counts
as a used disk: each one waits for your yes, or for its `--wipe`. Each
machine reinstalls once per run, so a machine whose firmware tries the
network first boots its new install afterwards rather than installing
again.

### Serving from a Mac

`relish netboot` runs on macOS too. A Mac usually joins a wired LAN through
a USB-C or Thunderbolt Ethernet adapter, and a few things are different:
- **Name the adapter.** The default route is usually Wi-Fi, so pass
  `--interface` with the adapter's name. `networksetup
  -listallhardwareports` lists them; the adapter is the `Device:` under its
  `Hardware Port:` (often `USB 10/100/1000 LAN`), something like `en7`.
  `ifconfig en7` shows its address.
- **Use sudo.** macOS lets anyone listen on low ports on every address, but
  relish's TFTP and HTTP listen on the adapter's own address, and that still
  needs root.
- **The application firewall.** If it's on, macOS may ask whether `relish`
  may accept incoming connections, or silently drop them for a binary run
  under sudo. Check it, and allow relish if it's on:

  ```sh
  /usr/libexec/ApplicationFirewall/socketfilterfw --getglobalstate
  sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add "$(which relish)"
  sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblockapp "$(which relish)"
  ```

- **Keep it awake.** A Mac that sleeps mid-install stops TFTP and HTTP, and
  the machine installing gives up. `caffeinate -i` keeps it awake for as
  long as relish runs:

  ```sh
  caffeinate -i sudo relish netboot os --interface en7
  ```

- **Internet Sharing** runs its own DHCP server, which holds the port relish
  needs. Turn it off for the adapter (relish says so if it's on). A VM tool
  with shared or host-only networking may run one too.
  `sudo lsof -nP -iUDP:67` shows who holds the port. The one DHCP server
  that can share the Mac with relish is one you set up for it, with
  `--mode-dhcp-proxy` (below).

### The Wyse lab

The lab we built this for is a Mac on a USB-C Ethernet adapter, a gigabit
switch, and three Dell Wyse 3040s. It runs either of two ways.

**On the home network.** The switch plugs into the home router, which hands
out the addresses, with a reservation for each Wyse and one for the Mac's
adapter, and takes the machines to the internet:

```
home router ── switch ─┬─ Mac, USB-C Ethernet (en7) 192.168.1.10, relish netboot
DHCP, DNS, NAT         ├─ wyse-1  192.168.1.11
                       ├─ wyse-2  192.168.1.12
                       └─ wyse-3  192.168.1.13
```

Turn the Mac's Wi-Fi off for the run, so the adapter is its only way onto
the LAN and the nodes always see it at the same address. relish runs on the
Mac and does only the boot part, for the machines you name:

```sh
caffeinate -i sudo relish netboot os --interface en7 --for 3h \
  --mac <mac-1> --mac <mac-2> --mac <mac-3>
```

**Isolated, with the Mac as the router.** The switch connects to nothing but
the Mac and the Wyses. The Mac's adapter gets a fixed address, dnsmasq on it
hands out the addresses, pf takes the lab out to the internet through the
Mac's Wi-Fi, and relish answers PXE on UDP 4011 only, beside dnsmasq:

```
home Wi-Fi ── en0  Mac  en7 10.77.0.1 ── switch ─┬─ wyse-1  10.77.0.11
                   NAT out through en0           ├─ wyse-2  10.77.0.12
                   dnsmasq: DHCP on UDP 67       └─ wyse-3  10.77.0.13
                   relish netboot: UDP 4011 and 69, HTTP
```

```sh
caffeinate -i sudo relish netboot os --interface en7 --for 3h --mode-dhcp-proxy \
  --mac <mac-1> --mac <mac-2> --mac <mac-3>
```

[`image/lab/mac/README.md`](https://github.com/reliaburger/reliaburger/blob/main/image/lab/mac/README.md)
sets up the address, dnsmasq and pf, and undoes them afterwards. The cluster
then depends on the Mac for its route out, so keep the Mac awake and
plugged in. A Wyse 3040 on BIOS 1.2.3 follows dnsmasq to relish on 4011.
If a machine's firmware takes dnsmasq's address but never asks relish on
4011, use the home network instead. If it asks, relish logs `ack on 4011`,
and then the firmware fetches nothing and moves on, the README's "If a
machine won't boot from relish's reply" has dnsmasq name the boot file
itself.

Either way, time comes from Ubuntu's NTP servers through the router. That
matters on second-hand machines, whose clock batteries may be flat: a node
whose clock is years out rejects every certificate.

If the switch has spanning tree, turn it off, or turn on its fast-start
setting for every port (often called PortFast or edge port). A port that
spends thirty seconds listening before it forwards makes the firmware's
network boot time out.

## Prepare the machines

In each machine's firmware setup:
- turn on UEFI network boot (sometimes called "Network Stack" or "PXE");
- turn Secure Boot off;
- put the disk before the network in the boot order.

A blank disk isn't bootable, so the first boot falls through to the network.
After that, the disk boots. A disk that still holds its old system boots that
instead, so for the first install pick the network from the firmware's boot
menu (F12 on a Dell), or put the network first until it has installed. The
installer puts the disk first again by itself.

If you leave the network first, it still works, only slower. With the netboot
server running, an installed machine is told to boot its disk, which costs a
few seconds. With the server off, the firmware tries every kind of network
boot it has before it gives up and boots the disk, which can take minutes.

## Install

Power the machines on, all at once if you like. Each one takes its address
from your router and its boot instructions from the netboot server, then
starts the installer. The console shows the installer's progress:

```
reliaburger-install: installer 2026.40.32, fetching from http://192.168.1.20:8080/x86_64
reliaburger-install: signature good; reliaburger-os_2026.40.32.raw.zst should hash to …
reliaburger-install: target /dev/sda, 8589 MB
reliaburger-install: installed 2026.40.32 to /dev/sda in 16 s
reliaburger-install: this machine: enp1s0 d8:9e:f3:12:34:56 192.168.1.51/24
reliaburger-install: rebooting into the installed system
```

The installer streams the image straight onto the disk and hashes it on the
way, so a 2 GiB machine installs a 1.7 GB image without holding any of it in
memory. It puts the disk first in the boot order and reboots. After a
reboot the node runs bun on its own, not yet in a cluster.

### A disk that isn't blank

A second-hand machine usually still has its old system on the disk. A Dell
Wyse 3040, for instance, may come with ThinOS. The installer never wipes a
disk on its own. Before it writes anything, it reports what's on the disk to
`relish netboot` and waits. The netboot terminal asks you:

```
21:04:17 relish netboot: 6c:4b:90:12:34:56: /dev/mmcblk0, 7.8 GB, gpt, 4 partitions: vfat "EFI", ext4 "ThinOS", (no filesystem), swap — wipe? [y/N]
```

Type `y` and Enter to wipe it and install. Anything else, or no answer within
10 minutes, leaves the disk as it was. The installer says so on the machine's
console and powers it off. For the rest of that `relish netboot` run, the
machine is told to boot its own disk and isn't asked again. Start `relish
netboot` again to be asked again.

A blank disk installs without a question. A disk that already holds
Reliaburger OS doesn't install at all: the installer boots it, unless
`relish netboot` runs with `--reinstall`, and then it asks like any other
used disk.

For an unattended run, list the machines whose disks may be wiped up front:

```sh
sudo relish netboot os --mac 6c:4b:90:12:34:56 --wipe 6c:4b:90:12:34:56
```

`--wipe` takes one MAC address and can be repeated. There's no way to say
"wipe everything": each machine is named. When `relish netboot` runs without
a terminal (from a script, or with its input redirected), there's nobody to
ask, so it wipes only the disks of `--wipe` machines and leaves every other
used disk alone. Every decision goes into its log:

```
21:04:17 http: 6c:4b:90:12:34:56: disk /dev/mmcblk0, 7.8 GB, gpt, 4 partitions: …: wiping it and installing (--wipe lists it)
```

If several machines report at once, the questions come one at a time.

### Watch the boot logs from your terminal

A machine with no serial port, such as the Wyse 3040, only shows its boot on
its monitor. With `--stream-logs`, each machine you install sends its kernel
and boot lines to `relish netboot` over UDP, from the installer on and on
every boot after:

```sh
sudo relish netboot os --stream-logs
```

They print among the netboot server's own lines, named by the machine's MAC:

```
log 6c:4b:90:12:34:56: reliaburger-install: target /dev/mmcblk0, 7818 MB
log 6c:4b:90:12:34:56: [    9.412871] r8169 0000:01:00.0 enp1s0: Link is Up - 1Gbps/Full
log 6c:4b:90:12:34:56: reliaburger: bun not healthy after 150 s on OS 2026.47.228
```

Each line also goes to `os/logs/<mac>.log`. Three machines booting at once
send hundreds of kernel lines each, so to keep the screen to what's
happening, print only the boot units' lines:

```sh
sudo relish netboot os --stream-logs=units
```

```
log 6c:4b:90:12:34:56: reliaburger-netlog: streaming from 192.168.1.77 (enp1s0 6c:4b:90:12:34:56) to 192.168.1.20:6666
log 6c:4b:90:12:34:56: [    9.412871] install[412]: reliaburger-install: target /dev/mmcblk0, 7818 MB
log 6c:4b:90:12:34:56: [   31.554310] reliaburger-boot-check: reliaburger: bun healthy on OS 2026.47.228
```

`--stream-logs` on its own, or `--stream-logs=all`, prints every line.
`--stream-logs=units` prints the lines of the installer, the boot check, bun,
the seed and the clock wait, and the machine's hello, which names its MAC.
It leaves the kernel's other lines out, except a panic, an oops or a `BUG:`,
which always print. Write the value with `=`: a word after a bare
`--stream-logs` is the netboot directory. Either way, every line goes to
`os/logs/<mac>.log`, so `tail -f os/logs/<mac>.log` shows one machine's whole
kernel log.

The machine's first line on
each boot names its MAC, so it keeps its name when your router gives the
installed system a different address from the one it network-booted with. A
line from a machine `relish netboot` didn't answer shows as
`log <address> (not served):`, and goes to `os/logs/unserved.log` with the
address in front, one file for all such senders. Each file stops growing at
64 MiB, and relish says so once; later lines only print.

- The installer writes your machine's address into `reliaburger/netlog.conf`
  on the installed disk's EFI partition. On each boot, once the network is up,
  the machine points the kernel's netconsole at it and sends the kernel log so
  far, then every kernel line after (panics too) and the lines of the boot
  units: the boot check, bun, the seed and the clock wait.
- A boot that dies before its network comes up sends nothing. Use the monitor.
- It's UDP: lines can be lost, and nothing is retried.
- The lines cross your LAN unencrypted, and anyone on it can send lines to
  the listener. That's why it's off unless you ask for it. To stop a machine
  sending, run `relish os boot-logs --off` (below).

To listen without serving anything, say while an installed machine reboots,
run only the listener. It doesn't need root:

```sh
relish machines logs --interface en7 --dir os
```

It listens on UDP 6666 until Ctrl-C. Without `--dir` it only prints. It
answered no machine, so it names each line by its address, marked
`(not served)`, and with `--dir` every line goes to `os/logs/unserved.log`.

#### Turn boot logs on or off later

Once the machines are a cluster, `relish os boot-logs` shows where each one
sends its boot logs, and changes it. Any token can show it. Changing it
needs a cluster-wide admin token, since the lines carry the node's whole
kernel log; an admin token scoped to namespaces gets 403. Each change is
audited as `os.boot_logs_set`, with the target or `off`:

```sh
relish os boot-logs                          # where each node sends them
relish os boot-logs --to 192.168.1.20        # to this machine, UDP 6666
relish os boot-logs --to 192.168.1.20:7000   # another port
relish os boot-logs --off                    # stop
relish os boot-logs --off --node home-3      # one node (repeat --node for more)
```

```
Boot logs go to 192.168.1.20:6666 over UDP, in cleartext, from now on. `relish machines logs` on that machine shows them.

NODE                 BOOT LOGS
home-1               to 192.168.1.20:6666
home-2               to 192.168.1.20:6666
laptop               not an appliance
```

Each node's bun rewrites `reliaburger/netlog.conf` on its EFI partition, or
deletes it for `--off`, so the setting holds across reboots and OS updates.
It also takes effect now: with `--to` the node starts streaming straight
away, saying hello to the listener, and with `--off` it switches the
kernel's netconsole off. With `--to`, the node then checks that the kernel
really sends to the new target. If it doesn't (no route to the listener,
say), or `--off` couldn't switch netconsole off, the node gives the reason in
brackets, and the next boot tries again from the file. A node that isn't an
appliance shows as `not an appliance` and is left alone. A node whose bun is
older than 0.3.3 has no boot-logs setting, and shows as failed until you
upgrade it. With `--to` or `--off`, the command fails if any node didn't
answer, or if no node is an appliance; showing the setting never fails, and
a node that didn't answer is a row in the table. A node that didn't answer may still have written the file before
the connection dropped, so run `relish os boot-logs` to see where it stands. The target must be
one machine's IPv4 address, as netconsole needs: not a broadcast,
multicast, loopback or `0.0.0.0` address. With `-o json`, each node has a
`state` of `on` (with its `target`), `off`, `not_an_appliance` or `failed`
(with a `reason`).

### Without network boot

A machine that can't network-boot, or that you'd rather not run a boot server
for, can have the image written straight onto its disk from another computer.
Take the disk out and attach it, or use a USB drive the machine boots from,
then name the device:

```sh
lsblk                         # Linux: find the device, e.g. /dev/sdb
diskutil list                 # macOS: e.g. /dev/disk4
diskutil unmountDisk /dev/disk4
sudo relish image write os/x86_64/reliaburger-os_2026.41.0.raw.zst /dev/disk4 --yes
```

- `relish image write` decompresses a `.raw.zst` image as it writes, and
  writes a `.raw` one as it is. It writes from the start of the device and
  flushes before it exits.
- Everything on the device is lost. Without `--yes` it refuses and says which
  device it would have erased.
- It doesn't check the image itself: `relish image download` already checked
  it against the release key. Nor does it check the device. It will write over
  the disk you're running from if you name it, so make sure of the name first.
- Writing to a device usually needs root.

The installer also makes a firmware boot entry for the disk and puts it first;
`image write` can't, since it runs on another computer. The written disk boots
through its fallback path, `\EFI\BOOT`, so set the machine's firmware to boot
that disk first. From there it's an appliance like any other: bun runs on its
own, unclaimed, and the next section claims it.

## Claim them

After the install, each machine reboots into the appliance and looks for a
*seed*: a small file that tells it which cluster it belongs to and who it is.
It has none yet, so it becomes *unclaimed*. It makes a key, shows a short
fingerprint of it on its monitor, and announces itself on the LAN over mDNS
until someone sends it a seed:

```
  Address    192.168.1.51
  MAC        d8:9e:f3:12:34:56
  Claim key  3f9a-12bc-77de-0a41
```

From your laptop, on the same LAN:

```sh
relish machines
```

```
ADDRESS           MAC               ARCH     CLAIM KEY
192.168.1.51      d8:9e:f3:12:34:56 x86_64   3f9a-12bc-77de-0a41
192.168.1.52      d8:9e:f3:12:34:57 x86_64   77c0-9e1d-4b2a-e816
192.168.1.53      d8:9e:f3:12:34:58 x86_64   c5d1-03aa-9f72-1b60
```

It listens for three seconds; `relish machines --wait 10` listens longer.
`relish machines claim`, below, listens the same three seconds to find the
machines you name by MAC. Name them by address and it doesn't need to.
Create the cluster from them, node 1 first, by MAC or by address:

```sh
relish machines claim ~/home-cluster --create --name home \
  --operator 192.168.1.10 --network 192.168.1.0/24 \
  d8:9e:f3:12:34:56 d8:9e:f3:12:34:57 d8:9e:f3:12:34:58
```

- `--operator` is your laptop's address, the one relish connects from. Only
  that address can reach the nodes' API.
- `--network` is your LAN. Each node's firewall lets machines on it reach the
  cluster ports before they've joined (those ports all need the cluster's
  certificates). Without it, only the machines listed here get through.
- The machines become `home-1`, `home-2` and `home-3`, in the order given.
- The council, the machines that hold the cluster's state and vote on every
  change, grows to five voters. Five ride out two failures at once. Seven
  would ride out three, but every voter keeps the council's log and votes on
  every write, which 2 GB machines feel. The rest are workers.
  `--council-size` takes another odd number from 1 to 7. An even size is
  refused: four voters survive no more failures than three. A cluster with
  fewer machines than its size makes every machine a voter. `relish council`
  shows the size as `Size: up to 5 voters`.

For each machine, relish shows the claim key it got and asks whether the
machine's monitor shows the same:

```
Does the console of d8:9e:f3:12:34:56 (192.168.1.51) show claim key 3f9a-12bc-77de-0a41? [y/N]
```

Check: anything on your LAN can announce itself, and this is what stops node
1's seed, which carries the cluster's keys, going to the wrong machine.
Anything but `y` claims nothing. relish then sends each seed over a
connection pinned to that key, and the machines carry on as if the seeds had
come on a stick.

With ten machines and one monitor, that's a lot of cable swaps. If you trust
everything on the network, on an isolated switch with nothing else on it,
say, `--trust-lan` skips the questions. relish refuses to claim without
either a terminal to ask at or `--trust-lan`.

The cluster's keys are made here, on your laptop, and never anywhere else
until node 1's seed carries them. The root CA's private key never leaves
the laptop at all: its only copy is `~/home-cluster/root-ca-backup.age`,
sealed to a fresh age identity in `~/.reliaburger/root-backup-keys/`. relish
prints both paths; move both offline, apart from each other. No seed and no
appliance ever carries either (`relish ca rotate` needs them, see
`security`). `~/home-cluster/secrets` holds the master key, and `relish
council recover` needs it if the cluster ever loses its quorum. So before
relish sends any seed, it stops and waits:

```
Back up /Users/you/home-cluster/secrets: it holds the master key, and `relish council recover` needs it if the cluster ever loses its quorum. Keep it somewhere safe off this machine, such as your password manager, but not with the root CA backup.
Type yes once it's backed up:
```

Copy the folder somewhere that isn't this laptop, then type `yes`. Anything
else asks again. Ctrl-D stops without claiming anything. relish then removes
the files it wrote, the keys and the root backup's identity included, so you
can run the same command again. Anything else in the directory stays,
including a copy of the keys you made while it waited. In a script, `--yes` prints the reminder and carries
on without waiting, so the backup is then your script's job. Without a
terminal and without `--yes`, relish refuses before it makes anything. Every other
machine's seed holds only a join token: single-use, bound to that machine's
name, and good for an hour (`--ttl` to change). Each machine fetches its
certificate and the master key from the cluster itself when it joins. relish
now points at node 1, with an admin token, and waits up to three minutes for
every machine it claimed to start, printing each as it comes up:

```
waiting for home-1, home-2, home-3 to start (up to 180 s)
  home-1: up
  home-2: up
  home-3: up
```

A machine that's still starting after that is named, with `relish nodes` to
check on it; the claim itself succeeded. Then:

```sh
relish nodes
relish council
relish wtf
```

All of them alive, all three in the council, and nothing to fix. Each
machine's monitor shows its dashboard ("The console", below): the Status
tab names the machine and its address, and says whether bun is running.

relish keeps one current cluster, in `~/.reliaburger/context.json`. The
claim makes the new cluster that one, unless another cluster's context is
already there and still works: then it keeps that one and prints the three
`export` lines that reach the new cluster instead. A context whose cluster
directory you've since deleted can't work, because its CA certificate went
with the directory, so the claim replaces it and says so:

```
relish now talks to this cluster (node 1, https://192.168.1.51:9117).
It replaced the context for bare-metal/lab, which no longer worked: /Users/you/lab/root-ca.crt is gone.
```

`relish cluster create --bare-metal` and `relish setup` do the same.

The context lists every machine's API, node 1 first, so relish keeps
working when a node is down. Pull node 1's power and the next command
waits up to 1.5 seconds for it, then asks the next node, says so on stderr,
and remembers which node answered:

```
warning: https://192.168.1.51:9117 didn't answer (no answer before the connect timeout; the host may be down, or a firewall may drop packets to that port); trying the cluster's next node
relish is using https://192.168.1.52:9117
```

Later commands ask that node first, without the wait, until it stops
answering in turn. Every node gets the same CA and token, so a machine
that answers with a certificate the cluster's CA didn't sign fails the
command, as it always did: relish only passes over nodes that don't accept
a connection. When none answer, the error lists each endpoint and why.
Machines you add later with `relish machines claim` or `relish image seed`
join the list. `relish setup --ssh` lists every machine it set up, the
same way. `--endpoint` still means exactly that node, with no fallback.

A context saved by 0.3.2 or earlier lists only node 1. Create the cluster
again, or add the other nodes yourself as
`"other_endpoints": ["https://192.168.1.52:9117", ...]` in
`~/.reliaburger/context.json`.

The claim uses each machine's current address as its node address, so
reserve those addresses for their MACs in your router first. mDNS doesn't
cross routers; if relish can't see a machine, give its address instead of
its MAC. To see what your laptop hears, `dns-sd -B _rb-unclaimed._tcp` on
macOS or `avahi-browse -r _rb-unclaimed._tcp` on Linux lists the machines'
announcements.

Name each machine once. relish refuses a machine listed twice, by MAC, by
address, or by its MAC and its address, before it writes anything, and so
does `relish cluster create --bare-metal`.

If a machine stops answering before its seed arrives, relish still sends the
others theirs, then lists the ones that missed out and exits with an error:

```
2 machines didn't get their seed:
  home-2 (d8:9e:f3:12:34:57 at 192.168.1.52): 192.168.1.52: connection refused
  home-3 (d8:9e:f3:12:34:58 at 192.168.1.53): 192.168.1.53: connection refused
Their seeds are still in the cluster directory. Once the machines answer, send them again with:
  relish machines claim /Users/you/home-cluster 192.168.1.52 192.168.1.53
or copy … into seeds/ on an RBSEED stick and boot the machines with it
```

The cluster is made either way, and relish points at node 1. Run the
command it prints once the machines are back: without `--create`, a machine
the cluster directory already lists gets the seed it was meant to have,
rather than a new one. Each join seed sent again gets a new join token from
the cluster, good for `--ttl` from then (an hour by default), so it doesn't
matter how long the machine was away. Only node 1 can mint one: if node 1
isn't running yet, a seed goes as written while its first token lasts, and
after that relish keeps it back, says the token expired, and asks you to run
the command again once node 1 is up.

`--ssh-key ~/.ssh/id_ed25519.pub` puts a key for root SSH into the seeds.
Only CI lab builds run sshd; published images ignore it.

## Or carry the seeds on a USB stick

If the machines can't be reached over the network from your laptop, make the
cluster and its seeds on your laptop, then walk them round on a stick. List
the machine that will be node 1 first, each as its MAC and address, from the
installer's `this machine` line or your router's reservations:

```sh
relish cluster create --bare-metal ~/home-cluster --name home \
  --operator 192.168.1.10 --network 192.168.1.0/24 \
  d8:9e:f3:12:34:56@192.168.1.51 \
  d8:9e:f3:12:34:57@192.168.1.52 \
  d8:9e:f3:12:34:58@192.168.1.53
```

`--operator`, `--network`, `--council-size` and `--yes` mean what they mean
for a claim. The secrets land in `~/home-cluster/secrets` the same way, and
relish waits for you to confirm you've backed them up before it tells you
how to make the stick. A
stick's join tokens are good for a week (`--ttl` to change), since a stick
travels slower than a claim.

Make the USB stick: a FAT filesystem labelled `RBSEED`, then copy the `seeds`
folder onto it. On macOS, check the disk number with `diskutil list` first,
because this erases it:

```sh
diskutil eraseDisk FAT32 RBSEED MBRFormat /dev/diskN
cp -R ~/home-cluster/stick/seeds /Volumes/RBSEED/
```

On Linux, use `mkfs.vfat -n RBSEED /dev/sdX1`, then mount the stick and copy
the folder.

### Start the machines

Plug the stick into node 1 and restart it (a power cycle is fine). While it
boots, it finds the seed with its own MAC address on the stick and becomes
the cluster's first node. Node 1's seed is the one that carries the cluster's
keys, so the machine wipes it from the stick once it has copied it:

```
reliaburger: seed from the RBSEED stick (seeds/d8-9e-f3-12-34-56.seed)
reliaburger: /etc/reliaburger/node.toml is ready to create home as home-1 on 192.168.1.51
reliaburger: wiped seeds/d8-9e-f3-12-34-56.seed from the RBSEED stick: it held the master key
reliaburger: bun healthy (bun 0.3.0) on OS 2026.41.0
```

Then take the stick to the others, restarting each with it in. Each picks its
own seed, enrols with its token and fetches the master key:

```
reliaburger: enrolled as home-2 through 192.168.1.51
reliaburger: fetched the master key from 192.168.1.51
```

The order doesn't matter: a machine that boots before node 1 is up waits for
it. From your laptop:

```sh
relish nodes
relish wtf
```

All of them alive, in the council, and nothing to fix. Each machine's
monitor shows its dashboard ("The console", below).

A machine only looks for a seed until it has one, and waits up to ten
seconds for the stick. So a stick left in later changes nothing.

## Add machines later

Claim them, without `--create`:

```sh
relish machines claim ~/home-cluster d8:9e:f3:12:34:59
```

The new machine becomes `home-4`, with a join token the cluster mints for
it. relish also asks every node to let the new machine's address through its
firewall for 15 minutes, so it can enrol even outside the `--network` you
created the cluster with. Once it has joined, the cluster knows it, and
relish's context lists it, so relish can reach the cluster through it when
other nodes are down.

Or, with the stick:

```sh
relish image seed ~/home-cluster d8:9e:f3:12:34:59@192.168.1.54
cp -R ~/home-cluster/stick/seeds /Volumes/RBSEED/
```

A stick seed can sit in a drawer for up to a week, so nothing opens a
firewall for it: if its address is outside `--network`, the existing nodes
drop it until you add it to their `[security] bootstrap_peers`.

## The console

Each machine's monitor shows a small dashboard. There's no login, so it's
read-only: anyone at the keyboard can look, and nothing they press changes
the node. The one exception is the invaders game's kill mode, which is off
until a cluster admin turns it on ("Invaders", below). The console never
shows secrets, tokens or environment variables.

```
 Reliaburger OS 2026.47.228 | home-1 | cluster home                 bun running
 [1 Status]  2 Metrics   3 Workloads   4 Cluster   5 Versions   6 Logs
┌ 1 Status ────────────────────────────────────────────────────────────────────┐
│Node       home-1                                                             │
│Cluster    home                                                               │
│Address    192.168.1.51                                                       │
│MAC        d8:9e:f3:12:34:56                                                  │
│bun        running (0.3.3)                                                    │
│                                                                              │
│Cluster HEALTHY, no problems (home-1 answered at 15:06:40)                    │
│wtf        0 critical, 1 warning                                              │
│nodes      3 alive, 0 suspect, 0 dead                                         │
│council    leader home-1 (term 4), 3/3 voters, quorum ok, 0 lagging           │
```

| Key | Does |
| --- | --- |
| `1` to `6` | pick a tab |
| `←` `→`, `Tab`, `h` `l` | the previous or next tab |
| `↑` `↓`, `k` `j`, `PgUp` `PgDn` | scroll a list, or back through the logs |
| `Home`, `End` | back to the top, or the newest log line |
| `q` | quit, on any terminal but tty1 (an SSH session) |

| Tab | Shows | From |
| --- | --- | --- |
| 1 Status | who the machine is, then `relish status`'s lines and `wtf`'s counts, the ones that need attention first | the machine, bun's summary, `wtf` every five minutes |
| 2 Metrics | CPU, memory, the data partition and the network, with four minutes of history | the machine |
| 3 Workloads | the instances on this node: state, restarts, CPU and memory | bun (`/v1/top`) |
| 4 Cluster | gossip's members and Raft's leader, term and quorum, then every node's role, applied and log index and lag | bun's summary and every node's council view |
| 5 Versions | the OS image, kernel, bun, runc, systemd and Buildah | the machine |
| 6 Logs | the journal of `reliaburger` and `reliaburger-boot-check` | `journalctl` |

A tab asks only while it's on screen: nothing runs `journalctl` while you
read the Status tab. A panel that can't refresh keeps what it last showed
and says when that was.

**When bun doesn't answer**, the Status tab is the screen the appliance
always had: name, cluster, address, MAC and bun's state. That's the one a
boot, a fallback or a claim needs. Metrics, Versions and Logs read the
machine itself, so they still work; Workloads and Cluster say bun isn't
answering. An unclaimed machine shows its claim key twice, once as text and
once in large letters you can read from across the room.

The console reads bun as its own principal, `__console`: read-only, with
a token derived from the cluster's master key, which the console reads as
root. A monitor on one node shows the whole cluster, as `relish status`
would. `wtf`'s token-lifetime check needs an admin, so the console leaves
it out instead of counting it as unknown.

Know what that token is before you rely on it:

- **It's valid on every node for the life of the cluster.** Every node
  derives the same value, and there's no master-key rotation, so it can't
  be revoked or rotated on its own. `relish token list` doesn't show it,
  and `relish token revoke` can't reach it.
- **Only root on a node can derive it**, from `/etc/reliaburger/master.key`.
  Root there already holds the master key, which is far more than this
  read-only token gives.
- **It only reads the API.** The dashboard's login (`/ui/session`) and the
  image registry don't accept it.

### Invaders

`7` opens a tab the tab bar doesn't show: a game with one invader for each
running app instance on this node, labelled `web-0` and so on (with the
namespace in front only when two would read the same). Jobs get none. The formation marches once a second, five invaders to a row and up to
three rows; the rest wait off the field.

| Key | Does |
| --- | --- |
| `h` `l`, `←` `→` | move the cannon |
| `space` | fire |
| `r` | bring every invader back |
| `Tab`, `1` to `6` | leave the game |

The top line says whether kill mode is on. While it's off, a hit only pops
the invader, and nothing on the node changes.

**Kill mode** makes a hit kill the instance the invader stands for. It's a
cluster setting, never a node one, because a switch on the machine would be
a switch for whoever stands at its keyboard. Only an unscoped cluster admin
can change it:

```bash
relish os settings                       # shows [appliance] and whether kill mode is on
relish os settings --set appliance.toml   # replaces it
```

where `appliance.toml` says

```toml
[appliance]
console_chaos = true
```

`--set` replaces the whole `[appliance]` section, so a key you leave
out is off, and a misspelt one is refused. The change is audited as
`os.settings_set`. Set `console_chaos = false`, or leave it out, to turn kill
mode off again.

Kill mode covers every namespace on the node: the console's invaders stand
for every app instance there, whoever's it is. Turn it on only on a cluster
where anyone at a keyboard may kill any of them.

Each node reads the setting from its own copy of the council's state when
it's a council member, and asks the leader when it isn't (a node without a
Raft seat, or one removed from the council). If it can't get an answer,
kill mode is off there.

A hit in kill mode is the request `relish fault kill web --instance ID
--node NODE` sends, from the console's principal `__console`, and the node
treats it as one:

- **The node needs a fault policy.** Its `[testing] allowed_operations` must
  include `inject_workload_faults` ("Breaking things on purpose" has the
  section). A cluster made with `relish cluster create --bare-metal --faults`
  or `relish machines claim --create --faults` has one on every node; without
  it, each kill is refused and the invader comes back.
- **Smoker's rails apply.** A kill that would take a service's last running
  replica is refused, and the invader comes back with the reason underneath.
  A killed replica counts as gone for ten seconds, so two kills in quick
  succession, from the console or `relish fault kill`, can't take the last
  one between them. A kill stays on `relish fault list` for those ten seconds.
- **An unclear answer isn't a no.** If bun doesn't answer in time, or answers
  with a server error, the kill may still land, so the game says it doesn't
  know and holds fire until the workload list shows whether the instance
  died, for up to 30 seconds.
- **It's on the record.** `relish fault list` shows the kill, and the events
  and `relish audit --action 'fault.*'` name `__console` as who did it.

The console may send that one kind of request and nothing else: a kill of
one named app instance on its own node, with no `--include-leader` and no
`--override-safety`. Anything else from the console is refused, kill mode
or not.

### Over SSH

`bun appliance console` runs it on whatever terminal it's started from, so
on a lab image, whose sshd runs, `ssh -t root@home-1
/var/lib/reliaburger/bin/bun appliance console` shows the same dashboard in
your own terminal. There, and on any terminal other than tty1, `q` quits.
On tty1 nothing quits it: it's the only thing on the screen.

A node that runs a newer bun on an OS image from before the dashboard
(0.3.0 to 0.3.2) still shows it, but without keys: that image's unit gives
the console no keyboard, so it stays on the Status tab until the OS is
updated.

## Take the tour

The cluster is ready for the [five-minute tour](08_five-minute-tour.md).
There are three differences:
- **The first build is slow.** Every appliance has Buildah, so
  `relish build burger/burger.toml` works as it does on a laptop. On a
  2 GB machine with eMMC, though, pulling the Go toolchain image and
  compiling the burger takes a few minutes the first time. Later builds
  reuse the cached base images.
- The ingress listens on port 80 of every node, not on `localhost:18080`. So
  use `curl -H 'Host: podinfo.localhost' http://192.168.1.51/`, or put the
  name in your laptop's hosts file.
- Instead of `relish local stop`, pull a node's power cord. Plug it back in
  later and it rejoins by itself.

### Where builds run

A build runs on the node that took the request, in its own Buildah storage
under `/var/lib/reliaburger/data/buildah`, on the data partition. That partition
is Btrfs, and Buildah's `vfs` driver makes each layer by copying the one
below it, which Btrfs turns into a reflink: a new layer shares its parent's
blocks instead of writing them to the eMMC again. After each build the node
removes what the build made and keeps the base images, up to 1 GiB, so the
next build doesn't pull them again. Over that, it drops them all. Nothing
lands in `/usr`, which stays read-only.

A `RUN` step runs on the node itself, under runc, with Buildah's own network
(`podman0`, `10.88.0.0/16`) for steps that use one. So a Dockerfile's `RUN`
steps run on the machines that hold the cluster's state, as on any other
node. A multi-platform build runs a `RUN` step for the other architecture
under emulation, which the appliance doesn't have. A Dockerfile that
cross-compiles, as the burger's does, needs none. For one that doesn't, set
`platform` to the architecture your nodes have
([Images and volumes](11_images-and-volumes.md)).

### Building elsewhere

You can still build on a bigger machine and push into Pickle, which saves
the appliances the work and the eMMC the writes. Your laptop has to reach a
node by its name and trust the cluster's CA
([Trusting the cluster CA](11_images-and-volumes.md#trusting-the-cluster-ca)).
With Docker Desktop, and `home-1` at 192.168.1.51 in your hosts file:

```sh
mkdir -p ~/.docker/certs.d/home-1:5050
cp ~/home-cluster/root-ca.crt ~/.docker/certs.d/home-1:5050/ca.crt
TOKEN="$(relish token create --name laptop-push --role deployer)"
docker login home-1:5050 -u push --password-stdin <<<"$TOKEN"
docker buildx build --platform linux/amd64 -t home-1:5050/burger:v1 --push burger
```

Restart Docker Desktop after adding the CA. Name the platforms your nodes
have: `linux/arm64` for Arm machines, or both, comma-separated. The pushed
image isn't signed by the cluster as a built one is, which only matters if
you turned `require_signatures` on.

## Updating the OS

Each node keeps two copies of the operating system: the one it runs, and a
spare slot for the next. An update goes into the spare slot. The node reboots
into it with two tries, and it's only marked good once bun is healthy again
and the node has a network address. If either is missing 150 seconds after
the kernel starts, the try fails, and after the second the node goes back to
the previous version by itself. The check doesn't wait for the network to
come up, so a node whose network never comes up on the new version falls
back too, rather than staying cut off. On a Wyse 3040, two tries should
fall back in about 8 minutes 20 seconds. The lab timed the older three-try
check at 10 min 28 s and 11 min 38 s; the two-try check's run on the
Wyses is still to come.

bun does this across the cluster, one node at a time. See what each node runs
and what the newest release is:

```sh
relish os list
```

```
Newest OS release: 2026.42.0

NODE                 OS           NOTE
home-1               2026.41.0    update available
home-2               2026.41.0    update available
home-3               2026.41.0    update available
```

Then roll it out:

```sh
relish os upgrade
relish os status
```

`relish os upgrade` takes the newest release, or a version you name. The
leader takes the nodes in turn: workers first, then the council (only while
the others can keep a majority), and itself last. For each node it first
moves the workloads it can elsewhere, then tells the node to update. The node
downloads the release from GitHub, checks its signature against the key its
own image carries and every file against the release's `SHA256SUMS`, checks
that the UKI is built for its own architecture, writes it into the spare slot
and reboots. Once it's healthy on the new version, the
leader moves on. Apps with a managed volume or a host-path mount (a volume
with a `source`) stay with their data and are back when the node is, a minute
or two later. While a node drains and updates it takes no new work, and
`relish nodes` and `relish status` say it's cordoned by the OS rollout.

If a node comes back on its old version, because the new one failed its boot
check on both tries, or doesn't come back within 45 minutes, the rollout
pauses and `relish os status` says which node and why. A download that
stops delivering bytes for a minute (`download stalled for 60 s`), or a
`systemd-sysupdate` that runs past 20 minutes, fails the node's update with
that reason, so the rollout pauses on it at once rather than after 45 minutes. `relish os resume`
tries that node again; `relish os abort` stops, leaving each node on the
version it has. A node that's already on the target is skipped, so after
adding machines from an older image, run `relish os upgrade` again to bring
them in line. To go back to an older version, name it and add
`--allow-downgrade`.

`relish wtf` warns about a node whose update failed, and about nodes left
on different versions.

A bun upgrade and an OS rollout never run at once: both restart nodes, so
each refuses to start while the other is under way.

### What happens to `/etc`

An update replaces `/usr`, but `/etc` lives on the node's data partition. So
each image also carries its own `/etc`, and on the first boot of a new
version the node brings its `/etc` up to that image:
- a file you never changed follows the new image, and new files appear;
- a file the new image no longer has is removed, unless you changed it;
- a file you changed or deleted stays as you left it, and the boot log says
  so: `journalctl -u reliaburger-etc-sync` lists each one as `kept`.

The node's own files are never touched: its identity and keys under
`/etc/reliaburger`, users and passwords, SSH host keys and `machine-id`.
Nodes installed from an image older than 2026.40.51 have no record of what
their image shipped, so their first update keeps every file that differs
from the new image; reinstall them to start clean.

### Updating a CI build

A CI run's images trust only that run's throwaway key, so they can't update
to a published release or to another run's build. So each x86_64 run also
builds the next version, one build number on (`2026.41.7` → `2026.41.8`),
signed with the same key, and uploads it as `appliance-x86_64-next`, laid out
like a GitHub release beside a lab `os-channel.json` that names it. Serve it
from your laptop, on the machines' network:

```sh
gh run download <run-id> -n appliance-x86_64-next -D next
(cd next && python3 -m http.server 8000)
```

The lab channel is signed with the run's throwaway key, not the release key,
so relish refuses it unless you give it that key with `--key`. The run puts
the public half beside the channel, as `next/lab-signing-key.pub.pem`:

```sh
channel=http://192.168.1.10:8000/releases/download/os-channel/os-channel.json
relish os list --channel "$channel" --key next/lab-signing-key.pub.pem
relish os upgrade --channel "$channel" --key next/lab-signing-key.pub.pem
relish os status
```

`relish os list` then shows the next version as the newest release, and
`relish os upgrade` rolls it out. The nodes fetch the release from beside
that channel and check it against the key their own image carries, which for
a CI build is the same run's key.

`--key` replaces the release keys rather than adding to them, and only for
that one command: relish says so on stderr each time, and names the key file
in the line that reports the check. It's never stored, and nothing on the
nodes changes. Without it, relish checks a channel against the release keys
it carries and nothing else, and refuses a lab channel with a pointer to
`--key`. `relish image download` takes the same option. `relish os upgrade`
refuses `--key` with a version you name, because it doesn't read the channel
then:

```sh
relish os upgrade 2026.41.8 --channel "$channel"
```

To check the channel by hand, it's signed like a published one:

```sh
openssl pkeyutl -verify -pubin -inkey next/lab-signing-key.pub.pem -rawin \
  -in next/releases/download/os-channel/os-channel.json \
  -sigfile next/releases/download/os-channel/os-channel.json.sig
```

`next/` also holds a broken version, two build numbers on (`2026.41.9`),
whose bun starts and never gets healthy. The channel doesn't name it. It's there to test the
fallback: a node updated to it fails its boot check twice, then comes
back on the version it ran before, by itself. CI rolls it out on every x86_64
build and records how long that took. To try it on one machine, stage it
there with the image's `os-stage` and reboot:

```sh
ssh root@<node> /usr/lib/reliaburger/os-stage \
  http://192.168.1.10:8000/releases/download/os-2026.41.9-x86_64 2026.41.9
ssh root@<node> systemctl reboot
```

`relish os upgrade 2026.41.9 --channel "$channel"` works too (a named
version, so no `--key`), but the leader picks the node, and the rollout
pauses once it falls back. Either way, it's a test: don't roll it
across a cluster you care about.

## What's missing

- **A CI build can only update to its own next version.** Each run's images
  trust only that run's throwaway key. Published releases update to each
  other, and to anything the release key signs.
- **CI builds no next version for arm64**, only for x86_64.
- **Secure Boot** has to be off.
- **Raspberry Pis** aren't supported yet. A Pi boots through its own
  firmware rather than UEFI, and Ubuntu runs it on a kernel of its own, so
  the Pi 4 and Pi 5 need a second boot platform. It's planned (#1971). Until
  then `relish netboot` names a Pi that asks it to boot, and leaves it alone.
- **Other network cards.** A machine whose only wired NIC isn't in "What
  you need" boots iPXE and the installer, which then can't reach the
  network: its screen shows `reliaburger-install: FAILED: can't fetch …`.
  A USB Ethernet adapter gets it going.
- **No emulation for builds.** A multi-platform build's `RUN` steps for
  the other architecture can't run on an appliance. Cross-compile, or build
  for your nodes' architecture only ("Where builds run", above).
- **Two machines can't roll a bun upgrade or an OS update.** Both are in
  the council, and taking one down would leave the other without a majority,
  so `relish upgrade start` and `relish os upgrade` refuse. Use three or more.

The design is in `docs/plans/2026-09-28-plan-appliance-lab.md`, and the
hardware results in `docs/qualification/2026-11-20-wyse-3040.md`.
The QEMU lab that exercises all of this without hardware is in `image/lab/`.
