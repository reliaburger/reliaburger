# Storage power-cut qualification: exporter

- VM: `rb-pcut-964b56a2` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_log_exports`
- Commit: `964b56a2c41e2b43fb48aadef7d01839155c0dca`; binary SHA-256 `1a7409432456d8cad8d39026d4d3fd396241a524c3d8197490436582c8e046fc`
- Started: 2026-10-29T13:03:19Z
- Host evidence: `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-power-cut.zW4UNA`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 16 | `/var/tmp/reliaburger-power-cut.TESYa4` | PASS | `{"acknowledged":206,"checkpointed":5,"destination_objects":208,"fixture":"exporter","generated":207,"pruned":201,"published":207,"staged_leftovers":0,"stale_temporaries":0}` |
| 2 | 7 | `/var/tmp/reliaburger-power-cut.pYT1Kl` | PASS | `{"acknowledged":88,"checkpointed":6,"destination_objects":89,"fixture":"exporter","generated":88,"pruned":82,"published":88,"staged_leftovers":0,"stale_temporaries":0}` |
| 3 | 14 | `/var/tmp/reliaburger-power-cut.igRoFY` | PASS | `{"acknowledged":178,"checkpointed":9,"destination_objects":179,"fixture":"exporter","generated":178,"pruned":169,"published":178,"staged_leftovers":0,"stale_temporaries":0}` |
| 4 | 4 | `/var/tmp/reliaburger-power-cut.zeknLt` | PASS | `{"acknowledged":55,"checkpointed":10,"destination_objects":56,"fixture":"exporter","generated":55,"pruned":45,"published":55,"staged_leftovers":0,"stale_temporaries":0}` |
| 5 | 16 | `/var/tmp/reliaburger-power-cut.wJgJy7` | PASS | `{"acknowledged":201,"checkpointed":6,"destination_objects":202,"fixture":"exporter","generated":201,"pruned":195,"published":201,"staged_leftovers":0,"stale_temporaries":0}` |
| 6 | 6 | `/var/tmp/reliaburger-power-cut.xSvIyM` | PASS | `{"acknowledged":71,"checkpointed":6,"destination_objects":73,"fixture":"exporter","generated":72,"pruned":65,"published":72,"staged_leftovers":0,"stale_temporaries":0}` |
| 7 | 12 | `/var/tmp/reliaburger-power-cut.oZ2hL0` | PASS | `{"acknowledged":159,"checkpointed":6,"destination_objects":160,"fixture":"exporter","generated":159,"pruned":153,"published":159,"staged_leftovers":0,"stale_temporaries":0}` |
| 8 | 0 | `/var/tmp/reliaburger-power-cut.NP0Hlr` | PASS | `{"acknowledged":3,"checkpointed":3,"destination_objects":4,"fixture":"exporter","generated":3,"pruned":0,"published":3,"staged_leftovers":0,"stale_temporaries":0}` |
| 9 | 15 | `/var/tmp/reliaburger-power-cut.0HIcbY` | PASS | `{"acknowledged":197,"checkpointed":7,"destination_objects":198,"fixture":"exporter","generated":197,"pruned":190,"published":197,"staged_leftovers":0,"stale_temporaries":0}` |
| 10 | 13 | `/var/tmp/reliaburger-power-cut.oeg9w6` | PASS | `{"acknowledged":170,"checkpointed":6,"destination_objects":171,"fixture":"exporter","generated":170,"pruned":164,"published":170,"staged_leftovers":0,"stale_temporaries":0}` |
| 11 | 1 | `/var/tmp/reliaburger-power-cut.hAYdZm` | PASS | `{"acknowledged":18,"checkpointed":7,"destination_objects":19,"fixture":"exporter","generated":18,"pruned":11,"published":18,"staged_leftovers":0,"stale_temporaries":0}` |
| 12 | 13 | `/var/tmp/reliaburger-power-cut.PgdTGj` | PASS | `{"acknowledged":153,"checkpointed":6,"destination_objects":154,"fixture":"exporter","generated":153,"pruned":147,"published":153,"staged_leftovers":0,"stale_temporaries":0}` |
| 13 | 19 | `/var/tmp/reliaburger-power-cut.zQiTQi` | PASS | `{"acknowledged":240,"checkpointed":6,"destination_objects":242,"fixture":"exporter","generated":241,"pruned":234,"published":241,"staged_leftovers":0,"stale_temporaries":0}` |
| 14 | 6 | `/var/tmp/reliaburger-power-cut.Ap1hP7` | PASS | `{"acknowledged":87,"checkpointed":7,"destination_objects":88,"fixture":"exporter","generated":87,"pruned":80,"published":87,"staged_leftovers":0,"stale_temporaries":0}` |
| 15 | 15 | `/var/tmp/reliaburger-power-cut.6iw1aE` | PASS | `{"acknowledged":185,"checkpointed":5,"destination_objects":186,"fixture":"exporter","generated":185,"pruned":180,"published":185,"staged_leftovers":0,"stale_temporaries":1}` |
| 16 | 12 | `/var/tmp/reliaburger-power-cut.2soHeh` | PASS | `{"acknowledged":156,"checkpointed":6,"destination_objects":157,"fixture":"exporter","generated":156,"pruned":150,"published":156,"staged_leftovers":0,"stale_temporaries":0}` |
| 17 | 2 | `/var/tmp/reliaburger-power-cut.SYpqha` | PASS | `{"acknowledged":27,"checkpointed":5,"destination_objects":28,"fixture":"exporter","generated":27,"pruned":22,"published":27,"staged_leftovers":0,"stale_temporaries":0}` |
| 18 | 1 | `/var/tmp/reliaburger-power-cut.0XC7Wj` | PASS | `{"acknowledged":16,"checkpointed":6,"destination_objects":17,"fixture":"exporter","generated":16,"pruned":10,"published":16,"staged_leftovers":0,"stale_temporaries":0}` |
| 19 | 16 | `/var/tmp/reliaburger-power-cut.tPPhLu` | PASS | `{"acknowledged":191,"checkpointed":7,"destination_objects":192,"fixture":"exporter","generated":192,"pruned":185,"published":191,"staged_leftovers":0,"stale_temporaries":0}` |
| 20 | 20 | `/var/tmp/reliaburger-power-cut.0Lp7Jr` | PASS | `{"acknowledged":251,"checkpointed":7,"destination_objects":253,"fixture":"exporter","generated":252,"pruned":244,"published":252,"staged_leftovers":0,"stale_temporaries":0}` |
| 21 | 20 | `/var/tmp/reliaburger-power-cut.PmL5tj` | PASS | `{"acknowledged":257,"checkpointed":6,"destination_objects":258,"fixture":"exporter","generated":257,"pruned":251,"published":257,"staged_leftovers":0,"stale_temporaries":0}` |
| 22 | 18 | `/var/tmp/reliaburger-power-cut.hiA9rR` | PASS | `{"acknowledged":226,"checkpointed":6,"destination_objects":227,"fixture":"exporter","generated":226,"pruned":220,"published":226,"staged_leftovers":0,"stale_temporaries":0}` |
| 23 | 9 | `/var/tmp/reliaburger-power-cut.cXJfhr` | PASS | `{"acknowledged":114,"checkpointed":8,"destination_objects":116,"fixture":"exporter","generated":115,"pruned":106,"published":115,"staged_leftovers":1,"stale_temporaries":0}` |
| 24 | 8 | `/var/tmp/reliaburger-power-cut.Bq8o3Z` | PASS | `{"acknowledged":98,"checkpointed":6,"destination_objects":100,"fixture":"exporter","generated":99,"pruned":92,"published":99,"staged_leftovers":0,"stale_temporaries":0}` |
| 25 | 4 | `/var/tmp/reliaburger-power-cut.Sn2T7a` | PASS | `{"acknowledged":55,"checkpointed":7,"destination_objects":56,"fixture":"exporter","generated":55,"pruned":49,"published":55,"staged_leftovers":0,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
