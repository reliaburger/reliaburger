# Power cuts and reboots on 964b56a2 (29 Oct 2026)

Release gate step 5's power-cut and reboot lanes ([releasing.md](../releasing.md#staging-a-candidate)), run by `qualify-power-cut-reboot.sh --commit 964b56a2 --iterations 25`.

**Result: every lane passed 25 of 25.** No iteration needed the VM start retry.

## Environment

| Item | Value |
|---|---|
| Host | Apple M2 Max, macOS 26.3.1, limactl version 2.1.0 |
| VM | `rb-pcut-964b56a2`, created for this run from [`power-cut-vm.yaml`](https://github.com/reliaburger/reliaburger/blob/964b56a2c41e2b43fb48aadef7d01839155c0dca/scripts/release/power-cut-vm.yaml) with 4 vCPUs, 6 GiB memory, 6 GiB swap and a 40 GiB disk, and only the candidate checkout mounted, read-only, all checked from inside the guest |
| Guest | kernel 6.8.0-147-generic |
| Source | [`964b56a2`](https://github.com/reliaburger/reliaburger/commit/964b56a2c41e2b43fb48aadef7d01839155c0dca), a clean checkout of it |
| Time | 2026-10-29 12:56 GMT to 2026-10-29 13:44 GMT |

## Results

| Lane | Claim | Command | Runs | Failures | 95% bound (rule of three) | Per-iteration record |
|---|---|---|---|---|---|---|
| C03 | Exporter checkpoint and lock | `qualify-storage-power-cut.sh --fixture exporter --iterations 25` | 25 | 0 | < 12% | [power-cut-exporter](power-cut-reboot/power-cut-exporter.md) |
| C11 | Lease stores | `qualify-storage-power-cut.sh --fixture leases --iterations 25` | 25 | 0 | < 12% | [power-cut-leases](power-cut-reboot/power-cut-leases.md) |
| Council backups | Upload, then prune to the newest 3 | `qualify-storage-power-cut.sh --fixture backups --iterations 25` | 25 | 0 | < 12% | [power-cut-backups](power-cut-reboot/power-cut-backups.md) |
| C47 | OCI holds and original execution | `qualify-oci-reboot.sh --vm rb-pcut-964b56a2, 25 runs` | 25 | 0 | < 12% | [oci-reboot](power-cut-reboot/oci-reboot.md) |
| C47 | Bun, kernel and discovery | `qualify-discovery-reboot.sh --vm rb-pcut-964b56a2, 25 runs` | 25 | 0 | < 12% | [discovery-reboot](power-cut-reboot/discovery-reboot.md) |

Zero failures in *n* independent runs bounds the true failure rate below 3/*n*
at 95% confidence. It says nothing about a rarer race.

The host evidence directories (under `/private/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T/reliaburger-pcut-evidence.r27sES`) are temporary and aren't kept.
The per-iteration records keep what each run verified.
