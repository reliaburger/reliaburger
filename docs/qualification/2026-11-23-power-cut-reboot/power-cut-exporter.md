# Storage power-cut qualification: exporter

- VM: `rb-pcut-2aae7e2a` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_log_exports`
- Commit: `2aae7e2a075f24614eb626aef83a5c29a279fb34`; binary SHA-256 `641bfed43d860a3da96e740299d8202827967c245ab2fa35f87da32f3f0fbef7`
- Started: 2026-11-23T01:15:19Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.65C3K4`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 0 | `/var/tmp/reliaburger-power-cut.XbkwJI` | PASS | `{"acknowledged":4,"checkpointed":4,"destination_objects":5,"fixture":"exporter","generated":4,"pruned":0,"published":4,"staged_leftovers":0,"stale_temporaries":0}` |
| 2 | 13 | `/var/tmp/reliaburger-power-cut.LE5O6J` | PASS | `{"acknowledged":165,"checkpointed":8,"destination_objects":166,"fixture":"exporter","generated":165,"pruned":157,"published":165,"staged_leftovers":0,"stale_temporaries":0}` |
| 3 | 18 | `/var/tmp/reliaburger-power-cut.teRuEY` | PASS | `{"acknowledged":235,"checkpointed":7,"destination_objects":236,"fixture":"exporter","generated":235,"pruned":228,"published":235,"staged_leftovers":0,"stale_temporaries":0}` |
| 4 | 19 | `/var/tmp/reliaburger-power-cut.sJVDRG` | PASS | `{"acknowledged":232,"checkpointed":6,"destination_objects":234,"fixture":"exporter","generated":233,"pruned":226,"published":233,"staged_leftovers":0,"stale_temporaries":0}` |
| 5 | 15 | `/var/tmp/reliaburger-power-cut.B7JoIn` | PASS | `{"acknowledged":180,"checkpointed":7,"destination_objects":181,"fixture":"exporter","generated":180,"pruned":174,"published":180,"staged_leftovers":0,"stale_temporaries":0}` |
| 6 | 0 | `/var/tmp/reliaburger-power-cut.hAodQG` | PASS | `{"acknowledged":3,"checkpointed":3,"destination_objects":4,"fixture":"exporter","generated":3,"pruned":0,"published":3,"staged_leftovers":0,"stale_temporaries":0}` |
| 7 | 2 | `/var/tmp/reliaburger-power-cut.w5swNH` | PASS | `{"acknowledged":29,"checkpointed":7,"destination_objects":31,"fixture":"exporter","generated":30,"pruned":22,"published":30,"staged_leftovers":0,"stale_temporaries":1}` |
| 8 | 18 | `/var/tmp/reliaburger-power-cut.roD8nj` | PASS | `{"acknowledged":227,"checkpointed":6,"destination_objects":229,"fixture":"exporter","generated":228,"pruned":221,"published":228,"staged_leftovers":0,"stale_temporaries":0}` |
| 9 | 18 | `/var/tmp/reliaburger-power-cut.5VnV4a` | PASS | `{"acknowledged":229,"checkpointed":7,"destination_objects":230,"fixture":"exporter","generated":229,"pruned":222,"published":229,"staged_leftovers":0,"stale_temporaries":0}` |
| 10 | 14 | `/var/tmp/reliaburger-power-cut.KQtM6a` | PASS | `{"acknowledged":179,"checkpointed":6,"destination_objects":180,"fixture":"exporter","generated":179,"pruned":173,"published":179,"staged_leftovers":0,"stale_temporaries":0}` |
| 11 | 19 | `/var/tmp/reliaburger-power-cut.Bryg9w` | PASS | `{"acknowledged":240,"checkpointed":6,"destination_objects":241,"fixture":"exporter","generated":240,"pruned":234,"published":240,"staged_leftovers":0,"stale_temporaries":0}` |
| 12 | 17 | `/var/tmp/reliaburger-power-cut.4wFC0z` | PASS | `{"acknowledged":204,"checkpointed":8,"destination_objects":205,"fixture":"exporter","generated":204,"pruned":196,"published":204,"staged_leftovers":0,"stale_temporaries":0}` |
| 13 | 5 | `/var/tmp/reliaburger-power-cut.0dksro` | PASS | `{"acknowledged":65,"checkpointed":6,"destination_objects":66,"fixture":"exporter","generated":65,"pruned":59,"published":65,"staged_leftovers":0,"stale_temporaries":0}` |
| 14 | 8 | `/var/tmp/reliaburger-power-cut.bRwc0Z` | PASS | `{"acknowledged":103,"checkpointed":6,"destination_objects":104,"fixture":"exporter","generated":103,"pruned":97,"published":103,"staged_leftovers":0,"stale_temporaries":0}` |
| 15 | 3 | `/var/tmp/reliaburger-power-cut.Z3p9N7` | PASS | `{"acknowledged":39,"checkpointed":6,"destination_objects":40,"fixture":"exporter","generated":39,"pruned":33,"published":39,"staged_leftovers":0,"stale_temporaries":0}` |
| 16 | 12 | `/var/tmp/reliaburger-power-cut.YiQCRt` | PASS | `{"acknowledged":157,"checkpointed":6,"destination_objects":158,"fixture":"exporter","generated":157,"pruned":151,"published":157,"staged_leftovers":0,"stale_temporaries":1}` |
| 17 | 15 | `/var/tmp/reliaburger-power-cut.NpTyQX` | PASS | `{"acknowledged":197,"checkpointed":6,"destination_objects":198,"fixture":"exporter","generated":197,"pruned":191,"published":197,"staged_leftovers":0,"stale_temporaries":0}` |
| 18 | 20 | `/var/tmp/reliaburger-power-cut.FQcvLy` | PASS | `{"acknowledged":251,"checkpointed":6,"destination_objects":253,"fixture":"exporter","generated":252,"pruned":245,"published":252,"staged_leftovers":0,"stale_temporaries":0}` |
| 19 | 18 | `/var/tmp/reliaburger-power-cut.6UzPRR` | PASS | `{"acknowledged":224,"checkpointed":6,"destination_objects":225,"fixture":"exporter","generated":224,"pruned":218,"published":224,"staged_leftovers":0,"stale_temporaries":0}` |
| 20 | 6 | `/var/tmp/reliaburger-power-cut.xANWta` | PASS | `{"acknowledged":77,"checkpointed":6,"destination_objects":78,"fixture":"exporter","generated":77,"pruned":71,"published":77,"staged_leftovers":0,"stale_temporaries":0}` |
| 21 | 17 | `/var/tmp/reliaburger-power-cut.COo64m` | PASS | `{"acknowledged":218,"checkpointed":7,"destination_objects":219,"fixture":"exporter","generated":218,"pruned":211,"published":218,"staged_leftovers":0,"stale_temporaries":0}` |
| 22 | 9 | `/var/tmp/reliaburger-power-cut.1ECu6j` | PASS | `{"acknowledged":118,"checkpointed":7,"destination_objects":119,"fixture":"exporter","generated":118,"pruned":111,"published":118,"staged_leftovers":0,"stale_temporaries":0}` |
| 23 | 12 | `/var/tmp/reliaburger-power-cut.91CKH9` | PASS | `{"acknowledged":156,"checkpointed":6,"destination_objects":157,"fixture":"exporter","generated":156,"pruned":150,"published":156,"staged_leftovers":0,"stale_temporaries":0}` |
| 24 | 8 | `/var/tmp/reliaburger-power-cut.dHZOfm` | PASS | `{"acknowledged":101,"checkpointed":8,"destination_objects":102,"fixture":"exporter","generated":101,"pruned":94,"published":101,"staged_leftovers":0,"stale_temporaries":0}` |
| 25 | 19 | `/var/tmp/reliaburger-power-cut.IRobPg` | PASS | `{"acknowledged":238,"checkpointed":5,"destination_objects":239,"fixture":"exporter","generated":238,"pruned":233,"published":238,"staged_leftovers":0,"stale_temporaries":1}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
