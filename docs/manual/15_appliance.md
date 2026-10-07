# Bare metal: the appliance and netboot (preview)

The five-minute tour builds its cluster in laptop VMs. This chapter builds one
on real machines: a handful of old mini PCs or thin clients, each erased and
turned into a Reliaburger appliance over the network. There's no distro to
install, nothing to log in to, and no USB stick to boot from. Each machine
boots from the network once, installs itself in under a minute, and from then
on it's a node.

It's a **preview**, and the rough edges are part of what it's for:
- Until 0.3.0 publishes the first signed OS release, the images come from CI
  lab builds, each signed with a throwaway key. A run you start by hand keeps
  them for a week; a pull request's run, for a day.
- We've run all of it in QEMU VMs, both aarch64 and x86_64. Real hardware (ten
  Dell Wyse 3040s, netbooted from a Mac) comes next, so expect firmware
  surprises.

The short version: `relish image download` fetches the OS, `sudo relish
netboot` installs it on every machine that network-boots, and `relish machines
claim` turns the installed machines into a cluster.

## What you need

- **Two or more machines**, ideally three or more:
  - x86_64 or arm64, with UEFI and network boot (PXE);
  - at least 2 GiB of RAM and an 8 GB disk;
  - wired to the same network.

  **The installer erases the largest built-in disk.** A blank one goes
  without asking; one that holds anything else is wiped only after you say
  yes at the `relish netboot` terminal, or name the machine with `--wipe`.
- **A router you control**, with a DHCP reservation for each machine, so each
  keeps the same address. Nodes find each other by address, and they need the
  router's default route to work out their own. Your home router will do. On
  an isolated switch, a Raspberry Pi can be the router:
  [`image/lab/pi/README.md`](https://github.com/reliaburger/reliaburger/blob/main/image/lab/pi/README.md)
  sets one up with DHCP, DNS, NTP and NAT. Leave booting to `relish netboot`:
  the router must not answer PXE.
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

Once an OS release is published, `relish image download --dir os` fetches the
newest one (below). Until then, take a CI lab build.

The appliance workflow builds both architectures on every change. For a lab
day, start a run by hand (Actions → Appliance image → Run workflow, with
`publish` left off), so its artefacts last a week rather than a day. GitHub
only offers that once the workflow is on `main`; until then, take the newest
green pull request run and download it the same day:

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

For a published release, `relish image download --dir os` fetches every
architecture the release has, whatever the machine you run it on, so an arm64
Mac serving x86_64 machines gets the right image. It checks each one against
the key relish carries, writes the same layout, `os/x86_64/` and
`os/aarch64/`, and ends by naming what it saved:

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
boot. It stops after an hour (`--for 3h` to change that) or at Ctrl-C.

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
- `--ipxe full` hands machines iPXE's full-driver build instead of the
  default `snp` one. `snp` drives the network card through the firmware's
  own driver, which suits odd cards; `full` brings iPXE's drivers, for
  firmware whose network stack misbehaves. It refuses to start if the
  release you serve doesn't have the full build.

Once the files check out, it broadcasts a network-boot request of its own
and listens for two seconds:
- If another netboot server answers, it refuses to start. Two on one LAN
  race to boot every machine.
- If your router answers, it says so (`192.168.1.1 hands out addresses on
  eth0`) and starts.
- If nothing answers, it warns that nothing hands out addresses there and
  starts anyway. Machines that network-boot need an address before they ask
  relish anything, so check that the router is up and on the same switch. A
  DHCP server that only answers machines it knows (reservations with no
  range) stays quiet too, and then the warning is harmless.

It also refuses to serve from a self-assigned address (`169.254.x.x`). An
interface gets one when no DHCP server answered it, and the machines you
boot couldn't reach it. Check the cable and the router, then reconnect the
interface, or give it a fixed address and pass that with `--address`.

It remembers each machine that downloaded the installer, by MAC address and
SMBIOS UUID, in `netboot-installed.json` in the directory it serves. Next time that machine
network-boots, it's told to boot its disk instead. Pass `--reinstall` to
install over them again.

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
  `sudo lsof -nP -iUDP:67` shows who holds the port.

### A lab of its own

The lab we built this for keeps the machines off the home network: a Mac on a
USB-C Ethernet adapter, a gigabit switch, ten Dell Wyse 3040s, and a
Raspberry Pi as the router.

```
home Wi-Fi ── Raspberry Pi 10.77.0.1 ── switch ─┬─ Mac, USB-C Ethernet (en7) 10.77.0.2
              DHCP, DNS, NTP, NAT               ├─ wyse-1  10.77.0.11
                                                ├─ …
                                                └─ wyse-10 10.77.0.20
```

The Pi hands out the addresses, one reservation per machine and one for the
Mac's adapter (with no default route, so the Mac's internet stays on Wi-Fi),
and routes the machines to the internet through its Wi-Fi. NTP from the Pi
matters on second-hand machines, whose clock batteries may be flat: a node
whose clock is years out rejects every certificate.
[`image/lab/pi/README.md`](https://github.com/reliaburger/reliaburger/blob/main/image/lab/pi/README.md)
sets it up. relish runs on the Mac and does only the boot part:

```sh
caffeinate -i sudo relish netboot os --interface en7 --for 3h
```

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
relish netboot: 6c:4b:90:12:34:56: /dev/mmcblk0, 7.8 GB, gpt, 4 partitions: vfat "EFI", ext4 "ThinOS", (no filesystem), swap — wipe? [y/N]
```

Type `y` and Enter to wipe it and install. Anything else, or no answer within
10 minutes, leaves the disk as it was. The installer says so on the machine's
console and powers it off. For the rest of that `relish netboot` run, the
machine is told to boot its own disk and isn't asked again. Start `relish
netboot` again to be asked again.

A blank disk installs without a question. So does a disk that already holds
Reliaburger OS: the installer boots it instead of reinstalling.

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
http: 6c:4b:90:12:34:56: disk /dev/mmcblk0, 7.8 GB, gpt, 4 partitions: …: wiping it and installing (--wipe lists it)
```

If several machines report at once, the questions come one at a time.

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
until node 1's seed carries them. `~/home-cluster/secrets` holds the master
key and the sealed root CA key: back them up, because `relish council
recover` needs them if the cluster ever loses its quorum. Every other
machine's seed holds only a join token: single-use, bound to that machine's
name, and good for an hour (`--ttl` to change). Each machine fetches its
certificate and the master key from the cluster itself when it joins. relish
now points at node 1, with an admin token. Then:

```sh
relish nodes
relish council
relish wtf
```

All of them alive, all three in the council, and nothing to fix. Each
machine's monitor shows its name, address and whether bun is running.

The claim uses each machine's current address as its node address, so
reserve those addresses for their MACs in your router first. mDNS doesn't
cross routers; if relish can't see a machine, give its address instead of
its MAC. To see what your laptop hears, `dns-sd -B _rb-unclaimed._tcp` on
macOS or `avahi-browse -r _rb-unclaimed._tcp` on Linux lists the machines'
announcements.

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

`--operator`, `--network` and `--council-size` mean what they mean for a
claim, and the secrets land in `~/home-cluster/secrets` the same way. A
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
monitor shows its name, address and whether bun is running.

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
created the cluster with. Once it has joined, the cluster knows it.

Or, with the stick:

```sh
relish image seed ~/home-cluster d8:9e:f3:12:34:59@192.168.1.54
cp -R ~/home-cluster/stick/seeds /Volumes/RBSEED/
```

A stick seed can sit in a drawer for up to a week, so nothing opens a
firewall for it: if its address is outside `--network`, the existing nodes
drop it until you add it to their `[security] bootstrap_peers`.

## Take the tour

The cluster is ready for the [five-minute tour](08_five-minute-tour.md), from
`relish apply` onwards. There are two differences:
- The ingress listens on port 80 of every node, not on `localhost:18080`. So
  use `curl -H 'Host: podinfo.localhost' http://192.168.1.51/`, or put the
  name in your laptop's hosts file.
- Instead of `relish local stop`, pull a node's power cord. Plug it back in
  later and it rejoins by itself.

## Updating the OS

Each node keeps two copies of the operating system: the one it runs, and a
spare slot for the next. An update goes into the spare slot. The node reboots
into it with three tries, and it's only marked good once bun is healthy
again. If bun doesn't come up, the node goes back to the previous version by
itself after the third try.

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
own image carries and every file against the release's `SHA256SUMS`, writes
it into the spare slot and reboots. Once it's healthy on the new version, the
leader moves on. Apps with a managed volume stay with their data and are back
when the node is, a minute or two later.

If a node comes back on its old version, because the new one failed its boot
checks three times, or doesn't come back within 45 minutes, the rollout
pauses and `relish os status` says which node and why. `relish os resume`
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

## What's missing

- **A CI build can only update to its own next version.** Each run's images
  trust only that run's throwaway key. Published releases update to each
  other, and to anything the release key signs.
- **CI builds no next version for arm64**, only for x86_64.
- **Secure Boot** has to be off.
- **Two machines can't roll a bun upgrade.** Both are in the council, and
  upgrading one would leave the other without a majority, so
  `relish upgrade start` refuses. Use three or more.

The design and the test results are in `docs/plans/2026-09-28-plan-appliance-lab.md`.
The QEMU lab that exercises all of this without hardware is in `image/lab/`.
