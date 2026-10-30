# Storage power-cut qualification: backups

- VM: `rb-pcut-964b56a2` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_council_backups`
- Commit: `964b56a2c41e2b43fb48aadef7d01839155c0dca`; binary SHA-256 `1a7409432456d8cad8d39026d4d3fd396241a524c3d8197490436582c8e046fc`
- Started: 2026-10-29T13:20:20Z
- Host evidence: `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-power-cut.19HkvW`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 12 | `/var/tmp/reliaburger-power-cut.PkAD7L` | PASS | `{"acknowledged":465,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":1}` |
| 2 | 7 | `/var/tmp/reliaburger-power-cut.HUlnq6` | PASS | `{"acknowledged":269,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 3 | 8 | `/var/tmp/reliaburger-power-cut.fMZmDm` | PASS | `{"acknowledged":323,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 4 | 11 | `/var/tmp/reliaburger-power-cut.MwAyPm` | PASS | `{"acknowledged":426,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 5 | 11 | `/var/tmp/reliaburger-power-cut.qYA7mY` | PASS | `{"acknowledged":448,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 6 | 10 | `/var/tmp/reliaburger-power-cut.gUElNJ` | PASS | `{"acknowledged":399,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 7 | 12 | `/var/tmp/reliaburger-power-cut.E1wpc5` | PASS | `{"acknowledged":454,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 8 | 10 | `/var/tmp/reliaburger-power-cut.ax1m9b` | PASS | `{"acknowledged":381,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 9 | 18 | `/var/tmp/reliaburger-power-cut.RiT7BT` | PASS | `{"acknowledged":691,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 10 | 10 | `/var/tmp/reliaburger-power-cut.1JQIJ3` | PASS | `{"acknowledged":401,"fixture":"backups","in_flight_landed":0,"present":4,"staged_leftovers":0}` |
| 11 | 10 | `/var/tmp/reliaburger-power-cut.MHnXjQ` | PASS | `{"acknowledged":384,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 12 | 5 | `/var/tmp/reliaburger-power-cut.vnwgWb` | PASS | `{"acknowledged":203,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 13 | 4 | `/var/tmp/reliaburger-power-cut.ES5Ofy` | PASS | `{"acknowledged":159,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 14 | 6 | `/var/tmp/reliaburger-power-cut.X6Sif8` | PASS | `{"acknowledged":238,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 15 | 4 | `/var/tmp/reliaburger-power-cut.fpHXaf` | PASS | `{"acknowledged":160,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 16 | 11 | `/var/tmp/reliaburger-power-cut.YR9WeU` | PASS | `{"acknowledged":412,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 17 | 20 | `/var/tmp/reliaburger-power-cut.XsIqm6` | PASS | `{"acknowledged":772,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 18 | 6 | `/var/tmp/reliaburger-power-cut.Uq7ovR` | PASS | `{"acknowledged":238,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 19 | 11 | `/var/tmp/reliaburger-power-cut.F6Bs9a` | PASS | `{"acknowledged":428,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 20 | 3 | `/var/tmp/reliaburger-power-cut.Tyn3qD` | PASS | `{"acknowledged":134,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 21 | 5 | `/var/tmp/reliaburger-power-cut.G5WxC2` | PASS | `{"acknowledged":196,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 22 | 0 | `/var/tmp/reliaburger-power-cut.91wArg` | PASS | `{"acknowledged":9,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 23 | 19 | `/var/tmp/reliaburger-power-cut.OzxEXF` | PASS | `{"acknowledged":731,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 24 | 14 | `/var/tmp/reliaburger-power-cut.BnaifI` | PASS | `{"acknowledged":556,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 25 | 20 | `/var/tmp/reliaburger-power-cut.SAcHlb` | PASS | `{"acknowledged":768,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
