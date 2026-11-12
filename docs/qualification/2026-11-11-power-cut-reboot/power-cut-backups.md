# Storage power-cut qualification: backups

- VM: `rb-pcut-e423d469` (guest kernel 6.8.0-147-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_council_backups`
- Commit: `e423d469a93b04d7bdc64582be1922ac5f453fd1`; binary SHA-256 `9049099169dcdd7e7a9666bef97b28ed88603a2d97b558eb8c8e473608acddbb`
- Started: 2026-11-11T13:24:16Z
- Host evidence: `/var/folders/tt/svvt5z395kgb9vqzn2wdq91h0000gn/T//reliaburger-power-cut.PqwPFo`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 17 | `/var/tmp/reliaburger-power-cut.vVd2HH` | PASS | `{"acknowledged":708,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 2 | 12 | `/var/tmp/reliaburger-power-cut.7A4FC5` | PASS | `{"acknowledged":485,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 3 | 19 | `/var/tmp/reliaburger-power-cut.frO6dA` | PASS | `{"acknowledged":780,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 4 | 4 | `/var/tmp/reliaburger-power-cut.yzgl2e` | PASS | `{"acknowledged":172,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 5 | 13 | `/var/tmp/reliaburger-power-cut.vFFx9f` | PASS | `{"acknowledged":538,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 6 | 11 | `/var/tmp/reliaburger-power-cut.EAq3s8` | PASS | `{"acknowledged":444,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 7 | 9 | `/var/tmp/reliaburger-power-cut.TndSqO` | PASS | `{"acknowledged":370,"fixture":"backups","in_flight_landed":0,"present":4,"staged_leftovers":0}` |
| 8 | 1 | `/var/tmp/reliaburger-power-cut.0WhldY` | PASS | `{"acknowledged":48,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 9 | 6 | `/var/tmp/reliaburger-power-cut.g9BMzx` | PASS | `{"acknowledged":253,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 10 | 15 | `/var/tmp/reliaburger-power-cut.Yay7fm` | PASS | `{"acknowledged":631,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 11 | 10 | `/var/tmp/reliaburger-power-cut.cO8sUZ` | PASS | `{"acknowledged":416,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 12 | 3 | `/var/tmp/reliaburger-power-cut.42Kr1O` | PASS | `{"acknowledged":129,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 13 | 5 | `/var/tmp/reliaburger-power-cut.5OB7Jo` | PASS | `{"acknowledged":212,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 14 | 4 | `/var/tmp/reliaburger-power-cut.pQtoOF` | PASS | `{"acknowledged":178,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 15 | 18 | `/var/tmp/reliaburger-power-cut.OAi8Un` | PASS | `{"acknowledged":728,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 16 | 0 | `/var/tmp/reliaburger-power-cut.S7Gg4m` | PASS | `{"acknowledged":8,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 17 | 15 | `/var/tmp/reliaburger-power-cut.KLnO4a` | PASS | `{"acknowledged":598,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 18 | 12 | `/var/tmp/reliaburger-power-cut.ctZo37` | PASS | `{"acknowledged":501,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 19 | 6 | `/var/tmp/reliaburger-power-cut.edmaqx` | PASS | `{"acknowledged":251,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 20 | 19 | `/var/tmp/reliaburger-power-cut.4TNyvu` | PASS | `{"acknowledged":774,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 21 | 7 | `/var/tmp/reliaburger-power-cut.ycO9Fc` | PASS | `{"acknowledged":286,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 22 | 14 | `/var/tmp/reliaburger-power-cut.kn2F62` | PASS | `{"acknowledged":582,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 23 | 3 | `/var/tmp/reliaburger-power-cut.xPLkjD` | PASS | `{"acknowledged":135,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 24 | 7 | `/var/tmp/reliaburger-power-cut.ESlolf` | PASS | `{"acknowledged":301,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |
| 25 | 2 | `/var/tmp/reliaburger-power-cut.vjiL6S` | PASS | `{"acknowledged":82,"fixture":"backups","in_flight_landed":0,"present":3,"staged_leftovers":0}` |

25 of 25 iterations passed. Rule-of-three bound: failure rate < 12.0% at 95% confidence.
