# Storage power-cut qualification: exporter

- VM: `rb-pcut-50596cfe` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_log_exports`
- Commit: `50596cfe9438d16070b1660db45b1d2a29f77106`; binary SHA-256 `fcb2942a43e0d89b809c9a478c9e9c70050375c6ddb82d112d37b92243ab3f58`
- Started: 2026-11-05T12:14:53Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.eLb2OZ`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 5 | `/var/tmp/reliaburger-power-cut.kmfBwT` | PASS | `{"acknowledged":64,"checkpointed":7,"destination_objects":65,"fixture":"exporter","generated":64,"pruned":58,"published":64,"staged_leftovers":0,"stale_temporaries":0}` |
| 2 | 14 | `/var/tmp/reliaburger-power-cut.hX1uD9` | PASS | `{"acknowledged":185,"checkpointed":8,"destination_objects":186,"fixture":"exporter","generated":185,"pruned":179,"published":185,"staged_leftovers":0,"stale_temporaries":0}` |
| 3 | 18 | `/var/tmp/reliaburger-power-cut.6nBKLD` | PASS | `{"acknowledged":234,"checkpointed":6,"destination_objects":235,"fixture":"exporter","generated":234,"pruned":228,"published":234,"staged_leftovers":0,"stale_temporaries":0}` |
| 4 | 2 | `/var/tmp/reliaburger-power-cut.zHWI4d` | PASS | `{"acknowledged":28,"checkpointed":6,"destination_objects":29,"fixture":"exporter","generated":28,"pruned":22,"published":28,"staged_leftovers":0,"stale_temporaries":0}` |
| 5 | 16 | `/var/tmp/reliaburger-power-cut.6lmyWd` | PASS | `{"acknowledged":197,"checkpointed":6,"destination_objects":198,"fixture":"exporter","generated":197,"pruned":191,"published":197,"staged_leftovers":0,"stale_temporaries":0}` |
| 6 | 4 | `/var/tmp/reliaburger-power-cut.pzFwnl` | PASS | `{"acknowledged":54,"checkpointed":7,"destination_objects":56,"fixture":"exporter","generated":55,"pruned":48,"published":55,"staged_leftovers":0,"stale_temporaries":0}` |
| 7 | 20 | `/var/tmp/reliaburger-power-cut.NcOrlb` | PASS | `{"acknowledged":255,"checkpointed":6,"destination_objects":257,"fixture":"exporter","generated":256,"pruned":249,"published":256,"staged_leftovers":0,"stale_temporaries":0}` |
| 8 | 3 | `/var/tmp/reliaburger-power-cut.yyeGM5` | PASS | `{"acknowledged":42,"checkpointed":7,"destination_objects":44,"fixture":"exporter","generated":43,"pruned":36,"published":43,"staged_leftovers":0,"stale_temporaries":0}` |
| 9 | 12 | `/var/tmp/reliaburger-power-cut.RcAZrL` | PASS | `{"acknowledged":159,"checkpointed":6,"destination_objects":160,"fixture":"exporter","generated":159,"pruned":153,"published":159,"staged_leftovers":0,"stale_temporaries":0}` |
| 10 | 12 | `/var/tmp/reliaburger-power-cut.HNcMws` | PASS | `{"acknowledged":153,"checkpointed":8,"destination_objects":154,"fixture":"exporter","generated":153,"pruned":145,"published":153,"staged_leftovers":0,"stale_temporaries":0}` |
| 11 | 4 | `/var/tmp/reliaburger-power-cut.j0UG4E` | PASS | `{"acknowledged":55,"checkpointed":6,"destination_objects":56,"fixture":"exporter","generated":55,"pruned":49,"published":55,"staged_leftovers":0,"stale_temporaries":0}` |
| 12 | 18 | `/var/tmp/reliaburger-power-cut.0bihGY` | PASS | `{"acknowledged":232,"checkpointed":6,"destination_objects":233,"fixture":"exporter","generated":232,"pruned":226,"published":232,"staged_leftovers":0,"stale_temporaries":0}` |
| 13 | 18 | `/var/tmp/reliaburger-power-cut.0aA1hA` | PASS | `{"acknowledged":225,"checkpointed":7,"destination_objects":226,"fixture":"exporter","generated":225,"pruned":219,"published":225,"staged_leftovers":0,"stale_temporaries":0}` |
| 14 | 4 | `/var/tmp/reliaburger-power-cut.XXf28Y` | PASS | `{"acknowledged":58,"checkpointed":6,"destination_objects":59,"fixture":"exporter","generated":58,"pruned":52,"published":58,"staged_leftovers":0,"stale_temporaries":0}` |
| 15 | 18 | `/var/tmp/reliaburger-power-cut.pBuXyD` | PASS | `{"acknowledged":226,"checkpointed":6,"destination_objects":227,"fixture":"exporter","generated":226,"pruned":220,"published":226,"staged_leftovers":0,"stale_temporaries":0}` |
| 16 | 11 | `/var/tmp/reliaburger-power-cut.czeSgH` | PASS | `{"acknowledged":140,"checkpointed":6,"destination_objects":141,"fixture":"exporter","generated":140,"pruned":134,"published":140,"staged_leftovers":0,"stale_temporaries":0}` |
| 17 | 20 | `/var/tmp/reliaburger-power-cut.7V33fm` | PASS | `{"acknowledged":254,"checkpointed":7,"destination_objects":255,"fixture":"exporter","generated":254,"pruned":247,"published":254,"staged_leftovers":0,"stale_temporaries":0}` |
| 18 | 19 | `/var/tmp/reliaburger-power-cut.5OlyCv` | PASS | `{"acknowledged":249,"checkpointed":7,"destination_objects":250,"fixture":"exporter","generated":249,"pruned":243,"published":249,"staged_leftovers":0,"stale_temporaries":0}` |
| 19 | 0 | `/var/tmp/reliaburger-power-cut.TE8zJ4` | PASS | `{"acknowledged":3,"checkpointed":3,"destination_objects":4,"fixture":"exporter","generated":3,"pruned":0,"published":3,"staged_leftovers":0,"stale_temporaries":0}` |
| 20 | 12 | `/var/tmp/reliaburger-power-cut.0VMl1s` | PASS | `{"acknowledged":154,"checkpointed":6,"destination_objects":155,"fixture":"exporter","generated":154,"pruned":148,"published":154,"staged_leftovers":0,"stale_temporaries":0}` |
| 21 | 4 | `/var/tmp/reliaburger-power-cut.zPnhVq` | PASS | `{"acknowledged":52,"checkpointed":7,"destination_objects":53,"fixture":"exporter","generated":52,"pruned":45,"published":52,"staged_leftovers":0,"stale_temporaries":0}` |
| 22 | 6 | `/var/tmp/reliaburger-power-cut.Ax2Opf` | PASS | `{"acknowledged":80,"checkpointed":7,"destination_objects":81,"fixture":"exporter","generated":80,"pruned":73,"published":80,"staged_leftovers":0,"stale_temporaries":0}` |
| 23 | 9 | `/var/tmp/reliaburger-power-cut.Av9Rez` | PASS | `{"acknowledged":123,"checkpointed":6,"destination_objects":124,"fixture":"exporter","generated":123,"pruned":117,"published":123,"staged_leftovers":0,"stale_temporaries":0}` |
| 24 | 3 | `/var/tmp/reliaburger-power-cut.gHUUqN` | PASS | `{"acknowledged":41,"checkpointed":7,"destination_objects":42,"fixture":"exporter","generated":41,"pruned":35,"published":41,"staged_leftovers":0,"stale_temporaries":0}` |
| 25 | 13 | `/var/tmp/reliaburger-power-cut.ibz8YN` | PASS | `{"acknowledged":165,"checkpointed":6,"destination_objects":166,"fixture":"exporter","generated":165,"pruned":159,"published":165,"staged_leftovers":0,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
