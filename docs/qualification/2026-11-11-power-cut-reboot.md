# Power cuts and reboots on e423d469 (11 Nov 2026)

Release gate step 5's power-cut and reboot lanes ([releasing.md](../releasing.md#staging-a-candidate)), run by `qualify-power-cut-reboot.sh --commit e423d469 --iterations 25`.

**Result: every lane passed 25 of 25.** No iteration needed the VM start retry.

## Environment

| Item | Value |
|---|---|
| Host | Apple M5 Max, macOS 27.2, limactl version 2.2.1 |
| VM | `rb-pcut-e423d469`, created for this run from [`power-cut-vm.yaml`](https://github.com/reliaburger/reliaburger/blob/e423d469a93b04d7bdc64582be1922ac5f453fd1/scripts/release/power-cut-vm.yaml) with 4 vCPUs, 6 GiB memory, 6 GiB swap and a 40 GiB disk, and only the candidate checkout mounted, read-only, all checked from inside the guest |
| Guest | kernel 6.8.0-147-generic |
| Source | [`e423d469`](https://github.com/reliaburger/reliaburger/commit/e423d469a93b04d7bdc64582be1922ac5f453fd1), a clean checkout of it |
| Time | 2026-11-11 12:54 GMT to 2026-11-11 13:58 GMT |

## Results

| Lane | Claim | Command | Runs | Failures | 95% bound (rule of three) | Per-iteration record |
|---|---|---|---|---|---|---|
| C03 | Exporter checkpoint and lock | `qualify-storage-power-cut.sh --fixture exporter --iterations 25` | 25 | 0 | < 12% | [power-cut-exporter](2026-11-11-power-cut-reboot/power-cut-exporter.md) |
| C11 | Lease stores | `qualify-storage-power-cut.sh --fixture leases --iterations 25` | 25 | 0 | < 12% | [power-cut-leases](2026-11-11-power-cut-reboot/power-cut-leases.md) |
| Council backups | Upload, then prune to the newest 3 | `qualify-storage-power-cut.sh --fixture backups --iterations 25` | 25 | 0 | < 12% | [power-cut-backups](2026-11-11-power-cut-reboot/power-cut-backups.md) |
| C47 | OCI holds and original execution | `qualify-oci-reboot.sh --vm rb-pcut-e423d469, 25 runs` | 25 | 0 | < 12% | [oci-reboot](2026-11-11-power-cut-reboot/oci-reboot.md) |
| C47 | Bun, kernel and discovery | `qualify-discovery-reboot.sh --vm rb-pcut-e423d469, 25 runs` | 25 | 0 | < 12% | [discovery-reboot](2026-11-11-power-cut-reboot/discovery-reboot.md) |

Zero failures in *n* independent runs bounds the true failure rate below 3/*n*
at 95% confidence. It says nothing about a rarer race.

The host evidence directories (under `/private/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T/reliaburger-pcut-evidence.9epZ9G`) are temporary and aren't kept.
The per-iteration records keep what each run verified.

## Host load

These lanes ran first in candidate 3's gate, on the quiet Mac Studio, after
the soak build was compiled in the development VM and that VM was stopped.
Nothing else ran on the host. 25 runs per lane, because 0.2.8's fixes change
startup adoption after a power cut
([#1851](https://github.com/reliaburger/reliaburger/pull/1851)) and where the
agent loop's record writes and file calls run
([#1854](https://github.com/reliaburger/reliaburger/pull/1854),
[#1858](https://github.com/reliaburger/reliaburger/pull/1858)). They took
64 minutes, against candidate 2's 45: VM starts were slower throughout
candidate 3's gate, with no load on the host to explain it.
