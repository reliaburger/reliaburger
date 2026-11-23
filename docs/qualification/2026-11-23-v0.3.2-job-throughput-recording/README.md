# Homepage job-throughput recording, v0.3.2

23 November 2026, 16:23–16:28 GMT, on the quiet Mac Studio (Apple M5 Max,
128 GiB, macOS 27.2). `jobs.cast` and `report.json` are byte-for-byte the
homepage's `assets/job-throughput.cast` and `assets/job-throughput-report.json`
(`scripts/ci/test_job_demo.py` checks it). Each scenario's directory keeps its
`relish bench` report, terminal output and the recorder's observations.

| Scenario | Completed in 60 seconds | Rate | Failures | Retries |
|---|---:|---:|---:|---:|
| VM baseline (raw exits) | 1,000,747 | 16,679.1/s | 0 | n/a |
| Fresh containers (runc) | 639 | 10.7/s | 0 | 1 |
| Shared containers (shared-runc) | 468,000 | 7,800.0/s | 0 | 0 |
| Host jobs (process) | 771,000 | 12,850.0/s | 0 | 0 |

One run, not a median; the homepage's table still gives the v0.2.0 medians of
five rounds on the M2 Max laptop, so the two sets of rates aren't comparable.

## The host

The recorder held `~/.reliaburger/quiet.lock` for the whole run, so the
paused power-cut endurance runs (#2067) stayed paused, and no other build,
test or VM ran; 0.3.2's gate chain had ended at 14:42. The host's 1-minute
load average was 1.11 when the run started (16:23:42) and 4.24 when it
ended (16:27:49); sampled every 10 s, it stayed between 1.11 and 4.18,
which is the VM's own four busy vCPUs.

Against the [0.3.1 recording](../2026-11-21-v0.3.1-job-throughput-recording/README.md),
on the same host and guest, the VM baseline (16,721/s then) and host jobs
(12,867/s) are within 0.3%, shared containers are 7% faster (7,283/s) and
fresh containers 17% slower (12.9/s), with one accepted retry, which the
packaged benchmark counts and keeps. Fresh containers start a runc container
per job, so their rate moves most from run to run. Read these as one run of
0.3.2, not as a comparison between releases; the release-to-release
comparison is the profile
([releasing step 9](../../releasing.md#staging-a-candidate)).

## Binaries

The published v0.3.2, each checked against the release's `SHA256SUMS`:

- `relish-macos-aarch64` (host): `e8255e80310689631bddc4cfd16f86cbb80b6c7feccdad3b49976a15d0d2f503`
- `relish-linux-aarch64` (guest, runs `relish bench`): `d0cebd9b201153b92b9fea3a5e239236054b8b350e9c437ace8cc21d1129da85`
- `bun-linux-aarch64` (guest): `4219a14b83fd85da12b0da574eb3ae026c2c47fec2a8f0b7d9f04297975845a4`
- BusyBox, the pinned image's (`public.ecr.aws/docker/library/busybox@sha256:9532d8c3…`), used by every
  scenario: `f19470457088612bc3285404783d9f93533d917e869050aca13a4139b937c0a5`

## Rig and method

1. `install.sh --install-only` (v0.3.2) into a throwaway `RELIABURGER_HOME`, then
   `relish setup --quickstart --nodes 1 --yes`: one node, as the manual asks for a
   matched comparison.
2. The VM stopped with `relish local stop`, resized with
   `limactl edit --cpus 4 --memory 8` to the homepage rig's 4 vCPU / 8 GiB, and
   started with `relish local start`. Guest: Ubuntu 26.04.1 aarch64, Linux 7.0.
3. node.toml `[process_workloads]`: `allowed_binaries` names a copy of the image's
   BusyBox at `/var/lib/reliaburger/cast/busybox`, and `mount_isolation = false`;
   Bun's unit changed from `--runtime runc` to `--runtime mixed`, then Bun restarted.
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

Nothing else ran on the Studio during the recording.
