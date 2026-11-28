# Appliance: netboot, claim and `relish os`

**Component:** the appliance OS, `relish netboot`, `relish machines claim`, `relish os`
**Status:** Shipped in 0.3.0 (#401–#406); the Wyse exit test (#407) runs alongside the release
**Reconciled against:** not reconciled
**Sketches:** §5 (what's deferred)

---

## 1. Overview

0.3.0 turns a bare x86_64 or arm64 machine into a node with no distro install
and no login. Three pieces do it, and this document covers each one's
protocol, state and security model:

- **`relish netboot`** answers a machine's network boot, hands it a signed
  installer, and writes the appliance image to its disk. Wiping a disk that
  isn't blank needs an operator's yes (§2).
- **Claiming** joins an installed machine to a cluster over the LAN. The
  machine shows a claim key on its console, and `relish machines claim` sends
  it a seed over TLS pinned to that key (§3). The other way in is a seed
  carried on an `RBSEED` USB stick (§3.6).
- **`relish os`** updates the appliance OS across the cluster, one node at a
  time, onto the inactive half of an A/B `/usr`. If a node doesn't come back
  healthy, systemd-boot falls back to the old image (§4).

Users read [manual chapter 17](../manual/17_appliance.md) and the book's
[Becoming the OS](../book/15a-becoming-the-os.md). The CI lab that runs all
of this against QEMU is `image/lab/` and `.github/workflows/appliance.yml`.

### 1.1 The image

The image is built with mkosi from Ubuntu 26.04 (`image/mkosi.conf`):
systemd-boot, an unsigned unified kernel image (UKI), and the partitions in
`image/mkosi.repart/`:

| Partition | Format | Size | Notes |
|---|---|---|---|
| ESP | vfat | 512M | systemd-boot and the UKIs |
| `/usr` slot A | EROFS, dm-verity data | 1100M | label `reliaburger_<version>`; its root hash is the UKI's `usrhash=` |
| `/usr` verity A | verity hash | 64M | |
| `reliaburger-data` | btrfs | grows to fill the disk | `/etc`, `/var`, `/var/lib/reliaburger` |

On first boot `systemd-repart` grows the data partition and adds an empty
`/usr` slot B and its verity partition (`/usr/lib/repart.d/50-usr-b.conf`,
`51-usr-verity-b.conf`). `/usr` is read-only and verified block by block, so
everything a node changes lives on the data partition. That's also why an OS
update never touches the cluster's state (§4.4).

The image has Buildah, so every appliance node can run `relish build`
(#2144). Its storage is `<storage.data>/buildah` on the data partition, where
the `vfs` driver's layer copies are Btrfs reflinks, so a layer costs metadata
rather than a second copy of its parent's blocks on the eMMC. The appliance
profile keeps a 1 GiB build cache (`src/appliance/seed.rs`). mkosi installs no
Recommends, so `mkosi.conf` names the two Buildah needs: `netavark`, the
network stack for `RUN` steps, and `gpg`, which satisfies libgpgme's
`gnupg | gpg` without GnuPG's agents. `RUN` steps use the image's runc. The
package's test helpers in `/usr/libexec/buildah` (97 MB of the package's
125 MB) are removed. Buildah adds 54 MB to each `/usr` slot: on x86-64 the
EROFS image went from 667 MB to 721 MB of the slot's 1,100 MB. `image/tests/builder_packages.py` checks the
config and each built manifest, and CI's seeded pair builds the tour's burger
on two appliances. With no node able to build, bun still refuses with
`NO_BUILDER_ERROR` (`src/bun/build_runner.rs`), which names the ways out.

Lab builds add `openssh-server` for debugging. Published images don't have
it, and `image/tests/no_ssh_server.py` checks it twice. Its unit test reads
the mkosi configs, where only a `Profiles=lab` drop-in may name an SSH
server. In CI it reads each built image's manifest, where a published build
must list none and a lab build must list `openssh-server`.

## 2. `relish netboot`

### 2.1 What it serves

`relish netboot` is three small servers inside relish (`src/relish/netboot/`).
Nothing is installed on the serving machine:

- **ProxyDHCP** on UDP 67 and 4011 (`dhcp.rs`). It never hands out an
  address: `yiaddr` is always `0.0.0.0`, so the router stays the DHCP server
  and netboot only adds boot options. It answers only clients whose vendor
  class starts with `PXEClient` or `HTTPClient`. A Discover on 67 gets an
  Offer, a Request on 67 gets an Ack only if option 54 names this server, and
  a Request on 4011 always gets an Ack. With `--mode-dhcp-proxy` it listens on
  4011 only and leaves 67 to a local dnsmasq. That's the Mac-as-router lab
  setup.
- **TFTP**, read-only, on UDP 69 (`tftp.rs`). It supports blksize (up to 1468),
  tsize and timeout, and serves the iPXE builds and a one-line `boot.ipxe`.
- **HTTP** on `--http-port` (default 8080, `http.rs`). Routes are
  `/boot.ipxe`, `/{arch}/{file}`, `POST /disk` and `GET /disk/{ticket}`.

UDP 67, 69 and 4011 need root or `CAP_NET_BIND_SERVICE`. The server runs for
`--for` (default an hour) or until Ctrl-C.

### 2.2 What it serves only after checking

At start, `artefacts.rs` takes the newest version in each architecture
directory and checks it before serving anything. It verifies the Ed25519
signature on `reliaburger-os_<version>.SHA256SUMS` against the release keys
compiled into relish, then checks the SHA-256 of every file the sums list
names. The installer, the `.raw.zst` disk image and the SNP iPXE build are
required. iPXE and the installer UKI are held in memory and sent from the
bytes that were hashed: the installer is what checks the disk image's
signature. Other files are served from disk, and one that changed since the
check gets a 409, not stale bytes. `--key` adds a trusted public key, with a
warning. A CI lab build uses it for its throwaway run key.

### 2.3 The boot chain

1. PXE firmware gets `ipxe-<arch>.efi` over TFTP. A UEFI HTTP Boot client
   (architecture 16 or 19, or class `HTTPClient`) gets
   `http://<server>/<arch>/ipxe-snp-<arch>.efi` instead.
2. iPXE (user class `iPXE`) gets `boot.ipxe`. The embedded script prefers the
   ProxyDHCP next-server, retries DHCP three times, and gives up after about
   30 seconds with `exit 1`, so the firmware moves on to the disk.
3. `boot.ipxe` over TFTP chains to the HTTP one, carrying the machine's MAC,
   SMBIOS UUID and architecture.
4. The HTTP `boot.ipxe` answers with either the install script or `exit 1`
   (§2.4). The install script chains the installer UKI with
   `reliaburger.url=`, `reliaburger.ask=http://<server>/disk?mac=…&uuid=…&ticket=…`
   and `reliaburger.done=http://<server>/installed?mac=…&uuid=…&ticket=…`
   (only when the machine sent a MAC or UUID), `reliaburger.reinstall=1` for
   `--reinstall`, and serial and VGA consoles. The ticket is a random install
   ticket issued with the script; the installer's reports carry it back.

UEFI only. A BIOS client is ignored and logged. Secure Boot must be off: iPXE
passes the kernel command line, and with Secure Boot on a UKI ignores it.

### 2.4 Which machines it answers

- `--mac` (repeatable) limits everything, DHCP, `boot.ipxe` and `POST /disk`,
  to the listed machines.
- Each machine it installed goes into `netboot-installed.json` in the served
  directory, written atomically, `{machines: [{mac, uuid, arch,
  installed_at}]}`. A machine is recorded when its installer reports success
  (`POST /installed`) with the install ticket issued to it, after the image
  is written, its hash checked and its boot entry made. Fetching the
  installer records nothing, so a machine whose install failed gets the
  install script again. A disk report (`POST /disk`) needs the ticket too,
  once per ticket. At most 256 tickets are outstanding, each for an hour,
  and a machine holds one: a new ticket cancels its older one.
- A ticket goes only to the address the machine's latest DHCP request (to
  UDP 67 or 4011, from an address of its own) came from, or asked the LAN's
  DHCP server for (option 50 in a broadcast DHCPREQUEST, ciaddr still
  0.0.0.0). iPXE takes its boot file from relish's proxy offer and its
  address from the LAN's server, and that address needn't be the one the
  firmware asked from on 4011: in CI's reinstall the firmware asked from
  `.111` and iPXE fetched `boot.ipxe` from `.110`. A `boot.ipxe` from
  anywhere else gets the install script without a ticket, and relish logs
  that it withheld one. So a host that only speaks HTTP can't mark another
  machine installed, ask a question about its disk, or fill the ticket table.
  This isn't authentication: DHCP isn't authenticated either, so a host that
  forges a DHCP request with another machine's MAC, or spoofs its address
  (which needs ARP spoofing to win the TCP handshake), can still get that
  machine's ticket. The install LAN is trusted for availability; image
  integrity doesn't depend on it, since the installer checks signed sums.
- `--for` doesn't cut an install off: when it's up, relish waits up to 20
  more minutes for installs that were sent the install script and haven't
  reported.
- A recorded machine gets `exit 1` and boots its disk. With `--reinstall` it
  gets the installer once per run.
- A machine whose wipe was declined this run gets a script that boots its
  disk.
- With `--stream-logs` (and in `relish machines logs`), boot log lines from
  a machine relish answered go to `logs/<mac>.log`; lines from any other
  address, which anyone can forge, share `logs/unserved.log`, each prefixed
  with its address. `relish machines logs` answers nobody, so there the
  machine's hello names it instead, for at most 64 machines: a hello can
  name any MAC, and past that strangers share `unserved.log` again. At most 64 files are open at once (the one written
  longest ago is closed, and reopened in append mode if needed), and each
  stops at 64 MiB, after which its lines only print.

### 2.5 Refusing to start beside another boot server

Two boot servers on one LAN would race to answer the same machines. So before
serving, netboot broadcasts a PXE DHCPDISCOVER, from a random
locally-administered MAC, and listens for 5 seconds (`server.rs`):

- Another boot server answering means it refuses to start.
- With `--mode-dhcp-proxy`, another DHCP server handing out addresses means
  it refuses too.
- Silence gets a warning, and it serves.
- If the probe can't run, it depends. A permission error is fatal (it needs
  root anyway). Any other failure to bind, send or receive prints "can't check
  the network … make sure no other netboot server runs on it" and carries on
  (`when_the_probe_cannot_run`). That's deliberate. The usual cause is the
  host's own DHCP client holding UDP 68 alone, and refusing then would stop
  netboot on an ordinary Linux laptop, with no fix short of stopping the
  DHCP client.

### 2.6 Asking before a wipe

The installer never wipes a disk that isn't blank on its own. The protocol
(`wipe.rs`, `image/…/ask-to-wipe`):

1. The installer posts `lsblk -nbpPo NAME,TYPE,SIZE,PTTYPE,FSTYPE,LABEL,PARTLABEL`
   for its target disk to `POST /disk?mac=…&uuid=…`. The body is capped at
   64 KiB. The reply is `202` with a 16-hex-digit ticket.
2. It polls `GET /disk/<ticket>` every 2 seconds. The answer is `wait`,
   `install`, `wipe` or `decline`. An unknown ticket is 404.
3. netboot decides:
   - a blank disk gets `install`;
   - a machine listed with `--wipe <MAC>` gets `wipe`;
   - otherwise the operator is asked `wipe? [y/N]` at the netboot terminal,
     and only `y` or `yes` means `wipe`;
   - no terminal, no answer within 10 minutes, or any other answer means
     `decline`.

   Questions come one at a time.
4. On `decline` the machine leaves the installed record, is remembered as
   declined for the run, and powers off after 60 seconds. The installer gives
   up by itself after 15 minutes.

No route takes a decision from the machine. The machine can only report what
it sees, and the answer comes from the operator or from `--wipe`. There's no
"wipe everything" flag: each machine is named.

### 2.7 The installer

The installer is a UKI whose initrd is a small Ubuntu system
(`image/mkosi.images/installer/`). Its `install` script:

1. Fetches `SHA256SUMS` and its signature, and verifies them with the OS
   signing key built into the installer. A CI lab installer carries the run's
   throwaway key instead.
2. Picks the target disk: `reliaburger.disk=`, or the largest fixed, writable,
   non-removable disk.
3. Treats a used disk that already has a `reliaburger-data` partition as
   installed. Unless wipe or reinstall is set, it puts that disk first in the
   boot order, sets `BootNext` and reboots.
4. Otherwise asks to wipe (§2.6), then runs `wipefs -a`.
5. Streams the image with `curl | tee | zstd -dc | dd oflag=direct`, hashing
   the compressed bytes on the way, so a 2 GiB machine installs a 1.7 GB image
   without holding it. A hash mismatch or failed write runs `wipefs` again and
   fails, so a tampered or half-written disk never gets a boot entry.
6. Moves the backup GPT to the end of the disk (`sgdisk -e`).
7. Replaces any old "Reliaburger OS" boot entries with one for the disk's
   systemd-boot, puts it first, and reboots. The removable-media path
   `\EFI\BOOT` stays as a fallback.

### 2.8 Without network boot

`relish image write` copies a downloaded `.raw` or `.raw.zst` image onto a
device from another computer (`src/relish/image.rs`). It needs `--yes`, and
it checks neither the image (`relish image download` did) nor the device. It
makes no firmware boot entry, so the disk boots through `\EFI\BOOT`.

## 3. Claim

### 3.1 An unclaimed machine

At boot, `reliaburger-seed.service` runs `reliaburger start prepare` until
`/etc/reliaburger/node.toml` exists (`src/appliance/prepare.rs`). Prepare
looks for a seed, in order:

1. a claimed seed, `/var/lib/reliaburger/claim/claimed.seed`;
2. the `reliaburger.seed` systemd credential;
3. an `RBSEED` stick (§3.6);
4. the `seed.toml` kept from an earlier boot.

With none, the machine is unclaimed and waits to be claimed
(`src/appliance/claim.rs`):

- **Claim key.** A self-signed certificate (`reliaburger-unclaimed`), kept in
  `/var/lib/reliaburger/claim/` (`claim.key` 0600) and reused until the
  machine is claimed.
- **Fingerprint.** `sha256:` and the hex SHA-256 of the certificate's DER. The
  short form people compare is the first 16 hex digits in four groups,
  `3f9a-12bc-77de-0a41`.
- **Console.** The tty1 status screen redraws every 5 seconds and shows the
  short claim key while the node has no name, with the hint to compare it in
  `relish machines claim`. `reliaburger-console.service` is ordered after
  none of our units: the seed unit stays activating until the claim, and a
  console ordered after it showed nothing until the key no longer mattered
  (#2161). It shares bun's launcher with prepare and bun, which installs
  bun under a lock (`/var/lib/reliaburger/bin/.install.lock`).
- **mDNS.** It announces `_rb-unclaimed._tcp.local.` (RFC 6763 caps the name
  at 15 bytes) as `reliaburger-<mac>`, with TXT `mac`, `arch` and `fp` (the
  short fingerprint). It waits up to 60 seconds for an IPv4 address before
  announcing, so `relish machines` sees a usable address. mDNS is best
  effort; claiming by address doesn't need it.
- **API.** TLS on port 9119 with the claim key, and no client authentication:
  - `GET /v1/claim` returns `{macs, arch, os_version, fingerprint}`.
  - `POST /v1/claim` takes the seed tarball, at most 1 MiB. It answers `200
    claimed`, `400 not a seed`, `409` once the machine is claimed, or `500` if
    saving failed.
  - The first valid seed wins. The server stops a second later.
- **Health.** While it waits, the boot check counts an answer on
  `:9119/v1/claim` as healthy, so an unclaimed machine isn't rolled back.

### 3.2 Claiming

`relish machines` browses mDNS for `--wait` seconds (default 3) and lists
`ADDRESS MAC ARCH CLAIM KEY`. It prefers IPv4, then routable IPv6, and never
link-local. `relish machines claim <DIR> <MAC|IP>…` (`src/relish/machines.rs`):

1. With `--create`, asks the operator to confirm the master key is backed up
   before anything is sent (§3.5).
2. Finds each machine. A MAC must have been seen on mDNS; an IP needn't.
3. Connects to `:9119` without pinning, records the certificate it was shown,
   and requires `GET /v1/claim`'s `fingerprint` to match it, and the short
   form to match what mDNS announced.
4. Asks, per machine: "Does the console of {mac} ({address}) show claim key
   {short}? [y/N]". `--trust-lan` skips the question. Without a terminal and
   without `--trust-lan` it refuses.
5. Builds the seeds. `--create` makes a new cluster from the machines, with
   the first as node 1 and a five-member council by default. Otherwise it
   joins the cluster in DIR: it opens join windows (§3.4), then mints a
   single-use join token per machine, bound to its node name, valid for
   `--ttl` (default an hour).
6. Posts each seed over TLS pinned to the full fingerprint, with a 15-second
   timeout.

### 3.3 The security model

The claim protocol trusts the LAN to deliver packets, not to be honest:

- **The claim key ties the network to the machine.** Anyone on the LAN can
  answer mDNS or port 9119. The operator compares the console's short key
  with the one relish shows, and relish then sends the seed only over TLS
  pinned to that key's full fingerprint. A machine in the middle would need a
  certificate whose 64-bit fingerprint prefix matches what the real console
  shows. `--trust-lan` gives that check up, and is for labs and CI.
- **The seed is the secret.** A join seed holds a single-use token bound to
  one node name. A create seed holds the master key (§3.5). Neither ever holds
  the root CA key: the root stays offline (#709).
- **Claim once.** A claimed machine stops serving 9119, and its seed is zeroed
  and removed once `node.toml` is in place (§3.5). A machine can't be claimed
  away from a cluster later.
- **The token never sees the master key.** A joiner enrols with its token
  through each member in turn, at `https://<member>:9117`, pinned to the CA
  fingerprint the seed names. It then fetches the master key from `GET
  /v1/cluster/master-key`, which answers only a caller presenting a node
  certificate that chains to the cluster's Node CA, isn't revoked and names a
  node that isn't retired. Like certificate renewal, only a member that
  confirms it leads answers, checking against the security state it has
  confirmed with a quorum; a follower answers 503, and the joiner asks each
  member in turn until the leader does.

### 3.4 Join windows

A node's perimeter firewall lets cluster ports through only from members,
operators and `[security] bootstrap_peers`. A machine being claimed into a
running cluster is none of those. `POST /v1/perimeter/admit {"address",
"minutes"}` opens a join window (G2):

- It needs an admin, unscoped token, and cluster admin permission under a
  `[permission]` block. It's audited as `perimeter.admitted`.
- It allows 1 to 60 minutes. relish asks for 15 on every node it can reach,
  and skips one that's down with a warning.
- While the window is open, the address counts as a bootstrap peer:
  TCP 9117 and 9443–9445, and UDP 9443–9445. Container host ports stay shut.
  Enrolment still needs the join token, and every cluster port is mTLS.

`relish cluster add --bare-metal` and `relish image seed` write stick seeds
and open no window. A machine outside the network the cluster admits
(`cluster create --network`) needs `bootstrap_peers`, and they warn about it.

### 3.5 Seeds and the prepare state machine

A seed is a gzipped tar of at most 1 MiB, with safe relative paths only
(`src/appliance/seed.rs`). Unpacked, it holds at most 64 entries and 4 MiB
of files, each at most 1 MiB; the parser adds up the sizes the tar headers
state and refuses a seed past either limit before reading the entry that
crosses it, so a body that gzip shrank a thousand to one can't fill memory. `seed.toml` (schema 1) names the `cluster`, the
`node`, the `role` (`create` or `join`), `advertise`, `peers` and
`operators`. A create seed may also set `council_size` and `faults =
"development"`. A join seed has `[join] members, ca_fingerprint, token`.
Either may carry `[upgrades] external_signing_key`.

- **A create seed** must carry `master.key`, `security-bootstrap.json` and
  `identity/bundle.committed`, and no `[join]`.
- **A join seed** must carry no `master.key` and no `council_size`.
- **Either** may carry `authorized_keys`, which only a lab image's SSH uses.

Prepare is a loop around a pure function, `next_step(seed, disk)`, so a power
cut anywhere just resumes on the next boot:

| Seed | On disk | Step |
|---|---|---|
| any | `node.toml` | Done |
| none | | NoSeed: with a claim directory (the appliance has one), wait for a claim (§3.1) and look again; without one, bun runs standalone |
| spike (`node.toml` in the seed) | | InstallLegacy: install its files as they are, then Done |
| create | no identity or no master key | InstallBootstrap |
| create | identity and master key | WriteConfig |
| join | no identity | Enrol |
| join | identity, no master key | FetchMasterKey |
| join | both | WriteConfig |

A spent, expired or mismatched token, a CA fingerprint mismatch, or an
incompatible cluster ends Enrol. Anything else is retried, backing off from
2 to 30 seconds. WriteConfig writes `node.toml` last, from the seed, with the
appliance profile's paths.

The stick's and the claimed copies of a create seed hold the master key,
which is why prepare zeroes them: when it's done, the claimed seed and the
stick's file are overwritten with zeros, flushed and only then removed. The claimed seed is also cleared on a boot that finds `node.toml`
already in place, in case the power went between the two.

The claimed seed lives in `/var/lib/reliaburger/claim`, on the Btrfs data
partition. Btrfs is copy-on-write, so an overwrite would go to new blocks and
leave the seed in the old ones. Before the claim server starts, prepare makes
the claim directory `NOCOW` (`FS_NOCOW_FL` through `FS_IOC_SETFLAGS`, what
`chattr +C` does). Files created in it inherit the flag while they're empty,
so the zeros reach the seed's own blocks. A filesystem without the flag
(tmpfs, ext4) overwrites in place anyway, and prepare carries on. Two limits
remain: an SSD or eMMC may keep the old data in cells its wear levelling
moved away from, and nothing above the device can reach those. The real
protection is that the seed lives on the disk only from the claim to the end
of prepare.

### 3.6 The RBSEED stick

Prepare waits up to 10 seconds for `/dev/disk/by-label/RBSEED`, mounts it
read-only, and reads `seeds/<mac-with-dashes>.seed` for each of the machine's
interfaces, then `reliaburger.seed`. After a create seed succeeds it remounts
the stick read-write to zero and remove that seed. `relish cluster create
--bare-metal` writes one seed per machine under `<dir>/stick/seeds/`, with
join tokens valid for 7 days by default (seed tokens may live up to 7 days;
ordinary join tokens up to an hour).

## 4. `relish os`

### 4.1 The release channel

`appliance.yml` publishes each OS release as GitHub releases
`os-<version>-<arch>`, versioned `YYYY.WW.N`. It also publishes a channel,
`os-channel.json`:

```json
{"architectures":{"x86_64":{"sums":"reliaburger-os_2026.41.0.SHA256SUMS","sums_sha256":"…","tag":"os-2026.41.0-x86_64"}},"schema":1,"version":"2026.41.0"}
```

- **Signing.** `scripts/release/os_release.py channel` writes the channel as
  canonical JSON (sorted keys, no spaces). The `sign` job signs it with the
  release key, as a raw 64-byte Ed25519 signature in `os-channel.json.sig`.
  The script refuses a key whose public half isn't in `src/upgrade/keys.rs`.
- **Verification** (`src/os/channel.rs`). The parser denies unknown fields.
  Each tag and sums name must match the version, and `sums_sha256` must be the
  digest of the `SHA256SUMS` it names.
- **Retention.** The last 8 versions are kept. A build whose packages haven't
  changed publishes nothing.

### 4.2 Commands

| Command | Does |
|---|---|
| `relish os list [--channel URL] [--key PEM]` | the newest version on the channel, and each node's |
| `relish os upgrade [VERSION] [--allow-downgrade]` | start a rollout, to the newest version by default |
| `relish os status` | the active rollout, and the last one |
| `relish os resume` | restart a paused rollout |
| `relish os abort` | stop a rollout |
| `relish os boot-logs [--to ADDRESS[:PORT] \| --off] [--node NODE]…` | where each node streams its boot logs (#2149), or change it |

The API is `POST /v1/os/stage` and `/v1/os/rollout/{start,resume,abort}`,
all admin, and `GET /v1/os/rollout`, for any token. `GET /v1/os/boot-logs`
(any token) and `POST /v1/os/boot-logs` (unscoped admin) read and rewrite
`reliaburger/netlog.conf` on that node's own ESP (`src/os/netlog.rs`); relish
asks each node in turn, as `relish os list` does. relish reads the channel
only to pick a version. Each node checks what it downloads against the keys
its own image carries.

### 4.3 The rollout

A rollout lives in Raft as `DesiredState::os_rollout` (`src/os/rollout.rs`).

**Starting.** It refuses while a bun upgrade or another OS rollout runs. It
also refuses if any node isn't answering or isn't an appliance, or if the
target is older than a node's version and `--allow-downgrade` isn't given.
It refuses a two-voter council too, for the same reason a bun upgrade does:
taking either voter down leaves the other without a majority, so the rollout
could never move past the council (`check_council_can_roll`).

**Order.** Workers first, then council members, then the leader. A node
already on the target is skipped.

**Each node.** The leader ticks every 5 seconds and makes at most one node
transition per tick:

1. **Pending → Draining.** The node is cordoned. A council member moves only
   when the council can lose it and keep a majority.
2. **Draining → Updating.** Once its workloads have moved, or after 5
   minutes, the leader sends the stage directive. Apps with a managed volume
   or a host-path mount aren't waited for: the scheduler keeps their
   replicas on the node through the drain and the reboot (#2120).
3. **Updating → Done.** The node reports the target version and is healthy.

**Pauses.** The rollout pauses, marking the node Failed, when:
- the node refuses the directive;
- the directive keeps failing for 2 minutes;
- the node reports `Failed` for this target;
- it isn't back on the target within 45 minutes.

**Resume and abort.** `resume` resets Failed nodes to Pending under a new
rollout id. `abort` clears the rollout: nodes keep whatever version they're
on, and one mid-update finishes its reboot.

**Two Raft writes.** `OsRolloutStart` (start and resume) applies only when
no bun upgrade is active and the id has never run, and only to an empty
slot, unless its `replaces` names the paused rollout in it: a resume names
the run it resumes, so it can't replace one it never saw. `OsRolloutUpdate` (the leader's progress, and the
abort's `Aborted`) applies only to the active rollout with the same id
before it has ended. Anything else gets `CouncilResponse::Refused` with a
reason, which the handlers answer with a 409. So a step the leader computed
before an abort, written after it, is refused rather than restarting the
rollout. `relish upgrade` and `relish upgrade rollback` answer 409 while an
OS rollout runs, and a bun upgrade's own start (`UpgradeStart`) and progress
(`UpgradeUpdate`) follow the same rules. A rollback over a paused bun upgrade
names it with `ReplacesPaused::Supersede`, which archives it as aborted in the
same entry that starts the rollback, so nothing can start in between.

### 4.4 Updating one node

`POST /v1/os/stage {rollout_id, version, channel_url}` (`src/os/slot.rs`)
answers `202 updating`, `200 already_running`, or `409` if the node isn't an
appliance or is already updating to another version. Then, off the request:

1. Fetch `reliaburger-os_<version>.SHA256SUMS` and its signature, and verify
   them against the release keys and the image's own OS signing key.
2. Download exactly three files, the UKI, `usr.<uuid>.raw.zst` and
   `usr-verity.<uuid>.raw.zst`, into a private `.incoming` directory,
   checking each one's hash, then move them into staging.
   The signed sums don't name the architecture (both builds name their files
   the same), so the node also reads the UKI's PE header and refuses one whose
   machine type (0x8664 x86_64, 0xAA64 aarch64) isn't its own. Binding the
   architecture into what is signed, by passing the channel's per-architecture
   `sums_sha256` in the directive, would change the wire format; it hasn't
   been done.
3. Run `systemd-sysupdate` with `/usr/lib/reliaburger/sysupdate.d`. It writes
   `/usr` and its verity into the inactive slot, and the UKI into
   `/EFI/Linux` with two tries left (`TriesLeft=2`). Its own verification is
   off, because bun has already checked every byte.
4. Reboot.

Every step is bounded, so staging ends in `Rebooting` or `Failed`, never in a
`Staging` only a bun restart clears: 30 s to connect, 60 s with no bytes
arriving on a download (there's no limit on a whole download), 1 MiB for
`SHA256SUMS` and its signature, 20 minutes for `systemd-sysupdate` and 2 for
the reboot command, whose child is killed when it runs over. The defaults are
`StagingTimeouts`; tests shorten them. A `Failed` node pauses the rollout with
its reason, and `resume` starts a fresh attempt.

The node's progress is in `/var/lib/reliaburger/os-update.json` (Idle,
Staging, Rebooting, Failed) and in `/v1/version` as `os_version` and
`os_update`.

**Boot counting.** `reliaburger-boot-check.service` is required by
`boot-complete.target`, so `systemd-bless-boot` marks the new UKI good only
once the check passes. On a counted boot, the check passes when bun answers
as healthy *and* the node has a global IPv4 address, and both must happen
within 150 seconds of the kernel starting. Otherwise it reboots, and after
two tries systemd-boot falls back to the previous UKI, whose `usrhash` pins
the old `/usr`. Back on the old version, bun turns `Rebooting` into `Failed`
("failed its boot checks, so systemd-boot fell back"), which pauses the
rollout. Any other boot needs only bun, gets 300 seconds from the check's
start once prepare is done, and never reboots. The clock is seconds since
boot (`/proc/uptime`), not the wall clock, which jumps when it first syncs: a
machine with a flat RTC battery boots behind, and the jump would otherwise
use the budget up at once.

The check isn't ordered after the network (#2245). It used to be, behind
`network-online.target` and `reliaburger-netlog`, and that had two costs. A
version whose network never came up never started the check, so it never
failed its tries and never fell back. And every try waited for the network
before the check's clock started, which made the Wyse 3040's fallback 11 min
38 s in the 0.3.2 lab. The address is part of passing for the same reason:
bun answers on the node itself, so without it an update that breaks the
network would be kept and strand the node.

The 150 s and the two tries are a trade between the two ways the check can
be wrong, and on the 3040 three tries can't satisfy both. Too long, and a
broken update falls back too slowly: each try costs the budget plus about
46 s of reboot and firmware, so two tries come to about 8 min 20 s from the
reboot into the broken version, inside the 10 minutes S5 allows (#2143).
Too short, and a slow but healthy first boot of a good update gets rolled
back. The slowest healthy start on the 3040 is about 134 s from the kernel:
the network about 66 s in, the 30 s clock wait below on a LAN with no NTP,
then bun's 38 s. Three tries would need each one under about 118 s to fit
in 10 minutes, which that boot can't make; the maintainer chose two tries
(24 November 2026), so a single bad boot still doesn't roll a good update
back. `image/tests/test_fallback_limit.py` models the Wyse's fallback from
its measured timings and fails if the budget would take it past 10 minutes;
`test_time_wait.py` fails if the budget no longer covers the slowest healthy
start. CI's `os-update.sh --no-link` cuts a VM's link as it reboots into a
good update, and wants two failed tries and the fallback.

When the node streams its boot logs (#2149), `reliaburger-netlog` gives the
boot units `StandardOutput=kmsg` through runtime drop-ins. The check is
already running by then, so it writes its own lines to `/dev/kmsg` once that
drop-in exists, and they go out with the kernel's.

**Time before bun.** bun and its peers expect a node's wall clock to move
forward across restarts, so `reliaburger.service` starts after
`reliaburger-time-wait.service`, which runs
`systemd-time-wait-sync` under `timeout 30` once per boot, after the network
and `systemd-timesyncd` are up. It waits only until the first sync, so a
node that can reach NTP waits a second or two. Giving up isn't a failure
(`ExecStart=-`), so a LAN with no NTP server delays bun by 30 s, still
inside a counted boot's 90 s even on a Wyse 3040. bun isn't ordered after
`time-sync.target`: systemd's own `systemd-time-wait-sync.service` never gives up
(`TimeoutStartSec=infinity`), and if anything enabled it, a node with no NTP
would never start bun. `image/tests/test_time_wait.py` holds the units to
this.

bun starts before the boot check passes. On a boot of the target that
started counted (systemd-boot's `LoaderBootCountPath` EFI variable exists,
and it stays for the whole boot) bun leaves `Rebooting` in `os-update.json`,
reports `Idle`, and reports `os_trial: true` in `/v1/version` until
`boot-complete.target` is reached; then it saves `Idle`. While on trial the
node refuses another stage directive, which would overwrite the marker, and
the leader doesn't mark it Done. A fallback after bun started on a trial
boot is still reported as `Failed`, and the leader is still watching.

**What an update can't change.** Only the UKI and a `/usr` slot are written.
The data partition (`/etc`, `/var`, bun's state under
`/var/lib/reliaburger`) is never touched, so a fallback loses nothing.

## 5. What's deferred

- **The weekly OS build is off** until v0.3.0 is promoted (maintainer, 7
  October 2026). `appliance.yml` publishes only on a dispatch with `publish`.
  [Releasing](../releasing.md#what-the-workflow-builds) says to restore the
  `schedule` trigger after the tag.
- **Secure Boot** has to be off: the UKIs are unsigned, and netboot relies on
  passing a command line.
- **BIOS** firmware isn't supported, only UEFI.
- **CI builds no next version for arm64**, so the update path is tested on
  x86_64 only. A CI build can only update to its own next version, since each
  run trusts only its own throwaway key.
- **Static musl builds** of bun and relish are planned
  ([#1774](https://github.com/reliaburger/reliaburger/issues/1774)).
- **The Raft snapshot is stored in plaintext**; only the log is encrypted
  ([#1775](https://github.com/reliaburger/reliaburger/issues/1775)).
