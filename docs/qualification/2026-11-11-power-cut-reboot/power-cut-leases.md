# Storage power-cut qualification: leases

- VM: `rb-pcut-e423d469` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_lease_operations`
- Commit: `e423d469a93b04d7bdc64582be1922ac5f453fd1`; binary SHA-256 `9049099169dcdd7e7a9666bef97b28ed88603a2d97b558eb8c8e473608acddbb`
- Started: 2026-11-11T13:12:05Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.TTmoyG`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 3 | `/var/tmp/reliaburger-power-cut.RKwbeD` | PASS | `{"acknowledged":462,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 2 | 18 | `/var/tmp/reliaburger-power-cut.CtaObr` | PASS | `{"acknowledged":2675,"fixture":"leases","in_flight_landed":0,"live_leases":24,"stale_temporaries":0}` |
| 3 | 9 | `/var/tmp/reliaburger-power-cut.KN3LDX` | PASS | `{"acknowledged":1327,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 4 | 13 | `/var/tmp/reliaburger-power-cut.ouBsyD` | PASS | `{"acknowledged":1918,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 5 | 11 | `/var/tmp/reliaburger-power-cut.ZEdZqC` | PASS | `{"acknowledged":1613,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":1}` |
| 6 | 13 | `/var/tmp/reliaburger-power-cut.okuiWC` | PASS | `{"acknowledged":1887,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 7 | 2 | `/var/tmp/reliaburger-power-cut.N9RILa` | PASS | `{"acknowledged":317,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 8 | 8 | `/var/tmp/reliaburger-power-cut.vo7fB0` | PASS | `{"acknowledged":1201,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 9 | 16 | `/var/tmp/reliaburger-power-cut.DBkvgV` | PASS | `{"acknowledged":2393,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 10 | 15 | `/var/tmp/reliaburger-power-cut.uPNOn8` | PASS | `{"acknowledged":2187,"fixture":"leases","in_flight_landed":0,"live_leases":13,"stale_temporaries":0}` |
| 11 | 4 | `/var/tmp/reliaburger-power-cut.0WaGL8` | PASS | `{"acknowledged":602,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 12 | 14 | `/var/tmp/reliaburger-power-cut.TcIF1E` | PASS | `{"acknowledged":2093,"fixture":"leases","in_flight_landed":0,"live_leases":15,"stale_temporaries":0}` |
| 13 | 17 | `/var/tmp/reliaburger-power-cut.QBc6Wk` | PASS | `{"acknowledged":2496,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 14 | 11 | `/var/tmp/reliaburger-power-cut.CjShVR` | PASS | `{"acknowledged":1664,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 15 | 7 | `/var/tmp/reliaburger-power-cut.ujmMaL` | PASS | `{"acknowledged":1013,"fixture":"leases","in_flight_landed":1,"live_leases":18,"stale_temporaries":1}` |
| 16 | 7 | `/var/tmp/reliaburger-power-cut.KDxvFt` | PASS | `{"acknowledged":1035,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 17 | 11 | `/var/tmp/reliaburger-power-cut.02Uh7C` | PASS | `{"acknowledged":1616,"fixture":"leases","in_flight_landed":1,"live_leases":21,"stale_temporaries":0}` |
| 18 | 11 | `/var/tmp/reliaburger-power-cut.tiil79` | PASS | `{"acknowledged":1648,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":1}` |
| 19 | 5 | `/var/tmp/reliaburger-power-cut.QDN1SL` | PASS | `{"acknowledged":745,"fixture":"leases","in_flight_landed":0,"live_leases":14,"stale_temporaries":0}` |
| 20 | 7 | `/var/tmp/reliaburger-power-cut.1RTIoZ` | PASS | `{"acknowledged":1067,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 21 | 0 | `/var/tmp/reliaburger-power-cut.asclsl` | PASS | `{"acknowledged":24,"fixture":"leases","in_flight_landed":0,"live_leases":13,"stale_temporaries":0}` |
| 22 | 17 | `/var/tmp/reliaburger-power-cut.NaFX72` | PASS | `{"acknowledged":2535,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":1}` |
| 23 | 14 | `/var/tmp/reliaburger-power-cut.967zRS` | PASS | `{"acknowledged":2080,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 24 | 4 | `/var/tmp/reliaburger-power-cut.uCfVOY` | PASS | `{"acknowledged":638,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 25 | 15 | `/var/tmp/reliaburger-power-cut.w8YHpK` | PASS | `{"acknowledged":2200,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
