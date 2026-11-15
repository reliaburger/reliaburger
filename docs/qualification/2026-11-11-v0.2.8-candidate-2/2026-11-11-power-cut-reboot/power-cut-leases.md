# Storage power-cut qualification: leases

- VM: `rb-pcut-e43417a8` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_lease_operations`
- Commit: `e43417a8a2a147910667ba9a075af03e0fcb6a8d`; binary SHA-256 `e73fed7fafe6d7c8bf9e57a0841877ebcbf91ac258c92308772dc8685f8d8802`
- Started: 2026-11-11T01:04:38Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.hNXNzi`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 17 | `/var/tmp/reliaburger-power-cut.TDRu26` | PASS | `{"acknowledged":2488,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":1}` |
| 2 | 17 | `/var/tmp/reliaburger-power-cut.E97JUv` | PASS | `{"acknowledged":2491,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 3 | 11 | `/var/tmp/reliaburger-power-cut.SjE25u` | PASS | `{"acknowledged":1668,"fixture":"leases","in_flight_landed":1,"live_leases":19,"stale_temporaries":0}` |
| 4 | 7 | `/var/tmp/reliaburger-power-cut.rKvZ1W` | PASS | `{"acknowledged":1060,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 5 | 14 | `/var/tmp/reliaburger-power-cut.RTtzry` | PASS | `{"acknowledged":2079,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 6 | 15 | `/var/tmp/reliaburger-power-cut.VRZb2b` | PASS | `{"acknowledged":2234,"fixture":"leases","in_flight_landed":0,"live_leases":18,"stale_temporaries":0}` |
| 7 | 19 | `/var/tmp/reliaburger-power-cut.IIwqau` | PASS | `{"acknowledged":2777,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 8 | 3 | `/var/tmp/reliaburger-power-cut.qifInI` | PASS | `{"acknowledged":464,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 9 | 8 | `/var/tmp/reliaburger-power-cut.cs5MoO` | PASS | `{"acknowledged":1173,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 10 | 18 | `/var/tmp/reliaburger-power-cut.IEHzVR` | PASS | `{"acknowledged":2657,"fixture":"leases","in_flight_landed":0,"live_leases":17,"stale_temporaries":0}` |
| 11 | 3 | `/var/tmp/reliaburger-power-cut.2LQEPt` | PASS | `{"acknowledged":474,"fixture":"leases","in_flight_landed":1,"live_leases":16,"stale_temporaries":1}` |
| 12 | 12 | `/var/tmp/reliaburger-power-cut.9ORyIH` | PASS | `{"acknowledged":1773,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 13 | 4 | `/var/tmp/reliaburger-power-cut.aezNWC` | PASS | `{"acknowledged":609,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |
| 14 | 0 | `/var/tmp/reliaburger-power-cut.xpvARb` | PASS | `{"acknowledged":24,"fixture":"leases","in_flight_landed":0,"live_leases":11,"stale_temporaries":0}` |
| 15 | 3 | `/var/tmp/reliaburger-power-cut.epfrSg` | PASS | `{"acknowledged":466,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 16 | 6 | `/var/tmp/reliaburger-power-cut.HvFHgV` | PASS | `{"acknowledged":911,"fixture":"leases","in_flight_landed":0,"live_leases":24,"stale_temporaries":0}` |
| 17 | 2 | `/var/tmp/reliaburger-power-cut.o3UNR8` | PASS | `{"acknowledged":321,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":0}` |
| 18 | 8 | `/var/tmp/reliaburger-power-cut.CvSyB7` | PASS | `{"acknowledged":1187,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 19 | 13 | `/var/tmp/reliaburger-power-cut.Ks0N8w` | PASS | `{"acknowledged":1875,"fixture":"leases","in_flight_landed":0,"live_leases":19,"stale_temporaries":0}` |
| 20 | 17 | `/var/tmp/reliaburger-power-cut.lWTfnF` | PASS | `{"acknowledged":2507,"fixture":"leases","in_flight_landed":0,"live_leases":21,"stale_temporaries":0}` |
| 21 | 11 | `/var/tmp/reliaburger-power-cut.zDZa3Z` | PASS | `{"acknowledged":1645,"fixture":"leases","in_flight_landed":0,"live_leases":16,"stale_temporaries":1}` |
| 22 | 17 | `/var/tmp/reliaburger-power-cut.Rm4nO0` | PASS | `{"acknowledged":2474,"fixture":"leases","in_flight_landed":0,"live_leases":15,"stale_temporaries":0}` |
| 23 | 0 | `/var/tmp/reliaburger-power-cut.RRfTny` | PASS | `{"acknowledged":26,"fixture":"leases","in_flight_landed":0,"live_leases":12,"stale_temporaries":0}` |
| 24 | 10 | `/var/tmp/reliaburger-power-cut.IPZPG1` | PASS | `{"acknowledged":1540,"fixture":"leases","in_flight_landed":0,"live_leases":23,"stale_temporaries":0}` |
| 25 | 6 | `/var/tmp/reliaburger-power-cut.iHEGcb` | PASS | `{"acknowledged":896,"fixture":"leases","in_flight_landed":0,"live_leases":22,"stale_temporaries":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
