# Storage power-cut qualification: exporter

- VM: `rb-pcut-efc3fb2f` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_log_exports`
- Commit: `efc3fb2fc7882500ed0a4ac050d4b525cb806a54`; binary SHA-256 `60c183ffefbe115642010287ead0e4e3c29cab39d76504e2c0e9c4d6e7f9af9e`
- Started: 2026-11-19T19:19:12Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.OiJKge`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 2 | `/var/tmp/reliaburger-power-cut.Ot0VDE` | PASS | `{"acknowledged":29,"checkpointed":6,"destination_objects":30,"fixture":"exporter","generated":29,"pruned":23,"published":29,"staged_leftovers":0,"stale_temporaries":0}` |
| 2 | 15 | `/var/tmp/reliaburger-power-cut.y4Lmxa` | PASS | `{"acknowledged":190,"checkpointed":6,"destination_objects":191,"fixture":"exporter","generated":190,"pruned":184,"published":190,"staged_leftovers":0,"stale_temporaries":0}` |
| 3 | 14 | `/var/tmp/reliaburger-power-cut.hNzhWI` | PASS | `{"acknowledged":176,"checkpointed":6,"destination_objects":177,"fixture":"exporter","generated":176,"pruned":170,"published":176,"staged_leftovers":0,"stale_temporaries":0}` |
| 4 | 17 | `/var/tmp/reliaburger-power-cut.29PiV0` | PASS | `{"acknowledged":220,"checkpointed":8,"destination_objects":221,"fixture":"exporter","generated":220,"pruned":212,"published":220,"staged_leftovers":0,"stale_temporaries":0}` |
| 5 | 20 | `/var/tmp/reliaburger-power-cut.VqvEML` | PASS | `{"acknowledged":264,"checkpointed":6,"destination_objects":265,"fixture":"exporter","generated":264,"pruned":258,"published":264,"staged_leftovers":0,"stale_temporaries":0}` |
| 6 | 13 | `/var/tmp/reliaburger-power-cut.r4wmdf` | PASS | `{"acknowledged":160,"checkpointed":7,"destination_objects":162,"fixture":"exporter","generated":161,"pruned":153,"published":161,"staged_leftovers":0,"stale_temporaries":0}` |
| 7 | 10 | `/var/tmp/reliaburger-power-cut.7Vp120` | PASS | `{"acknowledged":126,"checkpointed":6,"destination_objects":127,"fixture":"exporter","generated":126,"pruned":120,"published":126,"staged_leftovers":0,"stale_temporaries":0}` |
| 8 | 11 | `/var/tmp/reliaburger-power-cut.OyQNEy` | PASS | `{"acknowledged":142,"checkpointed":6,"destination_objects":144,"fixture":"exporter","generated":143,"pruned":136,"published":143,"staged_leftovers":0,"stale_temporaries":0}` |
| 9 | 16 | `/var/tmp/reliaburger-power-cut.dQ5l1u` | PASS | `{"acknowledged":207,"checkpointed":8,"destination_objects":209,"fixture":"exporter","generated":208,"pruned":199,"published":208,"staged_leftovers":0,"stale_temporaries":0}` |
| 10 | 11 | `/var/tmp/reliaburger-power-cut.Rbxyjw` | PASS | `{"acknowledged":140,"checkpointed":6,"destination_objects":142,"fixture":"exporter","generated":141,"pruned":134,"published":141,"staged_leftovers":0,"stale_temporaries":0}` |
| 11 | 2 | `/var/tmp/reliaburger-power-cut.QXWGxN` | PASS | `{"acknowledged":28,"checkpointed":7,"destination_objects":29,"fixture":"exporter","generated":28,"pruned":22,"published":28,"staged_leftovers":0,"stale_temporaries":0}` |
| 12 | 17 | `/var/tmp/reliaburger-power-cut.Lq8lFU` | PASS | `{"acknowledged":222,"checkpointed":7,"destination_objects":223,"fixture":"exporter","generated":222,"pruned":215,"published":222,"staged_leftovers":0,"stale_temporaries":0}` |
| 13 | 15 | `/var/tmp/reliaburger-power-cut.69b6ZG` | PASS | `{"acknowledged":195,"checkpointed":7,"destination_objects":197,"fixture":"exporter","generated":196,"pruned":188,"published":196,"staged_leftovers":0,"stale_temporaries":0}` |
| 14 | 2 | `/var/tmp/reliaburger-power-cut.sZXvel` | PASS | `{"acknowledged":29,"checkpointed":6,"destination_objects":30,"fixture":"exporter","generated":29,"pruned":23,"published":29,"staged_leftovers":0,"stale_temporaries":0}` |
| 15 | 18 | `/var/tmp/reliaburger-power-cut.Linjqc` | PASS | `{"acknowledged":238,"checkpointed":6,"destination_objects":239,"fixture":"exporter","generated":238,"pruned":232,"published":238,"staged_leftovers":0,"stale_temporaries":0}` |
| 16 | 2 | `/var/tmp/reliaburger-power-cut.Hp8GGg` | PASS | `{"acknowledged":29,"checkpointed":8,"destination_objects":31,"fixture":"exporter","generated":30,"pruned":22,"published":30,"staged_leftovers":0,"stale_temporaries":0}` |
| 17 | 7 | `/var/tmp/reliaburger-power-cut.FD2Cmv` | PASS | `{"acknowledged":95,"checkpointed":6,"destination_objects":96,"fixture":"exporter","generated":95,"pruned":89,"published":95,"staged_leftovers":0,"stale_temporaries":0}` |
| 18 | 11 | `/var/tmp/reliaburger-power-cut.5yea7d` | PASS | `{"acknowledged":138,"checkpointed":6,"destination_objects":139,"fixture":"exporter","generated":138,"pruned":132,"published":138,"staged_leftovers":0,"stale_temporaries":0}` |
| 19 | 14 | `/var/tmp/reliaburger-power-cut.xuPziz` | PASS | `{"acknowledged":177,"checkpointed":7,"destination_objects":178,"fixture":"exporter","generated":177,"pruned":171,"published":177,"staged_leftovers":0,"stale_temporaries":0}` |
| 20 | 1 | `/var/tmp/reliaburger-power-cut.QZ1eAH` | PASS | `{"acknowledged":19,"checkpointed":6,"destination_objects":21,"fixture":"exporter","generated":20,"pruned":13,"published":20,"staged_leftovers":1,"stale_temporaries":0}` |
| 21 | 10 | `/var/tmp/reliaburger-power-cut.sOCouF` | PASS | `{"acknowledged":127,"checkpointed":6,"destination_objects":129,"fixture":"exporter","generated":128,"pruned":121,"published":128,"staged_leftovers":0,"stale_temporaries":0}` |
| 22 | 13 | `/var/tmp/reliaburger-power-cut.6FXbDq` | PASS | `{"acknowledged":167,"checkpointed":6,"destination_objects":168,"fixture":"exporter","generated":167,"pruned":161,"published":167,"staged_leftovers":0,"stale_temporaries":0}` |
| 23 | 8 | `/var/tmp/reliaburger-power-cut.Biyk0f` | PASS | `{"acknowledged":102,"checkpointed":6,"destination_objects":104,"fixture":"exporter","generated":103,"pruned":96,"published":103,"staged_leftovers":0,"stale_temporaries":0}` |
| 24 | 15 | `/var/tmp/reliaburger-power-cut.PmX0R8` | PASS | `{"acknowledged":193,"checkpointed":6,"destination_objects":194,"fixture":"exporter","generated":193,"pruned":187,"published":193,"staged_leftovers":0,"stale_temporaries":0}` |
| 25 | 13 | `/var/tmp/reliaburger-power-cut.V3ZRNC` | PASS | `{"acknowledged":186,"checkpointed":6,"destination_objects":187,"fixture":"exporter","generated":186,"pruned":180,"published":186,"staged_leftovers":0,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
