# Homepage job-throughput recording, v0.3.1

21 November 2026, 22:27–22:32 GMT, on the quiet Mac Studio (Apple M5 Max,
128 GiB, macOS 27.2). `jobs.cast` and `report.json` are byte-for-byte the
homepage's `assets/job-throughput.cast` and `assets/job-throughput-report.json`
(`scripts/ci/test_job_demo.py` checks it). Each scenario's directory keeps its
`relish bench` report, terminal output and the recorder's observations.

| Scenario | Completed in 60 seconds | Rate | Failures | Retries |
|---|---:|---:|---:|---:|
| VM baseline (raw exits) | 1,003,259 | 16,721.0/s | 0 | n/a |
| Fresh containers (runc) | 774 | 12.9/s | 0 | 0 |
| Shared containers (shared-runc) | 437,000 | 7,283.3/s | 0 | 0 |
| Host jobs (process) | 772,000 | 12,866.7/s | 0 | 0 |

One run, not a median; the homepage's table still gives the v0.2.0 medians of
five rounds on the M2 Max laptop, so the two sets of rates aren't comparable.

## The host

The recorder held `~/.reliaburger/quiet.lock` for the whole run, so the
power-cut endurance run (#2067) stood down, and no other build, test or VM
ran. The host's 1-minute load average was 1.48 when the run started
(22:27:50) and 4.41 when it ended (22:31:56); sampled every 10 s, it
stayed between 1.48 and 5.39, which is the VM's own four busy vCPUs.

Against the [0.2.8 recording](../2026-11-12-v0.2.8-job-throughput-recording/README.md)
every rate is lower, the raw VM baseline by 22%, which no Reliaburger code
runs in. The guest changed between them: 0.3.0 moved the quickstart to
Ubuntu 26.04 and Linux 7.0 (0.2.8's was 24.04 and 6.8). Read these as one
run of 0.3.1, not as a comparison between releases; the release-to-release
comparison is the profile
([releasing step 9](../../releasing.md#staging-a-candidate)).

## Binaries

The published v0.3.1, each checked against the release's `SHA256SUMS`:

- `relish-macos-aarch64` (host): `38188ade1704b0cddf02a1508231f2c78cb21ee5da8e8d46a9bfd98201978534`
- `relish-linux-aarch64` (guest, runs `relish bench`): `7f9218249e5710b11ad875cb4908b016cb68240b1df2d3f149f5153845c4168c`
- `bun-linux-aarch64` (guest): `e857f9d1873c984ec4c0fa3ebece5a18837db33fd6e25ad0d7fbb9874cf8b5b2`
- BusyBox, the pinned image's (`public.ecr.aws/docker/library/busybox@sha256:9532d8c3…`), used by every
  scenario: `f19470457088612bc3285404783d9f93533d917e869050aca13a4139b937c0a5`

## Rig and method

1. `install.sh --install-only` (v0.3.1) into a throwaway `RELIABURGER_HOME`, then
   `relish setup --quickstart --nodes 1 --yes`: one node, as the manual asks for a
   matched comparison.
2. The VM stopped, resized with `limactl edit --cpus 4 --memory 8` to the
   homepage rig's 4 vCPU / 8 GiB, and started with `relish local start`. Guest:
   Ubuntu 26.04.1 aarch64, Linux 7.0.
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
