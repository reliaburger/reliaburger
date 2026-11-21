# Storage power-cut qualification: leases

- VM: `rb-pcut-1612ee38` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_lease_operations`
- Commit: `1612ee38cc2f976a28f2606c4d14a37ea77be6c0`; binary SHA-256 `4a66c3a4e012bf8ad14ae61f847d50705cad961af3cde97e490a7a7f2d04e995`
- Started: 2026-11-21T07:10:33Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.OGSVAJ`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 12 | `/var/tmp/reliaburger-power-cut.2FiZFC` | PASS | `{"acknowledged":1841,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 2 | 10 | `/var/tmp/reliaburger-power-cut.YqWj7g` | PASS | `{"acknowledged":1499,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 3 | 0 | `/var/tmp/reliaburger-power-cut.QKt128` | PASS | `{"acknowledged":24,"fixture":"leases","in_flight_landed":0,"live_leases":9,"stale_temporaries":0}` |
| 4 | 19 | `/var/tmp/reliaburger-power-cut.lNORL7` | PASS | `{"acknowledged":2815,"fixture":"leases","in_flight_landed":0,"live_leases":15,"stale_temporaries":0}` |
| 5 | 7 | `/var/tmp/reliaburger-power-cut.HVgoz6` | PASS | `{"acknowledged":1058,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 6 | 2 | `/var/tmp/reliaburger-power-cut.xBbdLF` | PASS | `{"acknowledged":340,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 7 | 11 | `/var/tmp/reliaburger-power-cut.AsCzkz` | PASS | `{"acknowledged":1650,"fixture":"leases","in_flight_landed":1,"live_leases":21,"stale_temporaries":0}` |
| 8 | 19 | `/var/tmp/reliaburger-power-cut.3VKLwf` | PASS | `{"acknowledged":2872,"fixture":"leases","in_flight_landed":0,"live_leases":15,"stale_temporaries":0}` |
| 9 | 9 | `/var/tmp/reliaburger-power-cut.xutXJz` | PASS | `{"acknowledged":1363,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":1}` |
| 10 | 3 | `/var/tmp/reliaburger-power-cut.YbiRcx` | PASS | `{"acknowledged":467,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 11 | 5 | `/var/tmp/reliaburger-power-cut.5Ue929` | PASS | `{"acknowledged":782,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 12 | 6 | `/var/tmp/reliaburger-power-cut.0b4AIJ` | PASS | `{"acknowledged":919,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":3}` |
| 13 | 9 | `/var/tmp/reliaburger-power-cut.yr8Xbv` | PASS | `{"acknowledged":1376,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 14 | 13 | `/var/tmp/reliaburger-power-cut.vonAz5` | PASS | `{"acknowledged":1972,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 15 | 0 | `/var/tmp/reliaburger-power-cut.iqSIfb` | PASS | `{"acknowledged":26,"fixture":"leases","in_flight_landed":0,"live_leases":15,"stale_temporaries":0}` |
| 16 | 12 | `/var/tmp/reliaburger-power-cut.4RKoMm` | PASS | `{"acknowledged":1798,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 17 | 20 | `/var/tmp/reliaburger-power-cut.8iPsog` | PASS | `{"acknowledged":2997,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 18 | 1 | `/var/tmp/reliaburger-power-cut.l6Mmct` | PASS | `{"acknowledged":180,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 19 | 18 | `/var/tmp/reliaburger-power-cut.EXZofQ` | PASS | `{"acknowledged":2671,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 20 | 3 | `/var/tmp/reliaburger-power-cut.JbtUGS` | PASS | `{"acknowledged":475,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 21 | 9 | `/var/tmp/reliaburger-power-cut.M3aOoV` | PASS | `{"acknowledged":1359,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":1}` |
| 22 | 5 | `/var/tmp/reliaburger-power-cut.hDgmp7` | PASS | `{"acknowledged":760,"fixture":"leases","in_flight_landed":0,"live_leases":14,"stale_temporaries":1}` |
| 23 | 14 | `/var/tmp/reliaburger-power-cut.9P1oUJ` | PASS | `{"acknowledged":2098,"fixture":"leases","in_flight_landed":0,"live_leases":13,"stale_temporaries":0}` |
| 24 | 15 | `/var/tmp/reliaburger-power-cut.j0Zglq` | PASS | `{"acknowledged":2233,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 25 | 13 | `/var/tmp/reliaburger-power-cut.ZrRGXS` | PASS (VM start retried) | `{"acknowledged":1987,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
