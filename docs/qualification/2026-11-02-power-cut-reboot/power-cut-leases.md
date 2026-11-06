# Storage power-cut qualification: leases

- VM: `rb-pcut-954bb7b1` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_lease_operations`
- Commit: `954bb7b107d78632eb1a5504d00d4843ceed0447`; binary SHA-256 `0422802037fd3d3e5800ee25f347b67834d0920baa6a2821d28abdae5f70bfb6`
- Started: 2026-11-02T00:58:53Z
- Host evidence: `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-power-cut.OMGWil`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 5 | `/var/tmp/reliaburger-power-cut.iwNyIs` | PASS | `{"acknowledged":767,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 2 | 5 | `/var/tmp/reliaburger-power-cut.U4OQz8` | PASS | `{"acknowledged":756,"fixture":"leases","in_flight_landed":1,"live_leases":17,"stale_temporaries":0}` |
| 3 | 15 | `/var/tmp/reliaburger-power-cut.0khZT5` | PASS | `{"acknowledged":2174,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 4 | 16 | `/var/tmp/reliaburger-power-cut.1HrfoB` | PASS | `{"acknowledged":2333,"fixture":"leases","in_flight_landed":0,"live_leases":15,"stale_temporaries":0}` |
| 5 | 9 | `/var/tmp/reliaburger-power-cut.W4lEim` | PASS | `{"acknowledged":1360,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 6 | 10 | `/var/tmp/reliaburger-power-cut.nK5QYn` | PASS | `{"acknowledged":1471,"fixture":"leases","in_flight_landed":0,"live_leases":14,"stale_temporaries":0}` |
| 7 | 14 | `/var/tmp/reliaburger-power-cut.cYb73H` | PASS | `{"acknowledged":2024,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 8 | 16 | `/var/tmp/reliaburger-power-cut.3nutXB` | PASS | `{"acknowledged":2318,"fixture":"leases","in_flight_landed":0,"live_leases":14,"stale_temporaries":0}` |
| 9 | 8 | `/var/tmp/reliaburger-power-cut.lVGGKg` | PASS | `{"acknowledged":1175,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 10 | 3 | `/var/tmp/reliaburger-power-cut.5C7GY3` | PASS | `{"acknowledged":450,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 11 | 12 | `/var/tmp/reliaburger-power-cut.bt7riu` | PASS | `{"acknowledged":1761,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 12 | 10 | `/var/tmp/reliaburger-power-cut.bEoW1Q` | PASS | `{"acknowledged":1453,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":1}` |
| 13 | 9 | `/var/tmp/reliaburger-power-cut.SWRSnV` | PASS | `{"acknowledged":1312,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 14 | 9 | `/var/tmp/reliaburger-power-cut.ut53vK` | PASS | `{"acknowledged":1280,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 15 | 9 | `/var/tmp/reliaburger-power-cut.9k5mJh` | PASS | `{"acknowledged":1302,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 16 | 15 | `/var/tmp/reliaburger-power-cut.RQ9M2r` | PASS | `{"acknowledged":2168,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 17 | 11 | `/var/tmp/reliaburger-power-cut.iQcWjG` | PASS | `{"acknowledged":1605,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 18 | 13 | `/var/tmp/reliaburger-power-cut.sR1Gfp` | PASS | `{"acknowledged":1882,"fixture":"leases","in_flight_landed":0,"live_leases":14,"stale_temporaries":0}` |
| 19 | 7 | `/var/tmp/reliaburger-power-cut.pA9yJz` | PASS | `{"acknowledged":1001,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 20 | 5 | `/var/tmp/reliaburger-power-cut.VG7P9M` | PASS | `{"acknowledged":768,"fixture":"leases","in_flight_landed":0,"live_leases":14,"stale_temporaries":0}` |
| 21 | 9 | `/var/tmp/reliaburger-power-cut.cGLs5n` | PASS | `{"acknowledged":1323,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 22 | 2 | `/var/tmp/reliaburger-power-cut.sLeWC0` | PASS | `{"acknowledged":308,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 23 | 17 | `/var/tmp/reliaburger-power-cut.q5BjE4` | PASS | `{"acknowledged":2489,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 24 | 11 | `/var/tmp/reliaburger-power-cut.NBvlG2` | PASS | `{"acknowledged":1591,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 25 | 14 | `/var/tmp/reliaburger-power-cut.t6h598` | PASS | `{"acknowledged":2033,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
