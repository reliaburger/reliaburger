# Power cuts and reboots on abec2c45 (21 Oct 2026)

Release gate step 5's power-cut and reboot lanes ([releasing.md](../releasing.md#staging-a-candidate)), run by `qualify-power-cut-reboot.sh --commit abec2c45 --iterations 25`.

**Result: every lane passed 25 of 25.** No iteration needed the VM start retry.

Every lane ran 25 times rather than 10, because 0.2.1 changes both paths they
cover. It upgrades redb to 4.x so that a power cut while the Raft store grows
can't leave it unopenable
([#1024](https://github.com/reliaburger/reliaburger/issues/1024),
[#1028](https://github.com/reliaburger/reliaburger/pull/1028)), and
`relish local stop` now powers VMs off from inside rather than cutting their
power ([#1023](https://github.com/reliaburger/reliaburger/issues/1023),
[#1025](https://github.com/reliaburger/reliaburger/pull/1025)). Nothing else
loaded the host: the soak cluster had been torn down and no build or test was
running.

## Environment

| Item | Value |
|---|---|
| Host | Apple M2 Max, macOS 26.3.1, limactl version 2.1.0 |
| VM | `rb-pcut-abec2c45`, created for this run from [`power-cut-vm.yaml`](https://github.com/reliaburger/reliaburger/blob/abec2c459d559652ced52a40d5c34973571cef9b/scripts/release/power-cut-vm.yaml) with 4 vCPUs, 6 GiB memory, 6 GiB swap and a 40 GiB disk, and only the candidate checkout mounted, read-only, all checked from inside the guest |
| Guest | kernel 6.8.0-147-generic |
| Source | [`abec2c45`](https://github.com/reliaburger/reliaburger/commit/abec2c459d559652ced52a40d5c34973571cef9b), a clean checkout of it |
| Time | 2026-10-21 09:51 +08 to 2026-10-21 10:47 +08 |

## Results

| Lane | Claim | Command | Runs | Failures | 95% bound (rule of three) | Per-iteration record |
|---|---|---|---|---|---|---|
| C03 | Exporter checkpoint and lock | `qualify-storage-power-cut.sh --fixture exporter --iterations 25` | 25 | 0 | < 12% | [power-cut-exporter](2026-10-21-power-cut-reboot/power-cut-exporter.md) |
| C11 | Lease stores | `qualify-storage-power-cut.sh --fixture leases --iterations 25` | 25 | 0 | < 12% | [power-cut-leases](2026-10-21-power-cut-reboot/power-cut-leases.md) |
| Council backups | Upload, then prune to the newest 3 | `qualify-storage-power-cut.sh --fixture backups --iterations 25` | 25 | 0 | < 12% | [power-cut-backups](2026-10-21-power-cut-reboot/power-cut-backups.md) |
| C47 | OCI holds and original execution | `qualify-oci-reboot.sh --vm rb-pcut-abec2c45, 25 runs` | 25 | 0 | < 12% | [oci-reboot](2026-10-21-power-cut-reboot/oci-reboot.md) |
| C47 | Bun, kernel and discovery | `qualify-discovery-reboot.sh --vm rb-pcut-abec2c45, 25 runs` | 25 | 0 | < 12% | [discovery-reboot](2026-10-21-power-cut-reboot/discovery-reboot.md) |

Zero failures in *n* independent runs bounds the true failure rate below 3/*n*
at 95% confidence. It says nothing about a rarer race.

The host evidence directories (under `/private/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T/reliaburger-pcut-evidence.1GHx7h`) are temporary and aren't kept.
The per-iteration records keep what each run verified.
