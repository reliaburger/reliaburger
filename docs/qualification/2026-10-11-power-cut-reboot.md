# Power cuts and reboots on a disposable VM (11 Oct 2026)

The storage power-cut and the OCI and discovery reboot qualifications, re-run
for [#691](https://github.com/reliaburger/reliaburger/issues/691). Their last
record was [25 September](2026-09-25-v02-power-cut.md), before 0.1.0 and
before [#613](https://github.com/reliaburger/reliaburger/pull/613) routed every
file lock through one guard. They now run on every release candidate
([release gate, step 5](../releasing.md#staging-a-candidate)).

**Result: every lane passed 25 of 25.** No iteration needed the VM start retry.

## Environment

| Item | Value |
|---|---|
| Host | Apple silicon, macOS 26.3.1, Lima 2.1.0 (VZ driver), other agents' builds running alongside |
| VM | `rb-691-crash`, created for this run from `~/.lima/reliaburger-test/lima.yaml` with 4 vCPUs, 6 GiB memory, 6 GiB swap and a 40 GiB disk; only the source checkout mounted, read-only; deleted afterwards |
| Guest | Ubuntu 24.04.5 LTS, kernel 6.8.0-142-generic, ext4 root mounted `commit=30` |
| Source | `ab18602bf9e6a9691d60853c690a4cabae1ef8d1` (the 0.2.0 train with [#676](https://github.com/reliaburger/reliaburger/issues/676) on top), a clean checkout of that commit |
| `power_cut` binary | SHA-256 `0d4e98edf5429e1fe0e97ea7a8165b5eb759027d98c68759b8e63a1931b4c185`, the same for all three storage lanes |
| Cut timing | storage lanes: uniform random 0–20 s after prepare confirmed its workers were acknowledging; reboot lanes: straight after prepare |

The shared development VM (`reliaburger-test`) wasn't touched; the drivers
refuse it by name.

## Results

Each lane ran as a batch of 10 and then a batch of 15, from the same build.

| Lane | Claim | Command | Runs | Failures | 95% bound (rule of three) | Per-iteration record |
|---|---|---|---|---|---|---|
| C03 | Exporter checkpoint and lock | `qualify-storage-power-cut.sh --fixture exporter` | 25 | 0 | < 12% | [first 10](2026-10-11-power-cut-reboot/power-cut-exporter-first-10.md), [next 15](2026-10-11-power-cut-reboot/power-cut-exporter-next-15.md) |
| C11 | Lease stores | `qualify-storage-power-cut.sh --fixture leases` | 25 | 0 | < 12% | [first 10](2026-10-11-power-cut-reboot/power-cut-leases-first-10.md), [next 15](2026-10-11-power-cut-reboot/power-cut-leases-next-15.md) |
| Council backups | Upload, then prune to the newest 3 | `qualify-storage-power-cut.sh --fixture backups` | 25 | 0 | < 12% | [first 10](2026-10-11-power-cut-reboot/power-cut-backups-first-10.md), [next 15](2026-10-11-power-cut-reboot/power-cut-backups-next-15.md) |
| C47 | OCI holds and original execution | `qualify-oci-reboot.sh --vm rb-691-crash` | 25 | 0 | < 12% | [all 25](2026-10-11-power-cut-reboot/oci-reboot.md) |
| C47 | Bun, kernel and discovery | `qualify-discovery-reboot.sh --vm rb-691-crash` | 25 | 0 | < 12% | [all 25](2026-10-11-power-cut-reboot/discovery-reboot.md) |

Zero failures in *n* independent runs bounds the true failure rate below 3/*n*
at 95% confidence, so 25 runs rule out rates above 12% and say nothing about
a rarer race.

The host evidence directories named in the per-iteration records lived in a
session scratch directory and weren't kept; the summaries each verify phase
printed are copied into those records.

## What this doesn't cover

- The snapshot uploader under a power cut still has no fixture (it needs Btrfs
  volumes), as on 25 September.
- These are debug builds of the test binaries on one Apple silicon host, not
  the release candidate's binaries; the release gate runs them on the
  candidate's commit.
