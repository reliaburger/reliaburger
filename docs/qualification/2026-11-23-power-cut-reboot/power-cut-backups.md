# Storage power-cut qualification: backups

- VM: `rb-pcut-2aae7e2a` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_council_backups`
- Commit: `2aae7e2a075f24614eb626aef83a5c29a279fb34`; binary SHA-256 `641bfed43d860a3da96e740299d8202827967c245ab2fa35f87da32f3f0fbef7`
- Started: 2026-11-23T01:31:37Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.BV0Mz6`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 19 | `/var/tmp/reliaburger-power-cut.dU9jGb` | PASS | `{"acknowledged":789,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 2 | 7 | `/var/tmp/reliaburger-power-cut.cSBoEI` | PASS | `{"acknowledged":305,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 3 | 13 | `/var/tmp/reliaburger-power-cut.TXABBz` | PASS | `{"acknowledged":546,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 4 | 14 | `/var/tmp/reliaburger-power-cut.DDfYvu` | PASS | `{"acknowledged":579,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 5 | 2 | `/var/tmp/reliaburger-power-cut.JkUb74` | PASS | `{"acknowledged":87,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 6 | 9 | `/var/tmp/reliaburger-power-cut.KYxNzM` | PASS | `{"acknowledged":375,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 7 | 16 | `/var/tmp/reliaburger-power-cut.mDZzNZ` | PASS | `{"acknowledged":649,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 8 | 7 | `/var/tmp/reliaburger-power-cut.7qdA4a` | PASS | `{"acknowledged":294,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 9 | 17 | `/var/tmp/reliaburger-power-cut.JQ8Rnj` | PASS | `{"acknowledged":691,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 10 | 0 | `/var/tmp/reliaburger-power-cut.2mDpCL` | PASS | `{"acknowledged":11,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 11 | 7 | `/var/tmp/reliaburger-power-cut.B5PuQC` | PASS | `{"acknowledged":301,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 12 | 14 | `/var/tmp/reliaburger-power-cut.US4yRA` | PASS | `{"acknowledged":578,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 13 | 15 | `/var/tmp/reliaburger-power-cut.sJtOak` | PASS | `{"acknowledged":637,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 14 | 4 | `/var/tmp/reliaburger-power-cut.IhVzwc` | PASS | `{"acknowledged":173,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 15 | 15 | `/var/tmp/reliaburger-power-cut.d8u56L` | PASS | `{"acknowledged":634,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 16 | 8 | `/var/tmp/reliaburger-power-cut.VufLfS` | PASS | `{"acknowledged":326,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 17 | 19 | `/var/tmp/reliaburger-power-cut.jjQ3sc` | PASS | `{"acknowledged":763,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 18 | 0 | `/var/tmp/reliaburger-power-cut.n3OzJ3` | PASS | `{"acknowledged":8,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 19 | 0 | `/var/tmp/reliaburger-power-cut.561dfZ` | PASS | `{"acknowledged":9,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 20 | 15 | `/var/tmp/reliaburger-power-cut.nrpOsp` | PASS | `{"acknowledged":641,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 21 | 1 | `/var/tmp/reliaburger-power-cut.RuI4o2` | PASS | `{"acknowledged":52,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 22 | 3 | `/var/tmp/reliaburger-power-cut.x9C1ET` | PASS | `{"acknowledged":133,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 23 | 13 | `/var/tmp/reliaburger-power-cut.yR7WN5` | PASS | `{"acknowledged":555,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 24 | 9 | `/var/tmp/reliaburger-power-cut.To87Tn` | PASS | `{"acknowledged":367,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 25 | 14 | `/var/tmp/reliaburger-power-cut.gRvpbd` | PASS | `{"acknowledged":555,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
