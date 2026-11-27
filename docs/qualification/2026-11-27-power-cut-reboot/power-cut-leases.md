# Storage power-cut qualification: leases

- VM: `rb-pcut-5f21eef6` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_lease_operations`
- Commit: `5f21eef6abc384d7e1c3cb8ff633aa7c94074a7f`; binary SHA-256 `3d186af5945868cde72d84b2302f5311112f21864a6a1978e640b37b01996795`
- Started: 2026-11-26T23:38:09Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.d2twJn`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 14 | `/var/tmp/reliaburger-power-cut.FpeGzW` | PASS | `{"acknowledged":2109,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 2 | 1 | `/var/tmp/reliaburger-power-cut.hyur2w` | PASS | `{"acknowledged":164,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 3 | 16 | `/var/tmp/reliaburger-power-cut.0dCtKK` | PASS | `{"acknowledged":2420,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 4 | 11 | `/var/tmp/reliaburger-power-cut.1yB6Vk` | PASS | `{"acknowledged":1658,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 5 | 17 | `/var/tmp/reliaburger-power-cut.49I2Bs` | PASS | `{"acknowledged":2525,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 6 | 1 | `/var/tmp/reliaburger-power-cut.zpP9hu` | PASS | `{"acknowledged":169,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 7 | 8 | `/var/tmp/reliaburger-power-cut.rb3Asq` | PASS | `{"acknowledged":1203,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 8 | 7 | `/var/tmp/reliaburger-power-cut.7GWl2j` | PASS | `{"acknowledged":1067,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 9 | 14 | `/var/tmp/reliaburger-power-cut.S1D5mF` | PASS | `{"acknowledged":2103,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 10 | 9 | `/var/tmp/reliaburger-power-cut.MUZ8Fq` | PASS | `{"acknowledged":1376,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 11 | 5 | `/var/tmp/reliaburger-power-cut.Gud54k` | PASS | `{"acknowledged":782,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 12 | 3 | `/var/tmp/reliaburger-power-cut.OCJoBa` | PASS | `{"acknowledged":480,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 13 | 13 | `/var/tmp/reliaburger-power-cut.uPXhYY` | PASS | `{"acknowledged":1940,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 14 | 14 | `/var/tmp/reliaburger-power-cut.GwcrVJ` | PASS | `{"acknowledged":2099,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 15 | 15 | `/var/tmp/reliaburger-power-cut.lvriR8` | PASS | `{"acknowledged":2259,"fixture":"leases","in_flight_landed":0,"live_leases":14,"stale_temporaries":1}` |
| 16 | 1 | `/var/tmp/reliaburger-power-cut.rkJeZ0` | PASS | `{"acknowledged":178,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 17 | 16 | `/var/tmp/reliaburger-power-cut.9PaE6V` | PASS | `{"acknowledged":2402,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 18 | 15 | `/var/tmp/reliaburger-power-cut.fIzV8M` | PASS | `{"acknowledged":2234,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":2}` |
| 19 | 19 | `/var/tmp/reliaburger-power-cut.IAeHcr` | PASS | `{"acknowledged":2822,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 20 | 9 | `/var/tmp/reliaburger-power-cut.ADp2WW` | PASS | `{"acknowledged":1378,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 21 | 11 | `/var/tmp/reliaburger-power-cut.9MUgGB` | PASS | `{"acknowledged":1635,"fixture":"leases","in_flight_landed":0,"live_leases":15,"stale_temporaries":0}` |
| 22 | 8 | `/var/tmp/reliaburger-power-cut.NUlkU1` | PASS | `{"acknowledged":1187,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 23 | 20 | `/var/tmp/reliaburger-power-cut.Z0U0M6` | PASS | `{"acknowledged":2985,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 24 | 0 | `/var/tmp/reliaburger-power-cut.mLpFmP` | PASS | `{"acknowledged":21,"fixture":"leases","in_flight_landed":0,"live_leases":13,"stale_temporaries":0}` |
| 25 | 4 | `/var/tmp/reliaburger-power-cut.UDmvj5` | PASS | `{"acknowledged":629,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
