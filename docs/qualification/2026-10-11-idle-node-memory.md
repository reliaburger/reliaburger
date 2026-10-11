# Idle node memory, 11 October 2026

How much memory one Bun node uses with nothing deployed, for whitepaper Q15
([#690](https://github.com/reliaburger/reliaburger/issues/690), decision D39 in
[#674](https://github.com/reliaburger/reliaburger/issues/674)). It replaces the
"80-120 MB" the whitepaper used to quote, which nobody had measured.

**This is a macOS measurement of a pre-release build, not the Linux release
candidate.** It's the first measured number, not the qualification of 0.2.0.
Run the same script on the 0.2.0 release candidate on a Linux node (rootful
runc, with eBPF and the DNS responder on) and add that record before quoting a
Linux figure.

## Binary and machine

- `bun 0.2.0-dev (7092181)`, a release build (`cargo build --locked --release
  --bin bun --bin relish`) of commit
  [`70921812`](https://github.com/reliaburger/reliaburger/commit/70921812) on the
  `release-0-2-0` train, SHA-256
  `a00901fc125a026ba0ca64dbda8853eedbbd9b65fc5e05e437127bca94f99e8b`.
- Apple M2 Max, 12 CPUs, 32 GiB, macOS 26.3.1.
- `--runtime auto`, which picked the process runtime. On macOS there's no
  runc, eBPF or DNS responder, and `relish init` leaves ingress off.
- The host was busy: other agents were compiling Rust, a Lima VM was running,
  and 24 GiB of the 25 GiB swap was in use throughout. macOS compresses and
  swaps idle pages under that pressure, so these figures may be lower than on
  a quiet machine.

## Method

[`scripts/release/idle_memory.py`](../../scripts/release/idle_memory.py) ran
twice at the same time, on separate ports and data directories:

```sh
python3 scripts/release/idle_memory.py --bun target/release/bun \
    --relish target/release/relish --record cluster.md --mode cluster
python3 scripts/release/idle_memory.py --bun target/release/bun \
    --relish target/release/relish --record standalone.md --mode standalone --port-base 39000
```

Each run makes a fresh `relish init` directory and starts Bun on it with no
workloads. In cluster mode (`--cluster`) the node is the single-voter council
leader (`relish council status` showed `idle-01 leader`, quorum 1/1); in
standalone mode it runs without Raft or gossip. After 600 s the script reads
Bun's resident set size with `ps -o rss` ten times, 30 s apart. Bun started no
child processes, so the tree column equals Bun's own.

RSS counts every resident page, including the clean, shared pages of the
binary's code. As a cross-check, `footprint <pid>` (macOS's dirty, private
memory, compressed pages included) was read every two minutes during the run.

## Result

| Node | RSS, median of 10 (MiB) | RSS range (MiB) | `footprint` (MB) |
|---|---|---|---|
| Single-node cluster (leader, council voter) | 62.8 | 62.8 | 20 |
| Standalone | 47.5 | 47.5-47.6 | 13-14 |

The council, Raft and gossip add about 15 MiB of RSS. Neither figure moved
during the five sampled minutes.

Not measured here: a council member that isn't the leader, a worker that isn't
on the council, a Linux node with eBPF maps loaded and the DNS responder
running, and memory once workloads run. For the last, the 0.1.6 soak's first
samples put Bun between 180 and 583 MB of RSS with its apps and jobs deployed,
and its peak at 810 MB
([record](2026-10-06-v0.1.6-sustained-v02-final.md)).

## Samples, cluster mode

| seconds since start | Bun RSS (MiB) | Bun and descendants (MiB) |
|---|---|---|
| 600 | 62.8 | 62.8 |
| 630 | 62.8 | 62.8 |
| 660 | 62.8 | 62.8 |
| 690 | 62.8 | 62.8 |
| 720 | 62.8 | 62.8 |
| 750 | 62.8 | 62.8 |
| 780 | 62.8 | 62.8 |
| 810 | 62.8 | 62.8 |
| 840 | 62.8 | 62.8 |
| 870 | 62.8 | 62.8 |

## Samples, standalone mode

| seconds since start | Bun RSS (MiB) | Bun and descendants (MiB) |
|---|---|---|
| 600 | 47.5 | 47.5 |
| 630 | 47.5 | 47.5 |
| 660 | 47.5 | 47.5 |
| 690 | 47.5 | 47.5 |
| 720 | 47.5 | 47.5 |
| 750 | 47.5 | 47.5 |
| 780 | 47.5 | 47.5 |
| 810 | 47.5 | 47.5 |
| 840 | 47.6 | 47.6 |
| 870 | 47.6 | 47.6 |
