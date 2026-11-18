# What a power cut loses: calibration on Lima VZ (18 Nov 2026)

Step 0 of the [power-cut endurance plan](../plans/2026-11-18-plan-power-cut-endurance.md)
([#2067](https://github.com/reliaburger/reliaburger/issues/2067)). Every
power-cut record we've published says "a VM forced off isn't a real power
loss". This run measures how it differs, so every later record can say which
fault model it tested:

1. **Lose the page cache.** Everything the virtual disk completed survives.
2. **Lose the disk's write cache too.** Everything no flush covered may be gone.
3. **The hardware lies.** Flushed data lost or torn. Only real hardware shows it.

**Result.** On Lima's VZ driver on macOS, `limactl stop --force`, a
`SIGKILL` of the Virtualization.framework process and an in-guest `sysrq b`
each lost every record that was only in the guest's page cache, and kept
every record a flush covered, in 150 cuts. That rules out anything harsher
than model 2 and shows the page cache goes. It doesn't show model 1 for the
stop and the kill: the writer ran its three modes at once on one disk, so
almost every `O_DIRECT` record was followed by another thread's `fdatasync`
(a flush of the whole virtual disk) or an ext4 journal commit before the cut
(see [Why `direct` doesn't settle it](#why-direct-doesnt-settle-it)). Whether
VZ holds a completed but unflushed write in its own process, where a stop or
a kill would lose it, is **untested**; the raw-disk mode below is the
experiment that tests it, and it hasn't run yet. `sysrq b` is model 1
whatever the answer, since the VZ process lives on through it. It reboots the
guest in place; the VM never stops. And the Ubuntu 26.04 guest kernel can
build **model 2**: a copy of a `dm-log-writes` log taken at a cut, with a mark
written at the cut, replayed, drops exactly the writes no flush covered.

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
| `direct` | `O_DIRECT` `pwrite` and no flush from this thread: the virtual disk completed it. The other two threads' flushes and ext4's commits still reach the same disk (below) |
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

Every cut changed the guest's boot id (`calibrate.py record` now refuses a
cut that didn't, or whose writer reported no boot id: a cut that never fired
loses nothing and would have tallied as clean). No record was torn and no file had a
hole (a record missing below a present one) in any mode. A few records per
run survived past the last ACK the host saw (written, ACK not yet printed):
in-flight work, not a problem. Zero losses in 50 cuts bounds the per-cut
loss rate for a mode below 6% at 95% (rule of three); the point here is the
mechanism, which the 100% loss of buffered records on the same cuts shows is
a real cut.

Write rates, roughly (records over the hold plus a second): a median of
about 85 a second for both `fdatasync` and `O_DIRECT`, buffered about 360.
Both synced modes slowed to about 45 a second at the busiest moments. In 27
of the 150 cuts (stop 8, kill 7, sysrq 12) `fdatasync` ran at over 1,000 a
second for the whole cut, and in 12 of them over 2,000; `O_DIRECT` ran at
110 to 360 a second in those cuts. Not occasional, then, and
unexplained: an `fdatasync` that returns in under a millisecond either found
nothing to flush or had its flush answered without waiting for the host's
disk. We didn't look into which. It doesn't change the counts above, but it
is one more reason not to read more into the flushed modes than "a flush the
guest asked for was honoured as far as this host goes".

### Why `direct` doesn't settle it

An `O_DIRECT` write with no flush is the record model 1 and model 2 disagree
on: model 1 keeps it, model 2 may not. But in this run each thread's records
went to its own file on the same ext4 root disk, `vda`, at the same time.
Each `fdatasync` sends `vda` a flush, which covers every write `vda` had
completed, whichever thread made it: about 85 flushes a second at the
median, over 1,000 in 27 cuts. ext4 also commits its journal every five
seconds, with a flush of its own. So by the time of each cut, nearly every
acknowledged `direct` record had a flush after it, and its surviving shows
only what the `fsync` records show: flushed writes survive. A surviving
`direct` record tells model 1 from model 2 only if no flush reached the disk
between its write and the cut, and nothing in this run recorded that.

`calibrate.sh --raw` is the experiment that does. It gives the VM a second,
1 GiB virtual disk with no filesystem
([`calibrate-vm.yaml`](../../scripts/release/power_cut/calibrate-vm.yaml)'s
`additionalDisks`), and `calibrate.py write-raw` writes `O_DIRECT` records to
it alone, reading the disk's completed-flush count from
`/sys/block/<disk>/stat` before each write and printing it with the ACK, and
again after. A record is *uncovered* when the count before its write equals
the last reading the host saw: no flush completed after it. A cut with no
uncovered record isn't counted; the tally reports those as `not_counted`.
The few milliseconds between the last reading and the cut aren't observed,
but nothing in the guest sends that disk a flush unless asked. The counting
and the verdict are unit-tested; the mode has **not yet run** on a VM, so it
still needs its first run to confirm Lima attaches the disk as the script
expects. Until it runs, the stop and kill rows above say model 2 or gentler,
not model 1.

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

**Pass**, with a mark written at the cut. The super block's entry count only
moves at a FUA write or a mark, so a copy taken at a cut nobody announced
holds entries past the count, and a reader that stops at the count drops
them. `logwrites.py --walk` reads on past the count while entries still look
like entries (known flags, a mark name that fits its block, data all
present); that walk is unit-tested on fixtures only, not yet proved against a
live log. The finding that changes step 4 of the plan: `dm-log-writes` passes
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
  loses the page cache and keeps what a flush covered. Whether it also keeps
  completed, unflushed writes (model 1: a disk with no volatile write cache,
  or one with power-loss protection) is untested for a stop or a kill; a
  `sysrq b` keeps them. Either way, write the records as "a hypervisor cut",
  not as a test of a volatile write cache.
- A store that skipped an `fsync` but wrote with `O_DIRECT`, or whose writes
  reached the virtual disk by writeback before the cut, can pass every such
  cut: a `sysrq b` certainly, a stop or kill whenever another flush reached
  the disk first. Only the write-cache cut (plan step 4) or real hardware
  (step 7a) catches it reliably.
- `sysrq b` is a full cut of the guest (page cache, kernel, every process), model 1 by construction, that
  fires from inside within milliseconds and costs no `limactl start`: the cut
  for timed cuts (plan step 3). A shell was back a median 9 s after the cut, against 15–16 s for a stop or a kill.
- The forced stop and the process kill lost the same things here. The harness
  can use `limactl stop --force`, which needs no process lookup, once the
  raw-disk run says whether either loses unflushed writes.

## What this doesn't cover

- Whether VZ keeps completed but unflushed writes through a stop or a kill
  (above): the raw-disk mode is written and unit-tested, not run.
- A cut that lands with no mark in the `dm-log-writes` log: the walk past the
  entry count is unit-tested only.
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
per cut, with the ACKs, the scan and the timings), `logwrites.json` and
`kill.killed.txt`: the process each kill cut hit, one per cut, all 50
`com.apple.Virtualization.VirtualMachine`. The raw ACK captures and scans
stayed in the host's scratch directory. These runs predate the check that
fails a cut when the guest's boot id didn't change; every line has
`rebooted: true` with a boot id before and after, and the summaries tally
the same under the new check.

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
