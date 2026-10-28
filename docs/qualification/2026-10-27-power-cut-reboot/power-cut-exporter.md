# Storage power-cut qualification: exporter

- VM: `rb-pcut-d387d569` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_log_exports`
- Commit: `d387d569bb1f18264382ffcffebe3bbf45fb7ed3`; binary SHA-256 `75bf9f19654613b2d8913f41c18654a062a69c43fff7cf40aa84bef2cbff4bce`
- Started: 2026-10-27T11:45:42Z
- Host evidence: `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-power-cut.hMoAWJ`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 13 | `/var/tmp/reliaburger-power-cut.aKIqlz` | PASS | `{"acknowledged":162,"checkpointed":7,"destination_objects":163,"fixture":"exporter","generated":162,"pruned":156,"published":162,"staged_leftovers":0,"stale_temporaries":0}` |
| 2 | 19 | `/var/tmp/reliaburger-power-cut.Ncv9f5` | PASS | `{"acknowledged":236,"checkpointed":6,"destination_objects":237,"fixture":"exporter","generated":236,"pruned":230,"published":236,"staged_leftovers":0,"stale_temporaries":0}` |
| 3 | 16 | `/var/tmp/reliaburger-power-cut.hcMGTA` | PASS | `{"acknowledged":204,"checkpointed":6,"destination_objects":205,"fixture":"exporter","generated":204,"pruned":198,"published":204,"staged_leftovers":0,"stale_temporaries":0}` |
| 4 | 5 | `/var/tmp/reliaburger-power-cut.mRKRHE` | PASS | `{"acknowledged":65,"checkpointed":7,"destination_objects":66,"fixture":"exporter","generated":65,"pruned":59,"published":65,"staged_leftovers":0,"stale_temporaries":0}` |
| 5 | 0 | `/var/tmp/reliaburger-power-cut.2xqQsv` | PASS | `{"acknowledged":4,"checkpointed":4,"destination_objects":5,"fixture":"exporter","generated":4,"pruned":0,"published":4,"staged_leftovers":0,"stale_temporaries":0}` |
| 6 | 18 | `/var/tmp/reliaburger-power-cut.06iv02` | PASS | `{"acknowledged":221,"checkpointed":7,"destination_objects":223,"fixture":"exporter","generated":222,"pruned":215,"published":222,"staged_leftovers":0,"stale_temporaries":0}` |
| 7 | 13 | `/var/tmp/reliaburger-power-cut.QYBGB4` | PASS | `{"acknowledged":163,"checkpointed":6,"destination_objects":164,"fixture":"exporter","generated":163,"pruned":157,"published":163,"staged_leftovers":0,"stale_temporaries":1}` |
| 8 | 17 | `/var/tmp/reliaburger-power-cut.auMHnT` | PASS | `{"acknowledged":207,"checkpointed":7,"destination_objects":208,"fixture":"exporter","generated":207,"pruned":201,"published":207,"staged_leftovers":0,"stale_temporaries":0}` |
| 9 | 4 | `/var/tmp/reliaburger-power-cut.QmFp3S` | PASS | `{"acknowledged":52,"checkpointed":6,"destination_objects":53,"fixture":"exporter","generated":52,"pruned":46,"published":52,"staged_leftovers":0,"stale_temporaries":0}` |
| 10 | 17 | `/var/tmp/reliaburger-power-cut.LkAukj` | PASS | `{"acknowledged":217,"checkpointed":6,"destination_objects":218,"fixture":"exporter","generated":217,"pruned":211,"published":217,"staged_leftovers":0,"stale_temporaries":0}` |
| 11 | 9 | `/var/tmp/reliaburger-power-cut.kqJ6yg` | PASS | `{"acknowledged":116,"checkpointed":7,"destination_objects":117,"fixture":"exporter","generated":116,"pruned":109,"published":116,"staged_leftovers":0,"stale_temporaries":0}` |
| 12 | 1 | `/var/tmp/reliaburger-power-cut.CY2ITQ` | PASS | `{"acknowledged":17,"checkpointed":10,"destination_objects":18,"fixture":"exporter","generated":17,"pruned":7,"published":17,"staged_leftovers":0,"stale_temporaries":0}` |
| 13 | 13 | `/var/tmp/reliaburger-power-cut.f08CbM` | PASS | `{"acknowledged":159,"checkpointed":6,"destination_objects":160,"fixture":"exporter","generated":159,"pruned":153,"published":159,"staged_leftovers":0,"stale_temporaries":0}` |
| 14 | 2 | `/var/tmp/reliaburger-power-cut.sOhqTA` | PASS | `{"acknowledged":29,"checkpointed":7,"destination_objects":30,"fixture":"exporter","generated":29,"pruned":22,"published":29,"staged_leftovers":0,"stale_temporaries":0}` |
| 15 | 8 | `/var/tmp/reliaburger-power-cut.cRJlsz` | PASS | `{"acknowledged":95,"checkpointed":6,"destination_objects":96,"fixture":"exporter","generated":95,"pruned":89,"published":95,"staged_leftovers":0,"stale_temporaries":0}` |
| 16 | 4 | `/var/tmp/reliaburger-power-cut.wIZNbk` | PASS | `{"acknowledged":47,"checkpointed":7,"destination_objects":48,"fixture":"exporter","generated":47,"pruned":40,"published":47,"staged_leftovers":0,"stale_temporaries":0}` |
| 17 | 12 | `/var/tmp/reliaburger-power-cut.ZmJAun` | PASS | `{"acknowledged":148,"checkpointed":7,"destination_objects":149,"fixture":"exporter","generated":148,"pruned":142,"published":148,"staged_leftovers":0,"stale_temporaries":0}` |
| 18 | 10 | `/var/tmp/reliaburger-power-cut.3dCRbv` | PASS | `{"acknowledged":124,"checkpointed":6,"destination_objects":125,"fixture":"exporter","generated":124,"pruned":120,"published":124,"staged_leftovers":0,"stale_temporaries":0}` |
| 19 | 14 | `/var/tmp/reliaburger-power-cut.8VKr18` | PASS | `{"acknowledged":190,"checkpointed":7,"destination_objects":191,"fixture":"exporter","generated":190,"pruned":183,"published":190,"staged_leftovers":0,"stale_temporaries":0}` |
| 20 | 17 | `/var/tmp/reliaburger-power-cut.yPdRLV` | PASS | `{"acknowledged":204,"checkpointed":7,"destination_objects":205,"fixture":"exporter","generated":204,"pruned":197,"published":204,"staged_leftovers":0,"stale_temporaries":0}` |
| 21 | 18 | `/var/tmp/reliaburger-power-cut.VT9lMN` | PASS | `{"acknowledged":225,"checkpointed":6,"destination_objects":227,"fixture":"exporter","generated":226,"pruned":219,"published":226,"staged_leftovers":0,"stale_temporaries":0}` |
| 22 | 4 | `/var/tmp/reliaburger-power-cut.LVRsYL` | PASS | `{"acknowledged":58,"checkpointed":7,"destination_objects":59,"fixture":"exporter","generated":58,"pruned":51,"published":58,"staged_leftovers":0,"stale_temporaries":0}` |
| 23 | 1 | `/var/tmp/reliaburger-power-cut.3ilxB6` | PASS | `{"acknowledged":15,"checkpointed":7,"destination_objects":16,"fixture":"exporter","generated":15,"pruned":8,"published":15,"staged_leftovers":0,"stale_temporaries":0}` |
| 24 | 16 | `/var/tmp/reliaburger-power-cut.GOZWuz` | PASS | `{"acknowledged":196,"checkpointed":7,"destination_objects":197,"fixture":"exporter","generated":196,"pruned":190,"published":196,"staged_leftovers":0,"stale_temporaries":0}` |
| 25 | 0 | `/var/tmp/reliaburger-power-cut.h7Yjl2` | PASS | `{"acknowledged":4,"checkpointed":4,"destination_objects":5,"fixture":"exporter","generated":4,"pruned":0,"published":4,"staged_leftovers":0,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
