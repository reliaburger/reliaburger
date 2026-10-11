# Storage power-cut qualification: leases

- VM: `rb-691-crash` (guest kernel 6.8.0-142-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_lease_operations`
- Commit: `ab18602bf9e6a9691d60853c690a4cabae1ef8d1`; binary SHA-256 `0d4e98edf5429e1fe0e97ea7a8165b5eb759027d98c68759b8e63a1931b4c185`
- Started: 2026-10-10T21:21:40Z
- Host evidence: `/private/tmp/claude-501/-Users-miko-github-reliaburger/68c7cd97-0874-4446-8c64-77c5be59a15c/scratchpad/qual-tmp/reliaburger-power-cut.6p1DcF`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 13 | `/var/tmp/reliaburger-power-cut.yaAOyw` | PASS | `{"fixture":"leases","acknowledged":1984,"in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 2 | 0 | `/var/tmp/reliaburger-power-cut.zjgG7U` | PASS | `{"fixture":"leases","acknowledged":28,"in_flight_landed":0,"live_leases":11,"stale_temporaries":0}` |
| 3 | 0 | `/var/tmp/reliaburger-power-cut.bVpt8Z` | PASS | `{"fixture":"leases","acknowledged":29,"in_flight_landed":0,"live_leases":10,"stale_temporaries":0}` |
| 4 | 18 | `/var/tmp/reliaburger-power-cut.9hUe3j` | PASS | `{"fixture":"leases","acknowledged":2705,"in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 5 | 0 | `/var/tmp/reliaburger-power-cut.xnmwCu` | PASS | `{"fixture":"leases","acknowledged":27,"in_flight_landed":0,"live_leases":11,"stale_temporaries":0}` |
| 6 | 16 | `/var/tmp/reliaburger-power-cut.wNP1YZ` | PASS | `{"fixture":"leases","acknowledged":2411,"in_flight_landed":0,"live_leases":17,"stale_temporaries":1}` |
| 7 | 11 | `/var/tmp/reliaburger-power-cut.KOvW63` | PASS | `{"fixture":"leases","acknowledged":1689,"in_flight_landed":1,"live_leases":17,"stale_temporaries":0}` |
| 8 | 19 | `/var/tmp/reliaburger-power-cut.O2ljVg` | PASS | `{"fixture":"leases","acknowledged":2891,"in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 9 | 17 | `/var/tmp/reliaburger-power-cut.Ob7ZVu` | PASS | `{"fixture":"leases","acknowledged":2572,"in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 10 | 8 | `/var/tmp/reliaburger-power-cut.24I5Ij` | PASS | `{"fixture":"leases","acknowledged":1218,"in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 11 | 2 | `/var/tmp/reliaburger-power-cut.vJ2TIH` | PASS | `{"fixture":"leases","acknowledged":346,"in_flight_landed":0,"live_leases":16,"stale_temporaries":1}` |
| 12 | 13 | `/var/tmp/reliaburger-power-cut.D5n8to` | PASS | `{"fixture":"leases","acknowledged":1980,"in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 13 | 3 | `/var/tmp/reliaburger-power-cut.e7rYgN` | PASS | `{"fixture":"leases","acknowledged":459,"in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 14 | 1 | `/var/tmp/reliaburger-power-cut.HCb8Cn` | PASS | `{"fixture":"leases","acknowledged":187,"in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 15 | 18 | `/var/tmp/reliaburger-power-cut.cKHHCQ` | PASS | `{"fixture":"leases","acknowledged":2690,"in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |

15 of 15 iterations passed. Rule-of-three bound: failure rate < 20.0% at 95% confidence.
