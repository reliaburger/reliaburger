# Power cuts and reboots on 43baee64 (25 Oct 2026)

Release gate step 5's power-cut and reboot lanes ([releasing.md](../releasing.md#staging-a-candidate)), run by `qualify-power-cut-reboot.sh --commit 43baee64 --iterations 25`.

**Result: every lane passed 25 of 25.** No iteration needed the VM start retry.

## Environment

| Item | Value |
|---|---|
| Host | Apple M2 Max, macOS 26.3.1, limactl version 2.1.0 |
| VM | `rb-pcut-43baee64`, created for this run from [`power-cut-vm.yaml`](https://github.com/reliaburger/reliaburger/blob/43baee64ae9689b7620f52c33e900acf8a407c43/scripts/release/power-cut-vm.yaml) with 4 vCPUs, 6 GiB memory, 6 GiB swap and a 40 GiB disk, and only the candidate checkout mounted, read-only, all checked from inside the guest |
| Guest | kernel 6.8.0-147-generic |
| Source | [`43baee64`](https://github.com/reliaburger/reliaburger/commit/43baee64ae9689b7620f52c33e900acf8a407c43), a clean checkout of it |
| Time | 2026-10-25 14:20 GMT to 2026-10-25 15:09 GMT |

## Results

| Lane | Claim | Command | Runs | Failures | 95% bound (rule of three) | Per-iteration record |
|---|---|---|---|---|---|---|
| C03 | Exporter checkpoint and lock | `qualify-storage-power-cut.sh --fixture exporter --iterations 25` | 25 | 0 | < 12% | [power-cut-exporter](2026-10-25-power-cut-reboot/power-cut-exporter.md) |
| C11 | Lease stores | `qualify-storage-power-cut.sh --fixture leases --iterations 25` | 25 | 0 | < 12% | [power-cut-leases](2026-10-25-power-cut-reboot/power-cut-leases.md) |
| Council backups | Upload, then prune to the newest 3 | `qualify-storage-power-cut.sh --fixture backups --iterations 25` | 25 | 0 | < 12% | [power-cut-backups](2026-10-25-power-cut-reboot/power-cut-backups.md) |
| C47 | OCI holds and original execution | `qualify-oci-reboot.sh --vm rb-pcut-43baee64, 25 runs` | 25 | 0 | < 12% | [oci-reboot](2026-10-25-power-cut-reboot/oci-reboot.md) |
| C47 | Bun, kernel and discovery | `qualify-discovery-reboot.sh --vm rb-pcut-43baee64, 25 runs` | 25 | 0 | < 12% | [discovery-reboot](2026-10-25-power-cut-reboot/discovery-reboot.md) |

Zero failures in *n* independent runs bounds the true failure rate below 3/*n*
at 95% confidence. It says nothing about a rarer race.

The host evidence directories (under `/private/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T/reliaburger-pcut-evidence.zyyj4O`) are temporary and aren't kept.
The per-iteration records keep what each run verified.
