# Storage power-cut qualification: exporter

- VM: `rb-pcut-e423d469` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_log_exports`
- Commit: `e423d469a93b04d7bdc64582be1922ac5f453fd1`; binary SHA-256 `9049099169dcdd7e7a9666bef97b28ed88603a2d97b558eb8c8e473608acddbb`
- Started: 2026-11-11T12:59:12Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.gdaZMR`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 14 | `/var/tmp/reliaburger-power-cut.Hn2lDQ` | PASS | `{"acknowledged":177,"checkpointed":4,"destination_objects":178,"fixture":"exporter","generated":177,"pruned":173,"published":177,"staged_leftovers":0,"stale_temporaries":0}` |
| 2 | 20 | `/var/tmp/reliaburger-power-cut.TFzux9` | PASS | `{"acknowledged":254,"checkpointed":6,"destination_objects":255,"fixture":"exporter","generated":254,"pruned":248,"published":254,"staged_leftovers":0,"stale_temporaries":0}` |
| 3 | 3 | `/var/tmp/reliaburger-power-cut.EpGozq` | PASS | `{"acknowledged":46,"checkpointed":7,"destination_objects":47,"fixture":"exporter","generated":46,"pruned":39,"published":46,"staged_leftovers":0,"stale_temporaries":0}` |
| 4 | 19 | `/var/tmp/reliaburger-power-cut.wCC10n` | PASS | `{"acknowledged":248,"checkpointed":7,"destination_objects":249,"fixture":"exporter","generated":248,"pruned":241,"published":248,"staged_leftovers":0,"stale_temporaries":0}` |
| 5 | 16 | `/var/tmp/reliaburger-power-cut.Ndyc8V` | PASS | `{"acknowledged":199,"checkpointed":8,"destination_objects":200,"fixture":"exporter","generated":199,"pruned":193,"published":199,"staged_leftovers":0,"stale_temporaries":0}` |
| 6 | 4 | `/var/tmp/reliaburger-power-cut.Nf9Mbp` | PASS | `{"acknowledged":56,"checkpointed":6,"destination_objects":57,"fixture":"exporter","generated":56,"pruned":50,"published":56,"staged_leftovers":0,"stale_temporaries":0}` |
| 7 | 20 | `/var/tmp/reliaburger-power-cut.lkriht` | PASS | `{"acknowledged":255,"checkpointed":6,"destination_objects":256,"fixture":"exporter","generated":255,"pruned":249,"published":255,"staged_leftovers":0,"stale_temporaries":0}` |
| 8 | 1 | `/var/tmp/reliaburger-power-cut.aRQQSJ` | PASS | `{"acknowledged":17,"checkpointed":8,"destination_objects":18,"fixture":"exporter","generated":17,"pruned":9,"published":17,"staged_leftovers":0,"stale_temporaries":0}` |
| 9 | 14 | `/var/tmp/reliaburger-power-cut.PMoKzr` | PASS | `{"acknowledged":181,"checkpointed":6,"destination_objects":182,"fixture":"exporter","generated":181,"pruned":175,"published":181,"staged_leftovers":0,"stale_temporaries":0}` |
| 10 | 13 | `/var/tmp/reliaburger-power-cut.TYFc80` | PASS | `{"acknowledged":162,"checkpointed":6,"destination_objects":163,"fixture":"exporter","generated":162,"pruned":156,"published":162,"staged_leftovers":0,"stale_temporaries":0}` |
| 11 | 17 | `/var/tmp/reliaburger-power-cut.7t5jt6` | PASS | `{"acknowledged":222,"checkpointed":6,"destination_objects":224,"fixture":"exporter","generated":223,"pruned":216,"published":222,"staged_leftovers":0,"stale_temporaries":1}` |
| 12 | 20 | `/var/tmp/reliaburger-power-cut.iec3hJ` | PASS | `{"acknowledged":259,"checkpointed":7,"destination_objects":260,"fixture":"exporter","generated":259,"pruned":252,"published":259,"staged_leftovers":0,"stale_temporaries":0}` |
| 13 | 0 | `/var/tmp/reliaburger-power-cut.Soc8Jq` | PASS | `{"acknowledged":3,"checkpointed":3,"destination_objects":5,"fixture":"exporter","generated":4,"pruned":0,"published":4,"staged_leftovers":0,"stale_temporaries":0}` |
| 14 | 0 | `/var/tmp/reliaburger-power-cut.RWe4NP` | PASS | `{"acknowledged":3,"checkpointed":3,"destination_objects":4,"fixture":"exporter","generated":3,"pruned":0,"published":3,"staged_leftovers":0,"stale_temporaries":0}` |
| 15 | 19 | `/var/tmp/reliaburger-power-cut.8cPEm7` | PASS | `{"acknowledged":248,"checkpointed":7,"destination_objects":249,"fixture":"exporter","generated":248,"pruned":241,"published":248,"staged_leftovers":0,"stale_temporaries":0}` |
| 16 | 13 | `/var/tmp/reliaburger-power-cut.u4obCR` | PASS | `{"acknowledged":161,"checkpointed":5,"destination_objects":163,"fixture":"exporter","generated":162,"pruned":156,"published":162,"staged_leftovers":0,"stale_temporaries":0}` |
| 17 | 8 | `/var/tmp/reliaburger-power-cut.dBOk8m` | PASS | `{"acknowledged":106,"checkpointed":9,"destination_objects":107,"fixture":"exporter","generated":106,"pruned":97,"published":106,"staged_leftovers":0,"stale_temporaries":0}` |
| 18 | 1 | `/var/tmp/reliaburger-power-cut.DoPkaL` | PASS | `{"acknowledged":18,"checkpointed":6,"destination_objects":19,"fixture":"exporter","generated":18,"pruned":12,"published":18,"staged_leftovers":0,"stale_temporaries":0}` |
| 19 | 9 | `/var/tmp/reliaburger-power-cut.QCsfL5` | PASS | `{"acknowledged":116,"checkpointed":6,"destination_objects":117,"fixture":"exporter","generated":116,"pruned":111,"published":116,"staged_leftovers":0,"stale_temporaries":0}` |
| 20 | 4 | `/var/tmp/reliaburger-power-cut.coCo70` | PASS | `{"acknowledged":58,"checkpointed":7,"destination_objects":59,"fixture":"exporter","generated":58,"pruned":52,"published":58,"staged_leftovers":0,"stale_temporaries":0}` |
| 21 | 17 | `/var/tmp/reliaburger-power-cut.1ezmL0` | PASS | `{"acknowledged":221,"checkpointed":6,"destination_objects":222,"fixture":"exporter","generated":221,"pruned":215,"published":221,"staged_leftovers":0,"stale_temporaries":0}` |
| 22 | 19 | `/var/tmp/reliaburger-power-cut.Adzprh` | PASS | `{"acknowledged":240,"checkpointed":6,"destination_objects":241,"fixture":"exporter","generated":240,"pruned":234,"published":240,"staged_leftovers":0,"stale_temporaries":0}` |
| 23 | 19 | `/var/tmp/reliaburger-power-cut.8EAJMy` | PASS | `{"acknowledged":237,"checkpointed":6,"destination_objects":239,"fixture":"exporter","generated":238,"pruned":231,"published":238,"staged_leftovers":0,"stale_temporaries":0}` |
| 24 | 7 | `/var/tmp/reliaburger-power-cut.lFfRtC` | PASS | `{"acknowledged":88,"checkpointed":7,"destination_objects":90,"fixture":"exporter","generated":89,"pruned":81,"published":89,"staged_leftovers":0,"stale_temporaries":0}` |
| 25 | 15 | `/var/tmp/reliaburger-power-cut.hGZMHj` | PASS | `{"acknowledged":204,"checkpointed":7,"destination_objects":205,"fixture":"exporter","generated":204,"pruned":198,"published":204,"staged_leftovers":0,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
