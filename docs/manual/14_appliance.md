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
- Two shell scripts from the repository stand in for commands relish doesn't
  have yet. `image/tools/netboot-server.sh` does the job of `relish netboot`.
  `image/tools/seed-fleet.sh` and a USB stick stand in for claiming machines
  over the LAN.
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
- **A Linux machine on that network** to serve the netboot: any spare box, or
  a VM bridged onto the LAN. It needs `dnsmasq` and `python3`, and root.
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

## Serve them

On the Linux machine, with the `art/` directory and the repository copied over:

```sh
sudo image/tools/netboot-server.sh art eth0
```

Use the interface that's on your LAN in place of `eth0`. It prints what it serves
and then logs every boot request. Leave it running until every machine has
installed.

It doesn't touch your router's DHCP. When a machine asks the network where to
boot from, your router still hands out the address, and this script answers
only the boot part: a *ProxyDHCP*. It never assigns an address, so it can't
break anything else on the LAN. The machine then fetches a small boot program
(iPXE) over TFTP, and iPXE fetches the installer over HTTP on port 8080. If
the Linux machine has a firewall, open UDP 67, 69 and 4011 and TCP 8080.

## Prepare the machines

In each machine's firmware setup:
- turn on UEFI network boot (sometimes called "Network Stack" or "PXE");
- turn Secure Boot off;
- put the disk before the network in the boot order.

A blank disk isn't bootable, so the first boot falls through to the network.
After that, the disk boots.

If you leave the network first, it still works, only slower. With the netboot
server running, each boot starts the installer. The installer sees the
installed disk and sends the machine straight back to it, which costs about
half a minute. With the server off, the firmware tries every kind of network
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

Back on your laptop, create the cluster and the first node's *seed*: its
configuration, its keys and its identity, in one small file. List the machine
that will be node 1 first:

```sh
image/tools/seed-fleet.sh init ~/home-cluster --cluster home --operator 192.168.1.10 \
  d8:9e:f3:12:34:56@192.168.1.51 \
  d8:9e:f3:12:34:57@192.168.1.52 \
  d8:9e:f3:12:34:58@192.168.1.53
```

`--operator` is your laptop's address, the one relish connects from. Only
that address can reach the nodes' API.

List every machine the cluster will ever have, now. Each seed carries the
list, and a node's firewall only lets in the listed addresses until they've
joined. So a machine added later can't reach the others (see
[What's missing](#whats-missing)). The first run builds a small helper
from the repository, which takes a few minutes.

`~/home-cluster/init` now holds the cluster's master key and its sealed root
CA key. Back them up: `relish council recover` needs them if the cluster ever
loses its quorum. `~/home-cluster/stick/seeds/` holds node 1's seed, named
after its MAC address.

Make the USB stick: a FAT filesystem labelled `RBSEED`, then copy the `seeds`
folder onto it. On macOS, check the disk number with `diskutil list` first,
because this erases it:

```sh
diskutil eraseDisk FAT32 RBSEED MBRFormat /dev/diskN
cp -R ~/home-cluster/stick/seeds /Volumes/RBSEED/
```

On Linux, use `mkfs.vfat -n RBSEED /dev/sdX1`, then mount the stick and copy
the folder.

## Start node 1

Plug the stick into node 1 and restart it (a power cycle is fine). While it
boots, it finds the seed with its own MAC address on the stick and becomes
the cluster's first node:

```
reliaburger: seed installed from the RBSEED stick (for d8:9e:f3:12:34:56): name = "node-01"
reliaburger: bun healthy (bun 0.1.0) on OS 2026.40.32
```

Then, from your laptop:

```sh
. ~/home-cluster/env.sh
relish nodes
```

`env.sh` points relish at node 1, with the cluster's CA and an admin token.
You'll see one node, alive, and the leader.

## Add the others

Now that node 1 is up, it can enrol the rest:

```sh
image/tools/seed-fleet.sh join ~/home-cluster
cp -R ~/home-cluster/stick/seeds /Volumes/RBSEED/
```

For each node, `join` asks node 1 for a single-use join token, enrols the node
with it, and writes its seed. It checks node 1's CA against the fingerprint
from `init`. One stick now carries every seed. Take it from machine to
machine, restarting each with the stick in. Each picks the seed with its own
MAC address. Then:

```sh
relish nodes
relish wtf
```

All of them alive, in the council, and nothing to fix.

A machine only looks for a seed until it has one, and waits up to ten
seconds for the stick. So a stick left in later changes nothing. Keep it
somewhere safe all the same: it holds every node's keys.

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
`--ssh-key ~/.ssh/id_ed25519.pub` to `seed-fleet.sh init` when you create the
cluster, and every seed carries your key. Then serve a newer run's artefacts
over HTTP, and on each node in turn (node 1 last):

```sh
ssh root@192.168.1.52
curl -fsSO http://192.168.1.20:8080/x86_64/spike-signing-key.pub.pem
/usr/lib/reliaburger/os-stage http://192.168.1.20:8080/x86_64 2026.40.40 spike-signing-key.pub.pem
systemctl reboot
```

`os-stage` checks the new version's signature and hashes, and writes it into
the spare slot. Each CI run signs with its own key, so name the new run's key
explicitly, as above. Serve the new run's artefacts yourself:
`netboot-server.sh` serves only what a fresh install needs. Download them the
same way as before, into `new/x86_64`, and run `python3 -m http.server 8080`
in `new/`, so the URLs above find them. Stop `netboot-server.sh` first, since
it also uses port 8080. `bootctl list` on the node shows both versions afterwards.

## What's missing

- **No `relish netboot` yet.** Serving from your laptop (macOS included) is
  Phase 2b.
- **No claiming machines over the LAN.** Seeds on a stick stand in for it.
  With claims, a new machine announces itself and `relish machines claim`
  enrols it.
- **No OS updates run by bun.** Staging by hand over SSH stands in for them.
- **Secure Boot** has to be off.
- **No adding machines later.** Seeds carry the address list from
  `seed-fleet.sh init`, and the other nodes' firewalls drop a machine that
  isn't on it. Adding one means editing `bootstrap_peers` in
  `/etc/reliaburger/node.toml` on every node and restarting bun.
- **Two machines can't roll a bun upgrade.** Both are in the council, and
  upgrading one would leave the other without a majority, so
  `relish upgrade` waits, and `relish upgrade status` doesn't say why. Use
  three or more.

The design and the test results are in `docs/plans/2026-09-28-plan-appliance-lab.md`.
The QEMU lab that exercises all of this without hardware is in `image/lab/`.
