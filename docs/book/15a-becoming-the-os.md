# Becoming the Operating System

The quickstart cheats. It starts Lima, Lima starts an Ubuntu VM that CI built with exactly the packages bun needs, and bun wakes up on a machine that was assembled for it. Then somebody reads the manual, finds five old mini PCs in a cupboard, and installs Reliaburger on real hardware. One box runs Debian, one runs whatever Ubuntu was current when it was last wiped, one has a hand-edited sysctl nobody remembers adding. The manual's prerequisites (Linux 5.8 or newer, cgroup v2, bpffs, rootful runc, eBPF) are now five separate questions with five separate answers.

Everything in this book so far has treated the operating system as somebody else's problem. This chapter makes it ours. We build an appliance image: Ubuntu 26.04 with bun as its only service, a read-only `/usr`, A/B update slots, and an installer that arrives over the network. Then we prove it works without owning a single physical test machine, which turned out to be half the work.

This was a spike, stages S1 to S4 of a six-stage plan, run in GitHub Actions and on one Mac over two days in late September 2026. The last stage, ten Dell Wyse 3040 thin clients on a real network, hasn't happened yet. So read this chapter as a report from the middle of the road. Most of the code here is shell, systemd units, mkosi configuration and iPXE script rather than Rust, and we'll explain each of those as it appears.

## Why remove the OS at all?

Look at what bun shells out to: `ip`, `nft`, `iptables`, `tc`, `ss`, `mount`, `mkfs.ext4`, `fallocate`, `btrfs` and `runc`. Every one of those is a version on somebody's disk. On a quickstart VM we control the versions. On a hand-built node we don't, and every hand-built node drifts into a snowflake: its own kernel, its own nft backend, its own unit-file edits.

An appliance collapses that. The image *is* the prerequisite list, tested together as one artefact, with one version per release across the whole fleet. Patching stops being the operator's job too: today Reliaburger upgrades only bun (Chapter 14's symlink swap), while the kernel and userland are left to whoever installed them. And by default there's no SSH, no shell and no package manager to harden, because the image has none of them. The `relish` API becomes the only way in.

Some things an appliance doesn't fix. Firmware, BIOS boot order, flaky NICs and dead disks remain the operator's problem. You still back up `master.key`. And laptops keep the quickstart on Lima; the appliance is another delivery channel, not a replacement.

## Why not Talos, or Kairos?

Talos is the obvious prior art: an immutable, API-only Linux with A/B upgrades, and a hardened kernel that ticks every box bun needs. We looked hard. Its Kubernetes-less mode is experimental and controlplane-only, host-mode services were three weeks old, and running bun there would mean two management planes and two PKIs (`talosctl` beside `relish`). Its musl root filesystem lacks the iproute2 and util-linux bun calls. Its vendor had also announced its own non-Kubernetes container scheduling for December 2026. Not a poor technical fit any more, but a poor product fit for "the OS disappears and you only see Reliaburger". Talos is parked, to revisit in 2027.

Kairos came closer: build `FROM ubuntu:26.04`, let AuroraBoot serve it with a built-in ProxyDHCP, and get the installer and A/B images for free. But on Debian-family bases its package map installs `openssh-server`, `fail2ban`, `neovim`, `snmpd` and friends, which an 8 GB eMMC pays for in every image copy. It adds a second agent to every node, upstream's CI builds Ubuntu 26.04 but never boots it, and its partitioner can't format Btrfs, which bun prefers for volumes.

So we build our own with [mkosi](https://github.com/systemd/mkosi), systemd's image builder, on Ubuntu 26.04 LTS. Why Ubuntu rather than Debian 13, which Incus OS uses for the same design? Mostly runc. Ubuntu 26.04 ships runc 1.4.0 in main, while trixie carries the upstream-EOL 1.1 line, which Debian's own security tracker lists as still vulnerable to CVE-2025-31133. The quickstart guest is Ubuntu too, so both channels can share one package list, and 26.04's standard support runs to May 2031. We ride Canonical's generic kernel and never build or sign a kernel ourselves.

## One recipe, two architectures

The whole OS is one directory, `image/`, and its heart is `mkosi.conf`. It's INI-style: sections in brackets, `Key=value` lines, and a multi-line value continues on indented lines below its key.

```ini
[Distribution]
Distribution=ubuntu
Release=resolute
# universe carries systemd-repart, which mkosi's default initrd needs. …
Repositories=main,universe

[Output]
ImageId=reliaburger-os
OutputDirectory=mkosi.output
Format=disk
CompressOutput=zstd
# The UKI and the /usr partition images are what an A/B update writes, so
# keep them as separate files too.
SplitArtifacts=uki,partitions
ManifestFormat=json

[Content]
Bootable=yes
Bootloader=systemd-boot
UnifiedKernelImages=unsigned
UnifiedKernelImageFormat=%i_%v
…
RootPassword=hashed:!*
Packages=
        linux-image-generic
        linux-firmware-minimal
        linux-firmware-realtek
        systemd
        …
        runc
        uidmap
        btrfs-progs
        nftables
        iptables
        iproute2
```

`resolute` is 26.04's codename. `Format=disk` produces a whole partitioned disk image, and `SplitArtifacts=` also writes the pieces an update needs as separate files. The boot artefact is a UKI (Unified Kernel Image): the kernel, the initrd and the kernel command line glued into one EFI executable, which systemd-boot starts like any other EFI program. `%i_%v` names it `reliaburger-os_<version>.efi`, a name that matters later. The locked root password means there's no console login at all. The last six packages match the quickstart guest's list, `scripts/release/guest-images.json`, so the two channels test the same userland.

The kernel package drags in the full `linux-firmware` set, hundreds of megabytes, unless something else provides it. `linux-firmware-minimal` does, and we add back only the Wyse's Realtek NIC firmware (and its Intel graphics firmware on x86-64, in `mkosi.conf.d/10-x86-64.conf`, a drop-in whose `[Match] Architecture=` line applies it to one architecture only).

The first green build still had a 226.6 MB UKI on x86_64. mkosi's default initrd carried far more kernel modules, with their firmware, than a machine needs to find its own disk. An explicit list fixed it:

```ini
KernelInitrdModules=
        /drivers/virtio/
        /drivers/block/virtio_blk
        …
        /drivers/nvme/host/
        /drivers/ata/ahci
        /drivers/mmc/
        /drivers/usb/storage/
        /drivers/md/dm-mod
        /drivers/md/dm-verity
        …
        /fs/btrfs/
        /fs/erofs/
        /fs/fat/
```

Virtio for VMs, NVMe and AHCI for mini PCs, MMC for the Wyse's eMMC, USB sticks, and the filesystems the image uses. The UKI dropped from 226.6 to 73.0 MB on x86_64 and from 213.4 to 58.6 MB on aarch64. On an ESP that has to hold two of them, that's the difference between fitting and not.

CI builds both architectures natively, x86_64 on `ubuntu-24.04` and aarch64 on `ubuntu-24.04-arm`, and neither needs root. mkosi builds in a user namespace and assembles the disk image with `systemd-repart` offline, without loop devices. The runner's own systemd is too old for the tools we need, so `ToolsTree=yes` has mkosi build its own pinned tools image first (1.6 to 1.7 GB, about a minute, and deliberately not cached to respect the repo's storage budget). Each run signs its `SHA256SUMS` with a throwaway Ed25519 key it generates on the spot. The real OS signing key comes after the spike, in its own protected environment.

### The disk, partition by partition

Here's where the appliance stops looking like a normal Linux install. This is the layout of a freshly booted node on an 8 GB disk, the Wyse's eMMC size:

```
vda1  512M vfat            esp                             /boot
vda2  1.1G erofs           reliaburger_2026.39.10          (/usr, verity)
vda3   64M DM_verity_hash  reliaburger_2026.39.10_verity
vda4  5.2G btrfs           reliaburger-data                /
vda5  1.1G                 _empty                          (slot B)
vda6   64M                 _empty                          (slot B verity)
```

Four of those partitions come from the build, one `mkosi.repart/*.conf` file each. Two appear on first boot. Let's read the most interesting build-time one, slot A of `/usr`:

```ini
# Slot A of /usr: read-only EROFS checked by dm-verity. mkosi puts the verity
# root hash on the UKI's command line (usrhash=), so each UKI pins exactly
# its own /usr. …
[Partition]
Type=usr
Label=reliaburger_%A
Format=erofs
CopyFiles=/usr:/
Verity=data
VerityMatchKey=usr
# The partition UUID in the file name: sysupdate's @u gives slot B the same
# UUID, which is how the verity generator finds /usr from usrhash=.
SplitName=usr.%U
Minimize=best
SizeMinBytes=1100M
SizeMaxBytes=1100M
```

`Type=usr` is a GPT partition type, which lets systemd find the partition by what it is rather than by a `root=` argument. EROFS is a compact read-only filesystem. `Verity=data` with a matching `usr-verity` partition (`11-usr-verity.conf`) sets up dm-verity: a Merkle tree of hashes over every block, checked by the kernel as blocks are read, so a single flipped bit in `/usr` becomes a read error rather than silently different code. The label carries the image version (`%A`), which is how the update tool will tell slots apart. And the size is fixed at 1100 MiB on both ends, because an update must fit into either slot.

mkosi writes the root hash of that Merkle tree onto the UKI's command line as `usrhash=`. Think about what that means. The UKI now names the exact `/usr` it expects, down to every byte. Boot a UKI and the initrd finds the `usr` partition whose verity root matches, and nothing else will do. Kernel and userland become one versioned unit, which is exactly what an A/B update wants.

The fourth partition is the writable one:

```ini
[Partition]
Type=root
Label=reliaburger-data
Format=btrfs
CopyFiles=/:/
ExcludeFiles=/usr/
MakeDirectories=/usr /var/lib/reliaburger
Minimize=guess
GrowFileSystem=yes
```

It holds `/etc`, `/var` and bun's `/var/lib/reliaburger`, on Btrfs. Why does `/etc` live here, when the purist design ships a `/usr`-only image and builds `/etc` from nothing? Because Ubuntu keeps things in `/etc` that the system needs to work: linker paths, alternatives, certificates. A `/usr`-only image would boot with an empty `/etc`, and all of that would be missing. The cost is real, and we'll come back to it: a later image's `/etc` never reaches a node that's already installed.

The build keeps that partition as small as its contents (`Minimize=guess`), so the image downloads quickly. On first boot, `systemd-repart` runs in the initrd with a second set of definitions:

```ini
# mkosi.extra/usr/lib/repart.d/50-usr-b.conf
[Partition]
Type=usr
Label=_empty
SizeMinBytes=1100M
SizeMaxBytes=1100M
```

The first four files in that directory match the partitions that already exist, and `40-root.conf` there is just `Type=root`, which lets the data partition grow over the free space behind it. The last two append an empty slot B and its verity partition, labelled `_empty`, the label systemd-sysupdate looks for when it needs somewhere to write. In the CI run that proved it, repart grew the data partition from 109 MB to 5.2 GB and added both halves of slot B, and bun was healthy 11.6 s into the boot.

Getting there took one detour. With these definitions in the initrd's own tree, repart listed the four existing partitions, said "No changes", and did nothing, and we never found out why. Moving them into the image's `/usr/lib/repart.d` fixed it, because the initrd's repart also reads `/sysusr/usr/lib/repart.d`, which is slot A's `/usr` seen from the initrd.

The finished image: 450.1 MB compressed for the whole x86_64 disk, 350.9 MB of that the `/usr` slot image, and a 76.5 MB UKI. A weekly update ships roughly the UKI plus `/usr`, about 430 MB on x86_64 or 380 MB on aarch64.

## Booting from a wire

Writing that image to a USB stick and walking it round five machines works. It's also the slowest, most hands-on step of the whole setup. Every mini PC we care about can boot from the network instead, so the installer should arrive that way.

The catch is the network. A home LAN already has a DHCP server, inside the router, and we can't ask people to reconfigure it. So we never hand out an address. We use ProxyDHCP: when a machine's firmware broadcasts its DHCPDISCOVER with `PXEClient` in option 60, the router answers with an address as usual, and our server answers the same broadcast with an offer that assigns nothing (`yiaddr` is 0.0.0.0) and only names a boot file. The firmware takes its address from one and its boot instructions from the other. Only PXE and HTTP Boot clients listen to proxy offers, so every other device on the LAN ignores us, and even a proxy left running by mistake can't break anyone's DHCP.

In the lab, dnsmasq plays both parts. The router config hands out addresses and nothing else, and the netboot server's config is the proxy:

```ini
port=0
dhcp-range=192.168.105.0,proxy,255.255.255.0
enable-tftp
tftp-root=/srv/tftp
dhcp-userclass=set:ipxe,iPXE
pxe-service=tag:!ipxe,ARM64_EFI,"Reliaburger iPXE arm64",ipxe-arm64.efi
pxe-service=tag:!ipxe,X86-64_EFI,"Reliaburger iPXE x86_64",ipxe-x86_64.efi
…
# iPXE itself (user class iPXE): hand it a script, never the iPXE binary again (no loop)
pxe-service=tag:ipxe,ARM64_EFI,"Reliaburger script",boot.ipxe
```

`port=0` turns off dnsmasq's DNS server, and the word `proxy` in the range is what makes it a proxy. That's a stand-in: the real thing will be `relish netboot`, in Rust, later.

### PXE, then iPXE, then HTTP

Firmware PXE fetches its boot file over TFTP, whose 16-bit block numbers cap a file at about 32 MB with 512-byte blocks. Our installer UKI is 99 to 135 MB. So TFTP carries only [iPXE](https://ipxe.org), a small network boot program we build in CI from a pinned commit, and iPXE fetches everything else over HTTP.

iPXE runs scripts, and ours come in two layers. `embed.ipxe` is compiled into the iPXE binary, so it's the same everywhere. `boot.ipxe` sits on the netboot server and holds anything site-specific, so one binary serves every site. iPXE's script language is small: `:label` and `goto` for control flow, `${name}` for settings, and `a || b` to run `b` if `a` fails. A failed command without `||` ends the script, which is why you'll see `|| echo -n` used as "ignore that". Here's the embedded script's core:

```
set tries:int32 0
set waits:int32 0
:retry
dhcp || goto wait
isset ${proxydhcp/next-server} || goto noproxy
set server ${proxydhcp/next-server}
goto chain
:noproxy
# The ProxyDHCP answer races the LAN's DHCP offer, and iPXE only waits for
# it when that offer says PXEClient, which a home router's doesn't. Ask
# again twice before settling for next-server.
inc tries
iseq ${tries} 3 || goto retry
set server ${next-server}
:chain
set tries:int32 0
echo reliaburger: iPXE ${buildarch} at ${ip}, boot server ${server}
chain --autofree tftp://${server}/boot.ipxe || goto wait
:wait
# Give up after about 30 s, so a machine whose boot order puts the network
# first still reaches its disk when no netboot server is around.
inc waits
iseq ${waits} 6 && goto give_up || echo -n
…
:give_up
echo reliaburger: no netboot server answered; trying the next boot option
exit 1
```

It prefers the proxy's server, falls back to plain `next-server` (which is what QEMU's built-in network provides in CI), and gives up after six waits so the firmware moves on to the disk. The race comment and `--autofree` are both scars, and we'll get to them in the lessons.

`boot.ipxe` then chains the installer, passing it a command line:

```
set base http://${server}:8080/${buildarch}
echo reliaburger: chaining ${base}/installer.efi
chain --autofree ${base}/installer.efi reliaburger.url=${base} console=tty0 console=${console},115200
```

Arguments given to a UKI replace its built-in command line (with Secure Boot off, which it is on the Wyse fleet), which is why this line repeats the console settings. `reliaburger.url=` tells the installer where the image lives.

### The installer: curl | zstd | dd

The installer is its own mkosi subimage, `image/mkosi.images/installer/`, with `Format=uki`. Its initrd *is* the whole installer: a small Ubuntu with networking, curl, zstd, openssl, the partition tools and `efibootmgr`, running from RAM. One systemd unit starts `/usr/lib/reliaburger/install` once the network is up.

The Wyse has 2 GB of RAM, and a netbooted installer runs from that RAM. If it downloaded a half-gigabyte image into tmpfs before writing it, it would be fighting the kernel for memory. So the installer never holds the image. First it fetches `SHA256SUMS` and its signature, checks them against the Ed25519 public key built into the installer, and pulls out the one hash it expects. Then it streams:

```bash
# Hash the compressed stream as it passes; dd writes with O_DIRECT so the
# page cache doesn't fill with the image either.
fifo=/run/image.fifo
rm -f "$fifo"
mkfifo "$fifo"
sha256sum <"$fifo" | cut -d' ' -f1 >/run/image.sha256 &
hasher=$!
…
curl -fsS --retry 5 --retry-connrefused "$url/$image" \
    | tee "$fifo" \
    | zstd -dc \
    | dd of="$disk" bs=4M iflag=fullblock oflag=direct conv=fsync status=none \
    || { wipefs -a -q "$disk" || true; fail "writing $image failed"; }
wait "$hasher"
…
actual=$(cat /run/image.sha256)
if [ "$actual" != "$expected" ]; then
    wipefs -a -q "$disk" || true
    fail "$image hashed to $actual, not $expected; wiped $disk"
fi
```

If you write more Python or Go than shell, a few pieces need unpacking. `mkfifo` creates a named pipe, a file-shaped channel with no storage behind it. `sha256sum` reads from it in the background (`&`), and `$!` remembers that background job's process ID. `tee` copies the compressed download into the pipe while also passing it along to `zstd -dc`, which decompresses to standard output, which `dd` writes to the disk. `iflag=fullblock` makes `dd` wait for full 4 MiB blocks from the pipe, `oflag=direct` bypasses the page cache, and `conv=fsync` flushes before exiting. So the bytes are hashed, decompressed and on disk in one pass, and the image never exists anywhere as a whole file.

Notice what that costs us: we only learn whether the image was genuine *after* writing it. The answer is to treat the disk as untrusted until the hash matches, and to wipe it if it doesn't. A half-written or tampered disk never gets a boot entry.

Did it really stay out of RAM? The script samples its own cgroup's `memory.stat` every half second while streaming. In CI, on x86_64 with 2 GiB, it used at most 20 MiB of anonymous memory. Its cgroup peaked at 780 MiB, but the rest of that was page cache the kernel can drop, and `MemAvailable` never fell below 1368 MiB. The install took 14 s from netboot, and the installed system reported `bun healthy` 8.7 s into its first boot.

Which disk? The largest fixed, writable one:

```bash
disk=$(lsblk -dnbpo NAME,TYPE,RM,RO,SIZE \
    | awk '$2 == "disk" && $3 == 0 && $4 == 0 && $1 !~ /\/(zram|loop|ram)/ { print $5, $1 }' \
    | sort -n | tail -n 1 | cut -d' ' -f2)
```

Not removable (`RM` is 0), not read-only (`RO` is 0), which also rules out the Wyse's eMMC boot partitions, since Linux exposes those read-only.

### Going back to the disk

After a successful write, two more things happen. `sgdisk -e` moves the backup GPT header to the real end of the disk (CI built the image for a smaller one), so first boot's repart can grow into the space. And the installer puts a firmware boot entry for the disk at the front of `BootOrder`:

```bash
efibootmgr -q -c -d "$disk" -p 1 -L "Reliaburger OS" -l "$loader" || return 1
new=$(efibootmgr | sed -n 's/^Boot\([0-9A-F]\{4\}\)\*\{0,1\} Reliaburger OS.*/\1/p' | head -n 1)
order=$(efibootmgr | sed -n 's/^BootOrder: //p' | tr ',' '\n' | grep -vx "$new" | paste -sd, -)
efibootmgr -q -o "$new${order:+,$order}"
```

Why bother, when the operator can set the boot order in the BIOS? Because you put network boot first to install, and then you forget. A machine that network-boots every time would reinstall itself every time, or stall waiting for a server that's gone home. So the installer makes the disk come first. And if a machine does network-boot onto a disk that already carries a `reliaburger-data` partition, the installer leaves the disk alone: it puts the disk's boot entry first again, also names it as the next boot with `efibootmgr -n`, and reboots into the existing install. Wiping a non-empty disk needs `reliaburger.wipe=1` on the command line. The removable-media path, `\EFI\BOOT\BOOT*.EFI`, stays as a fallback for firmware that forgets entries the OS created.

## Updating a read-only system

An appliance you can't update is a time bomb, and the whole point of the A/B layout is to make updates boring. The update path has three parts: something stages a new version into the idle slot, systemd-boot tries it a limited number of times, and a health check decides whether it stays.

### Staging: bun checks, sysupdate copies

systemd-sysupdate writes new versions into A/B slots, driven by `.transfer` files. It can download by itself, but it only verifies downloads with GPG, and we already have an Ed25519 trust chain. So the transfers read from a local directory, and something we trust fills that directory first. Eventually that's bun. During the spike it's a shell script, `os-stage`, which fetches the signed `SHA256SUMS`, checks the signature, downloads the UKI and both `/usr` images, checks their hashes, moves them into root-only `/var/lib/reliaburger/os-staging`, and runs sysupdate. The `/usr` transfer:

```ini
[Transfer]
ProtectVersion=%A
Verify=no

[Source]
Type=regular-file
Path=/var/lib/reliaburger/os-staging
MatchPattern=reliaburger-os_@v.usr.@u.raw.zst

[Target]
Type=partition
Path=auto
MatchPattern=reliaburger_@v
MatchPartitionType=usr
PartitionFlags=0
ReadOnly=1
InstancesMax=2
```

The `@` patterns are sysupdate's capture groups: `@v` is a version, `@u` a partition UUID. So `reliaburger-os_2026.40.23.usr.<uuid>.raw.zst` in the staging directory becomes a partition labelled `reliaburger_2026.40.23` with that UUID. `Verify=no` is honest here, because `os-stage` already verified everything, and `ProtectVersion=%A` stops sysupdate from overwriting the version we're running. `InstancesMax=2` means two slots, so the next version goes into `_empty` the first time and into the older slot after that.

The UUID matters more than it looks. mkosi names each split `/usr` image after its partition UUID (`SplitName=usr.%U`, in the repart file earlier), and `@u` carries it into slot B. The new UKI's `usrhash=` implies that UUID, and that's how the verity generator finds the right slot at boot. Without it, the new kernel would boot and find no `/usr`.

The UKI transfer targets the ESP:

```ini
[Target]
Type=regular-file
Path=/EFI/Linux
PathRelativeTo=boot
MatchPattern=reliaburger-os_@v+@l-@d.efi \
             reliaburger-os_@v+@l.efi \
             reliaburger-os_@v.efi
# Writable: systemd-boot renames the file to count down its tries (+3-0,
# +2-1, ...), which a FAT read-only attribute would stop.
Mode=0644
TriesLeft=3
TriesDone=0
InstancesMax=2
```

On node-05 of the lab cluster, `os-stage` took 14 s end to end, and the new UKI landed as `reliaburger-os_2026.40.23+3-0.efi`.

### Three tries, then fall back

That file name is the whole boot-counting protocol. `+3-0` means three tries left, none done. systemd-boot prefers the newest entry, and before starting a counted one it renames the file on the ESP: `+2-1`, then `+1-2`, then `+0-3`. An entry with no tries left sorts behind everything else, so the next boot falls back to the previous UKI, whose `usrhash=` points at the previous `/usr` slot. Nobody has to touch anything.

The other half is `systemd-bless-boot`, which removes the counter (renaming the file to plain `reliaburger-os_2026.40.23.efi`) once the boot reaches `boot-complete.target`. So the question "did this version work?" reduces to "what has to happen before `boot-complete.target`?". For us, bun has to be healthy:

```ini
[Unit]
Description=Reliaburger boot check
After=reliaburger.service
Wants=reliaburger.service
Before=boot-complete.target

[Service]
Type=oneshot
RemainAfterExit=yes
StandardOutput=journal+console
ExecStart=/usr/lib/reliaburger/boot-check

[Install]
WantedBy=multi-user.target
RequiredBy=boot-complete.target
```

`Before=` is ordering only; `RequiredBy=` makes the target fail if this unit fails. The script behind it polls bun's `/v1/health` for up to 300 s. On success it prints `reliaburger: bun healthy` to the console, which every boot test in CI and the lab waits for. On failure:

```sh
counted() {
    # systemd-boot sets this only when the entry has tries left (+N).
    [ -e /sys/firmware/efi/efivars/LoaderBootCountPath-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f ]
}
…
echo "reliaburger: bun not healthy after 300 s on OS ${IMAGE_VERSION:-unknown}"
if counted; then
    echo "reliaburger: counted boot failed, rebooting (systemd-boot falls back after the last try)"
    systemctl reboot
fi
exit 1
```

Only a counted boot reboots itself. A node running a blessed version whose bun is merely slow just reports the failure and stays up, because rebooting it would gain nothing.

The good update rolled across all five lab nodes, followers first and the leader last. Each came back blessed about 20 s after its reboot, the cluster never dropped below five nodes alive, and Raft carried on (term 12, log index 964 to 1008).

Then the bad one. CI can build an image whose bun never starts (a `workflow_dispatch` input drops in a unit override with `ExecStart=/bin/false`), and we staged it on node-05. Three counted boots each ended with `bun not healthy after 300 s` and a reboot. The UKI ran out of tries at `+0-3`, systemd-boot picked the previous version by itself, and node-05 was healthy 7.6 s into that boot and back in the cluster. From the first try to the fallback took about 15 min 40 s, with no hands. Nearly all of that is the 300 s timeout, three times over, which tells us where to tune.

### bun's own upgrades

Chapter 14's self-upgrade writes `bun-vX.Y.Z` beside the running binary and moves a `bun` symlink. On the appliance, bun lived in `/usr`, which is read-only. The lab's `relish upgrade start --binary` reached every node and was refused, correctly, for lack of an operator countersignature. But the attempt exposed a second problem waiting behind the first: even a properly countersigned binary would have had nowhere to go.

So bun now runs from `/var/lib/reliaburger/bin`, on the writable partition, and a small launcher decides which bun that is:

```sh
bin=/var/lib/reliaburger/bin
image=/usr/lib/reliaburger/bin/bun

version() { "$1" --version 2>/dev/null | awk '{ print $2 }'; }

install -d -m 0755 "$bin"
image_version=$(version "$image")
active_version=
[ -x "$bin/bun" ] && active_version=$(version "$bin/bun")
if [ -z "$active_version" ] \
    || [ "$(printf '%s\n%s\n' "$active_version" "$image_version" | sort -V | tail -n 1)" != "$active_version" ]; then
    # First boot, or the image brought a newer bun than the active one.
    install -m 0755 "$image" "$bin/.bun-v$image_version.tmp"
    mv -f "$bin/.bun-v$image_version.tmp" "$bin/bun-v$image_version"
    ln -sfn "bun-v$image_version" "$bin/.bun.tmp"
    mv -Tf "$bin/.bun.tmp" "$bin/bun"
    …
fi
```

`sort -V` sorts version numbers numerically, so the condition reads "the image's bun is strictly newer than the active one". The copy uses the same two-step as Chapter 14's `BinaryStore`: write a hidden temporary file, rename it into place, then build a new symlink and rename that over the old one. `mv -T` makes `mv` replace `bun` itself rather than treat it as a directory to move into, so the swap is a single atomic rename. The upshot: an OS update can't downgrade a bun that upgraded itself, and a bun upgrade can't be undone by the next OS image. We tested five cases on Linux with stand-in binaries (first boot, same version, newer image, older image, and a bun that upgraded itself past the image), and the second rolling update, to 2026.40.30, moved bun onto `/var` on every node.

The end-to-end `relish upgrade` on the appliance still hasn't run. Every staging candidate is v0.1.0, the launcher correctly refuses the same version with different bytes, and so the test waits for the next release-signed bun.

## A LAN on a laptop

We had no KVM machine and no spare PCs, only CI and one Apple silicon Mac without root. Everything had to run virtualised until the last step.

### CI boots every image

Each CI run boots what it built, with 2 GiB and an 8 GB disk, like the Wyse. The pass condition is a line on the serial console, not a process exit code:

```bash
while [ $(( $(date +%s) - start )) -lt "$deadline" ]; do
  if grep -q "reliaburger: bun healthy" "$log" 2>/dev/null; then result=pass; break; fi
  if grep -q "reliaburger: bun not healthy" "$log" 2>/dev/null; then result=fail; break; fi
  if ! kill -0 "$qemu" 2>/dev/null; then result=exited; break; fi
  sleep 5
done
```

x86_64 runs under KVM and reached bun in 9 to 16 s across runs. GitHub's arm64 runners have no `/dev/kvm`, so aarch64 runs under TCG, QEMU's pure software emulation, with `-cpu max,pauth-impdef=on` (QEMU's own pointer-authentication algorithm, much cheaper to emulate than the architected one) and a 20-minute deadline. It reached bun at 167 s kernel time. Slow, but it would have caught the stubble kernel before the Mac did, which is why we added it. A second x86_64 test netboots a blank disk through QEMU's built-in DHCP and TFTP, installs, and boots the result.

### The Mac lab

Apple silicon runs aarch64 guests at native speed under HVF, so day-to-day testing was aarch64. The plan was socket_vmnet, a root service giving QEMU processes a shared network. The Mac had no root. So the lab builds its own LAN out of QEMU:

```
Mac ── ssh 127.0.0.1:2222 ──> server VM (Ubuntu 26.04 arm64, HVF)
                               ├─ wan: QEMU user network (internet, NAT for the lab)
                               └─ lan ─┬─ br0 192.168.105.1   the "home router": dnsmasq,
                                       │                       addresses only, NAT out of wan
                                       └─ netns nb 192.168.105.2  the netboot server: dnsmasq as
                                                                  ProxyDHCP and TFTP, HTTP on 8080
QEMU hub 0 (inside the server's QEMU) ── 40 unix-socket slots ── nodes and virtual Wyses
```

The server VM's own QEMU process holds a hub, a virtual Ethernet segment, with a unix-socket port for each client. Inside the VM, a bridge plays the home router: dnsmasq handing out addresses only, with reservations by MAC because bun's `bootstrap_peers` need literal IPs. A separate network namespace, a second network stack inside the same kernel, plays the netboot server at .2 with the proxy config we saw earlier. Two DHCP servers on one wire, one giving addresses and one giving boot files: the exact situation a real home LAN presents.

One more workaround. Homebrew's aarch64 firmware can't network-boot under HVF on an M1 (it finds no random number generator, so its network stack never loads). The aarch64 nodes start our iPXE directly with QEMU's `-kernel`, the virtual equivalent of an iPXE USB stick. Firmware PXE still gets exercised, under TCG, by the virtual Wyse.

### Five at once, and the tour

```sh
for n in 1 2 3 4 5; do ./rbnode.sh $n install fresh & done; wait
```

Five aarch64 nodes, blank 10 GB disks, 2 GiB each. Each took its address from the router and its boot script from the proxy, and all five were installed in 42 s. No installer used more than 32 MiB of anonymous memory, and `MemAvailable` stayed above 1359 MiB.

Forming the cluster needed configuration, and the appliance has no login. The spike passes it as a systemd credential: a small blob that the firmware hands to PID 1, here through QEMU's `-smbios type=11`. `reliaburger-seed.service` picks it up with `ImportCredential=reliaburger.seed` and unpacks a tarball of `node.toml`, the master key and the node's identity into `/etc/reliaburger`, once. On real machines, `apply-seed` looks for a USB stick labelled `RBSEED` instead. Node 1 was healthy 7.7 s into its seeded boot, the four joiners (enrolled with `relish join` from the server VM) at 17 to 23 s, and all five became council voters.

Then the tour, the manual's five-minute walk through Reliaburger (`relish manual tour`): apply podinfo, ingress on two nodes, `relish path` through the eBPF service map, a 300 ms netem delay that `path` duly measured at 300 ms, `fault kill` and the restart, and a node "powered off" by killing its QEMU (four of four left alive, council healthy, replicas rescheduled). Power it back on and it rejoined: five of five, and `relish wtf` showed 12 OK and no warnings. Only `relish dashboard` was skipped, because it opens a browser and `relish` was running headless on the server VM.

### The virtual Wyse

The Wyse 3040's Atom has SSE4.2 and AES-NI but no AVX. The virtual Wyse is x86_64 under TCG with `-cpu Westmere`, which matches that, so any accidental AVX dependency in our binaries shows up before the hardware stage. It goes through the whole real path: OVMF's firmware PXE, the ProxyDHCP, our iPXE, the installer. It installed in 29 s, and the same QEMU process then booted the disk and reached `bun healthy` at 62 s kernel time. No AVX problems.

## What bit us

Most of the spike's time went into bugs that no amount of documentation would have predicted.

### Stubble

The x86_64 image booted first time. The aarch64 one, on the Mac, got as far as systemd-boot and stopped: `pe_kernel_check_no_relocation: Inner kernel image contains base relocations, which we do not support`.

On Ubuntu 26.04 arm64, the `vmlinuz` file isn't a kernel. It's Canonical's *stubble*, an EFI stub that picks a devicetree from 34 embedded ones and then starts the real kernel, which it carries in its own `.linux` section. mkosi didn't recognise stubble as a wrapper and embedded the whole thing as our UKI's kernel, and systemd-stub refused it. The fix unwraps it before mkosi builds the UKI. It's Python, using `pefile` to read the Windows PE format that all EFI binaries use:

```python
for kimg in sorted([*root.glob("boot/vmlinuz-*"), *root.glob("usr/lib/modules/*/vmlinuz")]):
    if kimg.is_symlink():
        continue
    pe = pefile.PE(str(kimg), fast_load=True)
    inner = next((s for s in pe.sections if s.Name.rstrip(b"\0") == b".linux"), None)
    if inner is None:
        continue
    # Misc_VirtualSize is the payload; SizeOfRawData is padded to the file
    # alignment.
    data = inner.get_data()[: inner.Misc_VirtualSize]
    pe.close()
    if not data.startswith(b"MZ"):
        raise SystemExit(f"{kimg}: .linux is not a PE kernel image")
    kimg.unlink()
    kimg.write_bytes(data)
```

x86_64 kernels have no `.linux` section and pass through untouched. On aarch64 the build now logs `Unwrapped usr/lib/modules/7.0.0-34-generic/vmlinuz: 17478144 bytes from .linux`, and the image booted to healthy bun in 7.9 s under HVF. We lose stubble's devicetree matching, which only devicetree-only arm64 laptops need.

The fix started life as a *finalize* script, the hook mkosi runs at the end. That worked for the disk image. Then the aarch64 installer failed with `Error 0x7f048281`, because for `Format=uki` mkosi sets the kernel aside *before* finalize scripts run. As a *postinst* script, which runs right after package installation, it catches both. Two lessons, then: the aarch64 boot test in CI exists because x86_64 alone let this through, and "when does this hook run?" deserves the same care as "what does this hook do?".

### A read-only file on FAT

The first OS update in the lab rebooted into 2026.40.23 and worked. Then nothing got blessed, ever.

sysupdate's transfer said `Mode=0444`, a sensible-looking choice for a file nothing should modify. On the ESP, which is FAT, that becomes the FAT read-only attribute. But boot counting *is* modifying the file: systemd-boot counts down by renaming it. It couldn't, so it never set `LoaderBootCountPath`. With no count, there was nothing for `systemd-bless-boot` to bless, and a broken version would never have run out of tries. The fix is `Mode=0644`, and the next boot ran `+2-1` and was blessed as it should be. This is the kind of failure a test that only checks "did the new version boot?" never sees. The spike checked for the blessing, which is why it caught it.

### "Already started"

On x86_64, the installer UKI died on start with `Error registering initrd: Already started`. iPXE, it turns out, fetches `autoexec.ipxe` from the TFTP server it came from, even with a script built in, and our TFTP directory had one. iPXE didn't run it, but held on to it, and iPXE passes every image it still holds to the next EFI binary as an initrd. The UKI's own systemd-stub then tried to register its initrd and found one already there. `embed.ipxe` now frees that file on its first line, and both scripts chain with `--autofree`, so a failed attempt can't leave an image behind for the next one either.

### The ProxyDHCP race

iPXE's `dhcp` command waits for proxy offers only when the router's offer says `PXEClient`. A home router's never does. So whichever answer arrived first won, and sometimes that was the router's plain offer with no boot server in it. The embedded script now asks up to three times before it settles for `next-server`. That's a mitigation, not a fix.

### macOS, multicast, and a hub that forgets

The research plan's no-root fallback was QEMU's `-netdev dgram`, joining VMs over UDP multicast. On macOS every send failed with `EADDRNOTAVAIL`. Hence the hub inside the server VM. Then the hub had its own quirk: its stream server doesn't deliver frames to a second client on a reused socket. The symptom was a node whose `DHCPDISCOVER` got an offer that never arrived, and ARP that failed. So every QEMU start takes a fresh slot from a counter, 40 per server start, and when they run out you restart the server. Not elegant, but reliable.

### A binary that can't write next to itself

We covered bun's version above. The lesson is broader: anything that assumes it can write next to its own executable breaks on an immutable OS, and it breaks late, at upgrade time, long after the first boot looked fine.

### Reading Sidero's Omni

We read Sidero Labs' Omni bare-metal provider (`siderolabs/omni-infra-provider-bare-metal`) for ideas only, and copied no code. It encodes few vendor quirks. Its main defence against boot loops is server-side state plus one-shot PXE through a BMC, which a Wyse doesn't have. Four ideas did transfer. The installer puts the disk first in the boot order, and sends machines that are already installed back to their disk. `embed.ipxe` gives up after about 30 s. CI builds iPXE's `snp.efi`, which drives the NIC through the firmware's own driver, and makes it the default. And `boot.ipxe` works out its own server, for firmware that has iPXE built in and skips our embedded script. The rest went into notes for `relish netboot`.

## What's next

The spike's interim record is a pass for everything that can be proven without hardware, with one item open (bun's own upgrade on the appliance). The rest of the road:

- **S5, ten Dell Wyse 3040s.** BIOS setup, PXE on the real Realtek NIC, whether the firmware keeps the boot entry the installer makes, `MemAvailable` under the tour, eMMC writes per day, one OS update across the fleet, and whether Linux 7.0 still hangs on reboot without our `dw_dmac` blacklist. A node that can't reboot can't finish an A/B update, so that last one matters more than it sounds.
- **`relish netboot` in Rust.** A ProxyDHCP on UDP 67 and 4011 that reads the client's architecture from option 93, TFTP for iPXE, and HTTP for the rest. It should remember installed machines by MAC and SMBIOS UUID, and serve them a `boot.ipxe` that just says `exit`.
- **Claims.** Seeds over SMBIOS and USB sticks were a spike shortcut. The product plan is a claim over the LAN: the machine boots `unclaimed`, and an operator with the admin token adopts it. Netboot never serves secrets.
- **The `/etc` gap.** Because `/etc` lives on the data partition, a later image's `/etc` never reaches installed nodes, and the seed and SSH credentials land there too. It has to close before OS updates ship for real.
- **Smaller things with known answers:** a shorter health timeout on counted boots, to bring the fallback well under 16 minutes; dropping the lab's credential-gated SSH once bun stages OS updates itself; the real OS signing key; and a way for the installer to find its server under Secure Boot, which drops the command line iPXE passes.

The shape is clear, though. The operating system is now one more artefact that Reliaburger builds, signs, rolls out and rolls back, like bun. It just happens to be the one bun stands on.
