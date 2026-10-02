# Bare metal: the appliance and netboot (preview)

The five-minute tour builds its cluster in laptop VMs. This chapter builds one
on real machines: a handful of old mini PCs or thin clients, each erased and
turned into a Reliaburger appliance over the network. There's no distro to
install, nothing to log in to, and no USB stick to boot from. Each machine
boots from the network once, installs itself in under a minute, and from then
on it's a node.

It's a **preview**, and the rough edges are part of what it's for:
- The images come from CI runs of the appliance branch, kept for a day, each
  signed with a throwaway key.
- We've run all of it in QEMU VMs, both aarch64 and x86_64. Real hardware (a
  fleet of Dell Wyse 3040s) comes next, so expect firmware surprises.

## What you need

- **Two to seven machines**, ideally three or more:
  - x86_64 or arm64, with UEFI and network boot (PXE);
  - at least 2 GiB of RAM and an 8 GB disk;
  - wired to the same network.

  **The installer erases the largest built-in disk.**
- **Your router**, with a DHCP reservation for each machine, so each keeps the
  same address. Nodes find each other by address.
- **A machine on that network to serve the netboot**: your laptop (Linux or
  macOS), any spare box, or a VM bridged onto the LAN. It needs `relish` and
  root.
- **Your laptop**, on the same network, with:
  - `relish`, from the same release as the images' bun;
  - `gh`, `openssl`, `python3`, and Rust (`rustup`), for a small helper;
  - a checkout of the repository;
  - a USB stick you can erase.

## Get the images

The appliance workflow builds both architectures on every change. Take the
newest green run:

```sh
gh run list --workflow appliance.yml --branch feat/appliance-image
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
same run's public key.

Once an OS release is published, `relish image download --dir os` fetches the
newest one instead (add `--arch aarch64` for arm64 machines). It checks the
release against the key relish carries and writes the same layout, `os/x86_64/`
or `os/aarch64/`.

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
- Two netboot servers on one LAN race to boot every machine, so it refuses
  to start when another one answers.

It remembers each machine that downloaded the installer, by MAC address and
SMBIOS UUID, in `netboot-installed.json` in the directory it serves. Next time that machine
network-boots, it's told to boot its disk instead. Pass `--reinstall` to
install over them again.

## Prepare the machines

In each machine's firmware setup:
- turn on UEFI network boot (sometimes called "Network Stack" or "PXE");
- turn Secure Boot off;
- put the disk before the network in the boot order.

A blank disk isn't bootable, so the first boot falls through to the network.
After that, the disk boots.

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

Write down each machine's MAC address and address from the `this machine`
line. You'll need them next. (Your router's list of reservations has them
too.)

## Create the cluster

Back on your laptop, create the cluster and a *seed* for every machine: a
small file that tells it who it is. List the machine that will be node 1
first:

```sh
relish cluster create --bare-metal ~/home-cluster --name home \
  --operator 192.168.1.10 --network 192.168.1.0/24 \
  d8:9e:f3:12:34:56@192.168.1.51 \
  d8:9e:f3:12:34:57@192.168.1.52 \
  d8:9e:f3:12:34:58@192.168.1.53
```

- `--operator` is your laptop's address, the one relish connects from. Only
  that address can reach the nodes' API.
- `--network` is your LAN. Each node's firewall lets machines on it reach the
  cluster ports before they've joined (those ports all need the cluster's
  certificates). Without it, only the machines listed here get through.
- The machines become `home-1`, `home-2` and `home-3`.

The cluster's keys are made here, on your laptop, and never anywhere else
until node 1's seed carries them. `~/home-cluster/secrets` holds the master
key and the sealed root CA key: back them up, because `relish council
recover` needs them if the cluster ever loses its quorum. Every other
machine's seed holds only a join token: single-use, bound to that machine's
name, and good for a week (`--ttl` to change). Each machine fetches its
certificate and the master key from the cluster itself when it joins. relish
now points at node 1, with an admin token.

Make the USB stick: a FAT filesystem labelled `RBSEED`, then copy the `seeds`
folder onto it. On macOS, check the disk number with `diskutil list` first,
because this erases it:

```sh
diskutil eraseDisk FAT32 RBSEED MBRFormat /dev/diskN
cp -R ~/home-cluster/stick/seeds /Volumes/RBSEED/
```

On Linux, use `mkfs.vfat -n RBSEED /dev/sdX1`, then mount the stick and copy
the folder.

## Start the machines

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

## Or claim them over the network

You don't need the stick. A machine that boots with no seed becomes
*unclaimed*: it makes a key, shows a short fingerprint of it on its monitor,
and announces itself on the LAN until someone sends it a seed:

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
```

Create the cluster from them, node 1 first. relish takes the same options as
`relish cluster create`, and the machines by MAC or by address:

```sh
relish machines claim ~/home-cluster --create --name home \
  --operator 192.168.1.10 --network 192.168.1.0/24 \
  d8:9e:f3:12:34:56 d8:9e:f3:12:34:57
```

For each machine, relish shows the claim key it got and asks whether the
machine's monitor shows the same. Check: anything on your LAN can announce
itself, and this is what stops node 1's seed, which carries the cluster's
keys, going to the wrong machine. relish then sends each seed over a
connection pinned to that key, and the machines carry on as if the seeds had
come on a stick. If you trust everything on the network, `--trust-lan` skips
the questions.

The claim uses each machine's current address as its node address, so
reserve those addresses for their MACs in your router first. mDNS doesn't
cross routers; if relish can't see a machine, give its address instead of
its MAC.

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

Eventually bun will do all of this across the cluster, one node at a time. In
the preview you stage each node by hand, which needs root SSH. Pass
`--ssh-key ~/.ssh/id_ed25519.pub` to `relish cluster create` when you create
the cluster, and every seed carries your key (only the lab images, from pull
requests, have sshd). Then serve a newer run's artefacts
over HTTP, and on each node in turn (node 1 last):

```sh
ssh root@192.168.1.52
curl -fsSO http://192.168.1.20:8080/x86_64/spike-signing-key.pub.pem
/usr/lib/reliaburger/os-stage http://192.168.1.20:8080/x86_64 2026.40.40 spike-signing-key.pub.pem
systemctl reboot
```

`os-stage` checks the new version's signature and hashes, and writes it into
the spare slot. Each CI run signs with its own key, so name the new run's key
explicitly, as above. Serve the new run's artefacts yourself: `relish
netboot` serves only files its `SHA256SUMS` lists, and the key isn't one of
them. Download them the same way as before, into `new/x86_64`, and run
`python3 -m http.server 8080` in `new/`, so the URLs above find them. Stop
`relish netboot` first, since it also uses port 8080. `bootctl list` on the
node shows both versions afterwards.

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

## What's missing

- **No OS updates run by bun.** Staging by hand over SSH stands in for them.
- **Secure Boot** has to be off.
- **Two machines can't roll a bun upgrade.** Both are in the council, and
  upgrading one would leave the other without a majority, so
  `relish upgrade start` refuses. Use three or more.

The design and the test results are in `docs/plans/2026-09-28-plan-appliance-lab.md`.
The QEMU lab that exercises all of this without hardware is in `image/lab/`.
