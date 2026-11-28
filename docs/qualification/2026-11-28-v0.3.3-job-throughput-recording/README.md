# Homepage job-throughput recording, v0.3.3

28 November 2026, 03:05–03:09 GMT, on the quiet Mac Studio (Apple M5 Max,
128 GiB, macOS 27.2). `jobs.cast` and `report.json` are byte-for-byte the
homepage's `assets/job-throughput.cast` and `assets/job-throughput-report.json`
(`scripts/ci/test_job_demo.py` checks it). Each scenario's directory keeps its
`relish bench` report, terminal output and the recorder's observations.

| Scenario | Completed in 60 seconds | Rate | Failures | Retries |
|---|---:|---:|---:|---:|
| VM baseline (raw exits) | 946,471 | 15,774.5/s | 0 | n/a |
| Fresh containers (runc) | 624 | 10.4/s | 0 | 2 |
| Shared containers (shared-runc) | 443,000 | 7,383.3/s | 0 | 0 |
| Host jobs (process) | 739,000 | 12,316.7/s | 0 | 0 |

One run, not a median; the homepage's table still gives the v0.2.0 medians of
five rounds on the M2 Max laptop, so the two sets of rates aren't comparable.

## The host

The recorder held `~/.reliaburger/quiet.lock` for the whole run, so the
power-cut endurance runs (#2067) paused at their next step and stayed
paused, and no build, test or soak ran. Two VMs from other work stayed up,
idle, at under 1% of a core between them. The host's 1-minute load average
was 0.90 when the run started (03:05:08) and 4.55 when it ended (03:09:19);
sampled every 10 s, it stayed between 0.90 and 4.71, which is the VM's own
four busy vCPUs.

Against the [0.3.2 recording](../2026-11-23-v0.3.2-job-throughput-recording/README.md),
on the same host and guest, every rate is 2 to 5% lower: the VM baseline
5.4% (16,679/s then), shared containers 5.3% (7,800/s), host jobs 4.2%
(12,850/s) and fresh containers 2.3% (10.65/s), with two accepted retries,
which the packaged benchmark counts and keeps. The VM baseline runs no
Reliaburger code and fell as much as the rest, so this run's VM was simply a
little slower than 0.3.2's. Read these as one run of 0.3.3, not as a
comparison between releases; the release-to-release comparison is the
profile ([releasing step 9](../../releasing.md#staging-a-candidate)).

These rates are still below 0.2.8's recording (21,388/s VM baseline, 18.0/s
fresh, 9,617/s shared and 13,100/s host jobs), as 0.3.1's and 0.3.2's were
([#2209](https://github.com/reliaburger/reliaburger/issues/2209)). The
[0.3.3 release profile](../2026-11-28-v0.3.3-profile/README.md), taken the
same night, ran 0.3.3 and 0.2.8 side by side on one Ubuntu 24.04 guest and
found host jobs at the same rate and shared containers 0.7% faster. So the gap
isn't in Reliaburger: these recordings run on the quickstart guest, which
0.3.0 moved to Ubuntu 26.04 and Linux 7.0, and the baseline that runs no
Reliaburger code fell with the rest. The guest is the likely cause, not yet a
proven one.

## Binaries

The published v0.3.3. The host's `relish` was checked against the release's
`SHA256SUMS`; `relish setup --quickstart` checked the guest's two against the
release's signed manifest as it installed them:

- `relish-macos-aarch64` (host): `4516fd785ea7bb5653bdc74fd70641d46e5d24b61537ae500d3e88cebaff2002`
- `relish-linux-aarch64` (guest, runs `relish bench`): `ad47433ba5ad6a8c09ff7de2e011461b9343c929abfcf04d1c39d91f4f93f5a5`
- `bun-linux-aarch64` (guest): `23adb27bbf101b0c0068beb5918f346ce166b190fb6282f1ffe4709a76740087`
- BusyBox, the pinned image's (`public.ecr.aws/docker/library/busybox@sha256:9532d8c3…`), used by every
  scenario: `f19470457088612bc3285404783d9f93533d917e869050aca13a4139b937c0a5`

## Rig and method

1. `install.sh --install-only` (v0.3.3) into a throwaway `RELIABURGER_HOME`, then
   `relish setup --quickstart --nodes 1 --yes`: one node, as the manual asks for a
   matched comparison.
2. The VM stopped with `relish local stop`, resized with
   `limactl edit --cpus 4 --memory 8` to the homepage rig's 4 vCPU / 8 GiB, and
   started with `relish local start`. Guest: Ubuntu 26.04.1 aarch64, Linux 7.0.
3. A one-second `relish bench --scenario jobs-vm-baseline --seconds 1` as root
   unpacked the pinned image into root's cache, and its BusyBox was copied to
   `/var/lib/reliaburger/cast/busybox`. node.toml `[process_workloads]`:
   `allowed_binaries` names that copy, and `mount_isolation = false`; Bun's unit
   changed from `--runtime runc` to `--runtime mixed`, then Bun restarted.
4. In the guest, as root, with an admin token and the cluster's root CA in the
   environment:

   ```sh
   python3 -I scripts/demo/record-job-bench.py \
     --host-binary /var/lib/reliaburger/cast/busybox \
     --service-url http://localhost/ \
     --observe-pid BUN_PID --observe-pid HELLO_HTTPD_PID \
     --observe-dir /var/lib/reliaburger/data --output /root/cast/rec
   ```

   The service is the quickstart's `hello` app through the node's ingress.
5. `relish local destroy --yes` and `relish uninstall --yes`.
