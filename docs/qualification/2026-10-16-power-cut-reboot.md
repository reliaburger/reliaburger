# Power cuts and reboots on candidate 8 (16 Oct 2026)

These are release gate step 5's power-cut and reboot lanes
([releasing.md](../releasing.md#staging-a-candidate)), run on 0.2.0's
candidate 8. Every lane ran 25 times rather than 10, because 0.2.0 changes
locking and ownership (`src/file_lock.rs`, discovery ownership) since the
[11 October record](2026-10-11-power-cut-reboot.md).

**Result: every lane passed 25 of 25.** No iteration needed the VM start retry.

## Environment

| Item | Value |
|---|---|
| Host | Apple M2 Max, macOS 26.3.1, Lima 2.1.0 (VZ driver), no other load on the host |
| VM | `rb-020-pcut`, created by hand for this run from `~/.lima/reliaburger-test/lima.yaml`, with 4 vCPUs, 6 GiB memory, 6 GiB swap and a 40 GiB disk, and only the candidate checkout mounted, read-only. Deleted afterwards. A script to do this is [#988](https://github.com/reliaburger/reliaburger/issues/988) |
| Guest | Ubuntu 24.04, kernel 6.8.0-142-generic, ext4 root mounted `commit=30` |
| Source | [`cc5f6439`](https://github.com/reliaburger/reliaburger/commit/cc5f64393350f2081f7d88512b95a88316172e5a), the candidate's commit, a clean checkout of it |
| `power_cut` binary | SHA-256 `33f3ba84343c37d41b6ebb9298e09d17b8e009d385ed84c0eb25ce5146ee3c56`, the same for all three storage lanes |
| Time | 12:26 to 13:28 (+08), 16 October 2026: storage lanes 12:26–13:05, reboot lanes 13:05–13:28 |

## Results

| Lane | Claim | Command | Runs | Failures | 95% bound (rule of three) | Per-iteration record |
|---|---|---|---|---|---|---|
| C03 | Exporter checkpoint and lock | `qualify-storage-power-cut.sh --fixture exporter --iterations 25` | 25 | 0 | < 12% | [exporter](2026-10-16-power-cut-reboot/power-cut-exporter.md) |
| C11 | Lease stores | `qualify-storage-power-cut.sh --fixture leases --iterations 25` | 25 | 0 | < 12% | [leases](2026-10-16-power-cut-reboot/power-cut-leases.md) |
| Council backups | Upload, then prune to the newest 3 | `qualify-storage-power-cut.sh --fixture backups --iterations 25` | 25 | 0 | < 12% | [backups](2026-10-16-power-cut-reboot/power-cut-backups.md) |
| C47 | OCI holds and original execution | `qualify-oci-reboot.sh --vm rb-020-pcut`, 25 times | 25 | 0 | < 12% | [all 25](2026-10-16-power-cut-reboot/oci-reboot.md) |
| C47 | Bun, kernel and discovery | `qualify-discovery-reboot.sh --vm rb-020-pcut`, 25 times | 25 | 0 | < 12% | [all 25](2026-10-16-power-cut-reboot/discovery-reboot.md) |

Zero failures in *n* independent runs bounds the true failure rate below 3/*n*
at 95% confidence. So 25 runs rule out rates above 12%, and say nothing about
a rarer race.

The host evidence directories were temporary and weren't kept. The per-iteration
records keep each verified summary and each verify step's last lines.

## What this doesn't cover

- The snapshot uploader under a power cut still has no fixture (it needs Btrfs
  volumes), as on 11 October.
- These are debug builds of the test binaries at the candidate's commit, not
  the release binaries.
