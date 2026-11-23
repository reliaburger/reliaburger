# Storage power-cut qualification: leases

- VM: `rb-pcut-2aae7e2a` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_lease_operations`
- Commit: `2aae7e2a075f24614eb626aef83a5c29a279fb34`; binary SHA-256 `641bfed43d860a3da96e740299d8202827967c245ab2fa35f87da32f3f0fbef7`
- Started: 2026-11-23T01:23:54Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.HcDRAG`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 18 | `/var/tmp/reliaburger-power-cut.XRmyA6` | PASS | `{"acknowledged":2611,"fixture":"leases","in_flight_landed":0,"live_leases":14,"stale_temporaries":0}` |
| 2 | 12 | `/var/tmp/reliaburger-power-cut.lJdUdw` | PASS | `{"acknowledged":1759,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":1}` |
| 3 | 19 | `/var/tmp/reliaburger-power-cut.GcfJMd` | PASS | `{"acknowledged":2746,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":1}` |
| 4 | 16 | `/var/tmp/reliaburger-power-cut.fLjcgz` | PASS | `{"acknowledged":2337,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 5 | 7 | `/var/tmp/reliaburger-power-cut.5ptNr4` | PASS | `{"acknowledged":1039,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 6 | 4 | `/var/tmp/reliaburger-power-cut.gYW1H2` | PASS | `{"acknowledged":593,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 7 | 13 | `/var/tmp/reliaburger-power-cut.uw85HW` | PASS | `{"acknowledged":1862,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 8 | 17 | `/var/tmp/reliaburger-power-cut.dGkpBk` | PASS | `{"acknowledged":2447,"fixture":"leases","in_flight_landed":0,"live_leases":24,"stale_temporaries":0}` |
| 9 | 1 | `/var/tmp/reliaburger-power-cut.OQpoCn` | PASS | `{"acknowledged":172,"fixture":"leases","in_flight_landed":0,"live_leases":14,"stale_temporaries":0}` |
| 10 | 9 | `/var/tmp/reliaburger-power-cut.BGWAuh` | PASS | `{"acknowledged":1329,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 11 | 18 | `/var/tmp/reliaburger-power-cut.YZVHe5` | PASS | `{"acknowledged":2621,"fixture":"leases","in_flight_landed":0,"live_leases":24,"stale_temporaries":0}` |
| 12 | 1 | `/var/tmp/reliaburger-power-cut.Nq6R2j` | PASS | `{"acknowledged":159,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 13 | 4 | `/var/tmp/reliaburger-power-cut.zY2p1k` | PASS | `{"acknowledged":603,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 14 | 12 | `/var/tmp/reliaburger-power-cut.KLxB0T` | PASS | `{"acknowledged":1766,"fixture":"leases","in_flight_landed":0,"live_leases":24,"stale_temporaries":0}` |
| 15 | 20 | `/var/tmp/reliaburger-power-cut.rBuCAK` | PASS | `{"acknowledged":2920,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":1}` |
| 16 | 1 | `/var/tmp/reliaburger-power-cut.VtNqJ6` | PASS | `{"acknowledged":162,"fixture":"leases","in_flight_landed":0,"live_leases":14,"stale_temporaries":0}` |
| 17 | 13 | `/var/tmp/reliaburger-power-cut.rQP0nM` | PASS | `{"acknowledged":1895,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 18 | 7 | `/var/tmp/reliaburger-power-cut.lATRNa` | PASS | `{"acknowledged":1029,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 19 | 5 | `/var/tmp/reliaburger-power-cut.OLb71T` | PASS | `{"acknowledged":735,"fixture":"leases","in_flight_landed":0,"live_leases":13,"stale_temporaries":0}` |
| 20 | 2 | `/var/tmp/reliaburger-power-cut.t3bUHG` | PASS | `{"acknowledged":314,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":3}` |
| 21 | 9 | `/var/tmp/reliaburger-power-cut.afJLlj` | PASS | `{"acknowledged":1321,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 22 | 11 | `/var/tmp/reliaburger-power-cut.AKpn10` | PASS | `{"acknowledged":1603,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 23 | 11 | `/var/tmp/reliaburger-power-cut.7vhn3g` | PASS | `{"acknowledged":1589,"fixture":"leases","in_flight_landed":0,"live_leases":12,"stale_temporaries":0}` |
| 24 | 3 | `/var/tmp/reliaburger-power-cut.CTuMVO` | PASS | `{"acknowledged":466,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 25 | 13 | `/var/tmp/reliaburger-power-cut.LG7lED` | PASS | `{"acknowledged":1925,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
