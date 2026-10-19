# Storage power-cut qualification: leases

- VM: `rb-691-crash` (guest kernel 6.8.0-142-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_lease_operations`
- Commit: `ab18602bf9e6a9691d60853c690a4cabae1ef8d1`; binary SHA-256 `0d4e98edf5429e1fe0e97ea7a8165b5eb759027d98c68759b8e63a1931b4c185`
- Started: 2026-10-10T20:48:15Z
- Host evidence: `/private/tmp/claude-501/-Users-miko-github-reliaburger/68c7cd97-0874-4446-8c64-77c5be59a15c/scratchpad/qual-tmp/reliaburger-power-cut.uHmJzw`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 5 | `/var/tmp/reliaburger-power-cut.LIq4Sl` | PASS | `{"fixture":"leases","acknowledged":477,"in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 2 | 16 | `/var/tmp/reliaburger-power-cut.k0AWIs` | PASS | `{"fixture":"leases","acknowledged":1375,"in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 3 | 10 | `/var/tmp/reliaburger-power-cut.tf9FmH` | PASS | `{"fixture":"leases","acknowledged":243,"in_flight_landed":2,"live_leases":18,"stale_temporaries":0}` |
| 4 | 7 | `/var/tmp/reliaburger-power-cut.24Ot9H` | PASS | `{"fixture":"leases","acknowledged":567,"in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 5 | 6 | `/var/tmp/reliaburger-power-cut.doS6SM` | PASS | `{"fixture":"leases","acknowledged":908,"in_flight_landed":0,"live_leases":13,"stale_temporaries":0}` |
| 6 | 7 | `/var/tmp/reliaburger-power-cut.iZAWNz` | PASS | `{"fixture":"leases","acknowledged":1093,"in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 7 | 18 | `/var/tmp/reliaburger-power-cut.SiZck3` | PASS | `{"fixture":"leases","acknowledged":2557,"in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 8 | 20 | `/var/tmp/reliaburger-power-cut.dzcg70` | PASS | `{"fixture":"leases","acknowledged":3131,"in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 9 | 8 | `/var/tmp/reliaburger-power-cut.YmKtEn` | PASS | `{"fixture":"leases","acknowledged":1256,"in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 10 | 7 | `/var/tmp/reliaburger-power-cut.ZxbXlT` | PASS | `{"fixture":"leases","acknowledged":1052,"in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |

10 of 10 iterations passed. Rule-of-three bound: failure rate < 30.0% at 95% confidence.
