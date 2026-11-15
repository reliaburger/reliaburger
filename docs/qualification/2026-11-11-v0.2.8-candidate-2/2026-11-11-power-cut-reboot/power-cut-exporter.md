# Storage power-cut qualification: exporter

- VM: `rb-pcut-e43417a8` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_log_exports`
- Commit: `e43417a8a2a147910667ba9a075af03e0fcb6a8d`; binary SHA-256 `e73fed7fafe6d7c8bf9e57a0841877ebcbf91ac258c92308772dc8685f8d8802`
- Started: 2026-11-11T00:56:35Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.xIhZyg`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 11 | `/var/tmp/reliaburger-power-cut.kfD8mc` | PASS | `{"acknowledged":146,"checkpointed":7,"destination_objects":147,"fixture":"exporter","generated":146,"pruned":139,"published":146,"staged_leftovers":0,"stale_temporaries":0}` |
| 2 | 17 | `/var/tmp/reliaburger-power-cut.QA78Mt` | PASS | `{"acknowledged":212,"checkpointed":8,"destination_objects":213,"fixture":"exporter","generated":212,"pruned":205,"published":212,"staged_leftovers":0,"stale_temporaries":0}` |
| 3 | 13 | `/var/tmp/reliaburger-power-cut.8TGEXu` | PASS | `{"acknowledged":168,"checkpointed":7,"destination_objects":169,"fixture":"exporter","generated":168,"pruned":162,"published":168,"staged_leftovers":0,"stale_temporaries":0}` |
| 4 | 9 | `/var/tmp/reliaburger-power-cut.dqHz1C` | PASS | `{"acknowledged":117,"checkpointed":7,"destination_objects":119,"fixture":"exporter","generated":118,"pruned":110,"published":118,"staged_leftovers":0,"stale_temporaries":0}` |
| 5 | 10 | `/var/tmp/reliaburger-power-cut.J4Fuhv` | PASS | `{"acknowledged":126,"checkpointed":6,"destination_objects":127,"fixture":"exporter","generated":126,"pruned":120,"published":126,"staged_leftovers":0,"stale_temporaries":0}` |
| 6 | 0 | `/var/tmp/reliaburger-power-cut.6HOhlD` | PASS | `{"acknowledged":2,"checkpointed":2,"destination_objects":4,"fixture":"exporter","generated":3,"pruned":0,"published":3,"staged_leftovers":0,"stale_temporaries":0}` |
| 7 | 6 | `/var/tmp/reliaburger-power-cut.WvY2Cg` | PASS | `{"acknowledged":86,"checkpointed":5,"destination_objects":87,"fixture":"exporter","generated":86,"pruned":81,"published":86,"staged_leftovers":0,"stale_temporaries":0}` |
| 8 | 17 | `/var/tmp/reliaburger-power-cut.rVbXGx` | PASS | `{"acknowledged":211,"checkpointed":7,"destination_objects":212,"fixture":"exporter","generated":211,"pruned":204,"published":211,"staged_leftovers":0,"stale_temporaries":0}` |
| 9 | 8 | `/var/tmp/reliaburger-power-cut.qLxZ8F` | PASS | `{"acknowledged":106,"checkpointed":6,"destination_objects":107,"fixture":"exporter","generated":106,"pruned":100,"published":106,"staged_leftovers":0,"stale_temporaries":0}` |
| 10 | 1 | `/var/tmp/reliaburger-power-cut.WRU7A1` | PASS | `{"acknowledged":18,"checkpointed":6,"destination_objects":19,"fixture":"exporter","generated":18,"pruned":12,"published":18,"staged_leftovers":0,"stale_temporaries":0}` |
| 11 | 11 | `/var/tmp/reliaburger-power-cut.wj6gH6` | PASS | `{"acknowledged":144,"checkpointed":7,"destination_objects":145,"fixture":"exporter","generated":144,"pruned":137,"published":144,"staged_leftovers":0,"stale_temporaries":0}` |
| 12 | 1 | `/var/tmp/reliaburger-power-cut.rghRJr` | PASS | `{"acknowledged":16,"checkpointed":5,"destination_objects":17,"fixture":"exporter","generated":16,"pruned":11,"published":16,"staged_leftovers":0,"stale_temporaries":0}` |
| 13 | 17 | `/var/tmp/reliaburger-power-cut.TjtLRw` | PASS | `{"acknowledged":221,"checkpointed":5,"destination_objects":222,"fixture":"exporter","generated":221,"pruned":216,"published":221,"staged_leftovers":0,"stale_temporaries":0}` |
| 14 | 5 | `/var/tmp/reliaburger-power-cut.4Jw2rK` | PASS | `{"acknowledged":69,"checkpointed":7,"destination_objects":70,"fixture":"exporter","generated":69,"pruned":63,"published":69,"staged_leftovers":0,"stale_temporaries":0}` |
| 15 | 19 | `/var/tmp/reliaburger-power-cut.HgMfYE` | PASS | `{"acknowledged":234,"checkpointed":7,"destination_objects":235,"fixture":"exporter","generated":234,"pruned":227,"published":234,"staged_leftovers":0,"stale_temporaries":0}` |
| 16 | 5 | `/var/tmp/reliaburger-power-cut.vP3WzC` | PASS | `{"acknowledged":69,"checkpointed":6,"destination_objects":70,"fixture":"exporter","generated":69,"pruned":63,"published":69,"staged_leftovers":0,"stale_temporaries":0}` |
| 17 | 18 | `/var/tmp/reliaburger-power-cut.4bHf1T` | PASS | `{"acknowledged":243,"checkpointed":6,"destination_objects":244,"fixture":"exporter","generated":243,"pruned":237,"published":243,"staged_leftovers":0,"stale_temporaries":0}` |
| 18 | 20 | `/var/tmp/reliaburger-power-cut.cEIUn8` | PASS | `{"acknowledged":252,"checkpointed":6,"destination_objects":253,"fixture":"exporter","generated":252,"pruned":246,"published":252,"staged_leftovers":0,"stale_temporaries":0}` |
| 19 | 14 | `/var/tmp/reliaburger-power-cut.rOBUa2` | PASS | `{"acknowledged":178,"checkpointed":6,"destination_objects":179,"fixture":"exporter","generated":178,"pruned":172,"published":178,"staged_leftovers":0,"stale_temporaries":0}` |
| 20 | 14 | `/var/tmp/reliaburger-power-cut.2Txs7y` | PASS | `{"acknowledged":175,"checkpointed":7,"destination_objects":176,"fixture":"exporter","generated":175,"pruned":168,"published":175,"staged_leftovers":0,"stale_temporaries":0}` |
| 21 | 19 | `/var/tmp/reliaburger-power-cut.aheEX0` | PASS | `{"acknowledged":235,"checkpointed":6,"destination_objects":236,"fixture":"exporter","generated":235,"pruned":229,"published":235,"staged_leftovers":0,"stale_temporaries":0}` |
| 22 | 10 | `/var/tmp/reliaburger-power-cut.a9G2M1` | PASS | `{"acknowledged":127,"checkpointed":7,"destination_objects":128,"fixture":"exporter","generated":127,"pruned":120,"published":127,"staged_leftovers":0,"stale_temporaries":0}` |
| 23 | 11 | `/var/tmp/reliaburger-power-cut.HWsfc6` | PASS | `{"acknowledged":153,"checkpointed":8,"destination_objects":155,"fixture":"exporter","generated":154,"pruned":145,"published":153,"staged_leftovers":0,"stale_temporaries":0}` |
| 24 | 6 | `/var/tmp/reliaburger-power-cut.sgmZYH` | PASS | `{"acknowledged":75,"checkpointed":7,"destination_objects":76,"fixture":"exporter","generated":75,"pruned":68,"published":75,"staged_leftovers":0,"stale_temporaries":0}` |
| 25 | 8 | `/var/tmp/reliaburger-power-cut.XGYLDE` | PASS | `{"acknowledged":100,"checkpointed":7,"destination_objects":101,"fixture":"exporter","generated":100,"pruned":94,"published":100,"staged_leftovers":0,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
