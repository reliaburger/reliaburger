# Storage power-cut qualification: backups

- VM: `rb-020-pcut` (guest kernel 6.8.0-142-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_council_backups`
- Commit: `cc5f64393350f2081f7d88512b95a88316172e5a`; binary SHA-256 `33f3ba84343c37d41b6ebb9298e09d17b8e009d385ed84c0eb25ce5146ee3c56`
- Started: 2026-10-16T04:54:28Z
- Host evidence: a temporary directory on the host, not kept

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 0 | `/var/tmp/reliaburger-power-cut.4d5Tqf` | PASS | `{"fixture":"backups","acknowledged":9,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 2 | 4 | `/var/tmp/reliaburger-power-cut.yGgOUz` | PASS | `{"fixture":"backups","acknowledged":179,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 3 | 11 | `/var/tmp/reliaburger-power-cut.dRXBC3` | PASS | `{"fixture":"backups","acknowledged":453,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 4 | 2 | `/var/tmp/reliaburger-power-cut.Qt0JjS` | PASS | `{"fixture":"backups","acknowledged":90,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 5 | 9 | `/var/tmp/reliaburger-power-cut.JhztWP` | PASS | `{"fixture":"backups","acknowledged":366,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 6 | 1 | `/var/tmp/reliaburger-power-cut.nsAddx` | PASS | `{"fixture":"backups","acknowledged":46,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 7 | 15 | `/var/tmp/reliaburger-power-cut.oQVFPv` | PASS | `{"fixture":"backups","acknowledged":612,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 8 | 18 | `/var/tmp/reliaburger-power-cut.zmBVp1` | PASS | `{"fixture":"backups","acknowledged":721,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 9 | 11 | `/var/tmp/reliaburger-power-cut.VFssbq` | PASS | `{"fixture":"backups","acknowledged":454,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 10 | 8 | `/var/tmp/reliaburger-power-cut.fuzhEr` | PASS | `{"fixture":"backups","acknowledged":331,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 11 | 8 | `/var/tmp/reliaburger-power-cut.99ldq0` | PASS | `{"fixture":"backups","acknowledged":346,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 12 | 13 | `/var/tmp/reliaburger-power-cut.goG7Mp` | PASS | `{"fixture":"backups","acknowledged":539,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 13 | 4 | `/var/tmp/reliaburger-power-cut.Igl5QT` | PASS | `{"fixture":"backups","acknowledged":183,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 14 | 15 | `/var/tmp/reliaburger-power-cut.CtA10S` | PASS | `{"fixture":"backups","acknowledged":616,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 15 | 9 | `/var/tmp/reliaburger-power-cut.HiRGiv` | PASS | `{"fixture":"backups","acknowledged":378,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 16 | 11 | `/var/tmp/reliaburger-power-cut.uPoJps` | PASS | `{"fixture":"backups","acknowledged":455,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 17 | 14 | `/var/tmp/reliaburger-power-cut.jJw6Ar` | PASS | `{"fixture":"backups","acknowledged":567,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 18 | 4 | `/var/tmp/reliaburger-power-cut.mIJry9` | PASS | `{"fixture":"backups","acknowledged":186,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 19 | 15 | `/var/tmp/reliaburger-power-cut.MeeMcI` | PASS | `{"fixture":"backups","acknowledged":620,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 20 | 12 | `/var/tmp/reliaburger-power-cut.LACLY2` | PASS | `{"fixture":"backups","acknowledged":481,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 21 | 3 | `/var/tmp/reliaburger-power-cut.Y8eSFR` | PASS | `{"fixture":"backups","acknowledged":127,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 22 | 3 | `/var/tmp/reliaburger-power-cut.89nRzF` | PASS | `{"fixture":"backups","acknowledged":131,"present":4,"in_flight_landed":1,"staged_leftovers":0}` |
| 23 | 12 | `/var/tmp/reliaburger-power-cut.xA0xzr` | PASS | `{"fixture":"backups","acknowledged":491,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 24 | 14 | `/var/tmp/reliaburger-power-cut.OB3CaM` | PASS | `{"fixture":"backups","acknowledged":559,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 25 | 3 | `/var/tmp/reliaburger-power-cut.TjcoA9` | PASS | `{"fixture":"backups","acknowledged":127,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
