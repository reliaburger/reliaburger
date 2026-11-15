# Storage power-cut qualification: leases

- VM: `rb-pcut-50596cfe` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_lease_operations`
- Commit: `50596cfe9438d16070b1660db45b1d2a29f77106`; binary SHA-256 `fcb2942a43e0d89b809c9a478c9e9c70050375c6ddb82d112d37b92243ab3f58`
- Started: 2026-11-05T12:22:44Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.9A0xcB`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 8 | `/var/tmp/reliaburger-power-cut.AVzy97` | PASS | `{"acknowledged":1226,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 2 | 17 | `/var/tmp/reliaburger-power-cut.Exu6QG` | PASS | `{"acknowledged":2567,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 3 | 7 | `/var/tmp/reliaburger-power-cut.OGXd20` | PASS | `{"acknowledged":1058,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 4 | 14 | `/var/tmp/reliaburger-power-cut.9GaHcp` | PASS | `{"acknowledged":2105,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 5 | 10 | `/var/tmp/reliaburger-power-cut.1ZLj8H` | PASS | `{"acknowledged":1465,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 6 | 10 | `/var/tmp/reliaburger-power-cut.C7mSGI` | PASS | `{"acknowledged":1530,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 7 | 13 | `/var/tmp/reliaburger-power-cut.9b6F43` | PASS | `{"acknowledged":1980,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 8 | 2 | `/var/tmp/reliaburger-power-cut.HCzwIR` | PASS | `{"acknowledged":327,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 9 | 6 | `/var/tmp/reliaburger-power-cut.RoKNDj` | PASS | `{"acknowledged":921,"fixture":"leases","in_flight_landed":0,"live_leases":24,"stale_temporaries":0}` |
| 10 | 10 | `/var/tmp/reliaburger-power-cut.GoPRVO` | PASS | `{"acknowledged":1503,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 11 | 6 | `/var/tmp/reliaburger-power-cut.YMikp3` | PASS | `{"acknowledged":921,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 12 | 15 | `/var/tmp/reliaburger-power-cut.AegGMJ` | PASS | `{"acknowledged":2242,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 13 | 12 | `/var/tmp/reliaburger-power-cut.y6QDJA` | PASS | `{"acknowledged":1803,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 14 | 13 | `/var/tmp/reliaburger-power-cut.2piigx` | PASS | `{"acknowledged":1940,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 15 | 11 | `/var/tmp/reliaburger-power-cut.ECnRAu` | PASS | `{"acknowledged":1643,"fixture":"leases","in_flight_landed":1,"live_leases":22,"stale_temporaries":0}` |
| 16 | 11 | `/var/tmp/reliaburger-power-cut.fcHDUi` | PASS | `{"acknowledged":1622,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 17 | 17 | `/var/tmp/reliaburger-power-cut.wMwSzk` | PASS | `{"acknowledged":2493,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 18 | 14 | `/var/tmp/reliaburger-power-cut.8UZs0b` | PASS | `{"acknowledged":2036,"fixture":"leases","in_flight_landed":0,"live_leases":13,"stale_temporaries":0}` |
| 19 | 9 | `/var/tmp/reliaburger-power-cut.BHe6hC` | PASS | `{"acknowledged":1351,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 20 | 14 | `/var/tmp/reliaburger-power-cut.3w957H` | PASS | `{"acknowledged":2126,"fixture":"leases","in_flight_landed":0,"live_leases":12,"stale_temporaries":0}` |
| 21 | 14 | `/var/tmp/reliaburger-power-cut.p66vnP` | PASS | `{"acknowledged":2086,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 22 | 19 | `/var/tmp/reliaburger-power-cut.p9WCUV` | PASS | `{"acknowledged":2804,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 23 | 5 | `/var/tmp/reliaburger-power-cut.CMJxAD` | PASS | `{"acknowledged":770,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 24 | 5 | `/var/tmp/reliaburger-power-cut.vVTIc1` | PASS | `{"acknowledged":771,"fixture":"leases","in_flight_landed":1,"live_leases":21,"stale_temporaries":0}` |
| 25 | 8 | `/var/tmp/reliaburger-power-cut.QLA0jq` | PASS | `{"acknowledged":1228,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
