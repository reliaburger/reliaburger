# Storage power-cut qualification: backups

- VM: `rb-pcut-1612ee38` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_council_backups`
- Commit: `1612ee38cc2f976a28f2606c4d14a37ea77be6c0`; binary SHA-256 `4a66c3a4e012bf8ad14ae61f847d50705cad961af3cde97e490a7a7f2d04e995`
- Started: 2026-11-21T07:18:14Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.ewv1wz`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 20 | `/var/tmp/reliaburger-power-cut.HhyQ2r` | PASS | `{"acknowledged":822,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 2 | 13 | `/var/tmp/reliaburger-power-cut.a3j6eS` | PASS | `{"acknowledged":515,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 3 | 17 | `/var/tmp/reliaburger-power-cut.EPgLfS` | PASS | `{"acknowledged":697,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 4 | 3 | `/var/tmp/reliaburger-power-cut.eDjvW8` | PASS | `{"acknowledged":133,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 5 | 7 | `/var/tmp/reliaburger-power-cut.jNDnOB` | PASS | `{"acknowledged":286,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 6 | 18 | `/var/tmp/reliaburger-power-cut.aHTTug` | PASS | `{"acknowledged":747,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 7 | 11 | `/var/tmp/reliaburger-power-cut.lex6wO` | PASS | `{"acknowledged":466,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 8 | 15 | `/var/tmp/reliaburger-power-cut.RpnrfH` | PASS | `{"acknowledged":623,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 9 | 7 | `/var/tmp/reliaburger-power-cut.KAudXa` | PASS | `{"acknowledged":304,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 10 | 2 | `/var/tmp/reliaburger-power-cut.h8U9Yf` | PASS | `{"acknowledged":93,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 11 | 3 | `/var/tmp/reliaburger-power-cut.1tkKwm` | PASS | `{"acknowledged":130,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 12 | 18 | `/var/tmp/reliaburger-power-cut.V4owHr` | PASS | `{"acknowledged":747,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 13 | 11 | `/var/tmp/reliaburger-power-cut.F2VQ0q` | PASS | `{"acknowledged":448,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 14 | 10 | `/var/tmp/reliaburger-power-cut.DImT2l` | PASS | `{"acknowledged":431,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 15 | 20 | `/var/tmp/reliaburger-power-cut.J2TLOn` | PASS | `{"acknowledged":826,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 16 | 18 | `/var/tmp/reliaburger-power-cut.kzfvv8` | PASS | `{"acknowledged":763,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 17 | 8 | `/var/tmp/reliaburger-power-cut.Cw1AYJ` | PASS | `{"acknowledged":339,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 18 | 9 | `/var/tmp/reliaburger-power-cut.c3RYk3` | PASS | `{"acknowledged":383,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 19 | 15 | `/var/tmp/reliaburger-power-cut.QqIEGW` | PASS | `{"acknowledged":630,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 20 | 14 | `/var/tmp/reliaburger-power-cut.F2voAm` | PASS | `{"acknowledged":593,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 21 | 13 | `/var/tmp/reliaburger-power-cut.AhFojR` | PASS | `{"acknowledged":537,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 22 | 20 | `/var/tmp/reliaburger-power-cut.EFA8P0` | PASS | `{"acknowledged":815,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 23 | 10 | `/var/tmp/reliaburger-power-cut.4oYhe0` | PASS | `{"acknowledged":427,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 24 | 3 | `/var/tmp/reliaburger-power-cut.XW7mj5` | PASS | `{"acknowledged":136,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 25 | 3 | `/var/tmp/reliaburger-power-cut.LTEvyr` | PASS | `{"acknowledged":131,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
