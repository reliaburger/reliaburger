# Storage power-cut qualification: backups

- VM: `rb-pcut-954bb7b1` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_council_backups`
- Commit: `954bb7b107d78632eb1a5504d00d4843ceed0447`; binary SHA-256 `0422802037fd3d3e5800ee25f347b67834d0920baa6a2821d28abdae5f70bfb6`
- Started: 2026-11-02T01:07:13Z
- Host evidence: `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-power-cut.SzVmz9`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 12 | `/var/tmp/reliaburger-power-cut.AjNyb7` | PASS | `{"acknowledged":473,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 2 | 20 | `/var/tmp/reliaburger-power-cut.G9lsdY` | PASS | `{"acknowledged":804,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 3 | 9 | `/var/tmp/reliaburger-power-cut.barK3E` | PASS | `{"acknowledged":346,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 4 | 0 | `/var/tmp/reliaburger-power-cut.rk9H64` | PASS | `{"acknowledged":11,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 5 | 12 | `/var/tmp/reliaburger-power-cut.TKid7z` | PASS | `{"acknowledged":454,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 6 | 12 | `/var/tmp/reliaburger-power-cut.dGx13X` | PASS | `{"acknowledged":478,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 7 | 19 | `/var/tmp/reliaburger-power-cut.CQ8A6B` | PASS | `{"acknowledged":739,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 8 | 17 | `/var/tmp/reliaburger-power-cut.W4Kqs1` | PASS | `{"acknowledged":668,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 9 | 3 | `/var/tmp/reliaburger-power-cut.V83gD8` | PASS | `{"acknowledged":124,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 10 | 10 | `/var/tmp/reliaburger-power-cut.rLusd5` | PASS | `{"acknowledged":393,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 11 | 2 | `/var/tmp/reliaburger-power-cut.ieo7VR` | PASS | `{"acknowledged":89,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 12 | 2 | `/var/tmp/reliaburger-power-cut.Ww0HMq` | PASS | `{"acknowledged":82,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 13 | 4 | `/var/tmp/reliaburger-power-cut.s9BHIf` | PASS | `{"acknowledged":169,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 14 | 3 | `/var/tmp/reliaburger-power-cut.MxYBQa` | PASS | `{"acknowledged":126,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 15 | 17 | `/var/tmp/reliaburger-power-cut.wg7KWO` | PASS | `{"acknowledged":659,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 16 | 1 | `/var/tmp/reliaburger-power-cut.Vwxr6A` | PASS | `{"acknowledged":50,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 17 | 15 | `/var/tmp/reliaburger-power-cut.YkvZt0` | PASS | `{"acknowledged":573,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 18 | 9 | `/var/tmp/reliaburger-power-cut.UZnQFv` | PASS | `{"acknowledged":355,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 19 | 19 | `/var/tmp/reliaburger-power-cut.Js6g5X` | PASS | `{"acknowledged":717,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 20 | 3 | `/var/tmp/reliaburger-power-cut.jYblr5` | PASS | `{"acknowledged":121,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 21 | 0 | `/var/tmp/reliaburger-power-cut.qjqtmX` | PASS | `{"acknowledged":12,"fixture":"backups","in_flight_landed":1,"present":4,"staged_leftovers":0}` |
| 22 | 8 | `/var/tmp/reliaburger-power-cut.Creg8j` | PASS | `{"acknowledged":323,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 23 | 19 | `/var/tmp/reliaburger-power-cut.2pmoHX` | PASS | `{"acknowledged":739,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 24 | 19 | `/var/tmp/reliaburger-power-cut.OuUxdJ` | PASS | `{"acknowledged":751,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 25 | 1 | `/var/tmp/reliaburger-power-cut.LP2aUS` | PASS | `{"acknowledged":46,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
