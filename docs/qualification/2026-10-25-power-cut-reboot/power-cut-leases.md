# Storage power-cut qualification: leases

- VM: `rb-pcut-43baee64` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_lease_operations`
- Commit: `43baee64ae9689b7620f52c33e900acf8a407c43`; binary SHA-256 `8a0e233adfeedf331c62d83be6db72b1411694529f0c8c3b779ceffd9dc411c8`
- Started: 2026-10-25T14:36:21Z
- Host evidence: `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-power-cut.z8ZGzX`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 12 | `/var/tmp/reliaburger-power-cut.w8jMBh` | PASS | `{"acknowledged":1772,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 2 | 8 | `/var/tmp/reliaburger-power-cut.q2nXul` | PASS | `{"acknowledged":1196,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 3 | 4 | `/var/tmp/reliaburger-power-cut.BQa1uO` | PASS | `{"acknowledged":574,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 4 | 16 | `/var/tmp/reliaburger-power-cut.peuWFH` | PASS | `{"acknowledged":2320,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 5 | 6 | `/var/tmp/reliaburger-power-cut.0u0fa6` | PASS | `{"acknowledged":871,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 6 | 17 | `/var/tmp/reliaburger-power-cut.kEkJmz` | PASS | `{"acknowledged":2374,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 7 | 12 | `/var/tmp/reliaburger-power-cut.RFHPH2` | PASS | `{"acknowledged":1731,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 8 | 14 | `/var/tmp/reliaburger-power-cut.R0qvZd` | PASS | `{"acknowledged":1972,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 9 | 10 | `/var/tmp/reliaburger-power-cut.1cjiB2` | PASS | `{"acknowledged":1452,"fixture":"leases","in_flight_landed":0,"live_leases":13,"stale_temporaries":0}` |
| 10 | 10 | `/var/tmp/reliaburger-power-cut.BQmBX4` | PASS | `{"acknowledged":1426,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 11 | 5 | `/var/tmp/reliaburger-power-cut.txOMgj` | PASS | `{"acknowledged":744,"fixture":"leases","in_flight_landed":1,"live_leases":22,"stale_temporaries":0}` |
| 12 | 11 | `/var/tmp/reliaburger-power-cut.vGYECe` | PASS | `{"acknowledged":1591,"fixture":"leases","in_flight_landed":1,"live_leases":21,"stale_temporaries":0}` |
| 13 | 11 | `/var/tmp/reliaburger-power-cut.iYmVYX` | PASS | `{"acknowledged":1574,"fixture":"leases","in_flight_landed":2,"live_leases":18,"stale_temporaries":0}` |
| 14 | 10 | `/var/tmp/reliaburger-power-cut.qDXEiY` | PASS | `{"acknowledged":1435,"fixture":"leases","in_flight_landed":0,"live_leases":15,"stale_temporaries":0}` |
| 15 | 18 | `/var/tmp/reliaburger-power-cut.QOP6dr` | PASS | `{"acknowledged":2622,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 16 | 19 | `/var/tmp/reliaburger-power-cut.fkjL6o` | PASS | `{"acknowledged":2775,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 17 | 2 | `/var/tmp/reliaburger-power-cut.eSPFlk` | PASS | `{"acknowledged":298,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":2}` |
| 18 | 10 | `/var/tmp/reliaburger-power-cut.iuk79o` | PASS | `{"acknowledged":1447,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 19 | 5 | `/var/tmp/reliaburger-power-cut.Ns3Oad` | PASS | `{"acknowledged":731,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 20 | 12 | `/var/tmp/reliaburger-power-cut.VXvdnH` | PASS | `{"acknowledged":1702,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 21 | 13 | `/var/tmp/reliaburger-power-cut.9OnenH` | PASS | `{"acknowledged":1866,"fixture":"leases","in_flight_landed":0,"live_leases":13,"stale_temporaries":0}` |
| 22 | 6 | `/var/tmp/reliaburger-power-cut.GVxrmp` | PASS | `{"acknowledged":885,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 23 | 4 | `/var/tmp/reliaburger-power-cut.IYu5mN` | PASS | `{"acknowledged":585,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 24 | 20 | `/var/tmp/reliaburger-power-cut.QSZGb1` | PASS | `{"acknowledged":2880,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 25 | 11 | `/var/tmp/reliaburger-power-cut.NiSRiJ` | PASS | `{"acknowledged":1602,"fixture":"leases","in_flight_landed":0,"live_leases":13,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
