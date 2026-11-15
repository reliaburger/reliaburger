# Storage power-cut qualification: exporter

- VM: `rb-pcut-954bb7b1` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_log_exports`
- Commit: `954bb7b107d78632eb1a5504d00d4843ceed0447`; binary SHA-256 `0422802037fd3d3e5800ee25f347b67834d0920baa6a2821d28abdae5f70bfb6`
- Started: 2026-11-02T00:50:17Z
- Host evidence: `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-power-cut.HziVQR`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 14 | `/var/tmp/reliaburger-power-cut.QeaLPo` | PASS | `{"acknowledged":174,"checkpointed":6,"destination_objects":175,"fixture":"exporter","generated":174,"pruned":168,"published":174,"staged_leftovers":0,"stale_temporaries":0}` |
| 2 | 1 | `/var/tmp/reliaburger-power-cut.Zv67o7` | PASS | `{"acknowledged":14,"checkpointed":6,"destination_objects":15,"fixture":"exporter","generated":14,"pruned":8,"published":14,"staged_leftovers":0,"stale_temporaries":0}` |
| 3 | 2 | `/var/tmp/reliaburger-power-cut.mxqCUn` | PASS | `{"acknowledged":26,"checkpointed":6,"destination_objects":28,"fixture":"exporter","generated":27,"pruned":20,"published":27,"staged_leftovers":0,"stale_temporaries":0}` |
| 4 | 13 | `/var/tmp/reliaburger-power-cut.587zcI` | PASS | `{"acknowledged":161,"checkpointed":8,"destination_objects":162,"fixture":"exporter","generated":161,"pruned":154,"published":161,"staged_leftovers":0,"stale_temporaries":0}` |
| 5 | 11 | `/var/tmp/reliaburger-power-cut.Bgg8it` | PASS | `{"acknowledged":140,"checkpointed":6,"destination_objects":141,"fixture":"exporter","generated":140,"pruned":134,"published":140,"staged_leftovers":0,"stale_temporaries":0}` |
| 6 | 10 | `/var/tmp/reliaburger-power-cut.aYYIZm` | PASS | `{"acknowledged":128,"checkpointed":6,"destination_objects":129,"fixture":"exporter","generated":128,"pruned":122,"published":128,"staged_leftovers":0,"stale_temporaries":0}` |
| 7 | 17 | `/var/tmp/reliaburger-power-cut.xyGTGe` | PASS | `{"acknowledged":207,"checkpointed":7,"destination_objects":208,"fixture":"exporter","generated":207,"pruned":201,"published":207,"staged_leftovers":0,"stale_temporaries":0}` |
| 8 | 12 | `/var/tmp/reliaburger-power-cut.sg8Z6p` | PASS | `{"acknowledged":155,"checkpointed":7,"destination_objects":156,"fixture":"exporter","generated":155,"pruned":148,"published":155,"staged_leftovers":0,"stale_temporaries":0}` |
| 9 | 4 | `/var/tmp/reliaburger-power-cut.rJjIZh` | PASS | `{"acknowledged":49,"checkpointed":9,"destination_objects":50,"fixture":"exporter","generated":49,"pruned":40,"published":49,"staged_leftovers":0,"stale_temporaries":0}` |
| 10 | 14 | `/var/tmp/reliaburger-power-cut.C4GI49` | PASS | `{"acknowledged":172,"checkpointed":7,"destination_objects":173,"fixture":"exporter","generated":172,"pruned":165,"published":172,"staged_leftovers":0,"stale_temporaries":0}` |
| 11 | 15 | `/var/tmp/reliaburger-power-cut.0lPFzV` | PASS | `{"acknowledged":192,"checkpointed":7,"destination_objects":193,"fixture":"exporter","generated":192,"pruned":185,"published":192,"staged_leftovers":0,"stale_temporaries":0}` |
| 12 | 10 | `/var/tmp/reliaburger-power-cut.ECS0N4` | PASS | `{"acknowledged":125,"checkpointed":7,"destination_objects":126,"fixture":"exporter","generated":125,"pruned":118,"published":125,"staged_leftovers":0,"stale_temporaries":0}` |
| 13 | 19 | `/var/tmp/reliaburger-power-cut.lFq2Yl` | PASS | `{"acknowledged":233,"checkpointed":6,"destination_objects":234,"fixture":"exporter","generated":233,"pruned":227,"published":233,"staged_leftovers":0,"stale_temporaries":0}` |
| 14 | 9 | `/var/tmp/reliaburger-power-cut.CANExz` | PASS | `{"acknowledged":117,"checkpointed":7,"destination_objects":118,"fixture":"exporter","generated":117,"pruned":111,"published":117,"staged_leftovers":0,"stale_temporaries":0}` |
| 15 | 7 | `/var/tmp/reliaburger-power-cut.lOVclQ` | PASS | `{"acknowledged":92,"checkpointed":7,"destination_objects":93,"fixture":"exporter","generated":92,"pruned":86,"published":92,"staged_leftovers":0,"stale_temporaries":1}` |
| 16 | 12 | `/var/tmp/reliaburger-power-cut.IDIvde` | PASS | `{"acknowledged":149,"checkpointed":6,"destination_objects":151,"fixture":"exporter","generated":150,"pruned":143,"published":150,"staged_leftovers":0,"stale_temporaries":0}` |
| 17 | 12 | `/var/tmp/reliaburger-power-cut.6gPfMj` | PASS | `{"acknowledged":153,"checkpointed":7,"destination_objects":154,"fixture":"exporter","generated":153,"pruned":146,"published":153,"staged_leftovers":0,"stale_temporaries":0}` |
| 18 | 10 | `/var/tmp/reliaburger-power-cut.TyYkmp` | PASS | `{"acknowledged":131,"checkpointed":7,"destination_objects":132,"fixture":"exporter","generated":131,"pruned":124,"published":131,"staged_leftovers":0,"stale_temporaries":0}` |
| 19 | 17 | `/var/tmp/reliaburger-power-cut.Usom85` | PASS | `{"acknowledged":217,"checkpointed":6,"destination_objects":218,"fixture":"exporter","generated":217,"pruned":211,"published":217,"staged_leftovers":0,"stale_temporaries":0}` |
| 20 | 4 | `/var/tmp/reliaburger-power-cut.WBkZjd` | PASS | `{"acknowledged":48,"checkpointed":5,"destination_objects":50,"fixture":"exporter","generated":49,"pruned":43,"published":49,"staged_leftovers":0,"stale_temporaries":0}` |
| 21 | 2 | `/var/tmp/reliaburger-power-cut.ImU9C4` | PASS | `{"acknowledged":26,"checkpointed":6,"destination_objects":28,"fixture":"exporter","generated":27,"pruned":20,"published":27,"staged_leftovers":0,"stale_temporaries":0}` |
| 22 | 16 | `/var/tmp/reliaburger-power-cut.2Ocag3` | PASS | `{"acknowledged":205,"checkpointed":6,"destination_objects":206,"fixture":"exporter","generated":205,"pruned":199,"published":205,"staged_leftovers":0,"stale_temporaries":0}` |
| 23 | 16 | `/var/tmp/reliaburger-power-cut.ckodx6` | PASS | `{"acknowledged":197,"checkpointed":7,"destination_objects":198,"fixture":"exporter","generated":197,"pruned":190,"published":197,"staged_leftovers":0,"stale_temporaries":0}` |
| 24 | 3 | `/var/tmp/reliaburger-power-cut.326q64` | PASS | `{"acknowledged":41,"checkpointed":7,"destination_objects":42,"fixture":"exporter","generated":41,"pruned":34,"published":41,"staged_leftovers":0,"stale_temporaries":0}` |
| 25 | 15 | `/var/tmp/reliaburger-power-cut.LZBVWD` | PASS | `{"acknowledged":189,"checkpointed":6,"destination_objects":190,"fixture":"exporter","generated":189,"pruned":183,"published":189,"staged_leftovers":0,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
