# Power cuts and reboots on 5f21eef6 (27 Nov 2026)

Release gate step 5's power-cut and reboot lanes ([releasing.md](../releasing.md#staging-a-candidate)), run by `qualify-power-cut-reboot.sh --commit 5f21eef6 --iterations 25`.

**Result: every lane passed 25 of 25.** No iteration needed the VM start retry.

## Environment

| Item | Value |
|---|---|
| Host | Apple M5 Max, macOS 27.2, limactl version 2.2.1 |
| VM | `rb-pcut-5f21eef6`, created for this run from [`power-cut-vm.yaml`](https://github.com/reliaburger/reliaburger/blob/5f21eef6abc384d7e1c3cb8ff633aa7c94074a7f/scripts/release/power-cut-vm.yaml) with 4 vCPUs, 6 GiB memory, 6 GiB swap and a 40 GiB disk, and only the candidate checkout mounted, read-only, all checked from inside the guest |
| Guest | kernel 6.8.0-147-generic |
| Source | [`5f21eef6`](https://github.com/reliaburger/reliaburger/commit/5f21eef6abc384d7e1c3cb8ff633aa7c94074a7f), a clean checkout of it |
| Time | 2026-11-26 23:24 GMT to 2026-11-27 00:10 GMT |

## Results

| Lane | Claim | Command | Runs | Failures | 95% bound (rule of three) | Per-iteration record |
|---|---|---|---|---|---|---|
| C03 | Exporter checkpoint and lock | `qualify-storage-power-cut.sh --fixture exporter --iterations 25` | 25 | 0 | < 12% | [power-cut-exporter](2026-11-27-power-cut-reboot/power-cut-exporter.md) |
| C11 | Lease stores | `qualify-storage-power-cut.sh --fixture leases --iterations 25` | 25 | 0 | < 12% | [power-cut-leases](2026-11-27-power-cut-reboot/power-cut-leases.md) |
| Council backups | Upload, then prune to the newest 3 | `qualify-storage-power-cut.sh --fixture backups --iterations 25` | 25 | 0 | < 12% | [power-cut-backups](2026-11-27-power-cut-reboot/power-cut-backups.md) |
| C47 | OCI holds and original execution | `qualify-oci-reboot.sh --vm rb-pcut-5f21eef6, 25 runs` | 25 | 0 | < 12% | [oci-reboot](2026-11-27-power-cut-reboot/oci-reboot.md) |
| C47 | Bun, kernel and discovery | `qualify-discovery-reboot.sh --vm rb-pcut-5f21eef6, 25 runs` | 25 | 0 | < 12% | [discovery-reboot](2026-11-27-power-cut-reboot/discovery-reboot.md) |

Zero failures in *n* independent runs bounds the true failure rate below 3/*n*
at 95% confidence. It says nothing about a rarer race.

The host evidence directories (under `/private/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T/reliaburger-pcut-evidence.7iIpxz`) are temporary and aren't kept.
The per-iteration records keep what each run verified.
