# Power cuts and reboots on 08618555 (10 Nov 2026)

Release gate step 5's power-cut and reboot lanes ([releasing.md](../../releasing.md#staging-a-candidate)), run by `qualify-power-cut-reboot.sh --commit 08618555 --iterations 25`.

**Result: every lane passed 25 of 25.** No iteration needed the VM start retry.

## Environment

| Item | Value |
|---|---|
| Host | Apple M5 Max, macOS 27.2, limactl version 2.2.1 |
| VM | `rb-pcut-08618555`, created for this run from [`power-cut-vm.yaml`](https://github.com/reliaburger/reliaburger/blob/08618555ffe3112f2bd67842cadedfdd61d8071c/scripts/release/power-cut-vm.yaml) with 4 vCPUs, 6 GiB memory, 6 GiB swap and a 40 GiB disk, and only the candidate checkout mounted, read-only, all checked from inside the guest |
| Guest | kernel 6.8.0-147-generic |
| Source | [`08618555`](https://github.com/reliaburger/reliaburger/commit/08618555ffe3112f2bd67842cadedfdd61d8071c), a clean checkout of it |
| Time | 2026-11-10 17:10 GMT to 2026-11-10 17:50 GMT |

## Results

| Lane | Claim | Command | Runs | Failures | 95% bound (rule of three) | Per-iteration record |
|---|---|---|---|---|---|---|
| C03 | Exporter checkpoint and lock | `qualify-storage-power-cut.sh --fixture exporter --iterations 25` | 25 | 0 | < 12% | [power-cut-exporter](2026-11-10-power-cut-reboot/power-cut-exporter.md) |
| C11 | Lease stores | `qualify-storage-power-cut.sh --fixture leases --iterations 25` | 25 | 0 | < 12% | [power-cut-leases](2026-11-10-power-cut-reboot/power-cut-leases.md) |
| Council backups | Upload, then prune to the newest 3 | `qualify-storage-power-cut.sh --fixture backups --iterations 25` | 25 | 0 | < 12% | [power-cut-backups](2026-11-10-power-cut-reboot/power-cut-backups.md) |
| C47 | OCI holds and original execution | `qualify-oci-reboot.sh --vm rb-pcut-08618555, 25 runs` | 25 | 0 | < 12% | [oci-reboot](2026-11-10-power-cut-reboot/oci-reboot.md) |
| C47 | Bun, kernel and discovery | `qualify-discovery-reboot.sh --vm rb-pcut-08618555, 25 runs` | 25 | 0 | < 12% | [discovery-reboot](2026-11-10-power-cut-reboot/discovery-reboot.md) |

Zero failures in *n* independent runs bounds the true failure rate below 3/*n*
at 95% confidence. It says nothing about a rarer race.

The host evidence directories (under `/private/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T/reliaburger-pcut-evidence.pztBeB`) are temporary and aren't kept.
The per-iteration records keep what each run verified.

## Host load

These lanes ran on the Mac Studio while candidate 1's build waited for a
hosted macOS runner, before the soaks. Until about 17:18 GMT, a stray
one-node patch cluster (a 2-vCPU Lima VM left from an earlier debugging
run, [#1847](https://github.com/reliaburger/reliaburger/issues/1847)), the idle
`reliaburger-test` development VM and Ollama were also running; then
they were stopped. Nothing else was built or tested on the host. 25 runs
per lane, as 0.2.7's gate ran, because 0.2.8 changes the agent loop's
discovery and egress ownership.
