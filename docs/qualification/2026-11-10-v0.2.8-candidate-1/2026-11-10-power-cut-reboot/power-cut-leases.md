# Storage power-cut qualification: leases

- VM: `rb-pcut-08618555` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_lease_operations`
- Commit: `08618555ffe3112f2bd67842cadedfdd61d8071c`; binary SHA-256 `1069749d07dc37f6711f6c8bfeb9fa0267ad00d7f9305ec9617fae15fdb5edf2`
- Started: 2026-11-10T17:23:17Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.QCWnlH`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 4 | `/var/tmp/reliaburger-power-cut.NygAuh` | PASS | `{"acknowledged":604,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 2 | 7 | `/var/tmp/reliaburger-power-cut.HoGQ5N` | PASS | `{"acknowledged":1050,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 3 | 10 | `/var/tmp/reliaburger-power-cut.GnXVHF` | PASS | `{"acknowledged":1459,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 4 | 12 | `/var/tmp/reliaburger-power-cut.zVsv56` | PASS | `{"acknowledged":1772,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 5 | 2 | `/var/tmp/reliaburger-power-cut.TglLni` | PASS | `{"acknowledged":306,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 6 | 4 | `/var/tmp/reliaburger-power-cut.CaSyfT` | PASS | `{"acknowledged":576,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 7 | 14 | `/var/tmp/reliaburger-power-cut.9iVEWY` | PASS | `{"acknowledged":2072,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 8 | 20 | `/var/tmp/reliaburger-power-cut.gmh3R0` | PASS | `{"acknowledged":2890,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 9 | 8 | `/var/tmp/reliaburger-power-cut.WpmP1B` | PASS | `{"acknowledged":1209,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":1}` |
| 10 | 4 | `/var/tmp/reliaburger-power-cut.yaMOU5` | PASS | `{"acknowledged":603,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 11 | 1 | `/var/tmp/reliaburger-power-cut.bJZhWb` | PASS | `{"acknowledged":185,"fixture":"leases","in_flight_landed":1,"live_leases":21,"stale_temporaries":1}` |
| 12 | 6 | `/var/tmp/reliaburger-power-cut.j96UUk` | PASS | `{"acknowledged":879,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 13 | 12 | `/var/tmp/reliaburger-power-cut.rz92UD` | PASS | `{"acknowledged":1743,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 14 | 0 | `/var/tmp/reliaburger-power-cut.CqxwVj` | PASS | `{"acknowledged":23,"fixture":"leases","in_flight_landed":0,"live_leases":11,"stale_temporaries":0}` |
| 15 | 9 | `/var/tmp/reliaburger-power-cut.hY4B9g` | PASS | `{"acknowledged":1332,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 16 | 8 | `/var/tmp/reliaburger-power-cut.9aBmkV` | PASS | `{"acknowledged":1179,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":1}` |
| 17 | 0 | `/var/tmp/reliaburger-power-cut.gvaTr0` | PASS | `{"acknowledged":21,"fixture":"leases","in_flight_landed":0,"live_leases":13,"stale_temporaries":0}` |
| 18 | 17 | `/var/tmp/reliaburger-power-cut.foUjoJ` | PASS | `{"acknowledged":2515,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 19 | 15 | `/var/tmp/reliaburger-power-cut.kwKprk` | PASS | `{"acknowledged":2220,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 20 | 2 | `/var/tmp/reliaburger-power-cut.tm9fpi` | PASS | `{"acknowledged":302,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 21 | 19 | `/var/tmp/reliaburger-power-cut.D1cqQt` | PASS | `{"acknowledged":2762,"fixture":"leases","in_flight_landed":0,"live_leases":15,"stale_temporaries":0}` |
| 22 | 13 | `/var/tmp/reliaburger-power-cut.2ReKzN` | PASS | `{"acknowledged":1894,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 23 | 4 | `/var/tmp/reliaburger-power-cut.CZkcO1` | PASS | `{"acknowledged":619,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 24 | 6 | `/var/tmp/reliaburger-power-cut.ZJvYE2` | PASS | `{"acknowledged":910,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 25 | 8 | `/var/tmp/reliaburger-power-cut.RdCpWK` | PASS | `{"acknowledged":1218,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
