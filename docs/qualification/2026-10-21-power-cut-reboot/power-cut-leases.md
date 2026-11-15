# Storage power-cut qualification: leases

- VM: `rb-pcut-abec2c45` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_lease_operations`
- Commit: `abec2c459d559652ced52a40d5c34973571cef9b`; binary SHA-256 `3360f06c981627fa0080a5f825d09b6a0a802237d06aede168415a81f774834a`
- Started: 2026-10-21T02:12:20Z
- Host evidence: `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-power-cut.MHXkU3`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 1 | `/var/tmp/reliaburger-power-cut.bFeVhf` | PASS | `{"fixture":"leases","acknowledged":174,"in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 2 | 10 | `/var/tmp/reliaburger-power-cut.Yet4qK` | PASS | `{"fixture":"leases","acknowledged":1471,"in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 3 | 0 | `/var/tmp/reliaburger-power-cut.aoqaEB` | PASS | `{"fixture":"leases","acknowledged":24,"in_flight_landed":0,"live_leases":12,"stale_temporaries":1}` |
| 4 | 5 | `/var/tmp/reliaburger-power-cut.078KxH` | PASS | `{"fixture":"leases","acknowledged":778,"in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 5 | 3 | `/var/tmp/reliaburger-power-cut.55EbG7` | PASS | `{"fixture":"leases","acknowledged":459,"in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 6 | 13 | `/var/tmp/reliaburger-power-cut.4LcbM9` | PASS | `{"fixture":"leases","acknowledged":1964,"in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 7 | 18 | `/var/tmp/reliaburger-power-cut.BtnWqM` | PASS | `{"fixture":"leases","acknowledged":2702,"in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 8 | 1 | `/var/tmp/reliaburger-power-cut.hEzw2E` | PASS | `{"fixture":"leases","acknowledged":175,"in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 9 | 17 | `/var/tmp/reliaburger-power-cut.XJEVbW` | PASS | `{"fixture":"leases","acknowledged":2500,"in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 10 | 12 | `/var/tmp/reliaburger-power-cut.QH7XxV` | PASS | `{"fixture":"leases","acknowledged":1778,"in_flight_landed":0,"live_leases":15,"stale_temporaries":0}` |
| 11 | 20 | `/var/tmp/reliaburger-power-cut.C8xqhL` | PASS | `{"fixture":"leases","acknowledged":3006,"in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 12 | 15 | `/var/tmp/reliaburger-power-cut.r0VlaG` | PASS | `{"fixture":"leases","acknowledged":2220,"in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 13 | 18 | `/var/tmp/reliaburger-power-cut.Cdov90` | PASS | `{"fixture":"leases","acknowledged":2679,"in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 14 | 4 | `/var/tmp/reliaburger-power-cut.uk4oxc` | PASS | `{"fixture":"leases","acknowledged":599,"in_flight_landed":0,"live_leases":15,"stale_temporaries":0}` |
| 15 | 3 | `/var/tmp/reliaburger-power-cut.MYypbD` | PASS | `{"fixture":"leases","acknowledged":471,"in_flight_landed":0,"live_leases":15,"stale_temporaries":1}` |
| 16 | 5 | `/var/tmp/reliaburger-power-cut.tqQNpr` | PASS | `{"fixture":"leases","acknowledged":786,"in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 17 | 5 | `/var/tmp/reliaburger-power-cut.3x5HJ2` | PASS | `{"fixture":"leases","acknowledged":780,"in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 18 | 1 | `/var/tmp/reliaburger-power-cut.XyTiU1` | PASS | `{"fixture":"leases","acknowledged":172,"in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 19 | 16 | `/var/tmp/reliaburger-power-cut.cb6GgR` | PASS | `{"fixture":"leases","acknowledged":2382,"in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 20 | 14 | `/var/tmp/reliaburger-power-cut.9sqvxm` | PASS | `{"fixture":"leases","acknowledged":2088,"in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 21 | 14 | `/var/tmp/reliaburger-power-cut.crsNEm` | PASS | `{"fixture":"leases","acknowledged":2088,"in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 22 | 11 | `/var/tmp/reliaburger-power-cut.9Ilv13` | PASS | `{"fixture":"leases","acknowledged":1588,"in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 23 | 20 | `/var/tmp/reliaburger-power-cut.0pFEMH` | PASS | `{"fixture":"leases","acknowledged":3012,"in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 24 | 5 | `/var/tmp/reliaburger-power-cut.vHXq5d` | PASS | `{"fixture":"leases","acknowledged":735,"in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 25 | 19 | `/var/tmp/reliaburger-power-cut.Nlegya` | PASS | `{"fixture":"leases","acknowledged":2878,"in_flight_landed":1,"live_leases":23,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
