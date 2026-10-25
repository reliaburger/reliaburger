# Storage power-cut qualification: exporter

- VM: `rb-pcut-43baee64` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_log_exports`
- Commit: `43baee64ae9689b7620f52c33e900acf8a407c43`; binary SHA-256 `8a0e233adfeedf331c62d83be6db72b1411694529f0c8c3b779ceffd9dc411c8`
- Started: 2026-10-25T14:27:21Z
- Host evidence: `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-power-cut.1pebWG`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 16 | `/var/tmp/reliaburger-power-cut.QVIeBN` | PASS | `{"acknowledged":205,"checkpointed":10,"destination_objects":206,"fixture":"exporter","generated":205,"pruned":195,"published":205,"staged_leftovers":0,"stale_temporaries":0}` |
| 2 | 4 | `/var/tmp/reliaburger-power-cut.FBtH1F` | PASS | `{"acknowledged":56,"checkpointed":6,"destination_objects":58,"fixture":"exporter","generated":57,"pruned":50,"published":57,"staged_leftovers":0,"stale_temporaries":0}` |
| 3 | 15 | `/var/tmp/reliaburger-power-cut.FSWkqb` | PASS | `{"acknowledged":188,"checkpointed":6,"destination_objects":189,"fixture":"exporter","generated":188,"pruned":182,"published":188,"staged_leftovers":0,"stale_temporaries":0}` |
| 4 | 16 | `/var/tmp/reliaburger-power-cut.t6pFJW` | PASS | `{"acknowledged":200,"checkpointed":6,"destination_objects":201,"fixture":"exporter","generated":200,"pruned":194,"published":200,"staged_leftovers":0,"stale_temporaries":0}` |
| 5 | 9 | `/var/tmp/reliaburger-power-cut.wz5O6W` | PASS | `{"acknowledged":112,"checkpointed":6,"destination_objects":114,"fixture":"exporter","generated":113,"pruned":106,"published":113,"staged_leftovers":0,"stale_temporaries":0}` |
| 6 | 10 | `/var/tmp/reliaburger-power-cut.9QIdcY` | PASS | `{"acknowledged":125,"checkpointed":7,"destination_objects":126,"fixture":"exporter","generated":125,"pruned":118,"published":125,"staged_leftovers":0,"stale_temporaries":0}` |
| 7 | 20 | `/var/tmp/reliaburger-power-cut.f1SD5P` | PASS | `{"acknowledged":245,"checkpointed":7,"destination_objects":246,"fixture":"exporter","generated":245,"pruned":239,"published":245,"staged_leftovers":0,"stale_temporaries":1}` |
| 8 | 13 | `/var/tmp/reliaburger-power-cut.TT2inU` | PASS | `{"acknowledged":167,"checkpointed":6,"destination_objects":168,"fixture":"exporter","generated":167,"pruned":161,"published":167,"staged_leftovers":0,"stale_temporaries":0}` |
| 9 | 13 | `/var/tmp/reliaburger-power-cut.sVL3Aa` | PASS | `{"acknowledged":156,"checkpointed":7,"destination_objects":157,"fixture":"exporter","generated":156,"pruned":149,"published":156,"staged_leftovers":0,"stale_temporaries":0}` |
| 10 | 5 | `/var/tmp/reliaburger-power-cut.lyRFGz` | PASS | `{"acknowledged":67,"checkpointed":6,"destination_objects":69,"fixture":"exporter","generated":68,"pruned":61,"published":68,"staged_leftovers":0,"stale_temporaries":0}` |
| 11 | 4 | `/var/tmp/reliaburger-power-cut.ugUbKF` | PASS | `{"acknowledged":53,"checkpointed":8,"destination_objects":54,"fixture":"exporter","generated":53,"pruned":45,"published":53,"staged_leftovers":0,"stale_temporaries":0}` |
| 12 | 19 | `/var/tmp/reliaburger-power-cut.IrBgBf` | PASS | `{"acknowledged":230,"checkpointed":7,"destination_objects":231,"fixture":"exporter","generated":230,"pruned":223,"published":230,"staged_leftovers":0,"stale_temporaries":0}` |
| 13 | 4 | `/var/tmp/reliaburger-power-cut.dDkSI4` | PASS | `{"acknowledged":54,"checkpointed":7,"destination_objects":55,"fixture":"exporter","generated":54,"pruned":48,"published":54,"staged_leftovers":0,"stale_temporaries":0}` |
| 14 | 0 | `/var/tmp/reliaburger-power-cut.TzuEvd` | PASS | `{"acknowledged":3,"checkpointed":3,"destination_objects":4,"fixture":"exporter","generated":3,"pruned":0,"published":3,"staged_leftovers":0,"stale_temporaries":0}` |
| 15 | 13 | `/var/tmp/reliaburger-power-cut.czpZ9K` | PASS | `{"acknowledged":163,"checkpointed":6,"destination_objects":165,"fixture":"exporter","generated":164,"pruned":157,"published":164,"staged_leftovers":0,"stale_temporaries":0}` |
| 16 | 16 | `/var/tmp/reliaburger-power-cut.XY3lHU` | PASS | `{"acknowledged":205,"checkpointed":7,"destination_objects":207,"fixture":"exporter","generated":206,"pruned":198,"published":206,"staged_leftovers":0,"stale_temporaries":0}` |
| 17 | 2 | `/var/tmp/reliaburger-power-cut.dsIusd` | PASS | `{"acknowledged":27,"checkpointed":7,"destination_objects":28,"fixture":"exporter","generated":27,"pruned":21,"published":27,"staged_leftovers":0,"stale_temporaries":0}` |
| 18 | 12 | `/var/tmp/reliaburger-power-cut.blXMCw` | PASS | `{"acknowledged":149,"checkpointed":6,"destination_objects":150,"fixture":"exporter","generated":149,"pruned":143,"published":149,"staged_leftovers":0,"stale_temporaries":0}` |
| 19 | 9 | `/var/tmp/reliaburger-power-cut.1qUNRE` | PASS | `{"acknowledged":111,"checkpointed":6,"destination_objects":112,"fixture":"exporter","generated":111,"pruned":105,"published":111,"staged_leftovers":0,"stale_temporaries":0}` |
| 20 | 17 | `/var/tmp/reliaburger-power-cut.lVTukG` | PASS | `{"acknowledged":206,"checkpointed":6,"destination_objects":207,"fixture":"exporter","generated":206,"pruned":200,"published":206,"staged_leftovers":0,"stale_temporaries":0}` |
| 21 | 17 | `/var/tmp/reliaburger-power-cut.5Tv0zl` | PASS | `{"acknowledged":219,"checkpointed":8,"destination_objects":220,"fixture":"exporter","generated":219,"pruned":211,"published":219,"staged_leftovers":0,"stale_temporaries":0}` |
| 22 | 18 | `/var/tmp/reliaburger-power-cut.gjJGMM` | PASS | `{"acknowledged":219,"checkpointed":7,"destination_objects":221,"fixture":"exporter","generated":220,"pruned":212,"published":220,"staged_leftovers":0,"stale_temporaries":0}` |
| 23 | 2 | `/var/tmp/reliaburger-power-cut.ASG0sF` | PASS | `{"acknowledged":29,"checkpointed":7,"destination_objects":31,"fixture":"exporter","generated":30,"pruned":22,"published":30,"staged_leftovers":0,"stale_temporaries":0}` |
| 24 | 15 | `/var/tmp/reliaburger-power-cut.Udc5N8` | PASS | `{"acknowledged":187,"checkpointed":6,"destination_objects":188,"fixture":"exporter","generated":187,"pruned":181,"published":187,"staged_leftovers":0,"stale_temporaries":0}` |
| 25 | 11 | `/var/tmp/reliaburger-power-cut.8E3quv` | PASS | `{"acknowledged":134,"checkpointed":7,"destination_objects":135,"fixture":"exporter","generated":134,"pruned":127,"published":134,"staged_leftovers":0,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
