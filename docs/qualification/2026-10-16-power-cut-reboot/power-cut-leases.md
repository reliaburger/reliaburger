# Storage power-cut qualification: leases

- VM: `rb-020-pcut` (guest kernel 6.8.0-142-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_lease_operations`
- Commit: `cc5f64393350f2081f7d88512b95a88316172e5a`; binary SHA-256 `33f3ba84343c37d41b6ebb9298e09d17b8e009d385ed84c0eb25ce5146ee3c56`
- Started: 2026-10-16T04:43:21Z
- Host evidence: a temporary directory on the host, not kept

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 14 | `/var/tmp/reliaburger-power-cut.iT6vBu` | PASS | `{"fixture":"leases","acknowledged":2105,"in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 2 | 13 | `/var/tmp/reliaburger-power-cut.oWeFlJ` | PASS | `{"fixture":"leases","acknowledged":1995,"in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 3 | 3 | `/var/tmp/reliaburger-power-cut.Ik0V1t` | PASS | `{"fixture":"leases","acknowledged":464,"in_flight_landed":0,"live_leases":14,"stale_temporaries":2}` |
| 4 | 16 | `/var/tmp/reliaburger-power-cut.AizwLY` | PASS | `{"fixture":"leases","acknowledged":2385,"in_flight_landed":0,"live_leases":15,"stale_temporaries":0}` |
| 5 | 8 | `/var/tmp/reliaburger-power-cut.L3anr9` | PASS | `{"fixture":"leases","acknowledged":1213,"in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 6 | 12 | `/var/tmp/reliaburger-power-cut.PPIqI0` | PASS | `{"fixture":"leases","acknowledged":1818,"in_flight_landed":0,"live_leases":15,"stale_temporaries":0}` |
| 7 | 11 | `/var/tmp/reliaburger-power-cut.6RzrrA` | PASS | `{"fixture":"leases","acknowledged":1628,"in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 8 | 5 | `/var/tmp/reliaburger-power-cut.ob31Y3` | PASS | `{"fixture":"leases","acknowledged":778,"in_flight_landed":0,"live_leases":21,"stale_temporaries":1}` |
| 9 | 2 | `/var/tmp/reliaburger-power-cut.llVovG` | PASS | `{"fixture":"leases","acknowledged":310,"in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 10 | 19 | `/var/tmp/reliaburger-power-cut.DwbZ5s` | PASS | `{"fixture":"leases","acknowledged":2834,"in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 11 | 18 | `/var/tmp/reliaburger-power-cut.qopTFo` | PASS | `{"fixture":"leases","acknowledged":2661,"in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 12 | 18 | `/var/tmp/reliaburger-power-cut.krnQEN` | PASS | `{"fixture":"leases","acknowledged":2676,"in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 13 | 0 | `/var/tmp/reliaburger-power-cut.s5lSBP` | PASS | `{"fixture":"leases","acknowledged":19,"in_flight_landed":0,"live_leases":12,"stale_temporaries":0}` |
| 14 | 6 | `/var/tmp/reliaburger-power-cut.qF0TB4` | PASS | `{"fixture":"leases","acknowledged":905,"in_flight_landed":0,"live_leases":13,"stale_temporaries":0}` |
| 15 | 8 | `/var/tmp/reliaburger-power-cut.K0hkz2` | PASS | `{"fixture":"leases","acknowledged":1202,"in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 16 | 20 | `/var/tmp/reliaburger-power-cut.D92ZwF` | PASS | `{"fixture":"leases","acknowledged":2973,"in_flight_landed":1,"live_leases":15,"stale_temporaries":0}` |
| 17 | 7 | `/var/tmp/reliaburger-power-cut.nR913Z` | PASS | `{"fixture":"leases","acknowledged":1067,"in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 18 | 6 | `/var/tmp/reliaburger-power-cut.Vu1dv4` | PASS | `{"fixture":"leases","acknowledged":923,"in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 19 | 4 | `/var/tmp/reliaburger-power-cut.vGsRBP` | PASS | `{"fixture":"leases","acknowledged":629,"in_flight_landed":1,"live_leases":17,"stale_temporaries":0}` |
| 20 | 2 | `/var/tmp/reliaburger-power-cut.KTTTmn` | PASS | `{"fixture":"leases","acknowledged":343,"in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 21 | 19 | `/var/tmp/reliaburger-power-cut.TiXD7J` | PASS | `{"fixture":"leases","acknowledged":2896,"in_flight_landed":1,"live_leases":13,"stale_temporaries":0}` |
| 22 | 16 | `/var/tmp/reliaburger-power-cut.4jFpld` | PASS | `{"fixture":"leases","acknowledged":2418,"in_flight_landed":0,"live_leases":18,"stale_temporaries":1}` |
| 23 | 5 | `/var/tmp/reliaburger-power-cut.4pHNMR` | PASS | `{"fixture":"leases","acknowledged":764,"in_flight_landed":0,"live_leases":17,"stale_temporaries":1}` |
| 24 | 7 | `/var/tmp/reliaburger-power-cut.umoZtM` | PASS | `{"fixture":"leases","acknowledged":1062,"in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 25 | 9 | `/var/tmp/reliaburger-power-cut.lCnwN4` | PASS | `{"fixture":"leases","acknowledged":1385,"in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
