# Storage power-cut qualification: leases

- VM: `rb-pcut-efc3fb2f` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_lease_operations`
- Commit: `efc3fb2fc7882500ed0a4ac050d4b525cb806a54`; binary SHA-256 `60c183ffefbe115642010287ead0e4e3c29cab39d76504e2c0e9c4d6e7f9af9e`
- Started: 2026-11-19T19:27:19Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.yXG27u`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 4 | `/var/tmp/reliaburger-power-cut.AC1tT5` | PASS | `{"acknowledged":632,"fixture":"leases","in_flight_landed":0,"live_leases":24,"stale_temporaries":0}` |
| 2 | 5 | `/var/tmp/reliaburger-power-cut.NB4ive` | PASS | `{"acknowledged":785,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 3 | 6 | `/var/tmp/reliaburger-power-cut.ctxKPT` | PASS | `{"acknowledged":977,"fixture":"leases","in_flight_landed":0,"live_leases":14,"stale_temporaries":0}` |
| 4 | 11 | `/var/tmp/reliaburger-power-cut.uCPUIJ` | PASS | `{"acknowledged":1728,"fixture":"leases","in_flight_landed":0,"live_leases":24,"stale_temporaries":0}` |
| 5 | 9 | `/var/tmp/reliaburger-power-cut.IZfZsN` | PASS | `{"acknowledged":1378,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 6 | 20 | `/var/tmp/reliaburger-power-cut.etam3A` | PASS | `{"acknowledged":3067,"fixture":"leases","in_flight_landed":0,"live_leases":15,"stale_temporaries":0}` |
| 7 | 9 | `/var/tmp/reliaburger-power-cut.yhggsm` | PASS | `{"acknowledged":1405,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 8 | 2 | `/var/tmp/reliaburger-power-cut.HjglGA` | PASS | `{"acknowledged":333,"fixture":"leases","in_flight_landed":0,"live_leases":14,"stale_temporaries":0}` |
| 9 | 20 | `/var/tmp/reliaburger-power-cut.ULUsqO` | PASS | `{"acknowledged":3098,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 10 | 3 | `/var/tmp/reliaburger-power-cut.kVYrqh` | PASS | `{"acknowledged":468,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 11 | 12 | `/var/tmp/reliaburger-power-cut.5B7lLA` | PASS | `{"acknowledged":1805,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 12 | 7 | `/var/tmp/reliaburger-power-cut.WbFOsN` | PASS | `{"acknowledged":1064,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 13 | 13 | `/var/tmp/reliaburger-power-cut.L2jrqX` | PASS | `{"acknowledged":1976,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 14 | 17 | `/var/tmp/reliaburger-power-cut.ezuXIg` | PASS | `{"acknowledged":2529,"fixture":"leases","in_flight_landed":0,"live_leases":13,"stale_temporaries":0}` |
| 15 | 7 | `/var/tmp/reliaburger-power-cut.89qMNK` | PASS | `{"acknowledged":1037,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 16 | 11 | `/var/tmp/reliaburger-power-cut.jkzyiw` | PASS | `{"acknowledged":1669,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 17 | 10 | `/var/tmp/reliaburger-power-cut.U0MxmD` | PASS | `{"acknowledged":1510,"fixture":"leases","in_flight_landed":0,"live_leases":15,"stale_temporaries":0}` |
| 18 | 6 | `/var/tmp/reliaburger-power-cut.l13GZj` | PASS | `{"acknowledged":913,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 19 | 3 | `/var/tmp/reliaburger-power-cut.XT6Csp` | PASS | `{"acknowledged":461,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 20 | 8 | `/var/tmp/reliaburger-power-cut.N6qH8W` | PASS | `{"acknowledged":1194,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 21 | 4 | `/var/tmp/reliaburger-power-cut.oqE3WA` | PASS | `{"acknowledged":617,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 22 | 17 | `/var/tmp/reliaburger-power-cut.NI0XtD` | PASS | `{"acknowledged":2563,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 23 | 4 | `/var/tmp/reliaburger-power-cut.8Shr4z` | PASS | `{"acknowledged":610,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":1}` |
| 24 | 12 | `/var/tmp/reliaburger-power-cut.OfnSnW` | PASS | `{"acknowledged":1792,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 25 | 6 | `/var/tmp/reliaburger-power-cut.cmj5GH` | PASS | `{"acknowledged":909,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
