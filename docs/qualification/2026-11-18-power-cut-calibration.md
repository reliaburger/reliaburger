# What a power cut loses: calibration on Lima VZ (17 Nov 2026)

Step 0 of the [power-cut endurance plan](../plans/2026-11-18-plan-power-cut-endurance.md)
([#2067](https://github.com/reliaburger/reliaburger/issues/2067)). Every
power-cut record we've published says "a VM forced off isn't a real power
loss". This run measures how it differs, so every later record can say which
fault model it tested:

1. **Lose the page cache.** Everything the virtual disk completed survives.
2. **Lose the disk's write cache too.** Everything no flush covered may be gone.
3. **The hardware lies.** Flushed data lost or torn. Only real hardware shows it.

**Result.** On Lima's VZ driver on macOS, all three ways we can cut a VM's
power give **model 1**: `limactl stop --force`, a `SIGKILL` of the
Virtualization.framework process and an in-guest `sysrq b` each kept every
record the virtual disk had completed, flushed (`fdatasync`) or not
(`O_DIRECT`), in 150 cuts, and lost every record that
was only in the guest's page cache. VZ holds nothing in its own process that
a kill loses. `sysrq b` reboots the guest in place; the VM never stops. And
the Ubuntu 26.04 guest kernel can build **model 2**: a copy of a
`dm-log-writes` log taken at a cut, replayed, drops exactly the writes no
flush covered.

## Environment

| Item | Value |
|---|---|
| Host | Apple M5 Max (Mac Studio, 18 cores, 128 GB), macOS 27.2, Lima 2.2.1 (VZ driver). Other work on the host: agents' builds and tests, the development VM and the endurance smoke cluster; no release gate |
| VMs | One per kind, `pc-cal-stop`, `pc-cal-kill`, `pc-cal-sysrq` and `pc-cal-logwrites`, created by [`calibrate.sh`](../../scripts/release/power_cut/calibrate.sh) from [`calibrate-vm.yaml`](../../scripts/release/power_cut/calibrate-vm.yaml): 2 vCPU, 2 GiB (a quickstart node's size), 10 GiB disk, nothing mounted from the host. Deleted afterwards |
| Guest | Ubuntu 26.04 cloud image `release-20260927` (the 0.3.0 quickstart image's base), kernel `7.0.0-34-generic`, ext4 root; `/sys/block/vda/queue/write_cache` = `write back`, so the guest sends flushes |
| Writer | [`calibrate.py`](../../scripts/release/power_cut/calibrate.py) `write`, three threads, one 128 MiB preallocated file each, 4 KiB records carrying the run id, sequence number, mode and a CRC |
| Seed | 2067 for every kind (the hold before each cut, 3–12 s) |
| Time | 20:52 to 21:30 UTC, 17 November 2026: forced stop and process kill 20:55–21:19, `sysrq b` 21:11–21:30, the `dm-log-writes` proof at 20:55 |

The writer prints `ACK <mode> <n>` once record *n* has gone as far as its
mode takes it, and the host captures every line:

| Mode | An ACK means |
|---|---|
| `fsync` | buffered `pwrite`, then `fdatasync`: on stable storage as far as the guest can tell |
| `direct` | `O_DIRECT` `pwrite` and no flush: the virtual disk completed it, nobody asked it to be durable |
| `buffered` | plain `pwrite`: in the guest's page cache only |

After each cut, `calibrate.py scan` reads each file back. A record counts only
if its CRC matches and it carries this cut's run id and its own number, so a
stale record from an earlier cut is "absent" and a half-written one "torn".
"Lost" means the host saw the ACK and the record isn't in the longest
unbroken prefix on disk.

## Results

| Cut | Command | Cuts | `fsync` records lost | `direct` records lost | `buffered` records lost | Came back |
|---|---|---|---|---|---|---|
| Forced stop | `calibrate.sh --kind stop --cuts 50` (`limactl stop --force`: `SIGKILL` to the host agent and driver) | 50 | 0 of 174,730 | 0 of 42,319 | 135,156 of 135,156 (every cut) | `limactl start`, 9–83 s (median 16 s); no VZ start retry |
| VZ process kill | `calibrate.sh --kind kill --cuts 50` (`kill -9` of the process holding the disk image: `com.apple.Virtualization.VirtualMachine`, every cut) | 50 | 0 of 124,955 | 0 of 40,505 | 143,626 of 143,626 (every cut) | `limactl start`, 8–53 s (median 15 s); no VZ start retry |
| In-guest `sysrq b` | `calibrate.sh --kind sysrq --cuts 50` (`echo b > /proc/sysrq-trigger`) | 50 | 0 of 190,653 | 0 of 46,893 | 133,657 of 133,657 (every cut) | Rebooted in place every time: the VM stayed `Running` and a shell answered 5–21 s after the cut (median 9 s) |

Every cut changed the guest's boot id. No record was torn and no file had a
hole (a record missing below a present one) in any mode. A few records per
run survived past the last ACK the host saw (written, ACK not yet printed):
in-flight work, not a problem. Zero losses in 50 cuts bounds the per-cut
loss rate for a mode below 6% at 95% (rule of three); the point here is the
mechanism, which the 100% loss of buffered records on the same cuts shows is
a real cut.

Write rates, roughly (records over the hold plus a second): a median of
about 85 a second for both `fdatasync` and `O_DIRECT`, buffered about 360.
Both synced modes slowed to about 45 a second at the busiest moments, and
`fdatasync` occasionally ran past 2,000 a second; we didn't look into why,
and it doesn't change any result.

### The dm-log-writes proof

`calibrate.sh --logwrites` ran [`logwrites-proof.sh`](../../scripts/release/power_cut/logwrites-proof.sh)
as root in `pc-cal-logwrites`. `dm-log-writes` is in the 26.04 guest's base
modules (no `linux-modules-extra`). The proof puts ext4 on a `log-writes`
device over two loop files, writes a file and syncs it, writes a second with
`O_DIRECT` and no flush, adds a mark and copies the log (the cut), syncs,
marks and copies again. [`logwrites.py`](../../scripts/release/power_cut/logwrites.py)
replays each copy onto a zeroed image:

| Replay | Unflushed bytes on it | Synced file | `e2fsck -fn` after journal recovery |
|---|---|---|---|
| The data device itself | yes | — | — |
| Log copied at the cut, to its end (132 entries) | **no** | intact | clean |
| Log copied after the sync, to its end (149 entries) | yes | intact | clean |
| Log copied after the sync, to the mark at the cut | no | intact | clean |

**Pass.** The finding that changes step 4 of the plan: `dm-log-writes` passes
every write straight to the data device but **logs a plain write only when a
later flush completes** (a FUA write at once). Until then it sits on a list in
memory, as in a drive's volatile cache. So a copy of the log taken at the cut,
replayed in full, already *is* the disk as of the last flush; there's no need
to find the last flush in it. Prefix states (CrashMonkey's) come from the batch
the last flush logged: a replay ending inside it is a disk that wrote its cache
back in order and lost power partway.

`mkfs.ext4` needs `-E lazy_itable_init=0,lazy_journal_init=0`: without it,
ext4 zeroes inode tables in the background after mounting, and those writes
bring flushes of their own that can make the "unflushed" file durable midway.

## What this means for our records

- Every hypervisor power cut we've run, in step 5 of the gate and in the soaks,
  tests model 1: a disk with no volatile write cache, or one with power-loss
  protection. Realistic for a good SSD, generous for SD cards and cheap eMMC.
- A store that skipped an `fsync` but wrote with `O_DIRECT`, or whose writes
  reached the virtual disk by writeback before the cut, passes every such
  cut. Only the write-cache cut (plan step 4) or real hardware (step 7a)
  catches it.
- `sysrq b` is a full cut of the guest (page cache, kernel, every process) that
  fires from inside within milliseconds and costs no `limactl start`: the cut
  for timed cuts (plan step 3). A shell was back a median 9 s after the cut, against 15–16 s for a stop or a kill.
- The forced stop and the process kill are the same cut. The harness can use
  `limactl stop --force`, which needs no process lookup.

## What this doesn't cover

- Lima's QEMU driver and Linux hosts: not measured. The plan expects the same
  model 1 (the host's page cache outlives the process), but nobody has
  checked.
- A host power cut: the host never lost power, so whether macOS's `fsync`
  (not `F_FULLFSYNC`) would have kept the flushed records is untested and
  doesn't matter here.
- Host bytes written per hour under the soak's workload, which the design
  draft listed for step 0, wasn't measured; it moves to the first endurance
  run's record.

## Evidence

The summaries and per-cut lines are in
[`2026-11-18-power-cut-calibration/`](2026-11-18-power-cut-calibration/):
`<kind>.summary.json` (`calibrate.py tally`), `<kind>.cuts.jsonl` (one line
per cut, with the ACKs, the scan and the timings) and `logwrites.json`. The raw
ACK captures and scans stayed in the host's scratch directory.

Two earlier sysrq runs didn't finish, and neither counts above. The first was
stopped after one cut to make the scan retry when a guest that has only just
come back refuses a shell. The second hung at its tenth cut: the driver bounded
its `limactl shell` probes with `perl -e 'alarm 15; exec …'`, and `limactl`,
being a Go program, catches `SIGALRM` and ignores it, so an `ssh` to a guest
that rebooted under it waited forever. `calibrate.sh` now runs them through a
`bounded` helper that kills the command's whole process group, with a test
that a command ignoring `SIGALRM` is stopped on time. Their ten cuts matched
the counted ones exactly: nothing flushed or direct lost, every buffered
record lost.
