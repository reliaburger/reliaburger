# Storage power-cut qualification: backups

- VM: `rb-691-crash` (guest kernel 6.8.0-142-generic)
- Test: `power_cut::actual_power_cut_preserves_acknowledged_council_backups`
- Commit: `ab18602bf9e6a9691d60853c690a4cabae1ef8d1`; binary SHA-256 `0d4e98edf5429e1fe0e97ea7a8165b5eb759027d98c68759b8e63a1931b4c185`
- Started: 2026-10-10T20:53:44Z
- Host evidence: `/private/tmp/claude-501/-Users-miko-github-reliaburger/68c7cd97-0874-4446-8c64-77c5be59a15c/scratchpad/qual-tmp/reliaburger-power-cut.2oAyjC`

| # | Cut after (s) | Guest directory | Result | Verified summary |
|---|---|---|---|---|
| 1 | 5 | `/var/tmp/reliaburger-power-cut.tnNizO` | PASS | `{"fixture":"backups","acknowledged":246,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 2 | 8 | `/var/tmp/reliaburger-power-cut.0lZ6Ah` | PASS | `{"fixture":"backups","acknowledged":351,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 3 | 1 | `/var/tmp/reliaburger-power-cut.yJsFih` | PASS | `{"fixture":"backups","acknowledged":59,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 4 | 4 | `/var/tmp/reliaburger-power-cut.2ulpd0` | PASS | `{"fixture":"backups","acknowledged":185,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 5 | 0 | `/var/tmp/reliaburger-power-cut.ezvnGy` | PASS | `{"fixture":"backups","acknowledged":10,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 6 | 20 | `/var/tmp/reliaburger-power-cut.VwSyGP` | PASS | `{"fixture":"backups","acknowledged":888,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 7 | 1 | `/var/tmp/reliaburger-power-cut.8PHwHE` | PASS | `{"fixture":"backups","acknowledged":53,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 8 | 6 | `/var/tmp/reliaburger-power-cut.WFu5VL` | PASS | `{"fixture":"backups","acknowledged":183,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 9 | 6 | `/var/tmp/reliaburger-power-cut.MTkGu4` | PASS | `{"fixture":"backups","acknowledged":217,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |
| 10 | 8 | `/var/tmp/reliaburger-power-cut.jzPMUR` | PASS | `{"fixture":"backups","acknowledged":160,"present":3,"in_flight_landed":0,"staged_leftovers":0}` |

10 of 10 iterations passed. Rule-of-three bound: failure rate < 30.0% at 95% confidence.
