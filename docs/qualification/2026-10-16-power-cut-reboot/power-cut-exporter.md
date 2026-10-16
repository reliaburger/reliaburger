# Storage power-cut qualification: exporter

- VM: `rb-020-pcut` (guest kernel 6.8.0-142-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_log_exports`
- Commit: `cc5f64393350f2081f7d88512b95a88316172e5a`; binary SHA-256 `33f3ba84343c37d41b6ebb9298e09d17b8e009d385ed84c0eb25ce5146ee3c56`
- Started: 2026-10-16T04:32:44Z
- Host evidence: a temporary directory on the host, not kept

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 2 | `/var/tmp/reliaburger-power-cut.iRIQUD` | PASS | `{"fixture":"exporter","generated":31,"published":31,"acknowledged":31,"checkpointed":7,"pruned":24,"destination_objects":32,"staged_leftovers":0,"stale_temporaries":0}` |
| 2 | 0 | `/var/tmp/reliaburger-power-cut.SpyXdG` | PASS | `{"fixture":"exporter","generated":4,"published":4,"acknowledged":4,"checkpointed":4,"pruned":0,"destination_objects":5,"staged_leftovers":0,"stale_temporaries":0}` |
| 3 | 2 | `/var/tmp/reliaburger-power-cut.YWytvU` | PASS | `{"fixture":"exporter","generated":30,"published":30,"acknowledged":30,"checkpointed":7,"pruned":23,"destination_objects":31,"staged_leftovers":0,"stale_temporaries":0}` |
| 4 | 18 | `/var/tmp/reliaburger-power-cut.fRVwol` | PASS | `{"fixture":"exporter","generated":239,"published":239,"acknowledged":239,"checkpointed":7,"pruned":232,"destination_objects":240,"staged_leftovers":0,"stale_temporaries":0}` |
| 5 | 14 | `/var/tmp/reliaburger-power-cut.pwdXQx` | PASS | `{"fixture":"exporter","generated":179,"published":179,"acknowledged":179,"checkpointed":6,"pruned":173,"destination_objects":180,"staged_leftovers":0,"stale_temporaries":0}` |
| 6 | 1 | `/var/tmp/reliaburger-power-cut.eAQ7v8` | PASS | `{"fixture":"exporter","generated":16,"published":16,"acknowledged":15,"checkpointed":6,"pruned":9,"destination_objects":17,"staged_leftovers":0,"stale_temporaries":0}` |
| 7 | 6 | `/var/tmp/reliaburger-power-cut.7l133F` | PASS | `{"fixture":"exporter","generated":79,"published":79,"acknowledged":79,"checkpointed":6,"pruned":73,"destination_objects":80,"staged_leftovers":0,"stale_temporaries":0}` |
| 8 | 11 | `/var/tmp/reliaburger-power-cut.iMDLon` | PASS | `{"fixture":"exporter","generated":143,"published":143,"acknowledged":143,"checkpointed":7,"pruned":136,"destination_objects":144,"staged_leftovers":0,"stale_temporaries":0}` |
| 9 | 18 | `/var/tmp/reliaburger-power-cut.eoLXsO` | PASS | `{"fixture":"exporter","generated":227,"published":227,"acknowledged":227,"checkpointed":5,"pruned":222,"destination_objects":228,"staged_leftovers":0,"stale_temporaries":0}` |
| 10 | 8 | `/var/tmp/reliaburger-power-cut.Zbb5Hk` | PASS | `{"fixture":"exporter","generated":109,"published":109,"acknowledged":109,"checkpointed":6,"pruned":103,"destination_objects":110,"staged_leftovers":0,"stale_temporaries":0}` |
| 11 | 11 | `/var/tmp/reliaburger-power-cut.OCCm9H` | PASS | `{"fixture":"exporter","generated":149,"published":149,"acknowledged":149,"checkpointed":8,"pruned":142,"destination_objects":150,"staged_leftovers":0,"stale_temporaries":0}` |
| 12 | 6 | `/var/tmp/reliaburger-power-cut.J8NbTq` | PASS | `{"fixture":"exporter","generated":86,"published":86,"acknowledged":86,"checkpointed":6,"pruned":80,"destination_objects":87,"staged_leftovers":0,"stale_temporaries":0}` |
| 13 | 9 | `/var/tmp/reliaburger-power-cut.FLmB0P` | PASS | `{"fixture":"exporter","generated":120,"published":120,"acknowledged":120,"checkpointed":6,"pruned":114,"destination_objects":121,"staged_leftovers":0,"stale_temporaries":0}` |
| 14 | 11 | `/var/tmp/reliaburger-power-cut.zP9W0p` | PASS | `{"fixture":"exporter","generated":145,"published":145,"acknowledged":145,"checkpointed":8,"pruned":139,"destination_objects":146,"staged_leftovers":0,"stale_temporaries":0}` |
| 15 | 5 | `/var/tmp/reliaburger-power-cut.SaHJnj` | PASS | `{"fixture":"exporter","generated":65,"published":65,"acknowledged":65,"checkpointed":7,"pruned":58,"destination_objects":66,"staged_leftovers":0,"stale_temporaries":0}` |
| 16 | 3 | `/var/tmp/reliaburger-power-cut.JnhCLu` | PASS | `{"fixture":"exporter","generated":46,"published":46,"acknowledged":45,"checkpointed":5,"pruned":40,"destination_objects":47,"staged_leftovers":0,"stale_temporaries":1}` |
| 17 | 5 | `/var/tmp/reliaburger-power-cut.Il9CSv` | PASS | `{"fixture":"exporter","generated":68,"published":68,"acknowledged":68,"checkpointed":7,"pruned":61,"destination_objects":69,"staged_leftovers":0,"stale_temporaries":0}` |
| 18 | 16 | `/var/tmp/reliaburger-power-cut.8oRBEX` | PASS | `{"fixture":"exporter","generated":205,"published":205,"acknowledged":205,"checkpointed":6,"pruned":199,"destination_objects":206,"staged_leftovers":0,"stale_temporaries":0}` |
| 19 | 0 | `/var/tmp/reliaburger-power-cut.cpf0PJ` | PASS | `{"fixture":"exporter","generated":4,"published":4,"acknowledged":4,"checkpointed":4,"pruned":0,"destination_objects":5,"staged_leftovers":0,"stale_temporaries":0}` |
| 20 | 10 | `/var/tmp/reliaburger-power-cut.CGcs2l` | PASS | `{"fixture":"exporter","generated":124,"published":124,"acknowledged":124,"checkpointed":6,"pruned":118,"destination_objects":125,"staged_leftovers":0,"stale_temporaries":0}` |
| 21 | 12 | `/var/tmp/reliaburger-power-cut.ZaEkpY` | PASS | `{"fixture":"exporter","generated":162,"published":162,"acknowledged":161,"checkpointed":6,"pruned":155,"destination_objects":163,"staged_leftovers":0,"stale_temporaries":0}` |
| 22 | 0 | `/var/tmp/reliaburger-power-cut.apBrL0` | PASS | `{"fixture":"exporter","generated":4,"published":4,"acknowledged":4,"checkpointed":4,"pruned":0,"destination_objects":5,"staged_leftovers":0,"stale_temporaries":0}` |
| 23 | 17 | `/var/tmp/reliaburger-power-cut.d4r9Tm` | PASS | `{"fixture":"exporter","generated":227,"published":227,"acknowledged":227,"checkpointed":6,"pruned":221,"destination_objects":228,"staged_leftovers":0,"stale_temporaries":0}` |
| 24 | 14 | `/var/tmp/reliaburger-power-cut.Q9WiiD` | PASS | `{"fixture":"exporter","generated":181,"published":181,"acknowledged":180,"checkpointed":6,"pruned":174,"destination_objects":182,"staged_leftovers":0,"stale_temporaries":0}` |
| 25 | 19 | `/var/tmp/reliaburger-power-cut.2Kmd6G` | PASS | `{"fixture":"exporter","generated":252,"published":252,"acknowledged":252,"checkpointed":6,"pruned":246,"destination_objects":253,"staged_leftovers":0,"stale_temporaries":1}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
