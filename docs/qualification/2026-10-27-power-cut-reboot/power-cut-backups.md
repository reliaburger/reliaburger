# Storage power-cut qualification: backups

- VM: `rb-pcut-d387d569` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_council_backups`
- Commit: `d387d569bb1f18264382ffcffebe3bbf45fb7ed3`; binary SHA-256 `75bf9f19654613b2d8913f41c18654a062a69c43fff7cf40aa84bef2cbff4bce`
- Started: 2026-10-27T12:03:01Z
- Host evidence: `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-power-cut.PGyOiE`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 7 | `/var/tmp/reliaburger-power-cut.Bq8lCs` | PASS | `{"acknowledged":258,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 2 | 20 | `/var/tmp/reliaburger-power-cut.6qizXS` | PASS | `{"acknowledged":774,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 3 | 2 | `/var/tmp/reliaburger-power-cut.RbBxjO` | PASS | `{"acknowledged":79,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 4 | 7 | `/var/tmp/reliaburger-power-cut.g0EVXW` | PASS | `{"acknowledged":272,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 5 | 14 | `/var/tmp/reliaburger-power-cut.NOtBb5` | PASS | `{"acknowledged":531,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 6 | 9 | `/var/tmp/reliaburger-power-cut.dCDjXS` | PASS | `{"acknowledged":345,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 7 | 5 | `/var/tmp/reliaburger-power-cut.bA1rYj` | PASS | `{"acknowledged":211,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 8 | 9 | `/var/tmp/reliaburger-power-cut.F9AXiE` | PASS | `{"acknowledged":352,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 9 | 11 | `/var/tmp/reliaburger-power-cut.6PKdRP` | PASS | `{"acknowledged":417,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 10 | 3 | `/var/tmp/reliaburger-power-cut.d4FVSs` | PASS | `{"acknowledged":123,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 11 | 13 | `/var/tmp/reliaburger-power-cut.wCP6l4` | PASS | `{"acknowledged":526,"fixture":"backups","in_flight_landed":1,"present":4,"staged_leftovers":0}` |
| 12 | 6 | `/var/tmp/reliaburger-power-cut.l5ErLs` | PASS | `{"acknowledged":238,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 13 | 7 | `/var/tmp/reliaburger-power-cut.kQU4Zc` | PASS | `{"acknowledged":265,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 14 | 18 | `/var/tmp/reliaburger-power-cut.2hHkSm` | PASS | `{"acknowledged":696,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 15 | 7 | `/var/tmp/reliaburger-power-cut.U4FTsK` | PASS | `{"acknowledged":278,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 16 | 9 | `/var/tmp/reliaburger-power-cut.NZKbjv` | PASS | `{"acknowledged":349,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 17 | 2 | `/var/tmp/reliaburger-power-cut.Nd1zNM` | PASS | `{"acknowledged":85,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 18 | 12 | `/var/tmp/reliaburger-power-cut.hYX7GF` | PASS | `{"acknowledged":449,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 19 | 14 | `/var/tmp/reliaburger-power-cut.v6SwdS` | PASS | `{"acknowledged":540,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 20 | 5 | `/var/tmp/reliaburger-power-cut.ezB5WN` | PASS | `{"acknowledged":199,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 21 | 15 | `/var/tmp/reliaburger-power-cut.hVoooa` | PASS | `{"acknowledged":579,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 22 | 12 | `/var/tmp/reliaburger-power-cut.pCG06m` | PASS | `{"acknowledged":471,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 23 | 20 | `/var/tmp/reliaburger-power-cut.ZeeEEV` | PASS | `{"acknowledged":749,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 24 | 1 | `/var/tmp/reliaburger-power-cut.xygs4D` | PASS | `{"acknowledged":45,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":1}` |
| 25 | 0 | `/var/tmp/reliaburger-power-cut.jPxAI1` | PASS | `{"acknowledged":10,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
