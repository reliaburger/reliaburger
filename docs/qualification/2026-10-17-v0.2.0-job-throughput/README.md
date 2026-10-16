# Job throughput on the released 0.2.0 (17 Oct 2026)

The four `relish bench` job scenarios, re-measured on the published
[v0.2.0](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.0)
binaries, as the [release gate](../../releasing.md#staging-a-candidate) asks
before the homepage's throughput table is published. This replaces the table
the homepage carried from development build `006aca5f`
([9 October record](../2026-10-09-timed-job-scenarios/README.md)).

**Result:** five rounds, every window complete, with no terminal failures and
cleanup verified after each one. One fresh-container window (round 3)
accepted one retry.

| Scenario | Median rate (range of five) | Median completed in 60 s | Extrapolated runs/day | 9 Oct table (development build) |
|---|---:|---:|---:|---:|
| Raw VM processes | 15,491.8/s (12,480.2–16,391.7) | 929,510 exits | 1.3B | 16,562.0/s |
| Fresh containers | 12.7/s (12.2–12.9) | 762 accepted | 1.1M | 3.55/s |
| Shared containers | 2,916.7/s (2,700.0–3,100.0) | 175,000 accepted | 252M | 966.7/s |
| Host jobs | 4,666.7/s (3,766.7–4,833.3) | 280,000 accepted | 403.2M | 4,900.0/s |

Runs/day multiplies the minute count by 1,440; it isn't an observed daily
total. The container paths are 3 to 3.6 times the development build's single
recording, in line with the [independent five-round
rerun](../2026-10-09-timed-job-scenarios/README.md#independent-five-round-rerun),
which measured them 1.7 to 3 times higher on later builds. Raw and host rates
are within the rig's variation of the old table.

## Method

[`scripts/release/job_bench_rounds.sh`](../../../scripts/release/job_bench_rounds.sh),
unchanged, with `--rounds 5`: a fresh single-node cluster (`--cluster
--runtime auto`) beside a 500m BusyBox web app, then five rounds of the four
scenarios for 60 seconds each, the order rotating by one each round.
`relish bench` defaults: concurrency 27 per node, 25m CPU request, one-core
limit, 32 MiB memory, the pinned image
`public.ecr.aws/docker/library/busybox@sha256:9532d8c3…6028`, warm images and
cold executors. Host jobs and the raw baseline run the allowlisted copy of the
image's BusyBox.

| Item | Value |
|---|---|
| Host | Apple M2 Max, 12 physical cores, 32 GiB, macOS; the same hardware as the 9 October recording |
| VM | `rb-bench-020`, Lima 2.1.0 (VZ), 4 vCPU, 8 GiB, made for this run and deleted afterwards |
| Guest | Ubuntu 24.04 aarch64, Linux 6.8.0-142-generic, runc 1.4.0 |
| `bun` | v0.2.0 (`cc5f643`) from the release, SHA-256 `a10896975da13146320bb65236566e537390b0ed6a0a4b400c7c3cbdc1b98c6d` |
| `relish` | v0.2.0 (`cc5f643`) from the release, SHA-256 `2d74f6685a963b0b64f98af1ce24aadc8e7bf9eb6ce2e51c38e3b744bc017fc5` |
| BusyBox | `/opt/bench/busybox`, SHA-256 `f19470457088612bc3285404783d9f93533d917e869050aca13a4139b937c0a5`, the same executable as on 9 October |
| Windows | 2026-10-16 19:09 to 19:33 UTC |

Both binaries were checked against the release's `SHA256SUMS` before
installing. [`binaries.txt`](binaries.txt) has the hashes the script recorded,
and [`progress.log`](progress.log) the order and exit of every window.

## Host load

No benchmark runs while other work loads the host. Other agents' builds were
held off for the whole run, and the run started once no compiler was running
and the 1-minute load was 2.40. [`host-load.log`](host-load.log) samples the
host's load average every 30 seconds. It sat at 3.2 to 4.6, which is about the
VM's own four busy vCPUs, apart from one spike to 9.8 at 19:30 UTC during round
5's container windows. The slowest raw and host windows (rounds 4 and 5)
weren't all at high load, so the ranges above are this rig's variation. Read
the medians, not a single window.

## Files

For each round `rN` and scenario: the `relish bench --report` JSON
(`rN-<scenario>.json`) and the command's output (`rN-<scenario>.out`). Compare
a later build with `relish bench --scenario S --compare rN-S.json` on the same
rig ([manual](../../manual/14_batch-jobs.md#measure-job-throughput-with-relish)).

## A first attempt that didn't run

The first attempt stopped before any window: Bun refused to start with
`cannot recover durable kernel policy: … kernel pins have no original
ownership manifest`. Before running the script, the VM had briefly hosted a
hand-built node (following [linux-servers.md](../../linux-servers.md)), which
left its eBPF pins in `/sys/fs/bpf`. The script then deletes
`/var/lib/reliaburger`, and with it those pins' ownership manifest, so the
fresh Bun correctly refused pins it couldn't account for. Restarting the VM
cleared bpffs, and the measured run above started on a clean boot. Run the
script on a VM that has never run Bun, as its usage text says.
