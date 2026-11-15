# Storage power-cut qualification: leases

- VM: `rb-pcut-ace072c7` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_lease_operations`
- Commit: `ace072c73c2d5a5d7124f3a4f879c88bf9c33839`; binary SHA-256 `fbfc5fb026052d4278028dd2bb2cf0cec3642059608076fc27296ff07bc6f7e6`
- Started: 2026-11-07T03:05:36Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.3de5FG`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 13 | `/var/tmp/reliaburger-power-cut.VJfmJf` | PASS | `{"acknowledged":1920,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 2 | 20 | `/var/tmp/reliaburger-power-cut.eTUpA8` | PASS | `{"acknowledged":2896,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":1}` |
| 3 | 5 | `/var/tmp/reliaburger-power-cut.jpbVkx` | PASS | `{"acknowledged":748,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 4 | 3 | `/var/tmp/reliaburger-power-cut.N0TArw` | PASS | `{"acknowledged":463,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 5 | 2 | `/var/tmp/reliaburger-power-cut.i1DXJb` | PASS | `{"acknowledged":326,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 6 | 5 | `/var/tmp/reliaburger-power-cut.QnHE1k` | PASS | `{"acknowledged":745,"fixture":"leases","in_flight_landed":0,"live_leases":14,"stale_temporaries":0}` |
| 7 | 14 | `/var/tmp/reliaburger-power-cut.eARJnr` | PASS | `{"acknowledged":2056,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 8 | 6 | `/var/tmp/reliaburger-power-cut.ZmvAvg` | PASS | `{"acknowledged":906,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 9 | 4 | `/var/tmp/reliaburger-power-cut.jBevoh` | PASS | `{"acknowledged":588,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 10 | 2 | `/var/tmp/reliaburger-power-cut.mZA3i8` | PASS | `{"acknowledged":324,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 11 | 3 | `/var/tmp/reliaburger-power-cut.sn313Q` | PASS | `{"acknowledged":448,"fixture":"leases","in_flight_landed":0,"live_leases":24,"stale_temporaries":0}` |
| 12 | 17 | `/var/tmp/reliaburger-power-cut.1e2XBq` | PASS | `{"acknowledged":2512,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 13 | 16 | `/var/tmp/reliaburger-power-cut.ciUcoV` | PASS | `{"acknowledged":2316,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 14 | 1 | `/var/tmp/reliaburger-power-cut.HeRRCV` | PASS | `{"acknowledged":159,"fixture":"leases","in_flight_landed":0,"live_leases":14,"stale_temporaries":0}` |
| 15 | 19 | `/var/tmp/reliaburger-power-cut.GoheyS` | PASS | `{"acknowledged":2768,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 16 | 2 | `/var/tmp/reliaburger-power-cut.Hm6fQX` | PASS | `{"acknowledged":313,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 17 | 16 | `/var/tmp/reliaburger-power-cut.M0AR7c` | PASS | `{"acknowledged":2368,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 18 | 17 | `/var/tmp/reliaburger-power-cut.7RLr1R` | PASS | `{"acknowledged":2558,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 19 | 4 | `/var/tmp/reliaburger-power-cut.GmAmDs` | PASS | `{"acknowledged":603,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 20 | 20 | `/var/tmp/reliaburger-power-cut.PhMgAR` | PASS | `{"acknowledged":2919,"fixture":"leases","in_flight_landed":0,"live_leases":20,"stale_temporaries":0}` |
| 21 | 12 | `/var/tmp/reliaburger-power-cut.4JrTa4` | PASS | `{"acknowledged":1734,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 22 | 15 | `/var/tmp/reliaburger-power-cut.r1J0wK` | PASS | `{"acknowledged":2208,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 23 | 17 | `/var/tmp/reliaburger-power-cut.9H3Rab` | PASS | `{"acknowledged":2500,"fixture":"leases","in_flight_landed":1,"live_leases":19,"stale_temporaries":1}` |
| 24 | 6 | `/var/tmp/reliaburger-power-cut.miOCgg` | PASS | `{"acknowledged":887,"fixture":"leases","in_flight_landed":1,"live_leases":18,"stale_temporaries":1}` |
| 25 | 16 | `/var/tmp/reliaburger-power-cut.H89ahn` | PASS | `{"acknowledged":2338,"fixture":"leases","in_flight_landed":0,"live_leases":13,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
