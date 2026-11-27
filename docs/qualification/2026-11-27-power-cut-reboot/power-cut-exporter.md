# Storage power-cut qualification: exporter

- VM: `rb-pcut-5f21eef6` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_log_exports`
- Commit: `5f21eef6abc384d7e1c3cb8ff633aa7c94074a7f`; binary SHA-256 `3d186af5945868cde72d84b2302f5311112f21864a6a1978e640b37b01996795`
- Started: 2026-11-26T23:29:36Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.AC5ljh`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 20 | `/var/tmp/reliaburger-power-cut.ERLFVI` | PASS | `{"acknowledged":265,"checkpointed":6,"destination_objects":266,"fixture":"exporter","generated":265,"pruned":259,"published":265,"staged_leftovers":0,"stale_temporaries":0}` |
| 2 | 19 | `/var/tmp/reliaburger-power-cut.JWxKpn` | PASS | `{"acknowledged":254,"checkpointed":6,"destination_objects":256,"fixture":"exporter","generated":255,"pruned":248,"published":255,"staged_leftovers":0,"stale_temporaries":0}` |
| 3 | 4 | `/var/tmp/reliaburger-power-cut.r1aoEn` | PASS | `{"acknowledged":58,"checkpointed":8,"destination_objects":59,"fixture":"exporter","generated":58,"pruned":50,"published":58,"staged_leftovers":0,"stale_temporaries":0}` |
| 4 | 12 | `/var/tmp/reliaburger-power-cut.dQbheG` | PASS | `{"acknowledged":157,"checkpointed":5,"destination_objects":158,"fixture":"exporter","generated":157,"pruned":152,"published":157,"staged_leftovers":0,"stale_temporaries":0}` |
| 5 | 14 | `/var/tmp/reliaburger-power-cut.24jCB9` | PASS | `{"acknowledged":184,"checkpointed":6,"destination_objects":185,"fixture":"exporter","generated":184,"pruned":178,"published":184,"staged_leftovers":0,"stale_temporaries":0}` |
| 6 | 11 | `/var/tmp/reliaburger-power-cut.szpeJu` | PASS | `{"acknowledged":143,"checkpointed":7,"destination_objects":144,"fixture":"exporter","generated":143,"pruned":136,"published":143,"staged_leftovers":0,"stale_temporaries":0}` |
| 7 | 20 | `/var/tmp/reliaburger-power-cut.A5ptmg` | PASS | `{"acknowledged":264,"checkpointed":6,"destination_objects":265,"fixture":"exporter","generated":264,"pruned":258,"published":264,"staged_leftovers":0,"stale_temporaries":0}` |
| 8 | 10 | `/var/tmp/reliaburger-power-cut.yNmnnn` | PASS | `{"acknowledged":132,"checkpointed":8,"destination_objects":133,"fixture":"exporter","generated":132,"pruned":124,"published":132,"staged_leftovers":0,"stale_temporaries":0}` |
| 9 | 20 | `/var/tmp/reliaburger-power-cut.IMkRG5` | PASS | `{"acknowledged":260,"checkpointed":6,"destination_objects":261,"fixture":"exporter","generated":260,"pruned":254,"published":260,"staged_leftovers":0,"stale_temporaries":0}` |
| 10 | 11 | `/var/tmp/reliaburger-power-cut.nvF4Ou` | PASS | `{"acknowledged":134,"checkpointed":6,"destination_objects":135,"fixture":"exporter","generated":134,"pruned":128,"published":134,"staged_leftovers":0,"stale_temporaries":0}` |
| 11 | 13 | `/var/tmp/reliaburger-power-cut.g5kJCA` | PASS | `{"acknowledged":170,"checkpointed":7,"destination_objects":171,"fixture":"exporter","generated":170,"pruned":164,"published":170,"staged_leftovers":0,"stale_temporaries":0}` |
| 12 | 7 | `/var/tmp/reliaburger-power-cut.Vid01H` | PASS | `{"acknowledged":93,"checkpointed":6,"destination_objects":94,"fixture":"exporter","generated":93,"pruned":87,"published":93,"staged_leftovers":0,"stale_temporaries":0}` |
| 13 | 15 | `/var/tmp/reliaburger-power-cut.jtXYXL` | PASS | `{"acknowledged":189,"checkpointed":6,"destination_objects":191,"fixture":"exporter","generated":190,"pruned":184,"published":190,"staged_leftovers":0,"stale_temporaries":0}` |
| 14 | 5 | `/var/tmp/reliaburger-power-cut.ttMlj3` | PASS | `{"acknowledged":74,"checkpointed":6,"destination_objects":75,"fixture":"exporter","generated":74,"pruned":68,"published":74,"staged_leftovers":0,"stale_temporaries":1}` |
| 15 | 7 | `/var/tmp/reliaburger-power-cut.6P2pck` | PASS | `{"acknowledged":84,"checkpointed":7,"destination_objects":85,"fixture":"exporter","generated":84,"pruned":77,"published":84,"staged_leftovers":0,"stale_temporaries":0}` |
| 16 | 19 | `/var/tmp/reliaburger-power-cut.uw5qyt` | PASS | `{"acknowledged":249,"checkpointed":6,"destination_objects":250,"fixture":"exporter","generated":249,"pruned":243,"published":249,"staged_leftovers":0,"stale_temporaries":0}` |
| 17 | 3 | `/var/tmp/reliaburger-power-cut.LOGjEj` | PASS | `{"acknowledged":41,"checkpointed":6,"destination_objects":42,"fixture":"exporter","generated":41,"pruned":35,"published":41,"staged_leftovers":0,"stale_temporaries":0}` |
| 18 | 8 | `/var/tmp/reliaburger-power-cut.yiuKCY` | PASS | `{"acknowledged":102,"checkpointed":6,"destination_objects":103,"fixture":"exporter","generated":102,"pruned":96,"published":102,"staged_leftovers":0,"stale_temporaries":0}` |
| 19 | 12 | `/var/tmp/reliaburger-power-cut.63dRzF` | PASS | `{"acknowledged":150,"checkpointed":6,"destination_objects":151,"fixture":"exporter","generated":150,"pruned":144,"published":150,"staged_leftovers":0,"stale_temporaries":0}` |
| 20 | 18 | `/var/tmp/reliaburger-power-cut.KjDnwD` | PASS | `{"acknowledged":236,"checkpointed":7,"destination_objects":237,"fixture":"exporter","generated":236,"pruned":230,"published":236,"staged_leftovers":0,"stale_temporaries":0}` |
| 21 | 19 | `/var/tmp/reliaburger-power-cut.tfzyBl` | PASS | `{"acknowledged":229,"checkpointed":6,"destination_objects":230,"fixture":"exporter","generated":229,"pruned":223,"published":229,"staged_leftovers":0,"stale_temporaries":0}` |
| 22 | 9 | `/var/tmp/reliaburger-power-cut.9LkvWc` | PASS | `{"acknowledged":112,"checkpointed":7,"destination_objects":113,"fixture":"exporter","generated":112,"pruned":105,"published":112,"staged_leftovers":0,"stale_temporaries":0}` |
| 23 | 20 | `/var/tmp/reliaburger-power-cut.mA1Law` | PASS | `{"acknowledged":249,"checkpointed":6,"destination_objects":250,"fixture":"exporter","generated":249,"pruned":243,"published":249,"staged_leftovers":0,"stale_temporaries":0}` |
| 24 | 9 | `/var/tmp/reliaburger-power-cut.Dsz0Xa` | PASS | `{"acknowledged":118,"checkpointed":6,"destination_objects":120,"fixture":"exporter","generated":119,"pruned":112,"published":119,"staged_leftovers":0,"stale_temporaries":0}` |
| 25 | 2 | `/var/tmp/reliaburger-power-cut.StQrnW` | PASS | `{"acknowledged":30,"checkpointed":6,"destination_objects":31,"fixture":"exporter","generated":30,"pruned":24,"published":30,"staged_leftovers":0,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
