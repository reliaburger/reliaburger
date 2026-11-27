# Storage power-cut qualification: backups

- VM: `rb-pcut-5f21eef6` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_council_backups`
- Commit: `5f21eef6abc384d7e1c3cb8ff633aa7c94074a7f`; binary SHA-256 `3d186af5945868cde72d84b2302f5311112f21864a6a1978e640b37b01996795`
- Started: 2026-11-26T23:45:56Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.zA8fv7`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 11 | `/var/tmp/reliaburger-power-cut.iGqHHA` | PASS | `{"acknowledged":494,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 2 | 7 | `/var/tmp/reliaburger-power-cut.Rz9BiU` | PASS | `{"acknowledged":300,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 3 | 13 | `/var/tmp/reliaburger-power-cut.tTdr45` | PASS | `{"acknowledged":576,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 4 | 15 | `/var/tmp/reliaburger-power-cut.UKdg17` | PASS | `{"acknowledged":662,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 5 | 16 | `/var/tmp/reliaburger-power-cut.AOnjNO` | PASS | `{"acknowledged":689,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 6 | 9 | `/var/tmp/reliaburger-power-cut.OKGXoE` | PASS | `{"acknowledged":414,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 7 | 1 | `/var/tmp/reliaburger-power-cut.3mVFkt` | PASS | `{"acknowledged":53,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 8 | 10 | `/var/tmp/reliaburger-power-cut.gfDiGj` | PASS | `{"acknowledged":437,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 9 | 9 | `/var/tmp/reliaburger-power-cut.3FkssE` | PASS | `{"acknowledged":395,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 10 | 18 | `/var/tmp/reliaburger-power-cut.nPb2sO` | PASS | `{"acknowledged":807,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 11 | 9 | `/var/tmp/reliaburger-power-cut.rrdHXE` | PASS | `{"acknowledged":409,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 12 | 20 | `/var/tmp/reliaburger-power-cut.UwapgA` | PASS | `{"acknowledged":879,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 13 | 9 | `/var/tmp/reliaburger-power-cut.xzv5K3` | PASS | `{"acknowledged":408,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 14 | 20 | `/var/tmp/reliaburger-power-cut.vlpNJ2` | PASS | `{"acknowledged":878,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 15 | 15 | `/var/tmp/reliaburger-power-cut.FW3OQI` | PASS | `{"acknowledged":663,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 16 | 16 | `/var/tmp/reliaburger-power-cut.QPPCTl` | PASS | `{"acknowledged":719,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 17 | 7 | `/var/tmp/reliaburger-power-cut.OUmzIg` | PASS | `{"acknowledged":316,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 18 | 1 | `/var/tmp/reliaburger-power-cut.RMjPrI` | PASS | `{"acknowledged":49,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 19 | 10 | `/var/tmp/reliaburger-power-cut.O0jFCG` | PASS | `{"acknowledged":451,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 20 | 20 | `/var/tmp/reliaburger-power-cut.sXK4VH` | PASS | `{"acknowledged":855,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 21 | 2 | `/var/tmp/reliaburger-power-cut.6gNQhU` | PASS | `{"acknowledged":92,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 22 | 9 | `/var/tmp/reliaburger-power-cut.cWIGYS` | PASS | `{"acknowledged":402,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 23 | 13 | `/var/tmp/reliaburger-power-cut.Ljn9cZ` | PASS | `{"acknowledged":568,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 24 | 19 | `/var/tmp/reliaburger-power-cut.vU2j67` | PASS | `{"acknowledged":839,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 25 | 14 | `/var/tmp/reliaburger-power-cut.JBfpPT` | PASS | `{"acknowledged":607,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
