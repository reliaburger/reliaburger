# Storage power-cut qualification: exporter

- VM: `rb-pcut-1612ee38` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_log_exports`
- Commit: `1612ee38cc2f976a28f2606c4d14a37ea77be6c0`; binary SHA-256 `4a66c3a4e012bf8ad14ae61f847d50705cad961af3cde97e490a7a7f2d04e995`
- Started: 2026-11-21T07:02:37Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.nH2tWD`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 7 | `/var/tmp/reliaburger-power-cut.7uJvAp` | PASS | `{"acknowledged":99,"checkpointed":7,"destination_objects":101,"fixture":"exporter","generated":100,"pruned":93,"published":100,"staged_leftovers":0,"stale_temporaries":0}` |
| 2 | 10 | `/var/tmp/reliaburger-power-cut.z0n7Oi` | PASS | `{"acknowledged":131,"checkpointed":6,"destination_objects":132,"fixture":"exporter","generated":131,"pruned":125,"published":131,"staged_leftovers":0,"stale_temporaries":0}` |
| 3 | 13 | `/var/tmp/reliaburger-power-cut.c1Jx6I` | PASS | `{"acknowledged":165,"checkpointed":6,"destination_objects":166,"fixture":"exporter","generated":165,"pruned":159,"published":165,"staged_leftovers":0,"stale_temporaries":0}` |
| 4 | 2 | `/var/tmp/reliaburger-power-cut.4QeRUC` | PASS | `{"acknowledged":31,"checkpointed":8,"destination_objects":32,"fixture":"exporter","generated":31,"pruned":23,"published":31,"staged_leftovers":0,"stale_temporaries":0}` |
| 5 | 0 | `/var/tmp/reliaburger-power-cut.oXzP69` | PASS | `{"acknowledged":3,"checkpointed":3,"destination_objects":4,"fixture":"exporter","generated":3,"pruned":0,"published":3,"staged_leftovers":0,"stale_temporaries":0}` |
| 6 | 3 | `/var/tmp/reliaburger-power-cut.TXwO4B` | PASS | `{"acknowledged":42,"checkpointed":6,"destination_objects":43,"fixture":"exporter","generated":42,"pruned":36,"published":42,"staged_leftovers":0,"stale_temporaries":0}` |
| 7 | 0 | `/var/tmp/reliaburger-power-cut.tdXXO9` | PASS | `{"acknowledged":4,"checkpointed":4,"destination_objects":5,"fixture":"exporter","generated":4,"pruned":0,"published":4,"staged_leftovers":0,"stale_temporaries":0}` |
| 8 | 18 | `/var/tmp/reliaburger-power-cut.ShfdUS` | PASS | `{"acknowledged":218,"checkpointed":6,"destination_objects":220,"fixture":"exporter","generated":219,"pruned":212,"published":219,"staged_leftovers":0,"stale_temporaries":0}` |
| 9 | 7 | `/var/tmp/reliaburger-power-cut.gk8L1g` | PASS | `{"acknowledged":90,"checkpointed":6,"destination_objects":91,"fixture":"exporter","generated":90,"pruned":84,"published":90,"staged_leftovers":0,"stale_temporaries":0}` |
| 10 | 19 | `/var/tmp/reliaburger-power-cut.7HYIFo` | PASS | `{"acknowledged":242,"checkpointed":5,"destination_objects":243,"fixture":"exporter","generated":242,"pruned":237,"published":242,"staged_leftovers":0,"stale_temporaries":0}` |
| 11 | 15 | `/var/tmp/reliaburger-power-cut.2FeqvJ` | PASS | `{"acknowledged":195,"checkpointed":7,"destination_objects":196,"fixture":"exporter","generated":195,"pruned":189,"published":195,"staged_leftovers":0,"stale_temporaries":0}` |
| 12 | 8 | `/var/tmp/reliaburger-power-cut.dd6yof` | PASS | `{"acknowledged":110,"checkpointed":7,"destination_objects":111,"fixture":"exporter","generated":110,"pruned":104,"published":110,"staged_leftovers":0,"stale_temporaries":0}` |
| 13 | 15 | `/var/tmp/reliaburger-power-cut.Akt1A2` | PASS | `{"acknowledged":192,"checkpointed":6,"destination_objects":193,"fixture":"exporter","generated":192,"pruned":186,"published":192,"staged_leftovers":0,"stale_temporaries":0}` |
| 14 | 2 | `/var/tmp/reliaburger-power-cut.016hDo` | PASS | `{"acknowledged":29,"checkpointed":6,"destination_objects":31,"fixture":"exporter","generated":30,"pruned":23,"published":30,"staged_leftovers":0,"stale_temporaries":0}` |
| 15 | 17 | `/var/tmp/reliaburger-power-cut.vOWeoF` | PASS | `{"acknowledged":219,"checkpointed":6,"destination_objects":220,"fixture":"exporter","generated":219,"pruned":213,"published":219,"staged_leftovers":0,"stale_temporaries":0}` |
| 16 | 10 | `/var/tmp/reliaburger-power-cut.GZsIPr` | PASS | `{"acknowledged":131,"checkpointed":7,"destination_objects":132,"fixture":"exporter","generated":131,"pruned":124,"published":131,"staged_leftovers":0,"stale_temporaries":0}` |
| 17 | 17 | `/var/tmp/reliaburger-power-cut.gdTdra` | PASS | `{"acknowledged":225,"checkpointed":6,"destination_objects":226,"fixture":"exporter","generated":225,"pruned":219,"published":225,"staged_leftovers":0,"stale_temporaries":0}` |
| 18 | 18 | `/var/tmp/reliaburger-power-cut.3HjFrZ` | PASS | `{"acknowledged":229,"checkpointed":6,"destination_objects":230,"fixture":"exporter","generated":229,"pruned":223,"published":229,"staged_leftovers":0,"stale_temporaries":0}` |
| 19 | 15 | `/var/tmp/reliaburger-power-cut.v4BgOW` | PASS | `{"acknowledged":187,"checkpointed":6,"destination_objects":189,"fixture":"exporter","generated":188,"pruned":181,"published":188,"staged_leftovers":0,"stale_temporaries":0}` |
| 20 | 16 | `/var/tmp/reliaburger-power-cut.FaCnnf` | PASS | `{"acknowledged":212,"checkpointed":6,"destination_objects":213,"fixture":"exporter","generated":212,"pruned":206,"published":212,"staged_leftovers":0,"stale_temporaries":0}` |
| 21 | 5 | `/var/tmp/reliaburger-power-cut.HxLYw8` | PASS | `{"acknowledged":65,"checkpointed":8,"destination_objects":66,"fixture":"exporter","generated":65,"pruned":57,"published":65,"staged_leftovers":0,"stale_temporaries":0}` |
| 22 | 8 | `/var/tmp/reliaburger-power-cut.CsWS1h` | PASS | `{"acknowledged":106,"checkpointed":6,"destination_objects":107,"fixture":"exporter","generated":106,"pruned":100,"published":106,"staged_leftovers":0,"stale_temporaries":0}` |
| 23 | 20 | `/var/tmp/reliaburger-power-cut.E13Bh6` | PASS | `{"acknowledged":259,"checkpointed":7,"destination_objects":261,"fixture":"exporter","generated":260,"pruned":252,"published":260,"staged_leftovers":0,"stale_temporaries":0}` |
| 24 | 7 | `/var/tmp/reliaburger-power-cut.fOIGAz` | PASS | `{"acknowledged":86,"checkpointed":6,"destination_objects":87,"fixture":"exporter","generated":87,"pruned":80,"published":86,"staged_leftovers":0,"stale_temporaries":0}` |
| 25 | 2 | `/var/tmp/reliaburger-power-cut.BHFua6` | PASS | `{"acknowledged":28,"checkpointed":6,"destination_objects":30,"fixture":"exporter","generated":29,"pruned":22,"published":29,"staged_leftovers":0,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
