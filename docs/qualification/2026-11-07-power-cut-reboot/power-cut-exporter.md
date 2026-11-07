# Storage power-cut qualification: exporter

- VM: `rb-pcut-ace072c7` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_log_exports`
- Commit: `ace072c73c2d5a5d7124f3a4f879c88bf9c33839`; binary SHA-256 `fbfc5fb026052d4278028dd2bb2cf0cec3642059608076fc27296ff07bc6f7e6`
- Started: 2026-11-07T02:57:16Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.jrfH4J`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 10 | `/var/tmp/reliaburger-power-cut.6DAbwF` | PASS | `{"acknowledged":133,"checkpointed":6,"destination_objects":135,"fixture":"exporter","generated":134,"pruned":127,"published":134,"staged_leftovers":0,"stale_temporaries":0}` |
| 2 | 15 | `/var/tmp/reliaburger-power-cut.49ON5s` | PASS | `{"acknowledged":186,"checkpointed":6,"destination_objects":187,"fixture":"exporter","generated":186,"pruned":180,"published":186,"staged_leftovers":0,"stale_temporaries":0}` |
| 3 | 2 | `/var/tmp/reliaburger-power-cut.h4729i` | PASS | `{"acknowledged":31,"checkpointed":6,"destination_objects":32,"fixture":"exporter","generated":31,"pruned":25,"published":31,"staged_leftovers":0,"stale_temporaries":0}` |
| 4 | 18 | `/var/tmp/reliaburger-power-cut.L6Y3H3` | PASS | `{"acknowledged":229,"checkpointed":6,"destination_objects":230,"fixture":"exporter","generated":229,"pruned":223,"published":229,"staged_leftovers":0,"stale_temporaries":0}` |
| 5 | 11 | `/var/tmp/reliaburger-power-cut.Yt3OuK` | PASS | `{"acknowledged":134,"checkpointed":6,"destination_objects":136,"fixture":"exporter","generated":135,"pruned":128,"published":135,"staged_leftovers":0,"stale_temporaries":0}` |
| 6 | 10 | `/var/tmp/reliaburger-power-cut.0R0L5H` | PASS | `{"acknowledged":124,"checkpointed":6,"destination_objects":125,"fixture":"exporter","generated":124,"pruned":118,"published":124,"staged_leftovers":0,"stale_temporaries":0}` |
| 7 | 15 | `/var/tmp/reliaburger-power-cut.PvAG7d` | PASS | `{"acknowledged":187,"checkpointed":7,"destination_objects":189,"fixture":"exporter","generated":188,"pruned":181,"published":188,"staged_leftovers":0,"stale_temporaries":0}` |
| 8 | 15 | `/var/tmp/reliaburger-power-cut.1nlHzo` | PASS | `{"acknowledged":185,"checkpointed":6,"destination_objects":186,"fixture":"exporter","generated":185,"pruned":179,"published":185,"staged_leftovers":0,"stale_temporaries":0}` |
| 9 | 13 | `/var/tmp/reliaburger-power-cut.fDnQSc` | PASS | `{"acknowledged":169,"checkpointed":6,"destination_objects":170,"fixture":"exporter","generated":169,"pruned":163,"published":169,"staged_leftovers":0,"stale_temporaries":0}` |
| 10 | 14 | `/var/tmp/reliaburger-power-cut.vTkE92` | PASS | `{"acknowledged":181,"checkpointed":6,"destination_objects":182,"fixture":"exporter","generated":181,"pruned":175,"published":181,"staged_leftovers":0,"stale_temporaries":0}` |
| 11 | 8 | `/var/tmp/reliaburger-power-cut.gqR3Cb` | PASS | `{"acknowledged":103,"checkpointed":5,"destination_objects":104,"fixture":"exporter","generated":103,"pruned":98,"published":103,"staged_leftovers":0,"stale_temporaries":0}` |
| 12 | 8 | `/var/tmp/reliaburger-power-cut.S2SO9V` | PASS | `{"acknowledged":106,"checkpointed":7,"destination_objects":107,"fixture":"exporter","generated":106,"pruned":100,"published":106,"staged_leftovers":0,"stale_temporaries":0}` |
| 13 | 9 | `/var/tmp/reliaburger-power-cut.se33rb` | PASS | `{"acknowledged":125,"checkpointed":6,"destination_objects":126,"fixture":"exporter","generated":125,"pruned":119,"published":125,"staged_leftovers":0,"stale_temporaries":0}` |
| 14 | 11 | `/var/tmp/reliaburger-power-cut.X1RFsx` | PASS | `{"acknowledged":139,"checkpointed":7,"destination_objects":140,"fixture":"exporter","generated":139,"pruned":132,"published":139,"staged_leftovers":0,"stale_temporaries":0}` |
| 15 | 17 | `/var/tmp/reliaburger-power-cut.1UTQD1` | PASS | `{"acknowledged":208,"checkpointed":5,"destination_objects":209,"fixture":"exporter","generated":208,"pruned":203,"published":208,"staged_leftovers":0,"stale_temporaries":0}` |
| 16 | 9 | `/var/tmp/reliaburger-power-cut.w7Eqpz` | PASS | `{"acknowledged":111,"checkpointed":6,"destination_objects":113,"fixture":"exporter","generated":112,"pruned":105,"published":112,"staged_leftovers":0,"stale_temporaries":0}` |
| 17 | 15 | `/var/tmp/reliaburger-power-cut.tVuIPn` | PASS | `{"acknowledged":189,"checkpointed":6,"destination_objects":190,"fixture":"exporter","generated":189,"pruned":183,"published":189,"staged_leftovers":0,"stale_temporaries":0}` |
| 18 | 17 | `/var/tmp/reliaburger-power-cut.SUiMr3` | PASS | `{"acknowledged":209,"checkpointed":7,"destination_objects":210,"fixture":"exporter","generated":209,"pruned":202,"published":209,"staged_leftovers":0,"stale_temporaries":0}` |
| 19 | 10 | `/var/tmp/reliaburger-power-cut.RpByI2` | PASS | `{"acknowledged":131,"checkpointed":7,"destination_objects":132,"fixture":"exporter","generated":131,"pruned":125,"published":131,"staged_leftovers":0,"stale_temporaries":0}` |
| 20 | 12 | `/var/tmp/reliaburger-power-cut.g8BJgM` | PASS | `{"acknowledged":156,"checkpointed":6,"destination_objects":157,"fixture":"exporter","generated":156,"pruned":150,"published":156,"staged_leftovers":0,"stale_temporaries":0}` |
| 21 | 15 | `/var/tmp/reliaburger-power-cut.ltrdBs` | PASS | `{"acknowledged":199,"checkpointed":6,"destination_objects":200,"fixture":"exporter","generated":199,"pruned":195,"published":199,"staged_leftovers":0,"stale_temporaries":0}` |
| 22 | 14 | `/var/tmp/reliaburger-power-cut.ki7h0G` | PASS | `{"acknowledged":177,"checkpointed":8,"destination_objects":178,"fixture":"exporter","generated":177,"pruned":169,"published":177,"staged_leftovers":0,"stale_temporaries":0}` |
| 23 | 0 | `/var/tmp/reliaburger-power-cut.qHbJTI` | PASS | `{"acknowledged":3,"checkpointed":3,"destination_objects":5,"fixture":"exporter","generated":4,"pruned":0,"published":4,"staged_leftovers":0,"stale_temporaries":0}` |
| 24 | 18 | `/var/tmp/reliaburger-power-cut.XA6csb` | PASS | `{"acknowledged":215,"checkpointed":9,"destination_objects":217,"fixture":"exporter","generated":216,"pruned":207,"published":216,"staged_leftovers":0,"stale_temporaries":0}` |
| 25 | 3 | `/var/tmp/reliaburger-power-cut.lzYznD` | PASS | `{"acknowledged":42,"checkpointed":8,"destination_objects":43,"fixture":"exporter","generated":42,"pruned":34,"published":42,"staged_leftovers":0,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
