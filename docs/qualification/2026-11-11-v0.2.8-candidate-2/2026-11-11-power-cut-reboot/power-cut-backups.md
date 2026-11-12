# Storage power-cut qualification: backups

- VM: `rb-pcut-e43417a8` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_council_backups`
- Commit: `e43417a8a2a147910667ba9a075af03e0fcb6a8d`; binary SHA-256 `e73fed7fafe6d7c8bf9e57a0841877ebcbf91ac258c92308772dc8685f8d8802`
- Started: 2026-11-11T01:12:12Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.PZJKY0`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 20 | `/var/tmp/reliaburger-power-cut.M8xuYL` | PASS | `{"acknowledged":846,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 2 | 2 | `/var/tmp/reliaburger-power-cut.HUgUEf` | PASS | `{"acknowledged":95,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 3 | 1 | `/var/tmp/reliaburger-power-cut.uszYWK` | PASS | `{"acknowledged":49,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 4 | 19 | `/var/tmp/reliaburger-power-cut.8oAxav` | PASS | `{"acknowledged":774,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 5 | 4 | `/var/tmp/reliaburger-power-cut.bzfnVg` | PASS | `{"acknowledged":185,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 6 | 15 | `/var/tmp/reliaburger-power-cut.015k5F` | PASS | `{"acknowledged":624,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 7 | 19 | `/var/tmp/reliaburger-power-cut.t4j295` | PASS | `{"acknowledged":788,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 8 | 3 | `/var/tmp/reliaburger-power-cut.sSXgKI` | PASS | `{"acknowledged":131,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 9 | 0 | `/var/tmp/reliaburger-power-cut.GUKd0B` | PASS | `{"acknowledged":10,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 10 | 0 | `/var/tmp/reliaburger-power-cut.CylDWt` | PASS | `{"acknowledged":7,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 11 | 15 | `/var/tmp/reliaburger-power-cut.DcyEL3` | PASS | `{"acknowledged":608,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 12 | 17 | `/var/tmp/reliaburger-power-cut.hlw2tK` | PASS | `{"acknowledged":699,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 13 | 7 | `/var/tmp/reliaburger-power-cut.GObYl5` | PASS | `{"acknowledged":307,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 14 | 16 | `/var/tmp/reliaburger-power-cut.eMhKDz` | PASS | `{"acknowledged":657,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 15 | 15 | `/var/tmp/reliaburger-power-cut.uQErZq` | PASS | `{"acknowledged":621,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 16 | 11 | `/var/tmp/reliaburger-power-cut.thOdKQ` | PASS | `{"acknowledged":464,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 17 | 3 | `/var/tmp/reliaburger-power-cut.g14DXj` | PASS | `{"acknowledged":128,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 18 | 3 | `/var/tmp/reliaburger-power-cut.EmskFD` | PASS | `{"acknowledged":130,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 19 | 4 | `/var/tmp/reliaburger-power-cut.b2YDj5` | PASS | `{"acknowledged":181,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 20 | 8 | `/var/tmp/reliaburger-power-cut.ZAsbuu` | PASS | `{"acknowledged":333,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 21 | 19 | `/var/tmp/reliaburger-power-cut.eEBxCT` | PASS | `{"acknowledged":777,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 22 | 18 | `/var/tmp/reliaburger-power-cut.LZIYA9` | PASS | `{"acknowledged":731,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 23 | 19 | `/var/tmp/reliaburger-power-cut.pEclEG` | PASS | `{"acknowledged":764,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 24 | 6 | `/var/tmp/reliaburger-power-cut.eEUNON` | PASS | `{"acknowledged":240,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 25 | 10 | `/var/tmp/reliaburger-power-cut.xAf9qk` | PASS | `{"acknowledged":405,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
