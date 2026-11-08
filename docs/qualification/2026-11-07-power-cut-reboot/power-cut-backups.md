# Storage power-cut qualification: backups

- VM: `rb-pcut-ace072c7` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_council_backups`
- Commit: `ace072c73c2d5a5d7124f3a4f879c88bf9c33839`; binary SHA-256 `fbfc5fb026052d4278028dd2bb2cf0cec3642059608076fc27296ff07bc6f7e6`
- Started: 2026-11-07T03:13:26Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.STOpZW`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 7 | `/var/tmp/reliaburger-power-cut.4idX2d` | PASS | `{"acknowledged":292,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 2 | 7 | `/var/tmp/reliaburger-power-cut.E3l5LD` | PASS | `{"acknowledged":292,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 3 | 5 | `/var/tmp/reliaburger-power-cut.YDXGKG` | PASS | `{"acknowledged":225,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 4 | 10 | `/var/tmp/reliaburger-power-cut.SwD8UY` | PASS | `{"acknowledged":411,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 5 | 13 | `/var/tmp/reliaburger-power-cut.bhVCH4` | PASS | `{"acknowledged":536,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 6 | 12 | `/var/tmp/reliaburger-power-cut.80PVAv` | PASS | `{"acknowledged":487,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 7 | 16 | `/var/tmp/reliaburger-power-cut.UkjXXt` | PASS | `{"acknowledged":675,"fixture":"backups","in_flight_landed":1,"present":4,"staged_leftovers":0}` |
| 8 | 18 | `/var/tmp/reliaburger-power-cut.35ZyrY` | PASS | `{"acknowledged":739,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 9 | 11 | `/var/tmp/reliaburger-power-cut.f0SLMa` | PASS | `{"acknowledged":467,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 10 | 18 | `/var/tmp/reliaburger-power-cut.n8KLd2` | PASS | `{"acknowledged":738,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 11 | 7 | `/var/tmp/reliaburger-power-cut.MOMyXn` | PASS | `{"acknowledged":287,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 12 | 20 | `/var/tmp/reliaburger-power-cut.2dEZ6Y` | PASS | `{"acknowledged":811,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 13 | 14 | `/var/tmp/reliaburger-power-cut.ErqLKP` | PASS | `{"acknowledged":579,"fixture":"backups","in_flight_landed":0,"present":4,"staged_leftovers":0}` |
| 14 | 13 | `/var/tmp/reliaburger-power-cut.zHyYa6` | PASS | `{"acknowledged":544,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 15 | 3 | `/var/tmp/reliaburger-power-cut.9OVb52` | PASS | `{"acknowledged":137,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 16 | 19 | `/var/tmp/reliaburger-power-cut.rsOtg0` | PASS | `{"acknowledged":772,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 17 | 14 | `/var/tmp/reliaburger-power-cut.QtK9tq` | PASS | `{"acknowledged":582,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 18 | 19 | `/var/tmp/reliaburger-power-cut.YQmTVn` | PASS | `{"acknowledged":775,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 19 | 8 | `/var/tmp/reliaburger-power-cut.fkdX2q` | PASS | `{"acknowledged":323,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 20 | 8 | `/var/tmp/reliaburger-power-cut.o2JR8p` | PASS | `{"acknowledged":341,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 21 | 5 | `/var/tmp/reliaburger-power-cut.eTX2cT` | PASS | `{"acknowledged":208,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 22 | 16 | `/var/tmp/reliaburger-power-cut.r0VXw3` | PASS | `{"acknowledged":657,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 23 | 0 | `/var/tmp/reliaburger-power-cut.IhTMSo` | PASS | `{"acknowledged":8,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 24 | 4 | `/var/tmp/reliaburger-power-cut.FvwiQB` | PASS | `{"acknowledged":170,"fixture":"backups","in_flight_landed":1,"present":4,"staged_leftovers":0}` |
| 25 | 4 | `/var/tmp/reliaburger-power-cut.lOOJT5` | PASS | `{"acknowledged":168,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
