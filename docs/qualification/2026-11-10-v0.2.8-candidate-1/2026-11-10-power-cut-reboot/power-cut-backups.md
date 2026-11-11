# Storage power-cut qualification: backups

- VM: `rb-pcut-08618555` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_council_backups`
- Commit: `08618555ffe3112f2bd67842cadedfdd61d8071c`; binary SHA-256 `1069749d07dc37f6711f6c8bfeb9fa0267ad00d7f9305ec9617fae15fdb5edf2`
- Started: 2026-11-10T17:30:18Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.mGiXIB`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 20 | `/var/tmp/reliaburger-power-cut.xMTXpg` | PASS | `{"acknowledged":830,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 2 | 15 | `/var/tmp/reliaburger-power-cut.QrVLUP` | PASS | `{"acknowledged":603,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 3 | 17 | `/var/tmp/reliaburger-power-cut.MNuNuE` | PASS | `{"acknowledged":689,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 4 | 14 | `/var/tmp/reliaburger-power-cut.kAYtIo` | PASS | `{"acknowledged":571,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 5 | 9 | `/var/tmp/reliaburger-power-cut.m31dSS` | PASS | `{"acknowledged":368,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 6 | 10 | `/var/tmp/reliaburger-power-cut.EzXKRD` | PASS | `{"acknowledged":412,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 7 | 20 | `/var/tmp/reliaburger-power-cut.SjWtN6` | PASS | `{"acknowledged":808,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 8 | 7 | `/var/tmp/reliaburger-power-cut.YpAbYN` | PASS | `{"acknowledged":295,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 9 | 10 | `/var/tmp/reliaburger-power-cut.YwWTVD` | PASS | `{"acknowledged":415,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 10 | 8 | `/var/tmp/reliaburger-power-cut.cNc9bn` | PASS | `{"acknowledged":330,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 11 | 2 | `/var/tmp/reliaburger-power-cut.r2s9Wv` | PASS | `{"acknowledged":92,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 12 | 2 | `/var/tmp/reliaburger-power-cut.pEwbtq` | PASS | `{"acknowledged":94,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 13 | 0 | `/var/tmp/reliaburger-power-cut.LDPp5c` | PASS | `{"acknowledged":8,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 14 | 5 | `/var/tmp/reliaburger-power-cut.7HlyfH` | PASS | `{"acknowledged":217,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 15 | 13 | `/var/tmp/reliaburger-power-cut.hThcJW` | PASS | `{"acknowledged":527,"fixture":"backups","in_flight_landed":0,"present":4,"staged_leftovers":0}` |
| 16 | 9 | `/var/tmp/reliaburger-power-cut.OmWv3W` | PASS | `{"acknowledged":387,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 17 | 10 | `/var/tmp/reliaburger-power-cut.u4oszD` | PASS | `{"acknowledged":419,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 18 | 15 | `/var/tmp/reliaburger-power-cut.DUdfRa` | PASS | `{"acknowledged":633,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 19 | 7 | `/var/tmp/reliaburger-power-cut.8fI6z5` | PASS | `{"acknowledged":300,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 20 | 3 | `/var/tmp/reliaburger-power-cut.pellO8` | PASS | `{"acknowledged":130,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 21 | 20 | `/var/tmp/reliaburger-power-cut.dXrvzH` | PASS | `{"acknowledged":787,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 22 | 17 | `/var/tmp/reliaburger-power-cut.Dx8hi1` | PASS | `{"acknowledged":703,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 23 | 2 | `/var/tmp/reliaburger-power-cut.44PiKy` | PASS | `{"acknowledged":94,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 24 | 5 | `/var/tmp/reliaburger-power-cut.GEKp8f` | PASS | `{"acknowledged":217,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 25 | 4 | `/var/tmp/reliaburger-power-cut.YmoPS0` | PASS | `{"acknowledged":169,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
