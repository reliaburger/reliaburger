# Storage power-cut qualification: backups

- VM: `rb-pcut-efc3fb2f` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_council_backups`
- Commit: `efc3fb2fc7882500ed0a4ac050d4b525cb806a54`; binary SHA-256 `60c183ffefbe115642010287ead0e4e3c29cab39d76504e2c0e9c4d6e7f9af9e`
- Started: 2026-11-19T19:34:34Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.07O1Yu`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 11 | `/var/tmp/reliaburger-power-cut.tFDUS2` | PASS | `{"acknowledged":466,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 2 | 0 | `/var/tmp/reliaburger-power-cut.Vs6IQV` | PASS | `{"acknowledged":8,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 3 | 3 | `/var/tmp/reliaburger-power-cut.Nqd20L` | PASS | `{"acknowledged":136,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 4 | 12 | `/var/tmp/reliaburger-power-cut.aK9DAO` | PASS | `{"acknowledged":502,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 5 | 11 | `/var/tmp/reliaburger-power-cut.t8lMyi` | PASS | `{"acknowledged":465,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 6 | 11 | `/var/tmp/reliaburger-power-cut.7NRzAQ` | PASS | `{"acknowledged":456,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 7 | 8 | `/var/tmp/reliaburger-power-cut.AJ6KVP` | PASS | `{"acknowledged":352,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 8 | 13 | `/var/tmp/reliaburger-power-cut.dRSibQ` | PASS | `{"acknowledged":560,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 9 | 13 | `/var/tmp/reliaburger-power-cut.5nXOYr` | PASS | `{"acknowledged":551,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 10 | 16 | `/var/tmp/reliaburger-power-cut.cvqSq4` | PASS | `{"acknowledged":667,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 11 | 10 | `/var/tmp/reliaburger-power-cut.mJcyTC` | PASS | `{"acknowledged":426,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 12 | 7 | `/var/tmp/reliaburger-power-cut.Vjt7Rv` | PASS | `{"acknowledged":299,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 13 | 7 | `/var/tmp/reliaburger-power-cut.14xBR2` | PASS | `{"acknowledged":299,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 14 | 19 | `/var/tmp/reliaburger-power-cut.hrnqvZ` | PASS | `{"acknowledged":794,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 15 | 1 | `/var/tmp/reliaburger-power-cut.jQs6wc` | PASS | `{"acknowledged":49,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 16 | 15 | `/var/tmp/reliaburger-power-cut.0eINL4` | PASS | `{"acknowledged":645,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 17 | 18 | `/var/tmp/reliaburger-power-cut.Dp85KL` | PASS | `{"acknowledged":744,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 18 | 10 | `/var/tmp/reliaburger-power-cut.k5NwU5` | PASS | `{"acknowledged":429,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 19 | 2 | `/var/tmp/reliaburger-power-cut.UdsXew` | PASS | `{"acknowledged":92,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 20 | 10 | `/var/tmp/reliaburger-power-cut.OLhVCZ` | PASS | `{"acknowledged":451,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 21 | 18 | `/var/tmp/reliaburger-power-cut.Kc7lYP` | PASS | `{"acknowledged":756,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 22 | 10 | `/var/tmp/reliaburger-power-cut.ZRN9jI` | PASS | `{"acknowledged":414,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 23 | 2 | `/var/tmp/reliaburger-power-cut.LrW1AY` | PASS | `{"acknowledged":88,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 24 | 4 | `/var/tmp/reliaburger-power-cut.kos1Ky` | PASS | `{"acknowledged":175,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 25 | 0 | `/var/tmp/reliaburger-power-cut.3P6jy3` | PASS | `{"acknowledged":9,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
