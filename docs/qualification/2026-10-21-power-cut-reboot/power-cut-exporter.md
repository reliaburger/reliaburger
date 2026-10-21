# Storage power-cut qualification: exporter

- VM: `rb-pcut-abec2c45` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_log_exports`
- Commit: `abec2c459d559652ced52a40d5c34973571cef9b`; binary SHA-256 `3360f06c981627fa0080a5f825d09b6a0a802237d06aede168415a81f774834a`
- Started: 2026-10-21T02:04:26Z
- Host evidence: `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-power-cut.wsGIrA`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 0 | `/var/tmp/reliaburger-power-cut.rbe3fE` | PASS | `{"fixture":"exporter","generated":3,"published":3,"acknowledged":3,"checkpointed":3,"pruned":0,"destination_objects":4,"staged_leftovers":0,"stale_temporaries":0}` |
| 2 | 15 | `/var/tmp/reliaburger-power-cut.PcbxnB` | PASS | `{"fixture":"exporter","generated":194,"published":194,"acknowledged":194,"checkpointed":6,"pruned":188,"destination_objects":195,"staged_leftovers":0,"stale_temporaries":0}` |
| 3 | 12 | `/var/tmp/reliaburger-power-cut.eOoJCf` | PASS | `{"fixture":"exporter","generated":162,"published":162,"acknowledged":162,"checkpointed":8,"pruned":156,"destination_objects":163,"staged_leftovers":0,"stale_temporaries":0}` |
| 4 | 18 | `/var/tmp/reliaburger-power-cut.xv85lp` | PASS | `{"fixture":"exporter","generated":234,"published":234,"acknowledged":234,"checkpointed":8,"pruned":226,"destination_objects":235,"staged_leftovers":0,"stale_temporaries":1}` |
| 5 | 1 | `/var/tmp/reliaburger-power-cut.QJICUt` | PASS | `{"fixture":"exporter","generated":19,"published":19,"acknowledged":19,"checkpointed":6,"pruned":13,"destination_objects":20,"staged_leftovers":0,"stale_temporaries":0}` |
| 6 | 18 | `/var/tmp/reliaburger-power-cut.qdgzhf` | PASS | `{"fixture":"exporter","generated":231,"published":231,"acknowledged":230,"checkpointed":6,"pruned":224,"destination_objects":232,"staged_leftovers":0,"stale_temporaries":0}` |
| 7 | 12 | `/var/tmp/reliaburger-power-cut.PhfC2G` | PASS | `{"fixture":"exporter","generated":159,"published":159,"acknowledged":159,"checkpointed":6,"pruned":153,"destination_objects":160,"staged_leftovers":0,"stale_temporaries":0}` |
| 8 | 12 | `/var/tmp/reliaburger-power-cut.Ig144l` | PASS | `{"fixture":"exporter","generated":161,"published":161,"acknowledged":161,"checkpointed":6,"pruned":155,"destination_objects":162,"staged_leftovers":0,"stale_temporaries":0}` |
| 9 | 8 | `/var/tmp/reliaburger-power-cut.VumYMg` | PASS | `{"fixture":"exporter","generated":107,"published":107,"acknowledged":107,"checkpointed":7,"pruned":100,"destination_objects":108,"staged_leftovers":0,"stale_temporaries":0}` |
| 10 | 2 | `/var/tmp/reliaburger-power-cut.mm1E3K` | PASS | `{"fixture":"exporter","generated":30,"published":30,"acknowledged":30,"checkpointed":6,"pruned":24,"destination_objects":31,"staged_leftovers":0,"stale_temporaries":0}` |
| 11 | 6 | `/var/tmp/reliaburger-power-cut.VZdeU9` | PASS | `{"fixture":"exporter","generated":84,"published":84,"acknowledged":84,"checkpointed":6,"pruned":78,"destination_objects":85,"staged_leftovers":0,"stale_temporaries":0}` |
| 12 | 3 | `/var/tmp/reliaburger-power-cut.YvTVoy` | PASS | `{"fixture":"exporter","generated":44,"published":44,"acknowledged":44,"checkpointed":6,"pruned":38,"destination_objects":45,"staged_leftovers":0,"stale_temporaries":0}` |
| 13 | 12 | `/var/tmp/reliaburger-power-cut.ZKAFlD` | PASS | `{"fixture":"exporter","generated":151,"published":151,"acknowledged":151,"checkpointed":6,"pruned":145,"destination_objects":152,"staged_leftovers":0,"stale_temporaries":0}` |
| 14 | 6 | `/var/tmp/reliaburger-power-cut.KbAxVj` | PASS | `{"fixture":"exporter","generated":74,"published":74,"acknowledged":74,"checkpointed":6,"pruned":68,"destination_objects":75,"staged_leftovers":0,"stale_temporaries":0}` |
| 15 | 16 | `/var/tmp/reliaburger-power-cut.HmE0ed` | PASS | `{"fixture":"exporter","generated":208,"published":208,"acknowledged":208,"checkpointed":7,"pruned":202,"destination_objects":209,"staged_leftovers":0,"stale_temporaries":0}` |
| 16 | 6 | `/var/tmp/reliaburger-power-cut.5HMhlL` | PASS | `{"fixture":"exporter","generated":83,"published":83,"acknowledged":83,"checkpointed":6,"pruned":77,"destination_objects":84,"staged_leftovers":0,"stale_temporaries":0}` |
| 17 | 20 | `/var/tmp/reliaburger-power-cut.KeGako` | PASS | `{"fixture":"exporter","generated":259,"published":259,"acknowledged":259,"checkpointed":6,"pruned":253,"destination_objects":260,"staged_leftovers":0,"stale_temporaries":0}` |
| 18 | 14 | `/var/tmp/reliaburger-power-cut.mKVkE9` | PASS | `{"fixture":"exporter","generated":176,"published":176,"acknowledged":176,"checkpointed":6,"pruned":170,"destination_objects":177,"staged_leftovers":0,"stale_temporaries":0}` |
| 19 | 3 | `/var/tmp/reliaburger-power-cut.9DyGFf` | PASS | `{"fixture":"exporter","generated":38,"published":38,"acknowledged":38,"checkpointed":6,"pruned":32,"destination_objects":39,"staged_leftovers":0,"stale_temporaries":0}` |
| 20 | 8 | `/var/tmp/reliaburger-power-cut.tOKTAN` | PASS | `{"fixture":"exporter","generated":97,"published":97,"acknowledged":97,"checkpointed":6,"pruned":91,"destination_objects":98,"staged_leftovers":0,"stale_temporaries":0}` |
| 21 | 3 | `/var/tmp/reliaburger-power-cut.muULxK` | PASS | `{"fixture":"exporter","generated":42,"published":42,"acknowledged":42,"checkpointed":6,"pruned":36,"destination_objects":43,"staged_leftovers":0,"stale_temporaries":0}` |
| 22 | 0 | `/var/tmp/reliaburger-power-cut.LXjyFz` | PASS | `{"fixture":"exporter","generated":4,"published":4,"acknowledged":3,"checkpointed":3,"pruned":0,"destination_objects":5,"staged_leftovers":0,"stale_temporaries":0}` |
| 23 | 10 | `/var/tmp/reliaburger-power-cut.nwglLK` | PASS | `{"fixture":"exporter","generated":127,"published":127,"acknowledged":127,"checkpointed":6,"pruned":121,"destination_objects":128,"staged_leftovers":0,"stale_temporaries":0}` |
| 24 | 2 | `/var/tmp/reliaburger-power-cut.mx9qd5` | PASS | `{"fixture":"exporter","generated":28,"published":28,"acknowledged":27,"checkpointed":6,"pruned":21,"destination_objects":29,"staged_leftovers":0,"stale_temporaries":0}` |
| 25 | 2 | `/var/tmp/reliaburger-power-cut.hrs5DH` | PASS | `{"fixture":"exporter","generated":30,"published":30,"acknowledged":30,"checkpointed":8,"pruned":22,"destination_objects":31,"staged_leftovers":0,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
