# Bun's periodic malloc_trim: before and after (#1936)

24 November 2026. One-node Bun, process runtime, on a disposable Lima VM
(Apple Virtualization, arm64 Ubuntu 24.04, glibc 2.39, 8 vCPUs, 16 GiB) on
the Mac Studio. **Before** is `origin/release-1-3-3` at `4b0014b4`; **after**
is the same tree with the periodic trim from
[#1936](https://github.com/reliaburger/reliaburger/issues/1936). Both were
built with the `hollow` profile (optimised, not stripped). The after build
set `SLOW_TRIM` to zero, so it logged every trim's duration; the shipped
code logs only trims of 100 ms or more.

> **Verdict: the trim does what it should, and no more.** It hands back the
> memory that is free when it runs. A Bun that has gone quiet after a burst
> of load sits 22 MiB lower 12 minutes later (92 MiB of anonymous memory
> against 114 MiB). Under steady load it makes no difference over 40
> minutes: the free memory it returns is the next second's allocations, and
> both builds averaged 113 MiB. Each trim took 2 to 41 ms. The hours-long
> growth #1417 found is the case this is for, and a 40-minute run can't show
> it; the next day tier's malloc points will.

## The load

A throwaway script (not in the tree) starts Bun under
`systemd-run`, deploys one `testapp` replica, and then, until the load ends:

- submits a 300-task host array (`relish run --runtime process --batch bN
  --count 300 --exec /usr/bin/printf`) every 3 seconds;
- runs three readers, each looping over `relish -o json nodes`, `top`,
  `metrics hello`, `status` and `jobs`, and `relish logs hello --tail 200`.

It samples `VmRSS`, `VmHWM` and `RssAnon` from `/proc/PID/status` every
30 seconds. When the load (and the idle phase, if any) ends, it waits 20
seconds, then attaches `gdb` to call `malloc_stats()` and `malloc_trim(0)`,
the same two calls the day tier's malloc points make
(`scripts/release/sustained/malloc-point.sh`). `RssAnon` is the number to
read: `VmRSS` adds about 97 MiB of file-backed pages, mostly the unstripped
270 MB binary's text.

Bun already caps glibc at two arenas (`M_ARENA_MAX = 2`), so both builds
ran with Arena 0 and Arena 1 only.

## Steady load, 40 minutes

| | before | after |
| --- | --- | --- |
| `RssAnon`, mean of 81 samples | 112.6 MiB | 113.6 MiB |
| `RssAnon`, highest sample | 135.7 MiB | 140.7 MiB |
| `RssAnon`, 20 s after the load | 124.6 MiB | 119.8 MiB |
| malloc system bytes, then | 118.6 MiB | 134.3 MiB |
| malloc in-use bytes, then | 21.2 MiB | 21.2 MiB |
| `RssAnon` after the final gdb trim | 62.5 MiB | 53.6 MiB |
| periodic trims (minutes 10, 20, 30, 40) | none | 2, 5, 18, 4 ms |

A `malloc_stats()` taken mid-load on the after build (minute 28, no trim)
read 120.0 MiB of system bytes with 51.7 MiB in use, so under load about
half of what malloc holds is live or transient data. The periodic trim at
minute 10 lowered `RssAnon` from 96.7 to 86.4 MiB, and the next sample, 30
seconds later, was 107.0 MiB: the pages it returned were faulted straight
back in. On both builds, 60 to 66 MiB was free again once the load stopped, and
only a trim gave it back.

## A burst, then quiet: 15 minutes of load, 12 minutes idle

| | before | after |
| --- | --- | --- |
| `RssAnon`, last sample under load | 104.3 MiB | 114.6 MiB |
| `RssAnon` 5 minutes into the quiet | 114 MiB | 104 MiB |
| periodic trim at minute 20 (5 min into the quiet) | none | 41 ms |
| `RssAnon` just after it | 114 MiB | 77 MiB |
| `RssAnon` 12 minutes into the quiet | 114.0 MiB | 92.4 MiB |
| malloc system / in-use bytes, then | 109.8 / 17.8 MiB | 101.1 / 17.1 MiB |
| `RssAnon` after the final gdb trim | 59.3 MiB | 63.7 MiB |

Without the trim, the quiet Bun moved between 104 and 117 MiB for four
minutes and then sat at 114 MiB for the last eight, with 55 MiB of it free
memory that only a trim gives back. With it, the trim gave back 27 MiB at
once. "Quiet" still means one app, its metric scrapes and the job store's
clean-up, and `RssAnon` crept back up by 15 MiB over the next 7 minutes,
which the next trim would take again.

## What this run doesn't show

- **Hours.** #1417's free bytes grew from 28 to 95 MiB over 24 hours under
  load, and a trim at hours 18 and 24 returned 32 to 56 MiB. A trim every
  ten minutes should stop that build-up, but the evidence for it is the next
  day tier's malloc points, not this record.
- **Throughput and agent-loop turns.** The load is light and paced, so it
  can't measure the trim's cost. The pauses it logged, 2 to 41 ms every ten
  minutes, are at most 0.007% of wall time.
- **Other allocators.** jemalloc and mimalloc weren't measured. They'd add a
  C build dependency to every target, including the static musl builds and
  the appliance image, and nothing here says glibc plus a trim falls short.
- **The appliance.** The VM is arm64 with 16 GiB. The 2 GB Wyse 3040 is
  x86-64; the call and the arenas behave the same there.
