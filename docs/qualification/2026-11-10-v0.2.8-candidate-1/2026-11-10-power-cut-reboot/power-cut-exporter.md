# Storage power-cut qualification: exporter

- VM: `rb-pcut-08618555` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_log_exports`
- Commit: `08618555ffe3112f2bd67842cadedfdd61d8071c`; binary SHA-256 `1069749d07dc37f6711f6c8bfeb9fa0267ad00d7f9305ec9617fae15fdb5edf2`
- Started: 2026-11-10T17:15:20Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.D87QP6`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 10 | `/var/tmp/reliaburger-power-cut.bHjIZe` | PASS | `{"acknowledged":126,"checkpointed":6,"destination_objects":127,"fixture":"exporter","generated":126,"pruned":120,"published":126,"staged_leftovers":0,"stale_temporaries":0}` |
| 2 | 10 | `/var/tmp/reliaburger-power-cut.c6Ubeg` | PASS | `{"acknowledged":121,"checkpointed":6,"destination_objects":122,"fixture":"exporter","generated":121,"pruned":115,"published":121,"staged_leftovers":0,"stale_temporaries":0}` |
| 3 | 10 | `/var/tmp/reliaburger-power-cut.iOSQMt` | PASS | `{"acknowledged":130,"checkpointed":6,"destination_objects":131,"fixture":"exporter","generated":130,"pruned":124,"published":130,"staged_leftovers":0,"stale_temporaries":0}` |
| 4 | 15 | `/var/tmp/reliaburger-power-cut.ITumPE` | PASS | `{"acknowledged":188,"checkpointed":7,"destination_objects":189,"fixture":"exporter","generated":188,"pruned":182,"published":188,"staged_leftovers":0,"stale_temporaries":0}` |
| 5 | 6 | `/var/tmp/reliaburger-power-cut.M1u7zb` | PASS | `{"acknowledged":80,"checkpointed":6,"destination_objects":81,"fixture":"exporter","generated":80,"pruned":74,"published":80,"staged_leftovers":0,"stale_temporaries":0}` |
| 6 | 6 | `/var/tmp/reliaburger-power-cut.Uzf6Mf` | PASS | `{"acknowledged":78,"checkpointed":7,"destination_objects":79,"fixture":"exporter","generated":78,"pruned":71,"published":78,"staged_leftovers":0,"stale_temporaries":0}` |
| 7 | 5 | `/var/tmp/reliaburger-power-cut.5UkDUv` | PASS | `{"acknowledged":70,"checkpointed":6,"destination_objects":71,"fixture":"exporter","generated":70,"pruned":64,"published":70,"staged_leftovers":0,"stale_temporaries":0}` |
| 8 | 13 | `/var/tmp/reliaburger-power-cut.X5JMbj` | PASS | `{"acknowledged":159,"checkpointed":6,"destination_objects":160,"fixture":"exporter","generated":159,"pruned":153,"published":159,"staged_leftovers":0,"stale_temporaries":1}` |
| 9 | 11 | `/var/tmp/reliaburger-power-cut.UpyJqH` | PASS | `{"acknowledged":143,"checkpointed":7,"destination_objects":144,"fixture":"exporter","generated":143,"pruned":136,"published":143,"staged_leftovers":0,"stale_temporaries":0}` |
| 10 | 4 | `/var/tmp/reliaburger-power-cut.OrDhJ4` | PASS | `{"acknowledged":54,"checkpointed":6,"destination_objects":55,"fixture":"exporter","generated":54,"pruned":48,"published":54,"staged_leftovers":0,"stale_temporaries":0}` |
| 11 | 16 | `/var/tmp/reliaburger-power-cut.hEIR01` | PASS | `{"acknowledged":199,"checkpointed":7,"destination_objects":200,"fixture":"exporter","generated":199,"pruned":192,"published":199,"staged_leftovers":0,"stale_temporaries":0}` |
| 12 | 13 | `/var/tmp/reliaburger-power-cut.u19Fqs` | PASS | `{"acknowledged":164,"checkpointed":9,"destination_objects":165,"fixture":"exporter","generated":164,"pruned":155,"published":164,"staged_leftovers":0,"stale_temporaries":0}` |
| 13 | 5 | `/var/tmp/reliaburger-power-cut.tjIjlC` | PASS | `{"acknowledged":66,"checkpointed":6,"destination_objects":67,"fixture":"exporter","generated":66,"pruned":60,"published":66,"staged_leftovers":0,"stale_temporaries":0}` |
| 14 | 1 | `/var/tmp/reliaburger-power-cut.Te7J63` | PASS | `{"acknowledged":16,"checkpointed":10,"destination_objects":18,"fixture":"exporter","generated":17,"pruned":7,"published":17,"staged_leftovers":0,"stale_temporaries":0}` |
| 15 | 18 | `/var/tmp/reliaburger-power-cut.bIQ9X4` | PASS | `{"acknowledged":227,"checkpointed":6,"destination_objects":228,"fixture":"exporter","generated":227,"pruned":221,"published":227,"staged_leftovers":0,"stale_temporaries":0}` |
| 16 | 5 | `/var/tmp/reliaburger-power-cut.LCAxX6` | PASS | `{"acknowledged":60,"checkpointed":8,"destination_objects":61,"fixture":"exporter","generated":60,"pruned":52,"published":60,"staged_leftovers":0,"stale_temporaries":0}` |
| 17 | 19 | `/var/tmp/reliaburger-power-cut.C3d5rA` | PASS | `{"acknowledged":231,"checkpointed":7,"destination_objects":232,"fixture":"exporter","generated":231,"pruned":224,"published":231,"staged_leftovers":0,"stale_temporaries":0}` |
| 18 | 16 | `/var/tmp/reliaburger-power-cut.18p3G8` | PASS | `{"acknowledged":203,"checkpointed":6,"destination_objects":204,"fixture":"exporter","generated":203,"pruned":197,"published":203,"staged_leftovers":0,"stale_temporaries":0}` |
| 19 | 1 | `/var/tmp/reliaburger-power-cut.JD0Qax` | PASS | `{"acknowledged":17,"checkpointed":5,"destination_objects":18,"fixture":"exporter","generated":17,"pruned":12,"published":17,"staged_leftovers":0,"stale_temporaries":0}` |
| 20 | 16 | `/var/tmp/reliaburger-power-cut.b97DTH` | PASS | `{"acknowledged":204,"checkpointed":9,"destination_objects":205,"fixture":"exporter","generated":204,"pruned":195,"published":204,"staged_leftovers":0,"stale_temporaries":0}` |
| 21 | 18 | `/var/tmp/reliaburger-power-cut.JCs6rO` | PASS | `{"acknowledged":229,"checkpointed":6,"destination_objects":231,"fixture":"exporter","generated":230,"pruned":223,"published":230,"staged_leftovers":0,"stale_temporaries":0}` |
| 22 | 3 | `/var/tmp/reliaburger-power-cut.hOnu2v` | PASS | `{"acknowledged":45,"checkpointed":7,"destination_objects":46,"fixture":"exporter","generated":45,"pruned":38,"published":45,"staged_leftovers":0,"stale_temporaries":0}` |
| 23 | 11 | `/var/tmp/reliaburger-power-cut.eIhrvH` | PASS | `{"acknowledged":138,"checkpointed":6,"destination_objects":140,"fixture":"exporter","generated":139,"pruned":132,"published":139,"staged_leftovers":0,"stale_temporaries":0}` |
| 24 | 19 | `/var/tmp/reliaburger-power-cut.1Hzcfe` | PASS | `{"acknowledged":241,"checkpointed":8,"destination_objects":242,"fixture":"exporter","generated":241,"pruned":234,"published":241,"staged_leftovers":0,"stale_temporaries":0}` |
| 25 | 17 | `/var/tmp/reliaburger-power-cut.Jpq558` | PASS | `{"acknowledged":211,"checkpointed":7,"destination_objects":212,"fixture":"exporter","generated":211,"pruned":204,"published":211,"staged_leftovers":0,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
