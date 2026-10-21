# Storage power-cut qualification: backups

- VM: `rb-pcut-abec2c45` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_council_backups`
- Commit: `abec2c459d559652ced52a40d5c34973571cef9b`; binary SHA-256 `3360f06c981627fa0080a5f825d09b6a0a802237d06aede168415a81f774834a`
- Started: 2026-10-21T02:20:52Z
- Host evidence: `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-power-cut.WXKOVN`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 17 | `/var/tmp/reliaburger-power-cut.p5SFgC` | PASS | `{"fixture":"backups","acknowledged":691,"present":4,"in_flight_landed":0,"staged_leftovers":0}` |
| 2 | 18 | `/var/tmp/reliaburger-power-cut.KtbKep` | PASS | `{"fixture":"backups","acknowledged":720,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 3 | 7 | `/var/tmp/reliaburger-power-cut.mBABcA` | PASS | `{"fixture":"backups","acknowledged":281,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 4 | 9 | `/var/tmp/reliaburger-power-cut.bbSeg7` | PASS | `{"fixture":"backups","acknowledged":372,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 5 | 2 | `/var/tmp/reliaburger-power-cut.GQ4GXs` | PASS | `{"fixture":"backups","acknowledged":79,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 6 | 18 | `/var/tmp/reliaburger-power-cut.xFbUK7` | PASS | `{"fixture":"backups","acknowledged":702,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 7 | 0 | `/var/tmp/reliaburger-power-cut.HN1mgb` | PASS | `{"fixture":"backups","acknowledged":11,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 8 | 2 | `/var/tmp/reliaburger-power-cut.hmr6ba` | PASS | `{"fixture":"backups","acknowledged":92,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 9 | 8 | `/var/tmp/reliaburger-power-cut.ruWqEJ` | PASS | `{"fixture":"backups","acknowledged":311,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 10 | 17 | `/var/tmp/reliaburger-power-cut.Ojx2oq` | PASS | `{"fixture":"backups","acknowledged":674,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 11 | 16 | `/var/tmp/reliaburger-power-cut.xWRzrE` | PASS | `{"fixture":"backups","acknowledged":646,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 12 | 18 | `/var/tmp/reliaburger-power-cut.s7ELKA` | PASS | `{"fixture":"backups","acknowledged":719,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 13 | 17 | `/var/tmp/reliaburger-power-cut.dd4bl0` | PASS | `{"fixture":"backups","acknowledged":683,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 14 | 9 | `/var/tmp/reliaburger-power-cut.WoTdbh` | PASS | `{"fixture":"backups","acknowledged":370,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 15 | 3 | `/var/tmp/reliaburger-power-cut.E22rM7` | PASS | `{"fixture":"backups","acknowledged":135,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 16 | 11 | `/var/tmp/reliaburger-power-cut.1Q6pmR` | PASS | `{"fixture":"backups","acknowledged":428,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 17 | 0 | `/var/tmp/reliaburger-power-cut.xY6peO` | PASS | `{"fixture":"backups","acknowledged":11,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 18 | 16 | `/var/tmp/reliaburger-power-cut.g8GkWv` | PASS | `{"fixture":"backups","acknowledged":632,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 19 | 19 | `/var/tmp/reliaburger-power-cut.QAzDNc` | PASS | `{"fixture":"backups","acknowledged":767,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 20 | 7 | `/var/tmp/reliaburger-power-cut.CGtu1J` | PASS | `{"fixture":"backups","acknowledged":295,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 21 | 13 | `/var/tmp/reliaburger-power-cut.8zqhDi` | PASS | `{"fixture":"backups","acknowledged":529,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 22 | 2 | `/var/tmp/reliaburger-power-cut.nmB5a1` | PASS | `{"fixture":"backups","acknowledged":94,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 23 | 15 | `/var/tmp/reliaburger-power-cut.mruslg` | PASS | `{"fixture":"backups","acknowledged":600,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 24 | 15 | `/var/tmp/reliaburger-power-cut.ZLvhpe` | PASS | `{"fixture":"backups","acknowledged":593,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 25 | 8 | `/var/tmp/reliaburger-power-cut.eFjYJJ` | PASS | `{"fixture":"backups","acknowledged":316,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
