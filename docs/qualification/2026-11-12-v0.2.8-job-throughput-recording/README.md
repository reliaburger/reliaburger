# Homepage job-throughput recording, v0.2.8

12 November 2026, 12:57–13:01 GMT, on the quiet Mac Studio (Apple M5 Max,
128 GiB, macOS 27.2). `jobs.cast` and `report.json` are byte-for-byte the
homepage's `assets/job-throughput.cast` and `assets/job-throughput-report.json`
(`scripts/ci/test_job_demo.py` checks it). Each scenario's directory keeps its
`relish bench` report, terminal output and the recorder's observations.

| Scenario | Completed in 60 seconds | Rate | Failures | Retries |
|---|---:|---:|---:|---:|
| VM baseline (raw exits) | 1,283,276 | 21,387.9/s | 0 | n/a |
| Fresh containers (runc) | 1,082 | 18.0/s | 0 | 1 |
| Shared containers (shared-runc) | 577,000 | 9,616.7/s | 0 | 0 |
| Host jobs (process) | 786,000 | 13,100.0/s | 0 | 0 |

One run, not a median; the homepage's table still gives the v0.2.0 medians of
five rounds on the M2 Max laptop, so the two sets of rates aren't comparable.

## Binaries

The published v0.2.8, each checked against the release's `SHA256SUMS`:

- `relish-macos-aarch64` (host): `a94e3a0ee43dd4c1c2a37bab9f82a81d0d02306fc3711f9358a55e4935b1278d`
- `relish-linux-aarch64` (guest, runs `relish bench`): `546d297331cc895e0d11360cb001c8a8ae3b529e6dfed854767792b2828923ae`
- `bun-linux-aarch64` (guest): `8a212344a4ccfc8fdefeff608ec3e74ce4f6bcbbe51bccda0da4a99a51c5568b`
- BusyBox, the pinned image's (`public.ecr.aws/docker/library/busybox@sha256:9532d8c3…`), used by every
  scenario: `f19470457088612bc3285404783d9f93533d917e869050aca13a4139b937c0a5`

## Rig and method

1. `install.sh --install-only` (v0.2.8) into a throwaway `RELIABURGER_HOME`, then
   `relish setup --quickstart --nodes 1 --yes`: one node, as the manual asks for a
   matched comparison.
2. The VM stopped, resized with `limactl edit --cpus 4 --memory 8` to the
   homepage rig's 4 vCPU / 8 GiB, and started with `relish local start`. Guest:
   Ubuntu 24.04 aarch64, Linux 6.8.
3. node.toml `[process_workloads]`: `allowed_binaries` names a copy of the image's
   BusyBox at `/var/lib/reliaburger/cast/busybox`, and `mount_isolation = false`;
   Bun's unit changed from `--runtime runc` to `--runtime mixed`, then Bun restarted.
4. In the guest, as root, with the cluster's token and CA in the environment:

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
