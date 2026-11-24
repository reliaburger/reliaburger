# Volume snapshots under power cuts (23 Nov 2026)

Part of [#287](https://github.com/reliaburger/reliaburger/issues/287): the V02 leftover "qualify the snapshot uploader under power cuts", which needed Btrfs volumes. The laptop session ran it on a disposable Lima VM made from [`power-cut-vm.yaml`](../../scripts/release/power-cut-vm.yaml), with the new `volumes` fixture of [`qualify-storage-power-cut.sh`](../../scripts/release/qualify-storage-power-cut.sh).

**Result: 20 of 20 cuts passed.** No acknowledged write, archive, manifest or receipt was lost, and the first sweep after every boot recovered without an error.

## What each cut does

Prepare loop-mounts a fresh 1 GiB Btrfs image as Bun's volumes directory and provisions one managed volume (`default/db`, `/data`) as a subvolume. Two workers then run until the cut:

- a **writer** fills the volume with 4 KiB records: temporary name, `fsync`, rename, directory `fsync`, then a ledger line;
- a **sweeper** runs Bun's own `snapshot_tick` about once a second (snapshot, export to a `file://` destination on the VM's disk, prune to the newest 3), and records each snapshot whose metadata shows a receipt.

The driver cuts the VM's power (`limactl stop --force`) 0–20 s after prepare returns. Verify, on the next boot:

1. runs `btrfs check --readonly` on the image, then mounts it again;
2. checks every acknowledged record is in the live volume, whole;
3. checks every acknowledged archive is at the destination with the SHA-256 its receipt names, beside its manifest, and unpacks to records running from 0 with no gap;
4. checks every snapshot still on disk keeps the receipt it was acknowledged with;
5. runs one more sweep, as Bun does on boot: no errors, no subvolume without metadata, a new snapshot made, and everything exported and within retention.

## Run

- VM: `rb-pcut-287a` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_volume_snapshots`
- Commit: `6826ec5e36523cbfbf99a666ec185c89ac06bdfb`; binary SHA-256 `309c6ad3385cd59d4713ab712ebc9b62304a957266271e45dcddc37b1decbd1f`
- Started: 2026-11-23T14:24:58Z

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 10 | `/var/tmp/reliaburger-power-cut.TUcznl` | PASS | `{"exports":11,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":510,"listed_before_sweep":4,"uploaded_by_sweep":2,"write_landed_unacknowledged":0,"writes":568}` |
| 2 | 12 | `/var/tmp/reliaburger-power-cut.tf4Ez9` | PASS | `{"exports":13,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":632,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":655}` |
| 3 | 2 | `/var/tmp/reliaburger-power-cut.URi5xy` | PASS | `{"exports":6,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":275,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":298}` |
| 4 | 14 | `/var/tmp/reliaburger-power-cut.AnO9XW` | PASS | `{"exports":15,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":704,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":740}` |
| 5 | 16 | `/var/tmp/reliaburger-power-cut.hqZBS7` | PASS | `{"exports":16,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":783,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":832}` |
| 6 | 4 | `/var/tmp/reliaburger-power-cut.Br24Y4` | PASS | `{"exports":7,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":329,"listed_before_sweep":4,"uploaded_by_sweep":2,"write_landed_unacknowledged":0,"writes":372}` |
| 7 | 13 | `/var/tmp/reliaburger-power-cut.XZuJil` | PASS | `{"exports":14,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":696,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":711}` |
| 8 | 1 | `/var/tmp/reliaburger-power-cut.RnDENH` | PASS | `{"exports":5,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":195,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":237}` |
| 9 | 14 | `/var/tmp/reliaburger-power-cut.08QcMc` | PASS | `{"exports":14,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":716,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":763}` |
| 10 | 3 | `/var/tmp/reliaburger-power-cut.pipHEz` | PASS | `{"exports":7,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":264,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":289}` |
| 11 | 17 | `/var/tmp/reliaburger-power-cut.jTiUDP` | PASS | `{"exports":17,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":829,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":875}` |
| 12 | 0 | `/var/tmp/reliaburger-power-cut.T7iLFb` | PASS | `{"exports":5,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":198,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":204}` |
| 13 | 18 | `/var/tmp/reliaburger-power-cut.t9E1S3` | PASS | `{"exports":18,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":893,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":908}` |
| 14 | 19 | `/var/tmp/reliaburger-power-cut.PNugS1` | PASS | `{"exports":18,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":877,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":927}` |
| 15 | 2 | `/var/tmp/reliaburger-power-cut.1eaqhT` | PASS | `{"exports":6,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":237,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":267}` |
| 16 | 0 | `/var/tmp/reliaburger-power-cut.373daK` | PASS | `{"exports":5,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":186,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":192}` |
| 17 | 17 | `/var/tmp/reliaburger-power-cut.WDRNkV` | PASS | `{"exports":17,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":831,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":876}` |
| 18 | 1 | `/var/tmp/reliaburger-power-cut.iDtLHm` | PASS | `{"exports":5,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":214,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":261}` |
| 19 | 2 | `/var/tmp/reliaburger-power-cut.Qxb2Qc` | PASS | `{"exports":6,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":266,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":299}` |
| 20 | 0 | `/var/tmp/reliaburger-power-cut.uqztxt` | PASS | `{"exports":5,"fixture":"volumes","interrupted_removed":0,"largest_archive_records":198,"listed_before_sweep":4,"uploaded_by_sweep":1,"write_landed_unacknowledged":0,"writes":203}` |

20 of 20 iterations passed. Rule-of-three bound: failure rate < 15.0% at 95% confidence.

## What it doesn't show

- **No cut caught a snapshot half made.** `interrupted_removed` is 0 in every run: `btrfs subvolume snapshot` takes milliseconds, so a random cut almost never lands inside it. The intent files that settle that case (#695) are covered by unit tests, not by this run.
- **No cut caught a write half acknowledged.** `write_landed_unacknowledged` is 0 everywhere: a record's rename and its ledger line are microseconds apart.
- **Cuts between an export and its receipt did happen.** Runs 1 and 6 shipped 2 snapshots in the recovery sweep: a snapshot whose archive had landed before the cut but whose receipt hadn't. Content addressing made the second upload a no-op, as designed.
- **A loop device on a VM disk is not an eMMC.** The loop driver turns Btrfs's flushes into `fsync`s of its backing file. The Wyse lab doesn't cut power by hand (maintainer decision, 22 November), so there is no bare-metal run.
- **Only the `file://` destination.** S3 and GCS acknowledge a write only once it's stored; `make test-s3` covers their round trip.
