# Storage power-cut qualification: leases

- VM: `rb-pcut-d387d569` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_lease_operations`
- Commit: `d387d569bb1f18264382ffcffebe3bbf45fb7ed3`; binary SHA-256 `75bf9f19654613b2d8913f41c18654a062a69c43fff7cf40aa84bef2cbff4bce`
- Started: 2026-10-27T11:54:15Z
- Host evidence: `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-power-cut.j7G21y`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 2 | `/var/tmp/reliaburger-power-cut.97jWJG` | PASS | `{"acknowledged":320,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 2 | 7 | `/var/tmp/reliaburger-power-cut.adXhQB` | PASS | `{"acknowledged":1024,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 3 | 14 | `/var/tmp/reliaburger-power-cut.fQT3Vc` | PASS | `{"acknowledged":2047,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 4 | 3 | `/var/tmp/reliaburger-power-cut.PVrZik` | PASS | `{"acknowledged":442,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 5 | 10 | `/var/tmp/reliaburger-power-cut.c7Ss1d` | PASS | `{"acknowledged":1450,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 6 | 16 | `/var/tmp/reliaburger-power-cut.JQuY8M` | PASS | `{"acknowledged":2256,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 7 | 14 | `/var/tmp/reliaburger-power-cut.WJTkMW` | PASS | `{"acknowledged":2007,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 8 | 3 | `/var/tmp/reliaburger-power-cut.6eZLo4` | PASS | `{"acknowledged":442,"fixture":"leases","in_flight_landed":0,"live_leases":24,"stale_temporaries":0}` |
| 9 | 17 | `/var/tmp/reliaburger-power-cut.M9O7pr` | PASS | `{"acknowledged":2488,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 10 | 2 | `/var/tmp/reliaburger-power-cut.G7sgZS` | PASS | `{"acknowledged":320,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 11 | 19 | `/var/tmp/reliaburger-power-cut.m3gMQb` | PASS | `{"acknowledged":2791,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 12 | 16 | `/var/tmp/reliaburger-power-cut.WiABYf` | PASS | `{"acknowledged":2309,"fixture":"leases","in_flight_landed":1,"live_leases":17,"stale_temporaries":0}` |
| 13 | 16 | `/var/tmp/reliaburger-power-cut.99tjMu` | PASS | `{"acknowledged":2337,"fixture":"leases","in_flight_landed":0,"live_leases":24,"stale_temporaries":0}` |
| 14 | 16 | `/var/tmp/reliaburger-power-cut.UGgXkm` | PASS | `{"acknowledged":2310,"fixture":"leases","in_flight_landed":2,"live_leases":19,"stale_temporaries":0}` |
| 15 | 15 | `/var/tmp/reliaburger-power-cut.S7fqr8` | PASS | `{"acknowledged":2152,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 16 | 7 | `/var/tmp/reliaburger-power-cut.yfgGKD` | PASS | `{"acknowledged":1063,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 17 | 9 | `/var/tmp/reliaburger-power-cut.UE45gG` | PASS | `{"acknowledged":1340,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 18 | 13 | `/var/tmp/reliaburger-power-cut.4Y8ZIF` | PASS | `{"acknowledged":1893,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 19 | 15 | `/var/tmp/reliaburger-power-cut.7MtJ8d` | PASS | `{"acknowledged":2120,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 20 | 13 | `/var/tmp/reliaburger-power-cut.pf9UIi` | PASS | `{"acknowledged":1866,"fixture":"leases","in_flight_landed":1,"live_leases":21,"stale_temporaries":0}` |
| 21 | 12 | `/var/tmp/reliaburger-power-cut.Hj1XwS` | PASS | `{"acknowledged":1730,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":1}` |
| 22 | 3 | `/var/tmp/reliaburger-power-cut.H7kiXD` | PASS | `{"acknowledged":440,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 23 | 20 | `/var/tmp/reliaburger-power-cut.xbbXnO` | PASS | `{"acknowledged":2874,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 24 | 8 | `/var/tmp/reliaburger-power-cut.lkOVj4` | PASS | `{"acknowledged":1173,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 25 | 4 | `/var/tmp/reliaburger-power-cut.bJpM1C` | PASS | `{"acknowledged":622,"fixture":"leases","in_flight_landed":0,"live_leases":15,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
