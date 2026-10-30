# Becoming the Operating System

The quickstart cheats. It starts Lima, Lima starts an Ubuntu VM that CI built with exactly the packages bun needs, and bun wakes up on a machine that was assembled for it. Then somebody reads the manual, finds five old mini PCs in a cupboard, and installs Reliaburger on real hardware. One box runs Debian, one runs whatever Ubuntu was current when it was last wiped, one has a hand-edited sysctl nobody remembers adding. The manual's prerequisites (Linux 5.8 or newer, cgroup v2, bpffs, rootful runc, eBPF) are now five separate questions with five separate answers.

Everything in this book so far has treated the operating system as somebody else's problem. This chapter makes it ours. We build an appliance image: Ubuntu 26.04 with bun as its only service, a read-only `/usr`, A/B update slots, and an installer that arrives over the network. Then we prove it works without owning a single physical test machine, which turned out to be half the work.

This was a spike, stages S1 to S4 of a six-stage plan, run in GitHub Actions and on one Mac over two days in late September 2026. The last stage, Dell Wyse 3040 thin clients on a real network, hasn't happened yet; the release gate runs it on three of them. So read this chapter as a report from the middle of the road. Most of the code here is shell, systemd units, mkosi configuration and iPXE script rather than Rust, and we'll explain each of those as it appears.

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

`resolute` is 26.04's codename. `Format=disk` produces a whole partitioned disk image, and `SplitArtifacts=` also writes the pieces an update needs as separate files. The boot artefact is a UKI (Unified Kernel Image): the kernel, the initrd and the kernel command line glued into one EFI executable, which systemd-boot starts like any other EFI program. `%i_%v` names it `reliaburger-os_<version>.efi`, a name that matters later. The locked root password means there's no console login at all. The last six packages match the quickstart guest's list, `scripts/release/guest-images.json`, less Buildah, which the guest needs for `relish build`. From 0.3.0 the quickstart guest runs Ubuntu 26.04 too, so the two channels install the same packages from the same archive and test the same userland. mkosi can't read a JSON file, so rather than generate one list from the other we let a test hold them together: `test_guest_images.py` parses the `Packages=` block and fails if its tail and the guest's list drift apart.

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

Here's where the appliance stops looking like a normal Linux install. This is the layout of a freshly booted node on the 8 GiB disk CI used at the time:

```
vda1  512M vfat            esp                             /boot
vda2  1.1G erofs           reliaburger_2026.39.10          (/usr, verity)
vda3   64M DM_verity_hash  reliaburger_2026.39.10_verity
vda4  5.2G btrfs           reliaburger-data                /
vda5  1.1G                 _empty                          (slot B)
vda6   64M                 _empty                          (slot B verity)
```

A Wyse's "8 GB" eMMC is 8 × 10⁹ bytes, about 7.3 GiB, so on the real machine the data partition is about 1 GiB smaller. Four of those partitions come from the build, one `mkosi.repart/*.conf` file each. Two appear on first boot. Let's read the most interesting build-time one, slot A of `/usr`:

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

`port=0` turns off dnsmasq's DNS server, and the word `proxy` in the range is what makes it a proxy. The lab still runs it, but people installing a fleet shouldn't need dnsmasq, Python and a shell script: `relish netboot` does the same job in Rust, and we'll get to it after the iPXE scripts.

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

### `relish netboot`: three servers and one rule

`relish netboot os` replaces dnsmasq, Python's `http.server` and the script gluing them together. It's three small servers in `src/relish/netboot/`: a ProxyDHCP on UDP 67 and 4011, a read-only TFTP server on UDP 69, and HTTP through axum on 8080. They share one rule: **nothing is served until it's checked.** At start-up, `artefacts::load` verifies each architecture's `SHA256SUMS` signature against the release keys, then hashes every file it lists, and refuses to start if anything is off. A netboot server that hands out an unverified file installs it on every machine that asks, so a refusal is the friendly outcome.

Each server is a pure function wrapped in a thin loop. The ProxyDHCP's whole policy is one function:

```rust
pub fn answer(
    request: &Message,
    port: ListenPort,
    context: &DhcpContext,
) -> Result<Answer, Ignored> {
    let mac = client_mac(request).ok_or(Ignored::NotEthernetRequest)?;
    let http_client = match class_identifier(request) {
        Some(class) if class.starts_with(b"PXEClient") => false,
        Some(class) if class.starts_with(b"HTTPClient") => true,
        _ => return Err(Ignored::NotPxe),
    };
    …
}
```

`Message` is a parsed DHCP packet from the `dhcproto` crate, and `Result<Answer, Ignored>` says the function either answers or explains why not. `Ignored` is an enum, so "a laptop asking for an address" (`NotPxe`, silent) and "a BIOS machine we can't boot" (`UnsupportedArch`, logged) are different values, and the compiler makes the log code handle each. `b"PXEClient"` is a byte-string literal, a `&[u8; 9]` rather than a `&str`, because DHCP options are bytes, not text. The `?` after `ok_or` turns a missing MAC into an early `return Err(…)`, the same trick `?` plays on any `Result`.

Because `answer` touches no sockets, the tests feed it hand-built packets: a UEFI DISCOVER (architecture 7) must get `ipxe-x86_64.efi`, arm64 (11) `ipxe-arm64.efi`, HTTP Boot (16, 19) an `http://` URL and `HTTPClient`, and iPXE (user class `iPXE`) `boot.ipxe`. Two property tests, with `proptest`, throw thousands of random requests at it and check what must never happen: a reply with a non-zero `yiaddr`, or any reply at all to a client that didn't say `PXEClient` or `HTTPClient`. Those two properties are why a ProxyDHCP can't hurt a LAN, so they're worth more than any number of examples.

One more line deserves a look. `dhcproto` has a few `debug_assert!`s on option lengths, which a crafted packet can trip in a debug build. A panic in the DHCP task would quietly take the server down, so decoding runs inside `std::panic::catch_unwind`, which turns a panic into an `Err` we treat as "not DHCP". It's not something you reach for often in Rust: panics are for bugs, not input. Here the bug is in someone else's parser, and our input comes from anyone on the LAN.

TFTP is RFC 1350 plus option negotiation: `blksize` (we cap it at 1468, so a block fits one Ethernet frame), `tsize` and `timeout`. The sender is a state machine, `Transfer`, with `on_packet` and `on_timeout` returning a `Step`: send this, wait, done, or give up. It only resends on a timeout, never on a duplicate ACK. Resending on both is the Sorcerer's Apprentice bug, where every late ACK doubles the traffic from then on. UEFI firmware also has a habit worth knowing: it asks for a file with only `tsize`, reads the size from our option acknowledgement, and sends an error to stop. That's a size probe, not a failure, so it isn't logged as one.

The last piece fixes something dnsmasq couldn't. A machine whose boot order puts the network first used to install itself again on every boot. Now the TFTP `boot.ipxe` is just a hand-over:

```
chain --autofree http://192.168.1.20:8080/boot.ipxe?mac=${netX/mac}&uuid=${uuid}&arch=${buildarch} || exit 1
```

The HTTP handler reads the MAC and SMBIOS UUID from the query and answers with the install script, or with `exit 1` if that machine has fetched the installer before. It remembers machines in a small JSON file next to the artefacts, and only once the whole installer has streamed out, so a download that dies halfway doesn't count. `exit 1` rather than `exit`: UEFI firmware may stop at its boot menu when a boot option returns success, but it moves on to the next one, the disk, after a failure.

The safety rails are small. `--mac` limits who gets answered, `--for` (an hour by default) stops a forgotten server, and before binding anything it broadcasts a PXE DISCOVER of its own and listens for five seconds. It was two until the netboot demo's router, a dnsmasq, answered after three: dnsmasq pings an address before it offers it, to be sure nobody holds it, and so do many home routers that run dnsmasq underneath. Another boot server answering stops the start, because two ProxyDHCPs race for every machine. The router answering is what we want. Nothing answering gets a warning but no refusal: "nothing hands out addresses on en7". A ProxyDHCP with no DHCP server beside it boots nothing, and the most likely reason in the lab is a switch that isn't plugged into the router, or a cable in the wrong port.

The listening loop reads well once you know two small idioms:

```rust
let mut address_servers: Vec<AddressServer> = Vec::new();
let deadline = tokio::time::Instant::now() + wait;
while let Ok(received) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await
{
    let (len, source) = received?;
    …
    if let Some(server) = dhcp::competing_server(&reply, xid, *source.ip())
        && server != own
    {
        return Ok(Network::BootServer(server));
    }
    if let Some(address) = dhcp::address_server(&reply, xid, *source.ip())
        && !address_servers.iter().any(|s| s.address == address)
    {
        address_servers.push(AddressServer {
            address,
            pxe_client: dhcp::says_pxe_client(&reply),
        });
    }
}
```

`timeout_at` wraps a future and returns `Err(Elapsed)` if the deadline passes first, so `while let Ok(…)` keeps reading until time's up, and the deadline is fixed once rather than restarted by every packet. Afterwards an empty list means `Network::Silent`, and anything else is `Network::AddressServers(address_servers)`. The probe takes its socket and its target as arguments, so the tests run it on loopback against a fake router, a fake competitor and silence, without root or a LAN.

The first version kept only the first DHCP server it heard, which was all a ProxyDHCP beside a router needs: `address_server.get_or_insert(server)` in the loop, then `address_server.map_or(Network::Silent, Network::AddressServer)`. That last call shows a small Rust trick. A tuple variant like `Network::AddressServer` *is* a function from its field to the enum, so it can go where a closure would, in place of `|s| Network::AddressServer(s)`. The list came later, with the isolated lab below, which needs to know every DHCP server on the wire and whether each one said `PXEClient`.

Making it work on a Mac taught us two things about BSD sockets. The probe listens on UDP 68, the DHCP client port, which the machine's own DHCP client may hold. On macOS, `configd` didn't hold it at all on our M2 while idle, but it may open it to renew a lease. Linux's `SO_REUSEADDR` lets two UDP sockets share a port loosely; BSD's doesn't, and the flag that does is `SO_REUSEPORT`, which both sockets must set. When they do, a broadcast goes to both, so the probe gets its copy of the router's offer without stealing it from anyone. The probe sets both flags. The servers' own sockets on 67, 69 and 4011 set neither: two netboot servers sharing port 67 would split the requests between them instead of the second one failing to start, which is the race we refuse elsewhere. They used to set `SO_REUSEADDR` out of habit, which is harmless on a Mac. CI caught it on the first Linux run: there, two sockets that both set it share the port, and a test that expected the second bind to fail watched it succeed. UDP has no `TIME_WAIT`, so the flag was buying nothing anyway.

### A lab with no router

All of this assumes a router: something on the LAN that hands out addresses, for as long as the cluster lives. The lab first planned a Raspberry Pi for that job, on a switch isolated from the home network. On 8 October 2026 we dropped the Pi and asked whether the Mac running `relish netboot` could be the router too, with dnsmasq for DHCP.

The obvious version fails at once. dnsmasq needs UDP 67, relish's ProxyDHCP needs UDP 67, and relish's server socket is exclusive on purpose. We could have let the two share it: on BSD both sockets set `SO_REUSEPORT` and each gets a copy of every broadcast. But that's exactly the loose sharing we'd just removed, because it lets two netboot servers run side by side without either noticing.

The PXE specification has a better answer, in section 2.2.4. A DHCP server can put option 60, the vendor class, in its offer, set to `PXEClient`. To PXE firmware that means "I gave you an address but no boot file; ask me for the boot part on UDP 4011". And 4011 is the port relish already answers for firmware that sends its ProxyDHCP a direct request. So dnsmasq keeps 67, and relish answers on 4011 alone. Neither shares a port, and relish needed only two changes: one flag, `--mode-dhcp-proxy`, and a different reading of the probe.

dnsmasq's side is two lines in `image/lab/mac/dnsmasq.conf`:

```
dhcp-vendorclass=set:pxe,PXEClient
dhcp-option-force=tag:pxe,60,PXEClient
```

The first tags any client whose own vendor class contains `PXEClient`: the firmware, and iPXE after it. The second sends option 60 to those clients only, so an installed node renewing its lease gets an ordinary offer. pf on the Mac takes the lab's traffic out through the Wi-Fi. It loads into an anchor under `com.apple`, because macOS's own `/etc/pf.conf` already has a `nat-anchor "com.apple/*"`, and the lab's rule can then be flushed without touching the system's.

On the Rust side, the flag becomes an enum, not a `bool` that travels through the code:

```rust
pub enum DhcpSetup {
    Router,
    ThisMachine,
}
```

and `run` skips the port-67 socket for `ThisMachine`. The servers run under one `tokio::select!`, which finishes when any of its branches does, so a missing server still needs a branch that never finishes:

```rust
let dhcp_task = async move {
    match dhcp_socket {
        Some(socket) => dhcp_loop(socket, ListenPort::Dhcp, dhcp_context, dhcp_log).await,
        None => std::future::pending().await,
    }
};
```

`std::future::pending()` is a future that never completes. Both arms of the `match` must have the same type, and `pending()` takes whatever type it's asked for (here `std::io::Result<()>`), so the branch type-checks and `select!` never picks it. `async move` makes the block own the socket and its own clones of the context and the log, so the original `context` can still go to the 4011 server on the next line.

The probe's reading changes with the setup. Beside a router, any DHCP server is welcome. Beside our own dnsmasq, a DHCP server at any *other* address is a refusal: machines that take its offer never learn about port 4011, and two DHCP servers on one switch is a fault in its own right. An offer from our own address without `PXEClient` gets a warning that names the two dnsmasq lines. And silence is ambiguous, because a server on the same machine doesn't always hear its own broadcasts. So the probe asks the kernel instead: it tries to bind UDP 67 with the same exclusive socket the servers use. If the bind fails with "address in use", something here is serving DHCP. If it succeeds, nothing is, and relish says to start dnsmasq first.

CI runs both setups on every lab build. `relish-netboot-install.sh` takes a mode. In `router` mode it runs the router in a network namespace, as before. In `dhcp-proxy` mode it runs the lab Mac's own `dnsmasq.conf` on the runner, with the bridge and subnet swapped in, and starts relish with `--mode-dhcp-proxy`. A VM installs from them, and the test checks relish's log for an `ack on 4011` to the firmware, another to iPXE, and no `offer` from port 67. OVMF follows section 2.2.4; whether the Wyse's firmware does is one of the things the lab's dry run finds out.

The second thing: since Mojave, macOS lets any user bind a port below 1024 on `0.0.0.0`, but not on a specific address. So DHCP binds without sudo on a Mac, and TFTP, which listens on the interface's own address, is what asks for root.

Two smaller fixes came from the Mac lab plan. An interface whose DHCP never answered gets a self-assigned `169.254.x.x` address from macOS, and relish used to serve from it, telling machines to fetch iPXE from an address they can't reach. `interface::pick` now refuses one and says what to check. And TFTP can hand out either of the two iPXE builds CI makes: `snp`, the default, or `full` with `--ipxe full`, for a machine whose firmware network stack misbehaves. Both are hashed and held in memory at start-up like the SNP build always was, so the choice is just which bytes go under the name DHCP gave out.

What `relish netboot` serves comes from `relish image download`, and that command first guessed the wrong architecture. Its `--arch` defaulted to `std::env::consts::ARCH`, the architecture relish itself was compiled for. On the maintainer's lab, an M2 MacBook serving ten x86_64 Wyses, that's `aarch64`: the download worked, the signatures checked out, and not one Wyse could have booted what it saved. The laptop that serves the files is rarely the architecture of the machines that run them. So the download now fetches every architecture the release offers, and `--arch` narrows it:

```rust
/// Only this architecture, x86_64 or aarch64 (repeat for more; default: every one the release has).
#[arg(long = "arch", value_name = "ARCH", value_parser = reliaburger::relish::image::parse_arch)]
arches: Vec<reliaburger::relish::netboot::Arch>,
```

A `Vec` field makes clap accept the flag any number of times, `--arch x86_64 --arch aarch64`, the same repeatable style as `relish netboot --mac`. With no `--arch` the vector is empty, and empty means "everything". `value_parser` runs `parse_arch` on each value as clap reads it, so `--arch riscv64` fails before relish touches the network, and the field holds `Arch`, the enum `relish netboot` already uses, not strings. The choice itself is a pure function, `select_architectures`, that takes what the channel offers and what the operator asked for: the tests check the default, a narrowed list, and a request for an architecture the release doesn't have. The last line names what landed, `Saved x86_64 and aarch64 under os (os/x86_64/, os/aarch64/)`, so nobody has to `ls` to find out. It costs one more image's download, a few hundred megabytes, which is cheap next to a lab of machines that won't boot.

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

Why bother, when the operator can set the boot order in the BIOS? Because you put network boot first to install, and then you forget. A machine that network-boots every time would reinstall itself every time, or stall waiting for a server that's gone home. So the installer makes the disk come first. And if a machine does network-boot onto a disk that already carries a `reliaburger-data` partition, the installer leaves the disk alone: it puts the disk's boot entry first again, also names it as the next boot with `efibootmgr -n`, and reboots into the existing install. Wiping any other non-empty disk needs `reliaburger.wipe=1` on the command line, or a yes from the operator, which is the next section. The removable-media path, `\EFI\BOOT\BOOT*.EFI`, stays as a fallback for firmware that forgets entries the OS created.

### Asking before wiping

Ten second-hand Dell Wyse 3040s arrived for the lab. Some had blank eMMCs. Some still had ThinOS on them. The installer above refuses a disk that isn't empty unless the kernel command line says `reliaburger.wipe=1`, and `relish netboot` writes that command line, so the obvious fix is a `--wipe` flag that adds it. It's also the wrong fix. A flag that wipes every machine that network-boots will one day meet a laptop on the same switch whose owner pressed F12 at the wrong moment.

So the decision moved to the person at the `relish netboot` terminal. Before it writes anything, the installer runs `ask-to-wipe`, which posts what `lsblk -P` sees on the disk to the URL `relish netboot` gave it on the command line (`reliaburger.ask=http://…/disk?mac=…&uuid=…`). It gets back a ticket, and polls `GET /disk/<ticket>` every two seconds. Meanwhile the operator sees:

```
relish netboot: 6c:4b:90:12:34:56: /dev/mmcblk0, 7.8 GB, gpt, 4 partitions: vfat "EFI", ext4 "ThinOS", (no filesystem), swap — wipe? [y/N]
```

The whole policy is one pure function in `src/relish/netboot/wipe.rs`:

```rust
pub fn decide(
    report: &DiskReport,
    mac: Option<MacAddress>,
    pre_approved: &[MacAddress],
    answer: Answer,
) -> WipeDecision {
    without_asking(report, mac, pre_approved).unwrap_or(match answer {
        Answer::Yes => WipeDecision::WipeAndInstall,
        Answer::No | Answer::NoAnswer | Answer::NoTerminal => WipeDecision::Decline,
    })
}
```

`without_asking` returns `Some(Install)` for a blank disk, `Some(WipeAndInstall)` for a MAC that `--wipe` lists, and `None` for everything else, which is when the operator's answer counts. `unwrap_or` takes the `Option`'s value if there is one, or the fallback. The `match` lists every `Answer` variant, so adding a fifth one later (say, "wipe all remaining") won't compile until someone decides what it means here. That's the decision table, and the unit test walks it row by row:

| Disk | `--wipe` lists it | Operator | Result |
|------|-------------------|----------|--------|
| blank | either | not asked | install |
| used | yes | not asked | wipe, install |
| used | no | `y` | wipe, install |
| used | no | anything else | leave it alone |
| used | no | no answer in 10 minutes | leave it alone |
| used | no | no terminal (stdin isn't a TTY) | leave it alone |

A disk that already holds Reliaburger OS never reaches the table: as before, the installer boots it.

#### Why a machine can't approve its own wipe

Look at what each endpoint accepts. `POST /disk` takes lsblk output and returns a random ticket. It never returns a decision, and nothing in the body is a decision: the only thing a report can influence is whether the disk looks blank. `GET /disk/<ticket>` reads. There's no `PUT`, no `POST` to a ticket, no `?answer=yes`, and a test sends all four write methods to a ticket and expects `405 Method Not Allowed`. The decision comes from exactly two places: a line typed on the terminal's stdin, or the `--wipe` list on the command line. Neither is reachable over the network.

Why tickets rather than polling by MAC? Because MACs are easy to fake. If the installer polled `GET /disk?mac=…`, any machine on the LAN could post a blank-looking report under a Wyse's MAC, collect an `install`, and the real Wyse would read that answer as its own. With tickets, each report gets its own question and its own answer, and the installer only ever reads the answer to *its* report. It also refuses `install` for a disk it saw was used ("that answer wasn't for this disk"), so only the word `wipe` lets it write over anything.

That doesn't make the network trusted. A machine can still lie about its own disk, but all it achieves is wiping itself, which it could do anyway. And a forged report under someone else's MAC shows up as a second question about the same machine, which an operator who's paying attention will notice. The `--mac` allow-list has the same limit: it trusts what the switch tells it.

A "no" means no for the rest of the session. `DiskSession` remembers the machine, its next `boot.ipxe` is an `exit 1` that sends it to its own disk, and its entry in `netboot-installed.json` (written when it downloaded the installer) is removed again, so the next `relish netboot` run asks afresh instead of treating it as installed. The installer prints why on the console and powers off.

#### Asking from async code

The HTTP handler can't block waiting for a human, and several machines may report at once. So each report spawns a task that sends a `Question` down a channel and awaits its answer:

```rust
let (reply, answer) = oneshot::channel();
let question = Question { machine, summary, deadline: Instant::now() + state.question_timeout, reply };
let answer = if operator.send(question).is_ok() {
    answer.await.unwrap_or(Answer::NoAnswer)
} else {
    Answer::NoAnswer
};
```

A `oneshot` channel carries exactly one value, from one sender to one receiver: here, the answer back to the task that asked. Moving `reply` into the `Question` moves the right to answer along with it, so whoever ends up holding the question is the only one who can answer it. If that side drops it without answering, `answer.await` returns an error and `unwrap_or` turns it into `NoAnswer`, which declines. A bug that loses a question can't wipe a disk.

One task, `ask_operator`, reads questions off that channel and asks them one at a time, so ten Wyses booting together become ten prompts in a row, not ten prompts fighting over one line of input. It waits with `tokio::time::timeout_at(question.deadline, lines.recv())`. The deadline is counted from the report, not from when the question appears, so a question stuck in the queue behind a slow operator still runs out on time. Before each question it throws away any lines typed since the last one: an Enter pressed twice shouldn't answer a question nobody has read yet.

stdin is the awkward part. Reading a terminal is a blocking call, and tokio can't cancel a blocking read once it starts, so tokio's own documentation suggests a dedicated thread for interactive input. `relish netboot` starts one `std::thread` that reads lines and sends them into an unbounded `mpsc` channel, and only when `std::io::stdin().is_terminal()` says there's a person there. Run it from a script, or in CI with `< /dev/null`, and there's no operator at all: every used disk that `--wipe` doesn't list is left alone. CI's `relish-netboot-install.sh` now writes a GPT and an ext4 filesystem labelled `ThinOS` onto the VM's disk first and passes `--wipe <the VM's MAC>`, so every lab build installs over a used disk.

## Updating a read-only system

An appliance you can't update is a time bomb, and the whole point of the A/B layout is to make updates boring. The update path has three parts: something stages a new version into the idle slot, systemd-boot tries it a limited number of times, and a health check decides whether it stays.

### Staging: bun checks, sysupdate copies

systemd-sysupdate writes new versions into A/B slots, driven by `.transfer` files. It can download by itself, but it only verifies downloads with GPG, and we already have an Ed25519 trust chain. So the transfers read from a local directory, and something we trust fills that directory first. Now that's bun ([Rolling it across the fleet](#rolling-it-across-the-fleet) below). During the spike it was a shell script, `os-stage`, which fetched the signed `SHA256SUMS`, checked the signature, downloaded the UKI and both `/usr` images, checked their hashes, moved them into root-only `/var/lib/reliaburger/os-staging`, and ran sysupdate. The `/usr` transfer:

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

The `@` patterns are sysupdate's capture groups: `@v` is a version, `@u` a partition UUID. So `reliaburger-os_2026.40.23.usr.<uuid>.raw.zst` in the staging directory becomes a partition labelled `reliaburger_2026.40.23` with that UUID. `Verify=no` is honest here, because whatever staged the files (bun now, `os-stage` then) already verified everything, and `ProtectVersion=%A` stops sysupdate from overwriting the version we're running. `InstancesMax=2` means two slots, so the next version goes into `_empty` the first time and into the older slot after that.

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
After=network-online.target
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

`Before=` is ordering only; `RequiredBy=` makes the target fail if this unit fails. It isn't ordered after `reliaburger.service`, though it once was: a machine with no seed waits for a claim instead of starting bun (see [Becoming a node](#becoming-a-node)), and a unit ordered after bun would wait with it, forever. So the script itself waits while `bun appliance prepare` is still running, counts the claim API answering as healthy, and starts bun's clock only once prepare is done. The script behind it polls bun's `/v1/health` for up to 300 s. On success it prints `reliaburger: bun healthy` to the console, which every boot test in CI and the lab waits for. On failure:

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

Then the bad one. CI can build an image whose bun never starts (a `workflow_dispatch` input drops in a unit override with `ExecStart=/bin/false`), and we staged it on node-05. Three counted boots each ended with `bun not healthy after 300 s` and a reboot. The UKI ran out of tries at `+0-3`, systemd-boot picked the previous version by itself, and node-05 was healthy 7.6 s into that boot and back in the cluster. From the first try to the fallback took about 15 min 40 s, with no hands. Nearly all of that is the 300 s timeout, three times over, which told us where to tune: a counted boot now gets 120 s, since a healthy lab node answers in 8 to 23 s, and an ordinary boot keeps 300 s because nothing falls back from it. The Wyse will tell us whether 120 s is right for slow hardware.

A test you run by hand proves something once. So CI now runs this one on every x86_64 lab build. The run builds a third image, two versions on, whose bun never starts. The override lives in a small tree, `image/tests/broken-bun`, which mkosi adds with a second `--extra-tree`, so this build and the dispatched broken one break bun in exactly the same way. Then `image/tests/os-update.sh --fallback` boots a one-node cluster on the lab build and runs `relish os upgrade` to the broken version, as an operator would. It reuses the update test's harness (the bridge, dnsmasq, the release server, the seeded VM) and differs only in what it waits for. Each boot prints a kernel banner on the serial console, so counting `Linux version` lines says which boot we're in: one before the update, three tries, one back. The test insists on exactly three `bun not healthy after 120 s` lines and no healthy one on the broken version. Then it waits for `relish os status` to say the rollout paused because the node booted the old version instead. The hand-run test never checked that part: bun noticing, on its own, that it isn't where it was sent.

The first green run (7 October, on the train) took 7 min 12 s from `relish os upgrade` to the paused rollout, staging and drain included. The kernel's own clocks show where it went. Each of the three tries reached `bun not healthy after 120 s` about 129 s into its boot, and the fallback boot had bun healthy after 12 s. So the fallback itself, from the reboot into the broken version to healthy on the old one, was a little under 7 minutes, and 6 of those were the boot check doing its job. That's inside the 10 minutes the Wyse has to show, on a VM that boots in 9 s. A Wyse that needs a minute to boot would spend about 3 more, which is why the hardware run, not this one, decides.

Building the broken version in the same run matters more than it looks. The first runbook staged a broken build from a *second* CI run. Each run signs with its own throwaway key, and a lab fleet trusts only its own run's, so `relish os upgrade` could never reach that build, and `os-stage` had to be handed the other run's public key. Worse, lab versions are `<week>.<run number>`, so a broken run numbered one after the lab build carried the same version as the lab build's next one. Two runs, two keys, one collision. One run with three versions (the build, the next one, and the broken one, all under one key) has none of that, and the Wyse lab now stages the very version CI tested.

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

We later ran both directions with real releases. An image carrying bun 0.1.1, updated to a newer image carrying 0.1.0, kept running 0.1.1, exactly as the launcher promises. And `relish upgrade` from 0.1.0 to 0.1.1 was refused, as Chapter 14's compatibility rules say it must be: the state format moved from 44 to 46 between them, so the run paused on the first node and no node moved. A rolling bun upgrade that actually swaps a binary on the appliance waits for two releases that share their formats, and so far every release has changed them.

### Rolling it across the fleet

Updating one node is a script. Updating a cluster is a little distributed system of its own, and Chapter 14 already built one: the rolling bun upgrade, where the leader walks the nodes and keeps the walk in Raft so a leader change is a resume, not a restart. The OS rollout copies its shape but not its code, because the two differ in what matters. A bun swap takes a second and keeps the containers running; an OS update reboots the machine, so the node's workloads have to go somewhere first. And a bun upgrade's progress is a version number, while an OS update can come back on the *old* version, on purpose, because systemd-boot fell back.

The rollout is one record in Raft, `DesiredState::os_rollout`, holding the target, each node's phase and when it entered it:

```rust
pub enum OsNodePhase {
    Pending,
    Draining,
    Updating,
    Done,
    Skipped,
    Failed { reason: String },
}
```

The leader runs a loop every five seconds, and the loop calls one pure function, `step`, which looks at the record, asks the nodes what they're doing through a trait, and returns the next record. Pure apart from the trait, it's tested like `next_step` earlier in this chapter: a table of situations and what should come out. One node at a time, workers first, council members only while the council can lose a voter and keep its majority, and the leader last.

`Draining` is the new part. While a node is draining or updating, the scheduler's cache marks it not ready, exactly as Chapter 14's upgrade cordon does, so on its next pass the scheduler moves that node's replicas elsewhere. The leader waits until no movable replica is placed there, or five minutes, whichever comes first. Replicas of apps with a managed volume don't move: their data is on that disk, so they wait for the node, as they would through any reboot.

Then the leader posts the directive, and the node takes over. It fetches the release's `SHA256SUMS` and signature from the GitHub release named after the version, checks the signature, streams each file to disk while hashing it, and hands the directory to sysupdate, exactly as `os-stage` did. Which key does it check against? The release keys compiled into bun, *and* the key in `/usr/lib/reliaburger/os-signing-key.pub.pem` in its own image. dm-verity guards that file like every other byte of `/usr`, so an image vouches for its successors. A published image carries the release key; a CI lab build carries its own throwaway key, which is what lets CI test a real update between two lab builds.

Before it reboots, the node writes what it's doing to `/var/lib/reliaburger/os-update.json`: `{"state": "rebooting", "target": "2026.42.0"}`. That one file answers the hard question after the reboot. If the node comes up on 2026.42.0, the update worked. If it comes up on anything else, it was a fallback, and bun says so in `/v1/version`:

```rust
pub fn after_boot(saved: OsUpdateState, running: Option<&str>) -> OsUpdateState {
    match saved {
        OsUpdateState::Rebooting { target } if running == Some(target.as_str()) => {
            OsUpdateState::Idle
        }
        OsUpdateState::Rebooting { target } => OsUpdateState::Failed {
            reason: format!(
                "booted {} instead: {target} failed its boot checks, so systemd-boot fell back",
                running.unwrap_or("an unknown version")
            ),
            target,
        },
        // ...
    }
}
```

The match guard (`if running == Some(target.as_str())`) is a condition on an arm: the first arm matches a `Rebooting` state only when the running version is its target, and every other `Rebooting` falls through to the second arm. `Option<&str>` compares with `Some(...)` directly, so there's no unwrapping to get wrong. The leader reads that failure, marks the node failed and pauses the rollout, rather than waiting for a timeout to guess. A paused rollout keeps its record, and `relish os resume` starts the failed node again under a new run id. That's the same trick Chapter 14's resume uses so the state machine can tell a resume from a second, concurrent start.

Two rollouts that restart nodes must never overlap, and neither must a rollout and a bun upgrade. Checking in the handler and then writing is a race, which Chapter 14 learned the hard way (M13), so the rule lives where writes are serialised, in the Raft state machine: an OS rollout write is ignored while a bun upgrade is active, and the other way round. Raft can't return an error from `apply`, so the start handler reads the record back after writing it, and if it isn't there, something else won.

CI runs the whole thing. A lab build makes the image twice, one version apart, and signs both with its throwaway key. It also writes a lab `os-channel.json` naming the second version, with the same `os_release.py` code and the same canonical bytes as the published channel, signed with that throwaway key instead of the release key. One node boots the first version and forms a cluster from its seed, the runner serves the second laid out like a GitHub release beside that channel, and `relish os upgrade` has to end with the node healthy on the new version. The run uploads exactly that tree as `appliance-x86_64-next`, so the Wyse lab updates from what CI tested.

The lab channel is a stand-in, not a back door. relish checks a channel against the release keys compiled into it and nothing else, so it refuses a lab channel. At first that meant the lab had to name the version, `relish os upgrade <version>`, which never reads the channel, while `relish os list` called the newest release unknown. That's a different path from the one a user takes, in the one test that's meant to look like a user. So `relish image download`, `relish os list` and `relish os upgrade` grew a `--key <PEM>` option: check the channel against this public key instead of the release keys. The run already puts the key's public half beside the channel, so the lab passes `--key next/lab-signing-key.pub.pem` and then runs exactly what a user runs.

How do you add a "trust this other key" switch without building a back door? We settled on four rules. The default doesn't move: no `--key`, release keys. `--key` replaces the release keys rather than adding to them, so a lab command reads only its own build's channel and can't quietly accept something else. relish says so out loud: a warning on stderr every time, and the line that reports the check names the key file instead of saying "the release key". And it stays on the operator's laptop. It changes what one relish command believes, for one run, and nothing else: no config file, no API, no Raft entry.

That last rule matters because of the nodes. They never read the channel. The leader passes the version and the channel's URL, and each node fetches that version's `SHA256SUMS` from beside the channel and checks it against the release keys plus the key its own image carries (`os::slot`). A lab image carries its run's throwaway key in `/usr`, under dm-verity like the rest of `/usr`, so the trust root for a lab fleet is fixed when the image is built. We could have let `relish os upgrade --key` send the key to the nodes too. That's a remote way to swap a running cluster's trust root, which is the one thing an attacker with an operator's credentials would most like to have. So it doesn't.

All three commands share one small type in `src/relish/image.rs`:

```rust
#[derive(Debug, Clone)]
pub struct ChannelTrust {
    keys: Vec<PublicKey>,
    /// The `--key` file, when there is one.
    operator_key: Option<PathBuf>,
}
```

`ChannelTrust::from_key_file(None)` gives the release keys, and `from_key_file(Some(path))` reads and parses one Ed25519 PEM key. The fields are private, so nothing outside the module can push a key in some other way. `verify` wraps `OsChannel::verified` and makes its errors say which key failed. Without `--key`, a failed check adds a pointer to `--key`, since a lab channel is by far the likeliest reason. With it, the error names the file. Rust's `match` on a tuple, `(&error, &self.operator_key)`, picks the message for each pair of cases, and the compiler makes sure no pair is left out.

`relish os upgrade` takes `--key` only when it reads the channel, which is when you don't name a version. clap's `conflicts_with = "version"` turns the other case into an argument error, so a key that would silently do nothing is refused instead. The tests in `src/relish/image.rs` check the lab channel from `os_release.py` against each kind of trust: refused by the release keys with a pointer to `--key`, accepted with its run's key, refused with any other key, and the release keys gone once `--key` is given. CI's OS update test now runs `relish os list --key` and expects the next version to be the newest, then `relish os upgrade --key` without naming it. A test in `src/os/channel.rs` still pins down the channel itself: it passes every rule a published channel does with its run's key, and fails the signature check against the release keys.

### Keeping /etc in step

Remember the cost we put off at the start, when `/etc` went onto the data partition? An OS update writes a new `/usr` and a new UKI, and that's all it writes. So a node installed from one image keeps that image's `/etc` forever: new CA certificates, a changed linker path or a unit some later image enables never arrive. That's fine for a spike and wrong for a product.

Debian solved this decades ago for packages, and its rule is worth stealing. dpkg calls the files in `/etc` *conffiles*, and on an upgrade it asks one question per file: did the administrator change it? If not, the new version replaces it. If so, the administrator's version stays. To answer that question you need a record of what you shipped last time, so dpkg keeps a hash of every conffile it installed.

We do the same, with two pieces. The build copies the finished `/etc` into `/usr/share/factory/etc`, so every image carries a pristine copy of its own `/etc` inside the read-only, verity-checked `/usr`. That's a four-line `mkosi.finalize` script: finalize scripts run after every other step has written to `/etc`, presets included, so the copy is the real thing. Then, on the first boot of each new version, `reliaburger-etc-sync.service` compares three things for every path: what the new image ships, what's on the node now, and what the last sync installed (kept in `/var/lib/reliaburger/etc-factory`). A file that still matches what we installed follows the new image. A file the node changed stays, and the boot log names it. A file the new image dropped goes, unless the node changed it. And the node's own files (its identity under `/etc/reliaburger`, `passwd` and `shadow`, SSH host keys, `machine-id`) are never compared at all.

There's a small trap in enabling the service. A preset would put its `WantedBy=` symlink in `/etc`, and `/etc` is exactly the thing an old node never gets from a new image. So the symlink ships in `/usr/lib/systemd/system/sysinit.target.wants/` instead, where systemd looks too, and where an OS update does write.

The first version hashed each file with its own `sha256sum` and read its mode with its own `stat`: two processes per file, for about 480 files. On the aarch64 boot test, which runs under emulation in CI, that held up boot by 90 seconds. The fix hashes the whole tree in one go:

```bash
scan() {
    local -n into=$2
    local -A hash=()
    local line type mode target rel
    while IFS= read -r -d '' line; do
        hash[${line#*  }]=${line%% *}
    done < <(cd "$1" && find . -type f -printf '%P\0' | xargs -0 -r sha256sum -z --)
    while IFS=$'\037' read -r -d '' type mode rel target; do
        case $type in
            l) into[$rel]="l $target" ;;
            f) into[$rel]="f ${hash[$rel]} $mode" ;;
        esac
    done < <(cd "$1" && find . ! -type d -printf '%y\037%m\037%P\037%l\0')
}
```

If you mostly write Python, read `local -A hash` as `hash = {}` (a bash associative array), and `local -n into=$2` as "`into` is another name for the caller's variable whose name is in `$2`", which is how bash passes a dictionary by reference. `< <(...)` feeds the loop from a command, and every name travels between NUL bytes (`-print0`, `-z`, `read -d ''`), the only byte a path can't contain. That's the same reason Rust's `std::ffi::OsStr` exists: a file name isn't text until you check. The `\037` is the old ASCII unit separator, and it isn't decoration. Our first try split fields on tabs, and `read` treats a tab as whitespace, collapsing the empty link target of every regular file and shifting the name into the wrong field. Nine tests in `image/tests/test_etc_sync.py` run the real script against temporary directories, and they caught it before any machine did. A full `/etc` of 1,439 files now syncs in 0.2 s, and the emulated boot reaches it at 10 s.

## Becoming a node

A freshly installed appliance knows nothing. It has bun, an empty data partition and a network card. Before bun can start, the machine needs a `node.toml`, a certificate signed by the cluster's CA and the cluster's master key. Where do they come from on a box with no login?

From a *seed*: a small tarball with a `seed.toml` in it. Node 1's seed is a *create* seed, and carries the cluster's freshly made keys. Every other machine's is a *join* seed, and carries only a join token: single-use, bound to that machine's node name, and good for a week at most. `relish cluster create --bare-metal` makes all of them on the operator's laptop, so the cluster's keys are born there and nowhere else.

The create seed carries one more thing: the council size. An appliance cluster grows its council to five voters rather than the usual seven (`--council-size` changes it), because the lab it was built for is ten 2 GB Wyses, and two more voters would buy one more tolerated failure at the price of two more machines doing council work. Node 1 commits the size to the council when it bootstraps, so every later leader reads the same number. Chapter 2's "Five voters, not seven" has the reconciler's side.

### Waiting for the backup

Born on the laptop also means the laptop holds the only copy. Lose it before node 1 boots and the cluster never existed; lose it later and `relish council recover`, which needs the master key, has nothing to work with. The first version of `relish cluster create --bare-metal` printed `Back up ~/home-cluster/secrets` between a list of seeds and the relish context, where nobody reads anything. So now it stops and waits. Both commands that make a cluster's keys, `cluster create --bare-metal` and `machines claim --create`, print what to back up and why, then ask `Type yes once it's backed up:` until the answer is yes. Nothing goes out (no stick instructions, no seed over the network) before that.

A prompt that waits has an obvious failure mode: a script, or CI, with no one at the keyboard. Reading stdin there either hangs or reads end-of-file straight away, depending on what's attached. Neither is a good answer, and the first is worse. So the decision is made before anything is created, from two facts:

```rust
pub fn backup_check(yes: bool, terminal: bool, secrets: &Path) -> Result<BackupCheck, RelishError> {
    match (yes, terminal) {
        (true, _) => Ok(BackupCheck::Remind),
        (false, true) => Ok(BackupCheck::Ask),
        (false, false) => Err(RelishError::BackupConfirmationRequired {
            secrets: secrets.to_path_buf(),
        }),
    }
}
```

`--yes` prints the reminder and carries on, and the CI scripts that boot appliance VMs pass it. A terminal without `--yes` gets the question. No terminal and no `--yes` is refused with an error naming `--yes`, before a single key exists, so a script that forgot the flag leaves no half-made cluster behind. `terminal` comes from `std::io::stdin().is_terminal()`, a method of the standard library's `IsTerminal` trait. In Rust a trait's methods are only callable where the trait is in scope, so the caller needs `use std::io::IsTerminal;` first, even though `Stdin` already implements it.

The question itself takes its input and output as parameters rather than reaching for stdin and stdout:

```rust
pub fn confirm_backup(
    input: &mut impl std::io::BufRead,
    output: &mut impl std::io::Write,
    secrets: &Path,
) -> Result<(), RelishError> {
```

`impl BufRead` in argument position means "any type that implements `BufRead`". It's a generic parameter without the angle brackets, and the compiler makes a copy of the function for each type it's called with, much as a C++ template would. The real caller passes `std::io::stdin().lock()`; the tests pass a `std::io::Cursor` over a byte string, so `"\nno\nlater\nyes\n"` checks the loop asks four times, and `""` checks that end-of-file (Ctrl-D, or a pipe that closes) refuses with `BackupNotConfirmed` rather than spinning. `read_line` returns how many bytes it read, and zero is the only way Rust's standard library says end-of-file. Go's `bufio.Reader` returns `io.EOF` as an error instead; Rust treats it as a successful read of nothing.

Three black-box tests in `tests/suite/bare_metal.rs` run the real binary with stdin from `/dev/null`: `cluster create --bare-metal` without `--yes` exits 1 and leaves no directory, with `--yes` it makes the cluster and prints the reminder, and `machines claim --create` without `--yes` fails before it tries to reach the machine. That last one points at an address in TEST-NET-1, where nothing answers, so a check in the wrong place shows up as a test that fails or hangs, not one that passes.

### A state machine that can lose power

`bun appliance prepare` turns a seed into a node. It runs at every boot until there's a `node.toml`, and a machine can lose power at any point along the way. So rather than a script that does five things in order, it's a function that looks at what's on disk and says what to do next:

```rust
pub fn next_step(seed: Option<&Seed>, disk: Disk) -> Step {
    if disk.node_toml {
        return Step::Done;
    }
    match seed {
        None => Step::NoSeed,
        Some(Seed::Legacy { .. }) => Step::InstallLegacy,
        Some(Seed::V1 { config, .. }) => match (config.role, disk.identity, disk.master_key) {
            (SeedRole::Create, true, true) => Step::WriteConfig,
            (SeedRole::Create, _, _) => Step::InstallBootstrap,
            (SeedRole::Join, false, _) => Step::Enrol,
            (SeedRole::Join, true, false) => Step::FetchMasterKey,
            (SeedRole::Join, true, true) => Step::WriteConfig,
        },
    }
}
```

Matching on a tuple is one of Rust's quiet pleasures. The compiler checks that the arms cover every combination of role and two booleans, so a case nobody thought of is a compile error rather than a machine stuck at boot. `_` matches anything, and `Seed::Legacy { .. }` matches that variant whatever its fields hold. The loop around it does the step, then asks again, until the answer is `Done`. Pull the plug half-way through enrolling and the next boot simply picks up where the disk says it was. And because `next_step` touches nothing, the tests are a table of inputs and the step each should give.

A join seed's token buys a certificate, and the certificate buys the master key: `GET /v1/cluster/master-key` answers only a caller presenting a node certificate the cluster signed and hasn't retired. The token never sees the key.

### Claiming over the network

Seeds on a USB stick work, but you have to walk the stick round. So a machine that boots with no seed becomes *unclaimed* instead. It makes a self-signed key, shows a short fingerprint of it on its monitor, announces itself over mDNS as `_rb-unclaimed._tcp`, and serves a tiny API on port 9119: `GET /v1/claim` says what it is, and `POST /v1/claim` with a seed claims it, once. From there it carries on exactly as if the seed had come on a stick.

The interesting part is trust. Anyone on the LAN can answer mDNS, and node 1's seed carries the cluster's keys. A certificate authority can't help, because the machine made its key thirty seconds ago and nobody has signed it. What we can do is what SSH does the first time you connect: show the key's fingerprint and ask a human to compare it with the machine's console. relish fetches the machine's details, prints `claim key 3f9a-12bc-77de-0a41`, and asks whether the monitor shows the same. If it does, relish posts the seed over a connection that accepts that one certificate and nothing else.

Doing that in Rust means writing our own rustls certificate verifier. rustls describes one as a trait, `ServerCertVerifier`, and calls its methods during the handshake:

```rust
impl ServerCertVerifier for ClaimKeyVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        /* ... */
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fingerprint = certificate_fingerprint(end_entity);
        if let Ok(mut seen) = self.seen.lock() {
            *seen = Some(fingerprint.clone());
        }
        match &self.expected {
            Some(expected) if *expected != fingerprint => Err(rustls::Error::General(format!(
                "the machine's claim key is {fingerprint}, not {expected}"
            ))),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }
    // ...the signature checks delegate to rustls's own.
}
```

The `'_` in `CertificateDer<'_>` is an elided lifetime: the certificate borrows bytes from the handshake, and `'_` says "some lifetime the compiler can work out", without our naming it. The verifier ignores the server name and the CA chain completely. The SHA-256 of the certificate is the whole identity. With `expected` unset, on the first look, it accepts anything but records what it saw in `seen`, an `Arc<Mutex<Option<String>>>` that relish reads once the request is done. That's a `std::sync::Mutex`, not tokio's, which [CLAUDE.md](https://github.com/reliaburger/reliaburger/blob/main/CLAUDE.md) normally frowns on. Here it's right: rustls calls the verifier synchronously, and the lock is held only long enough to store one string. Nothing awaits while holding it.

Notice what the verifier doesn't skip. The certificate must still match the key that signed the handshake, so a machine that merely *copies* another's certificate can't finish connecting. The test for all this runs the real claim server on loopback: a post pinned to the wrong fingerprint never connects and the seed never lands, then the right one claims the machine.

### A window in the firewall

One more problem. Each node's perimeter firewall lets cluster ports through only from known nodes, the operator, and the bootstrap peers in `node.toml`. A machine claimed into a running cluster is none of those yet, so its enrolment request is dropped before it can show its token. Editing every node's `node.toml` for each new machine defeats the point of claiming.

So `relish machines claim` first asks each node to open a *join window*: `POST /v1/perimeter/admit` with an address and up to 60 minutes. The agent keeps the windows in a map from address to closing time, and the firewall loop, which already rebuilt the ruleset whenever gossip membership changed, now also rebuilds it when the set of open windows changes, adding them to the bootstrap peers. When the window closes, the next tick drops it again. By then the machine has joined, and gossip lets it in for good. The window only lets packets in: enrolment still needs the token, and the cluster ports still need certificates. A stick seed gets no window, because it can sit in a drawer for a week.

## A LAN on a laptop

We had no KVM machine and no spare PCs, only CI and one Apple silicon Mac without root. Everything had to run virtualised until the last step.

### CI boots every image

Each CI run boots what it built, with 2 GiB and a 7.25 GiB disk, like the Wyse. For a long time the disk was `truncate -s 8G`, and 8G means 8 GiB, about 1.3 GiB more than a Wyse's 8 GB eMMC really holds. Every test passed on space the hardware doesn't have. Now every QEMU test sources `image/tests/disk.sh`, which sets the size in one place and refuses an image that leaves no room for slot B, saying so. It never lets `truncate` shrink the disk image, which would silently cut it short and make the failure a confusing boot error instead. The pass condition is a line on the serial console, not a process exit code:

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

Forming the cluster needed configuration, and the appliance has no login. The spike passes it as a systemd credential: a small blob that the firmware hands to PID 1, here through QEMU's `-smbios type=11`. `reliaburger-seed.service` picks it up with `ImportCredential=reliaburger.seed` and unpacks a tarball of `node.toml`, the master key and the node's identity into `/etc/reliaburger`, once. On real machines, a script called `apply-seed` looked for a USB stick labelled `RBSEED` instead (bun does this now, as [Becoming a node](#becoming-a-node) describes). Node 1 was healthy 7.7 s into its seeded boot, the four joiners (enrolled with `relish join` from the server VM) at 17 to 23 s, and all five became council voters.

Then the tour, the manual's five-minute walk through Reliaburger (`relish manual tour`): apply podinfo, ingress on two nodes, `relish path` through the eBPF service map, a 300 ms netem delay that `path` duly measured at 300 ms, `fault kill` and the restart, and a node "powered off" by killing its QEMU (four of four left alive, council healthy, replicas rescheduled). Power it back on and it rejoined: five of five, and `relish wtf` showed 12 OK and no warnings. Only `relish dashboard` was skipped, because it opens a browser and `relish` was running headless on the server VM.

### The virtual Wyse

The Wyse 3040's Atom has SSE4.2 and AES-NI but no AVX. The virtual Wyse is x86_64 under TCG with `-cpu Westmere`, which matches that, so any accidental AVX dependency in our binaries shows up before the hardware stage. It goes through the whole real path: OVMF's firmware PXE, the ProxyDHCP, our iPXE, the installer. It installed in 29 s, and the same QEMU process then booted the disk and reached `bun healthy` at 62 s kernel time. No AVX problems.

### Taking the scaffolding down

Before relish could do any of this, shell scripts did. `image/tools/` held the bare-metal preview: `netboot-server.sh` glued dnsmasq to Python's `http.server`, `seed-fleet.sh` packed seeds, `node-toml.py` wrote each node's config, and `seed-admin`, a separate little Cargo project, hashed an admin token into the security bootstrap. Its first build took eight minutes. The lab had its own copies too: `seed-node1.sh` and `seed-joiner.sh`, which enrolled each joiner by hand from the server VM.

Scaffolding is useful right up to the moment it disagrees with the building. Every one of those scripts now has a relish command that does the job and is tested in CI: `relish netboot` serves, `relish cluster create --bare-metal` and `relish image seed` write seeds, and `relish machines claim` sends them over the network. Two ways of doing the same thing would drift, and people would follow whichever one they found first. So the scripts went. The lab now forms its cluster with `seed-lab.sh`, which is little more than a `relish cluster create --bare-metal` naming each VM's MAC and reserved address. The joiners carry join tokens and enrol by themselves on their first boot, just as a Wyse does.

Three things stayed, each for a reason we could write down:
- `make-seed-stick.sh` turns the `stick/` directory relish writes into a FAT disk image for QEMU. relish writes the seeds, not filesystems, and on a real machine you'd just copy the folder onto a stick.
- `fleet-measure.sh` samples memory, zram, eMMC writes and temperature over SSH for the Wyse lab's 24-hour run. No relish command reads those numbers yet, so it moved into `image/lab/`, without the code that read `seed-fleet.sh`'s old fleet list.
- `os-stage` stages an OS update by hand. bun does it for real now, but the Wyse runbook's fallback test deliberately stages a broken image underneath bun, and that needs a tool that isn't bun.

One idea went without ever being built: `relish netboot --node`, a node serving the netboot for the rest of the fleet. The lab doesn't need it, and a feature nobody needs is a feature nobody tests.

Deleting code is easy; keeping it deleted takes a test. `image/tests/test_retired_tools.py` walks the repository and fails if anything but the history (this book, the plans and the qualification records) still names a retired script, so a stale README line or a lab script reaching for `image/tools/` breaks the build rather than a lab day.

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

### Fifteen bytes of service name

The claimed-pair test in CI failed every run it reached. Both VMs booted, both printed their claim keys, and `relish machines` never listed either. We first suspected the usual multicast suspects: the runner's route for 224.0.0.251, bridge snooping, a firewall. The cause was in our own constant. RFC 6763 caps a DNS-SD service name at 15 bytes, and we'd called ours `_reliaburger-unclaimed`, which is 21. mdns-sd checks that limit inside its daemon thread, after `register` has already returned `Ok`, so the machine announced nothing and said nothing about it. It's now `_rb-unclaimed._tcp`. Two tests guard it: one checks the length against mdns-sd's own limit, and one announces a machine and finds it with `relish machines`' browse, over this host's real interfaces. The second would have caught it on day one. A unit test of the parts proves little when the bug lives in the handshake between them.

The netboot demo found the next one, the day after. Three machines netbooted, installed and announced themselves, and `relish machines` listed all three, each with an address beginning `fe80::`. Claiming them by MAC then failed. Those are IPv6 link-local addresses, which every interface has from the moment it comes up. They're only reachable through a named interface (`fe80::1%eth0`), and mDNS doesn't say which one. The machines had IPv4 addresses too, from the router, a few seconds later. But the announcement had registered with mdns-sd's `enable_addr_auto()` before DHCP finished, and the IPv4 address never made it into what `relish machines` heard. The claimed-pair test missed it, because it checked only that the MACs were listed, then claimed by IP.

Three changes fix it. The machine waits, up to a minute, for an IPv4 address on its default-route interface (`appliance::address::detect`, the same function bun uses for its own address), and puts it in the announcement from the first packet. The claim API doesn't wait for this: it serves at once, and the announcement runs in a task of its own. Waiting for the address only delays the announcement. relish now merges the addresses from every answer a machine sends, instead of keeping the first, and `reachable_address` never picks a link-local IPv6 address to connect to. A machine that announces nothing else gets an error that says to give its IPv4 address. And claimed-pair now claims its first machine by MAC, and insists each listed MAC shows its IPv4 address.

### A binary that can't write next to itself

We covered bun's version above. The lesson is broader: anything that assumes it can write next to its own executable breaks on an immutable OS, and it breaks late, at upgrade time, long after the first boot looked fine.

### Reading Sidero's Omni

We read Sidero Labs' Omni bare-metal provider (`siderolabs/omni-infra-provider-bare-metal`) for ideas only, and copied no code. It encodes few vendor quirks. Its main defence against boot loops is server-side state plus one-shot PXE through a BMC, which a Wyse doesn't have. Four ideas did transfer. The installer puts the disk first in the boot order, and sends machines that are already installed back to their disk. `embed.ipxe` gives up after about 30 s. CI builds iPXE's `snp.efi`, which drives the NIC through the firmware's own driver, and makes it the default. And `boot.ipxe` works out its own server, for firmware that has iPXE built in and skips our embedded script. The rest went into notes for `relish netboot`.

## What's next

The spike's interim record is a pass for everything that can be proven without hardware. Turning it into part of Reliaburger is the [product plan](../plans/2026-10-01-plan-appliance-product.md) for 0.3.0, and these are the big pieces:

- **S5, three Dell Wyse 3040s.** The lab has ten, but on 7 October 2026 we decided the release gate needs only three. The go/no-go is 3 of 3 claimed, a working cluster within an hour of power-on, 1.0 GB free per node under the tour, five years of eMMC life, and a fallback within ten minutes. The run covers BIOS setup, PXE on the real Realtek NIC, whether the firmware keeps the boot entry the installer makes, `MemAvailable` under the tour, eMMC writes per day, one OS update across the fleet, and whether Linux 7.0 still hangs on reboot without our `dw_dmac` blacklist. A node that can't reboot can't finish an A/B update, so that last one matters more than it sounds. The run uses only the real commands: `relish netboot` on an M2 MacBook over a USB-C Ethernet adapter, either the home router or the Mac itself for addresses, and `relish machines claim` for the cluster ([the S5 runbook](../plans/2026-10-01-plan-appliance-s5-wyse.md)). We first planned a Raspberry Pi as the lab's own router, to keep the lab off the home network, and dropped it on 8 October 2026 for two setups that need no extra box. On the home network, the rails relish already has protect the rest of the LAN: `--mac`, the yes before a used disk is wiped, and the refusal to start beside another boot server. Isolated, the Mac runs dnsmasq and relish answers on port 4011 beside it ("A lab with no router", above). The Pi will come back as an arm64 worker, for a cluster that mixes architectures.
- **`relish netboot` on real firmware.** It's written and CI installs through it, but never yet on a Wyse, and never with a Mac serving real hardware rather than a VM.
- **Smaller things with known answers:** dropping the lab's credential-gated SSH, now that bun stages OS updates itself; and a way for the installer to find its server under Secure Boot, which drops the command line iPXE passes.

The shape is clear, though. The operating system is now one more artefact that Reliaburger builds, signs, rolls out and rolls back, like bun. It just happens to be the one bun stands on.
