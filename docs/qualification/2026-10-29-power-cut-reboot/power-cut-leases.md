# Storage power-cut qualification: leases

- VM: `rb-pcut-964b56a2` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_lease_operations`
- Commit: `964b56a2c41e2b43fb48aadef7d01839155c0dca`; binary SHA-256 `1a7409432456d8cad8d39026d4d3fd396241a524c3d8197490436582c8e046fc`
- Started: 2026-10-29T13:12:06Z
- Host evidence: `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-power-cut.YoTWkH`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 2 | `/var/tmp/reliaburger-power-cut.yZR7FA` | PASS | `{"acknowledged":311,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 2 | 8 | `/var/tmp/reliaburger-power-cut.MHHcCZ` | PASS | `{"acknowledged":1163,"fixture":"leases","in_flight_landed":1,"live_leases":20,"stale_temporaries":0}` |
| 3 | 13 | `/var/tmp/reliaburger-power-cut.ifpzGE` | PASS | `{"acknowledged":1903,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 4 | 9 | `/var/tmp/reliaburger-power-cut.YPQQIg` | PASS | `{"acknowledged":1313,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 5 | 1 | `/var/tmp/reliaburger-power-cut.IcBeSY` | PASS | `{"acknowledged":157,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 6 | 11 | `/var/tmp/reliaburger-power-cut.zRRiib` | PASS | `{"acknowledged":1599,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 7 | 19 | `/var/tmp/reliaburger-power-cut.EQcqdx` | PASS | `{"acknowledged":2718,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 8 | 11 | `/var/tmp/reliaburger-power-cut.ryfoRS` | PASS | `{"acknowledged":1589,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 9 | 3 | `/var/tmp/reliaburger-power-cut.9gn9R1` | PASS | `{"acknowledged":460,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 10 | 10 | `/var/tmp/reliaburger-power-cut.XIZNTT` | PASS | `{"acknowledged":1494,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 11 | 3 | `/var/tmp/reliaburger-power-cut.gcUXsU` | PASS | `{"acknowledged":451,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 12 | 10 | `/var/tmp/reliaburger-power-cut.SUKDVS` | PASS | `{"acknowledged":1477,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 13 | 19 | `/var/tmp/reliaburger-power-cut.khGaZu` | PASS | `{"acknowledged":2739,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 14 | 12 | `/var/tmp/reliaburger-power-cut.mcVTrH` | PASS | `{"acknowledged":1731,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 15 | 5 | `/var/tmp/reliaburger-power-cut.xeI2e8` | PASS | `{"acknowledged":777,"fixture":"leases","in_flight_landed":1,"live_leases":12,"stale_temporaries":0}` |
| 16 | 4 | `/var/tmp/reliaburger-power-cut.xbsXGe` | PASS | `{"acknowledged":602,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 17 | 18 | `/var/tmp/reliaburger-power-cut.z26BZ6` | PASS | `{"acknowledged":2630,"fixture":"leases","in_flight_landed":0,"live_leases":15,"stale_temporaries":0}` |
| 18 | 14 | `/var/tmp/reliaburger-power-cut.ckwRgX` | PASS | `{"acknowledged":2024,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 19 | 17 | `/var/tmp/reliaburger-power-cut.GujldX` | PASS | `{"acknowledged":2501,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 20 | 9 | `/var/tmp/reliaburger-power-cut.mhe63d` | PASS | `{"acknowledged":1286,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 21 | 7 | `/var/tmp/reliaburger-power-cut.crvwGQ` | PASS | `{"acknowledged":1021,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 22 | 1 | `/var/tmp/reliaburger-power-cut.mAof4f` | PASS | `{"acknowledged":171,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 23 | 6 | `/var/tmp/reliaburger-power-cut.NG1Cw3` | PASS | `{"acknowledged":864,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 24 | 11 | `/var/tmp/reliaburger-power-cut.7OwetH` | PASS | `{"acknowledged":1638,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 25 | 17 | `/var/tmp/reliaburger-power-cut.05BzUz` | PASS | `{"acknowledged":2499,"fixture":"leases","in_flight_landed":2,"live_leases":20,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
