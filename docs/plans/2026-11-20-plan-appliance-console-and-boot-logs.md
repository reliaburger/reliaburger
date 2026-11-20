# Plan: the appliance's console and its boot logs

Dated 20 November 2026, from the first evening of the Wyse 3040 lab
([#407](https://github.com/reliaburger/reliaburger/issues/407)). Two things
made that evening harder than it should have been. A node's monitor showed
its name, address and whether bun was up, and nothing else. And while a node
rebooted through a broken update three times, the only way to see why was to
photograph the screen. The 3040 has no serial port.

This plan covers both:

1. **Boot logs over the network** ([#2149](https://github.com/reliaburger/reliaburger/issues/2149), 0.3.1): a node's kernel and boot messages go to `relish netboot`, from the initrd onwards.
2. **A console dashboard** ([#2150](https://github.com/reliaburger/reliaburger/issues/2150), 0.3.2): tabs for status, metrics, workloads, the cluster, versions and logs on tty1. The invaders game's kill mode is a follow-up ([#2151](https://github.com/reliaburger/reliaburger/issues/2151)).

The logs come first: they're smaller, they pay off on the rest of the lab,
and they would have shown the cause of the evening's one failed criterion
([#2143](https://github.com/reliaburger/reliaburger/issues/2143), boot-check's
120 s per try) without a camera.

## 1. Boot logs over the network

### What we want to see

During the fallback test a node boots a broken version, `reliaburger-boot-check`
waits for bun, gives up, and reboots, three times. What it logs on each try
("bun not healthy after 120 s", then bun's last 60 lines) goes to the journal
of a boot that is about to end, and to tty0. Anyone not standing at the
monitor sees nothing until the node is back on the old version and they can
`journalctl -b -1` over SSH. A boot that never gets that far (a kernel panic,
a missing driver in the initrd, a disk the installer can't find) can't be
read afterwards at all.

So the logs have to leave the machine while it boots, without bun, without
SSH, and without a working clock.

### The design

**Kernel messages: netconsole.** The kernel's
[netconsole](https://docs.kernel.org/networking/netconsole.html) sends every
printk line as a UDP packet to a configured address. It needs no userspace,
so it carries panics and oopses. It's configured at run time through configfs
(`/sys/kernel/config/netconsole/<target>/`), which is the part that matters
here: the UKI's command line is fixed when it's built and signed, so
`netconsole=` on the command line would need a different UKI per lab.

**Userspace messages: into the kernel log, per unit.** journald's
[`ForwardToKMsg=`](https://www.freedesktop.org/software/systemd/man/latest/journald.conf.html)
is all or nothing, so instead the units that matter during a boot send their
output to both places, with
[`StandardOutput=journal+kmsg`](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html)
in a drop-in: `reliaburger-boot-check`, `reliaburger`, `reliaburger-seed`,
`reliaburger-time-wait` and the installer's `reliaburger-install`. netconsole
then sends their lines on. Everything else stays local.

**Where it starts: the initrd.** A small unit in the initrd,
`reliaburger-netlog.service`, runs once the network card is up:

1. reads `/boot/reliaburger/netlog.conf` from the ESP (a target address and port), and stops if there isn't one;
2. loads `netconsole`, creates a target in configfs bound to the interface that has the default route, and enables it;
3. sends the kernel log so far (`dmesg`) once, as UDP lines to the same target, so the start of the boot isn't lost.

The target is kernel state, so it outlives the switch from the initrd to the
real root. Whether it survives `systemd-networkd` taking the link down and
up again is the first thing the image test checks; if it doesn't, the unit
runs again after `network-online.target`.

**Off by default.** The lines cross the LAN in cleartext and can hold
addresses, app names and bun's errors. Two ways to turn it on:

- `relish netboot --stream-logs` tells the installer, through the boot script, to write the server's lab address to `netlog.conf` on the ESP. The installer's own messages go the same way, so the install itself streams too.
- `relish os boot-logs --to <address>[:<port>]` and `--off` change it later on a claimed cluster, through bun, which rewrites `netlog.conf`.

**relish's side.** `relish netboot` listens on UDP 6666 on the lab interface
and prints each line among its DHCP, TFTP and HTTP lines, prefixed with the
sender's MAC (looked up from its own DHCP and PXE answers) or address:

```
log <mac>: reliaburger: bun not healthy after 120 s on OS 2026.47.228
```

`relish machines logs [--interface <if>]` runs only the listener, for a lab
where nothing is being served. Both write the raw lines to
`<netboot dir>/logs/<mac>.log` as well, so a lab record can quote them.

### What it doesn't do

- **No delivery guarantee.** UDP, no retries, no ordering beyond the network's. It's a lab and diagnosis tool, not log shipping: a claimed node's logs already go through the cluster's log pipeline.
- **No early firmware or bootloader output.** netconsole starts with the network driver, so a node that never loads the kernel still needs the monitor.
- **No authentication.** Anyone on the LAN can send lines to the listener. The listener only prints, and it marks lines from a source it didn't serve as `log <address> (not served):`.

### Considered and not chosen

- **[`systemd-journal-upload`](https://www.freedesktop.org/software/systemd/man/latest/systemd-journal-upload.service.html)** sends the journal over HTTP(S), but only once the network and the clock are up and the upload service has started. The boots that fail are the ones that don't get that far.
- **The serial console.** The images already pass `console=ttyS0`, but the 3040 has no serial port and the Pi needs a UART adapter.
- **A `netconsole=` command-line argument.** It works from the earliest kernel boot, but the UKI's command line is fixed at build time (and only overridable with Secure Boot off), so the target can't be the operator's address.

### Tests

- **Unit:** `netlog.conf` parsing (valid, missing, malformed); the listener's line parsing and source labelling; the CLI flags.
- **Image test (QEMU, OVMF):** boot the image with a `netlog.conf` pointing at the host, stage a broken update, and assert the listener receives `bun not healthy` three times, then `bun healthy` on the fallback version. This extends the existing fallback test in `image/tests/`.
- **Netboot test:** `relish netboot --stream-logs` against the CI netboot VM; assert the installer's `reliaburger-install:` lines arrive.
- **Lab:** the checklist's step 1.7 reads the fallback from relish's log instead of the monitor.

### Order

1. The initrd unit, `netlog.conf` and the journald drop-in, with the image test.
2. The listener in `relish netboot`, `relish machines logs` and `--stream-logs`.
3. `relish os boot-logs` through bun.
4. Manual 17 (the appliance), the book's appliance chapter and the lab checklist.

## 2. A console dashboard

### Today

`bun appliance console` (`src/appliance/console.rs`) redraws a block of
`label  value` lines to tty1 every five seconds: OS version, node, cluster,
address, MAC, bun's health, and the claim key while unclaimed. It doesn't
read the keyboard. On the lab, it was the only view of a node that didn't
need the laptop.

### The design

**Reuse relish's terminal UI.** `src/relish/tui/` (book chapter 13) already
has the app loop, keys, views, a palette and a data provider that polls bun's
API. The console becomes a full-screen instance of it on tty1, with a
different set of tabs and a data provider that talks to the local bun. A
second UI would drift from the first within a release.

| Tab | Shows | From |
| --- | --- | --- |
| 1 Status | node, cluster, address, MAC, claim key (large while unclaimed), bun's health, the `wtf` summary | what the console reads now, plus `/v1/wtf` |
| 2 Metrics | this node's CPU, memory, disk and network, as gauges and a short history | bun's metrics |
| 3 Workloads | the instances on this node: app, state, restarts, CPU, memory | the status API, filtered to this node |
| 4 Cluster | gossip (members, suspects, updates in and out, last change) and Raft (leader, term, applied and log index, each voter's lag) | the summary and council APIs |
| 5 Versions | OS image, kernel, bun, runc, systemd, and whether Buildah is there | local files and `--version` |
| 6 Logs | the journal tail for `reliaburger` and `reliaburger-boot-check` | `journalctl`, local |
| Easter egg | invaders, one per container on this node; a hit pops it | the Workloads tab's data |

**Bun down, old screen.** When bun doesn't answer, the console shows
today's screen. That's the one that matters during a boot or a fallback, and
it can't depend on the API it's reporting on.

**Who can see it.** There's no login on the console, so whoever has the
keyboard sees what it shows. The console reads bun with a read-only identity
that only it holds (a root-only Unix socket, or a read-only token bun mints
when the node is seeded; the choice is the first PR's), and the views it
reuses never show secrets, environment variables or tokens.

**The Linux console.** tty1 has 16 colours and the kernel's console font,
so the console gets an ASCII and box-drawing theme: no braille sparklines and
no emoji. Snapshot tests (`insta`) render every tab at 80×25 with fixtures.

### Kill mode (#2151)

Shooting an invader kills its container only if the cluster opts in
(`[appliance] console_chaos = true`, a council setting). A hit then goes
through Smoker as an ordinary fault, so it's audited, shows in `relish fault`
and the events, and Smoker's own safety rules apply. Physical access alone
never enables it.

### Order

1. The read-only local identity and the data provider for the console.
2. Tabs 1, 5 and 6 (no new API), with the fallback to today's screen.
3. Tabs 2, 3 and 4.
4. The invaders, harmless.
5. Kill mode, after the rest has shipped.

Book chapter 13 gets a section on the console; manual 17 documents the keys.

## Sources

- [netconsole](https://docs.kernel.org/networking/netconsole.html), the kernel documentation, including dynamic configuration through configfs
- [journald.conf](https://www.freedesktop.org/software/systemd/man/latest/journald.conf.html), `ForwardToKMsg=`
- [systemd.exec](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html), `StandardOutput=journal+kmsg`
- [systemd-journal-upload.service](https://www.freedesktop.org/software/systemd/man/latest/systemd-journal-upload.service.html)
- [Ratatui](https://ratatui.rs/), the TUI library relish's UI uses
