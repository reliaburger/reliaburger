# Staged install qualification: FAIL

17 October 2026. `curl | sh` against a staged candidate, from empty caches in an
isolated `RELIABURGER_HOME`, then a short tour and teardown.

## Setup

- Host: `Darwin 25.3.0 arm64`
- macOS 26.3.1, Apple M2 Max, 32 GiB
- Staged base URL: <https://github.com/reliaburger/reliaburger/releases/download/staging-v0.2.1-39021688274-1>
- Bootstrap: `https://reliaburger.com/install.sh` (SHA-256 `391e348d9d12b9ddc292b230c779181ca891b8575d06e299c9e55697bc420c9d`)
- `candidate.json` SHA-256: `3243869da2235834d3c92cd3b31c9258c480b20562fdea039115fd304c621c00` (required: `3243869da2235834d3c92cd3b31c9258c480b20562fdea039115fd304c621c00`)
- Command: `curl -fsSL https://reliaburger.com/install.sh | RELIABURGER_RELEASE_BASE_URL=https://github.com/reliaburger/reliaburger/releases/download/staging-v0.2.1-39021688274-1 sh -s -- --timings`
- Relish: `not installed`
- Failed during: install (curl | sh)

## Install

Wall time from the first `curl` to a ready cluster: n/a s.

```
where the time went (198.6s in total):
  host checks          0.0s
  downloads          162.6s  879.0 MiB at 5.4 MiB/s
  VM boot             21.3s
  node setup          14.6s
  cluster checks       0.0s

every step (duration, start offset):
      0.0s +   0.0s  check host                   Done
     20.9s +   0.0s  install Lima 2.2.1           Done
    162.6s +   0.0s  download guest image         Done
     60.5s +   0.0s  download bun                 Done
     54.1s +   0.0s  download relish              Done
     21.3s + 162.7s  boot VM 1                    Done
     20.8s + 163.2s  boot VM 2                    Done
     20.8s + 163.2s  boot VM 3                    Done
      0.9s + 184.1s  install files on node 1      Done
      0.7s + 185.0s  start node 1                 Done
      2.0s + 185.7s  install files on node 2      Done
      2.0s + 185.7s  install files on node 3      Done
      0.2s + 187.7s  enrol node 3                 Done
      0.3s + 187.7s  enrol node 2                 Done
      5.6s + 188.0s  start node 3                 Done
     10.6s + 188.0s  start node 2                 Done
      0.0s + 198.6s  form council quorum          Done
      0.0s + 198.6s  run hello through ingress    Failed

timings saved to /private/tmp/rbq.zmZuO3/clusters/laptop/timings.json
error: cluster initialisation failed: setup stopped; checkpoints are in /private/tmp/rbq.zmZuO3/clusters/laptop. Re-run the same setup command to resume: API error (status 400): {"error":"health.timeout in \"app \\\"hello\\\"\": timeout (5s) must not be longer than interval (1s)"}
```

## Downloaded bytes

Not collected.

## Tour

| Command | Time | Exit |
|---|---|---|

## Teardown

`relish local destroy --yes` and `relish uninstall --yes` succeeded

Evidence (logs, timings, candidate record): `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-qualify.yIJSzi`
