# Storage power-cut qualification: backups

- VM: `rb-pcut-50596cfe` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_council_backups`
- Commit: `50596cfe9438d16070b1660db45b1d2a29f77106`; binary SHA-256 `fcb2942a43e0d89b809c9a478c9e9c70050375c6ddb82d112d37b92243ab3f58`
- Started: 2026-11-05T12:30:37Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.AoxQmP`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 19 | `/var/tmp/reliaburger-power-cut.JSH8Un` | PASS | `{"acknowledged":788,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 2 | 9 | `/var/tmp/reliaburger-power-cut.UUrZMr` | PASS | `{"acknowledged":370,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 3 | 0 | `/var/tmp/reliaburger-power-cut.y7ykwx` | PASS | `{"acknowledged":10,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 4 | 13 | `/var/tmp/reliaburger-power-cut.FUHwZC` | PASS | `{"acknowledged":570,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 5 | 10 | `/var/tmp/reliaburger-power-cut.dtq7Y7` | PASS | `{"acknowledged":425,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 6 | 0 | `/var/tmp/reliaburger-power-cut.oP9jnx` | PASS | `{"acknowledged":9,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 7 | 20 | `/var/tmp/reliaburger-power-cut.8V7LTq` | PASS | `{"acknowledged":809,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 8 | 14 | `/var/tmp/reliaburger-power-cut.iFpGqt` | PASS | `{"acknowledged":589,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 9 | 4 | `/var/tmp/reliaburger-power-cut.EmCToK` | PASS | `{"acknowledged":182,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 10 | 13 | `/var/tmp/reliaburger-power-cut.OtVT9d` | PASS | `{"acknowledged":548,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 11 | 4 | `/var/tmp/reliaburger-power-cut.W1T3M2` | PASS | `{"acknowledged":169,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 12 | 15 | `/var/tmp/reliaburger-power-cut.FSpBpI` | PASS | `{"acknowledged":627,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 13 | 12 | `/var/tmp/reliaburger-power-cut.yCGOHJ` | PASS | `{"acknowledged":512,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 14 | 15 | `/var/tmp/reliaburger-power-cut.WYNxQY` | PASS | `{"acknowledged":621,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 15 | 2 | `/var/tmp/reliaburger-power-cut.Db977w` | PASS | `{"acknowledged":91,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 16 | 18 | `/var/tmp/reliaburger-power-cut.hyEtge` | PASS | `{"acknowledged":720,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 17 | 12 | `/var/tmp/reliaburger-power-cut.Lz6ivN` | PASS | `{"acknowledged":514,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 18 | 5 | `/var/tmp/reliaburger-power-cut.gZlt2F` | PASS | `{"acknowledged":228,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 19 | 9 | `/var/tmp/reliaburger-power-cut.onGSvX` | PASS | `{"acknowledged":378,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 20 | 8 | `/var/tmp/reliaburger-power-cut.b0k73z` | PASS | `{"acknowledged":336,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 21 | 8 | `/var/tmp/reliaburger-power-cut.Ha9Z5J` | PASS | `{"acknowledged":331,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 22 | 0 | `/var/tmp/reliaburger-power-cut.vY7OCs` | PASS | `{"acknowledged":8,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 23 | 1 | `/var/tmp/reliaburger-power-cut.j9iSoz` | PASS | `{"acknowledged":47,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 24 | 2 | `/var/tmp/reliaburger-power-cut.jBU5sp` | PASS | `{"acknowledged":96,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 25 | 3 | `/var/tmp/reliaburger-power-cut.cLVZp3` | PASS | `{"acknowledged":131,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
