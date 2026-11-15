# Staged install qualification: FAIL

20 October 2026. `curl | sh` against a staged candidate, from empty caches in an
isolated `RELIABURGER_HOME`, then a short tour and teardown.

## Setup

- Host: `Darwin 25.3.0 arm64`
- macOS 26.3.1, Apple M2 Max, 32 GiB
- Staged base URL: <https://github.com/reliaburger/reliaburger/releases/download/staging-v0.2.1-39340788164-1>
- Bootstrap: `https://reliaburger.com/install.sh` (SHA-256 `391e348d9d12b9ddc292b230c779181ca891b8575d06e299c9e55697bc420c9d`)
- `candidate.json` SHA-256: `2470c93e0ec58d1b6d0504a8daef85f21ab1120e400761ecc903eb661353e3e0` (required: `2470c93e0ec58d1b6d0504a8daef85f21ab1120e400761ecc903eb661353e3e0`)
- Command: `curl -fsSL https://reliaburger.com/install.sh | RELIABURGER_RELEASE_BASE_URL=https://github.com/reliaburger/reliaburger/releases/download/staging-v0.2.1-39340788164-1 sh -s -- --timings`
- Relish: `not installed`
- Failed during: install (curl | sh)

## Install

Wall time from the first `curl` to a ready cluster: n/a s.

```
where the time went (97.8s in total):
  host checks          0.0s
  downloads           97.7s  879.4 MiB at 9.0 MiB/s

every step (duration, start offset):
      0.0s +   0.0s  check host                   Done
      4.5s +   0.0s  install Lima 2.2.1           Done
     97.7s +   0.0s  download guest image         Done
     23.1s +   0.0s  download bun                 Done
     15.7s +   0.0s  download relish              Done

timings saved to /private/tmp/rbq.1vCpgX/clusters/laptop/timings.json
error: cluster initialisation failed: setup stopped; checkpoints are in /private/tmp/rbq.1vCpgX/clusters/laptop. Re-run the same setup command to resume: starting 3 VM(s) needs 15 GiB free disk space
```

## Downloaded bytes

Not collected.

## Tour

| Command | Time | Exit |
|---|---|---|

## Teardown

`relish local destroy --yes` and `relish uninstall --yes` succeeded

Evidence (logs, timings, candidate record): `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-qualify.sVBSVp`
