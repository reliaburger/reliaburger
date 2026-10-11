# Storage power-cut qualification: exporter

- VM: `rb-691-crash` (guest kernel 6.8.0-142-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_log_exports`
- Commit: `ab18602bf9e6a9691d60853c690a4cabae1ef8d1`; binary SHA-256 `0d4e98edf5429e1fe0e97ea7a8165b5eb759027d98c68759b8e63a1931b4c185`
- Started: 2026-10-10T21:14:42Z
- Host evidence: `/private/tmp/claude-501/-Users-miko-github-reliaburger/68c7cd97-0874-4446-8c64-77c5be59a15c/scratchpad/qual-tmp/reliaburger-power-cut.1m2hCM`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 9 | `/var/tmp/reliaburger-power-cut.etu31I` | PASS | `{"fixture":"exporter","generated":113,"published":113,"acknowledged":113,"checkpointed":7,"pruned":107,"destination_objects":114,"staged_leftovers":0,"stale_temporaries":0}` |
| 2 | 4 | `/var/tmp/reliaburger-power-cut.8xRarm` | PASS | `{"fixture":"exporter","generated":53,"published":53,"acknowledged":53,"checkpointed":6,"pruned":47,"destination_objects":54,"staged_leftovers":0,"stale_temporaries":0}` |
| 3 | 9 | `/var/tmp/reliaburger-power-cut.uwpQ6Q` | PASS | `{"fixture":"exporter","generated":115,"published":115,"acknowledged":115,"checkpointed":7,"pruned":109,"destination_objects":116,"staged_leftovers":0,"stale_temporaries":0}` |
| 4 | 20 | `/var/tmp/reliaburger-power-cut.1tigTq` | PASS | `{"fixture":"exporter","generated":269,"published":269,"acknowledged":269,"checkpointed":6,"pruned":263,"destination_objects":270,"staged_leftovers":0,"stale_temporaries":0}` |
| 5 | 15 | `/var/tmp/reliaburger-power-cut.ctjQHr` | PASS | `{"fixture":"exporter","generated":200,"published":200,"acknowledged":200,"checkpointed":7,"pruned":194,"destination_objects":201,"staged_leftovers":0,"stale_temporaries":0}` |
| 6 | 17 | `/var/tmp/reliaburger-power-cut.4V6qCZ` | PASS | `{"fixture":"exporter","generated":221,"published":221,"acknowledged":221,"checkpointed":4,"pruned":217,"destination_objects":222,"staged_leftovers":0,"stale_temporaries":1}` |
| 7 | 19 | `/var/tmp/reliaburger-power-cut.fVLGB4` | PASS | `{"fixture":"exporter","generated":238,"published":238,"acknowledged":238,"checkpointed":6,"pruned":232,"destination_objects":239,"staged_leftovers":0,"stale_temporaries":0}` |
| 8 | 18 | `/var/tmp/reliaburger-power-cut.km87Z6` | PASS | `{"fixture":"exporter","generated":231,"published":231,"acknowledged":231,"checkpointed":6,"pruned":225,"destination_objects":232,"staged_leftovers":0,"stale_temporaries":0}` |
| 9 | 11 | `/var/tmp/reliaburger-power-cut.CzIdBU` | PASS | `{"fixture":"exporter","generated":142,"published":142,"acknowledged":142,"checkpointed":7,"pruned":136,"destination_objects":143,"staged_leftovers":0,"stale_temporaries":0}` |
| 10 | 11 | `/var/tmp/reliaburger-power-cut.fsvEM7` | PASS | `{"fixture":"exporter","generated":144,"published":144,"acknowledged":144,"checkpointed":6,"pruned":138,"destination_objects":145,"staged_leftovers":0,"stale_temporaries":0}` |
| 11 | 9 | `/var/tmp/reliaburger-power-cut.lZ4FnP` | PASS | `{"fixture":"exporter","generated":125,"published":125,"acknowledged":124,"checkpointed":6,"pruned":118,"destination_objects":126,"staged_leftovers":0,"stale_temporaries":0}` |
| 12 | 15 | `/var/tmp/reliaburger-power-cut.Y89jXm` | PASS | `{"fixture":"exporter","generated":196,"published":196,"acknowledged":196,"checkpointed":7,"pruned":190,"destination_objects":197,"staged_leftovers":0,"stale_temporaries":0}` |
| 13 | 5 | `/var/tmp/reliaburger-power-cut.8MsP2S` | PASS | `{"fixture":"exporter","generated":64,"published":64,"acknowledged":64,"checkpointed":7,"pruned":57,"destination_objects":65,"staged_leftovers":0,"stale_temporaries":0}` |
| 14 | 0 | `/var/tmp/reliaburger-power-cut.gxovmP` | PASS | `{"fixture":"exporter","generated":5,"published":4,"acknowledged":4,"checkpointed":4,"pruned":0,"destination_objects":5,"staged_leftovers":0,"stale_temporaries":0}` |
| 15 | 6 | `/var/tmp/reliaburger-power-cut.AOPOd9` | PASS | `{"fixture":"exporter","generated":85,"published":85,"acknowledged":85,"checkpointed":7,"pruned":79,"destination_objects":86,"staged_leftovers":0,"stale_temporaries":0}` |

15 of 15 iterations passed. Rule-of-three bound: failure rate < 20.0% at 95% confidence.
