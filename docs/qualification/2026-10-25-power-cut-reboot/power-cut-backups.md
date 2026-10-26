# Storage power-cut qualification: backups

- VM: `rb-pcut-43baee64` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_council_backups`
- Commit: `43baee64ae9689b7620f52c33e900acf8a407c43`; binary SHA-256 `8a0e233adfeedf331c62d83be6db72b1411694529f0c8c3b779ceffd9dc411c8`
- Started: 2026-10-25T14:44:59Z
- Host evidence: `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-power-cut.MrjDb2`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 2 | `/var/tmp/reliaburger-power-cut.wPwTl0` | PASS | `{"acknowledged":87,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 2 | 1 | `/var/tmp/reliaburger-power-cut.V4HxDi` | PASS | `{"acknowledged":51,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 3 | 13 | `/var/tmp/reliaburger-power-cut.KApsyn` | PASS | `{"acknowledged":505,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 4 | 14 | `/var/tmp/reliaburger-power-cut.c1LgpV` | PASS | `{"acknowledged":518,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 5 | 5 | `/var/tmp/reliaburger-power-cut.GDWX3E` | PASS | `{"acknowledged":208,"fixture":"backups","in_flight_landed":1,"present":4,"staged_leftovers":0}` |
| 6 | 9 | `/var/tmp/reliaburger-power-cut.5ppG8w` | PASS | `{"acknowledged":362,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 7 | 6 | `/var/tmp/reliaburger-power-cut.3OxRwM` | PASS | `{"acknowledged":239,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 8 | 15 | `/var/tmp/reliaburger-power-cut.5w3ygj` | PASS | `{"acknowledged":579,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 9 | 10 | `/var/tmp/reliaburger-power-cut.HapW2i` | PASS | `{"acknowledged":391,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 10 | 14 | `/var/tmp/reliaburger-power-cut.SYTA9j` | PASS | `{"acknowledged":542,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 11 | 19 | `/var/tmp/reliaburger-power-cut.TSeTB4` | PASS | `{"acknowledged":740,"fixture":"backups","in_flight_landed":1,"present":4,"staged_leftovers":0}` |
| 12 | 9 | `/var/tmp/reliaburger-power-cut.Dw5fhK` | PASS | `{"acknowledged":350,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 13 | 3 | `/var/tmp/reliaburger-power-cut.bsgwUL` | PASS | `{"acknowledged":129,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 14 | 11 | `/var/tmp/reliaburger-power-cut.BSFVdT` | PASS | `{"acknowledged":443,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 15 | 8 | `/var/tmp/reliaburger-power-cut.vXkqTp` | PASS | `{"acknowledged":311,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 16 | 16 | `/var/tmp/reliaburger-power-cut.w9fqHu` | PASS | `{"acknowledged":624,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 17 | 13 | `/var/tmp/reliaburger-power-cut.kZqmVi` | PASS | `{"acknowledged":506,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 18 | 12 | `/var/tmp/reliaburger-power-cut.X2n4qQ` | PASS | `{"acknowledged":457,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 19 | 16 | `/var/tmp/reliaburger-power-cut.oepyjp` | PASS | `{"acknowledged":619,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 20 | 8 | `/var/tmp/reliaburger-power-cut.ltwtna` | PASS | `{"acknowledged":312,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 21 | 19 | `/var/tmp/reliaburger-power-cut.UjTCR9` | PASS | `{"acknowledged":719,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 22 | 10 | `/var/tmp/reliaburger-power-cut.eCzLX2` | PASS | `{"acknowledged":386,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 23 | 0 | `/var/tmp/reliaburger-power-cut.lJCsR9` | PASS | `{"acknowledged":10,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 24 | 13 | `/var/tmp/reliaburger-power-cut.e2MGYJ` | PASS | `{"acknowledged":502,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 25 | 14 | `/var/tmp/reliaburger-power-cut.aQK9ZO` | PASS | `{"acknowledged":525,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
