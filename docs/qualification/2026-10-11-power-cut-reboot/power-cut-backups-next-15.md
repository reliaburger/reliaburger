# Storage power-cut qualification: backups

- VM: `rb-691-crash` (guest kernel 6.8.0-142-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_council_backups`
- Commit: `ab18602bf9e6a9691d60853c690a4cabae1ef8d1`; binary SHA-256 `0d4e98edf5429e1fe0e97ea7a8165b5eb759027d98c68759b8e63a1931b4c185`
- Started: 2026-10-10T21:28:10Z
- Host evidence: `/private/tmp/claude-501/-Users-miko-github-reliaburger/68c7cd97-0874-4446-8c64-77c5be59a15c/scratchpad/qual-tmp/reliaburger-power-cut.g2CqoI`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 6 | `/var/tmp/reliaburger-power-cut.yTRkud` | PASS | `{"fixture":"backups","acknowledged":250,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 2 | 11 | `/var/tmp/reliaburger-power-cut.SWa6qH` | PASS | `{"fixture":"backups","acknowledged":466,"present":3,"in_flight_landed":0,"staged_leftovers":1}` |
| 3 | 15 | `/var/tmp/reliaburger-power-cut.z0iSoS` | PASS | `{"fixture":"backups","acknowledged":648,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 4 | 15 | `/var/tmp/reliaburger-power-cut.TR5mvH` | PASS | `{"fixture":"backups","acknowledged":633,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 5 | 0 | `/var/tmp/reliaburger-power-cut.PhNfpc` | PASS | `{"fixture":"backups","acknowledged":9,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 6 | 8 | `/var/tmp/reliaburger-power-cut.Gxc3YM` | PASS | `{"fixture":"backups","acknowledged":346,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 7 | 15 | `/var/tmp/reliaburger-power-cut.y4wlLO` | PASS | `{"fixture":"backups","acknowledged":640,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 8 | 13 | `/var/tmp/reliaburger-power-cut.VhC8rv` | PASS | `{"fixture":"backups","acknowledged":547,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 9 | 18 | `/var/tmp/reliaburger-power-cut.vZ2yUI` | PASS | `{"fixture":"backups","acknowledged":736,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 10 | 8 | `/var/tmp/reliaburger-power-cut.niEPlR` | PASS | `{"fixture":"backups","acknowledged":334,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 11 | 13 | `/var/tmp/reliaburger-power-cut.ouuC0B` | PASS | `{"fixture":"backups","acknowledged":559,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 12 | 11 | `/var/tmp/reliaburger-power-cut.E7NlzS` | PASS | `{"fixture":"backups","acknowledged":460,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 13 | 8 | `/var/tmp/reliaburger-power-cut.Sjmwd0` | PASS | `{"fixture":"backups","acknowledged":342,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 14 | 15 | `/var/tmp/reliaburger-power-cut.IUSwGU` | PASS | `{"fixture":"backups","acknowledged":634,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 15 | 17 | `/var/tmp/reliaburger-power-cut.aWjdLN` | PASS | `{"fixture":"backups","acknowledged":685,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |

15 of 15 iterations passed. Rule-of-three bound: failure rate < 20.0% at 95% confidence.
