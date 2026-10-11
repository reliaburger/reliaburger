# Storage power-cut qualification: exporter

- VM: `rb-691-crash` (guest kernel 6.8.0-142-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_log_exports`
- Commit: `ab18602bf9e6a9691d60853c690a4cabae1ef8d1`; binary SHA-256 `0d4e98edf5429e1fe0e97ea7a8165b5eb759027d98c68759b8e63a1931b4c185`
- Started: 2026-10-10T20:43:32Z
- Host evidence: `/private/tmp/claude-501/-Users-miko-github-reliaburger/68c7cd97-0874-4446-8c64-77c5be59a15c/scratchpad/qual-tmp/reliaburger-power-cut.mSRhkH`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 15 | `/var/tmp/reliaburger-power-cut.q3CL0F` | PASS | `{"fixture":"exporter","generated":199,"published":199,"acknowledged":199,"checkpointed":8,"pruned":192,"destination_objects":200,"staged_leftovers":0,"stale_temporaries":0}` |
| 2 | 18 | `/var/tmp/reliaburger-power-cut.2uUm4a` | PASS | `{"fixture":"exporter","generated":237,"published":237,"acknowledged":237,"checkpointed":6,"pruned":231,"destination_objects":238,"staged_leftovers":0,"stale_temporaries":0}` |
| 3 | 0 | `/var/tmp/reliaburger-power-cut.MFGyKn` | PASS | `{"fixture":"exporter","generated":5,"published":5,"acknowledged":5,"checkpointed":5,"pruned":0,"destination_objects":6,"staged_leftovers":0,"stale_temporaries":0}` |
| 4 | 16 | `/var/tmp/reliaburger-power-cut.yFqMfl` | PASS | `{"fixture":"exporter","generated":212,"published":212,"acknowledged":212,"checkpointed":6,"pruned":206,"destination_objects":213,"staged_leftovers":0,"stale_temporaries":0}` |
| 5 | 18 | `/var/tmp/reliaburger-power-cut.AqpxUd` | PASS | `{"fixture":"exporter","generated":234,"published":234,"acknowledged":234,"checkpointed":6,"pruned":228,"destination_objects":235,"staged_leftovers":0,"stale_temporaries":0}` |
| 6 | 3 | `/var/tmp/reliaburger-power-cut.RLG5R5` | PASS | `{"fixture":"exporter","generated":37,"published":37,"acknowledged":37,"checkpointed":7,"pruned":30,"destination_objects":38,"staged_leftovers":0,"stale_temporaries":0}` |
| 7 | 3 | `/var/tmp/reliaburger-power-cut.cd3SLP` | PASS | `{"fixture":"exporter","generated":42,"published":42,"acknowledged":42,"checkpointed":7,"pruned":35,"destination_objects":43,"staged_leftovers":0,"stale_temporaries":0}` |
| 8 | 8 | `/var/tmp/reliaburger-power-cut.e7OlEi` | PASS | `{"fixture":"exporter","generated":105,"published":105,"acknowledged":105,"checkpointed":8,"pruned":99,"destination_objects":106,"staged_leftovers":0,"stale_temporaries":0}` |
| 9 | 1 | `/var/tmp/reliaburger-power-cut.08IR1O` | PASS | `{"fixture":"exporter","generated":19,"published":19,"acknowledged":18,"checkpointed":6,"pruned":12,"destination_objects":20,"staged_leftovers":0,"stale_temporaries":0}` |
| 10 | 17 | `/var/tmp/reliaburger-power-cut.zQKajL` | PASS | `{"fixture":"exporter","generated":233,"published":233,"acknowledged":232,"checkpointed":5,"pruned":227,"destination_objects":234,"staged_leftovers":0,"stale_temporaries":0}` |

10 of 10 iterations passed. Rule-of-three bound: failure rate < 30.0% at 95% confidence.
