# Staged install on Apple silicon: 0.1.0 candidate

Two cold `curl | sh` installs of the signed 0.1.0 candidate on the same Mac, each
from empty caches in its own `RELIABURGER_HOME`, then the tour and a full
teardown. Both passed.

| | |
|---|---|
| Commit | `79bd08092ba42f874d5f03eac02e02367a6a23b8` |
| Build run | [36078958881](https://github.com/reliaburger/reliaburger/actions/runs/36078958881), attempt 1 |
| Stage run | [36081287288](https://github.com/reliaburger/reliaburger/actions/runs/36081287288) |
| Staging tag | `staging-v0.1.0-36078958881-1` |
| `candidate.json` SHA-256 | `f0bc876a59595b96bc8e989ae3e85a1f599fe307abe8a9b62839fc05ed19e455` |
| Host | Apple M2 Max, 32 GiB, macOS 26.3.1 |

| Run | First `curl` to ready cluster | Downloads | Setup total | Teardown |
|---|---|---|---|---|
| 1 | 126 s | 829.6 MiB in 72.6 s | 117.4 s | destroy and uninstall succeeded |
| 2 | 125 s | 829.6 MiB in 73.9 s | 116.6 s | destroy and uninstall succeeded |

Both teardowns left only `~/.reliaburger/context.lock` in the isolated home.

## Run 1

### Setup

- Host: `Darwin 25.3.0 arm64`
- macOS 26.3.1, Apple M2 Max, 32 GiB
- Staged base URL: <https://github.com/reliaburger/reliaburger/releases/download/staging-v0.1.0-36078958881-1>
- Bootstrap: `https://reliaburger.com/install.sh` (SHA-256 `085bf57e223799d28d89ef8fae50aac3c69931513facf15784bf20c7ca3a0ef2`)
- `candidate.json` SHA-256: `f0bc876a59595b96bc8e989ae3e85a1f599fe307abe8a9b62839fc05ed19e455` (required: `f0bc876a59595b96bc8e989ae3e85a1f599fe307abe8a9b62839fc05ed19e455`)
- Command: `curl -fsSL https://reliaburger.com/install.sh | RELIABURGER_RELEASE_BASE_URL=https://github.com/reliaburger/reliaburger/releases/download/staging-v0.1.0-36078958881-1 sh -s -- --timings`
- Relish: `relish 0.1.0`
- Finished every step

### Install

Wall time from the first `curl` to a ready cluster: 126 s.

```
where the time went (117.4s in total):
  host checks          0.0s
  downloads           72.6s  829.6 MiB at 11.4 MiB/s
  VM boot             21.4s
  node setup          14.1s
  cluster checks       9.2s

every step (duration, start offset):
      0.0s +   0.0s  check host                   Done
      7.2s +   0.0s  install Lima 2.1.0           Done
     72.6s +   0.0s  download guest image         Done
     41.2s +   0.0s  download bun                 Done
     37.9s +   0.0s  download relish              Done
     18.9s +  72.7s  boot VM 1                    Done
     19.8s +  73.2s  boot VM 2                    Done
     20.9s +  73.2s  boot VM 3                    Done
      0.8s +  94.1s  install files on node 1      Done
      0.5s +  94.9s  start node 1                 Done
      1.8s +  95.4s  install files on node 2      Done
      1.8s +  95.4s  install files on node 3      Done
      0.3s +  97.2s  enrol node 2                 Done
      0.2s +  97.2s  enrol node 3                 Done
     10.7s +  97.5s  start node 3                 Done
     10.7s +  97.5s  start node 2                 Done
      0.0s + 108.2s  form council quorum          Done
      9.2s + 108.2s  run hello through ingress    Done

timings saved to /private/tmp/rbq.atdfaT/clusters/laptop/timings.json
cluster laptop ready in 117.4s
  app: http://localhost:18080
  next: /tmp/rbq.atdfaT/bin/relish manual tour   (the five-minute tour of this cluster)
  or: /tmp/rbq.atdfaT/bin/relish status; /tmp/rbq.atdfaT/bin/relish logs hello; /tmp/rbq.atdfaT/bin/relish dashboard
  lifecycle: /tmp/rbq.atdfaT/bin/relish local status|stop|start|destroy --name laptop
```

<details><summary>timings.json</summary>

```json
{
  "schema": 1,
  "version": "0.1.0",
  "os": "macos",
  "arch": "aarch64",
  "nodes": 3,
  "development_binaries": false,
  "succeeded": true,
  "total_seconds": 117.415027167,
  "steps": [
    {
      "label": "check host",
      "stage": "host",
      "start_seconds": 0.000017583,
      "seconds": 0.00937725,
      "outcome": "done"
    },
    {
      "label": "install Lima 2.1.0",
      "stage": "download",
      "start_seconds": 0.009544417,
      "seconds": 7.204565208,
      "outcome": "done",
      "bytes": 37187062
    },
    {
      "label": "download guest image",
      "stage": "download",
      "start_seconds": 0.009624292,
      "seconds": 72.566088833,
      "outcome": "done",
      "bytes": 633581056
    },
    {
      "label": "download bun",
      "stage": "download",
      "start_seconds": 0.009947833,
      "seconds": 41.156456667,
      "outcome": "done",
      "bytes": 104598176
    },
    {
      "label": "download relish",
      "stage": "download",
      "start_seconds": 0.009949125,
      "seconds": 37.851760667,
      "outcome": "done",
      "bytes": 94558112
    },
    {
      "label": "boot VM 1",
      "stage": "boot",
      "start_seconds": 72.677386542,
      "seconds": 18.858508166,
      "outcome": "done"
    },
    {
      "label": "boot VM 2",
      "stage": "boot",
      "start_seconds": 73.183178375,
      "seconds": 19.832250583,
      "outcome": "done"
    },
    {
      "label": "boot VM 3",
      "stage": "boot",
      "start_seconds": 73.183786583,
      "seconds": 20.8683665,
      "outcome": "done"
    },
    {
      "label": "install files on node 1",
      "stage": "configure",
      "start_seconds": 94.060969792,
      "seconds": 0.82044025,
      "outcome": "done"
    },
    {
      "label": "start node 1",
      "stage": "configure",
      "start_seconds": 94.900806125,
      "seconds": 0.533093708,
      "outcome": "done"
    },
    {
      "label": "install files on node 2",
      "stage": "configure",
      "start_seconds": 95.433903375,
      "seconds": 1.751111917,
      "outcome": "done"
    },
    {
      "label": "install files on node 3",
      "stage": "configure",
      "start_seconds": 95.434014583,
      "seconds": 1.7546236670000002,
      "outcome": "done"
    },
    {
      "label": "enrol node 2",
      "stage": "configure",
      "start_seconds": 97.199060417,
      "seconds": 0.255803791,
      "outcome": "done"
    },
    {
      "label": "enrol node 3",
      "stage": "configure",
      "start_seconds": 97.211227042,
      "seconds": 0.243016791,
      "outcome": "done"
    },
    {
      "label": "start node 3",
      "stage": "configure",
      "start_seconds": 97.495112583,
      "seconds": 10.708472917,
      "outcome": "done"
    },
    {
      "label": "start node 2",
      "stage": "configure",
      "start_seconds": 97.504029375,
      "seconds": 10.699708542,
      "outcome": "done"
    },
    {
      "label": "form council quorum",
      "stage": "verify",
      "start_seconds": 108.203740708,
      "seconds": 0.031775334,
      "outcome": "done"
    },
    {
      "label": "run hello through ingress",
      "stage": "verify",
      "start_seconds": 108.235517042,
      "seconds": 9.16915725,
      "outcome": "done"
    }
  ]
}```

</details>

### Downloaded bytes

| File | SHA-256 | Candidate asset |
|---|---|---|
| `bin/relish` | `cd1ef247d8b3aff7e2db209b656b0ae39881e02870b21ebf6fb76c5ff355442f` | relish-macos-aarch64 |
| `cache/bun-v0.1.0-linux-aarch64` | `720d0c0efbfae28a69afb8385462e31656fd981ef89610207258c7c5c55d338a` | bun-linux-aarch64 |
| `cache/reliaburger-guest-ubuntu-24.04-20260911-aarch64.qcow2` | `c4a206d68d5c3794c4d1124b2ba6b88a042970839e6d1c3825b49a45e5294623` | reliaburger-guest-ubuntu-24.04-20260911-aarch64.qcow2 |
| `cache/relish-v0.1.0-linux-aarch64` | `f64142233e8ba1609a9d68e3e2defe9702e4fffd2a1a5828c2a8160891abc0b8` | relish-linux-aarch64 |

### Tour

| Command | Time | Exit |
|---|---|---|
| `relish apply -f /Users/miko/github/reliaburger/.claude/worktrees/dl/examples/kubernetes/podinfo.yaml` | 1 s | 0 |
| `relish status` | 0 s | 0 |
| `relish path frontend --to redis` | 1 s | 0 |
| `relish metrics frontend` | 0 s | 0 |

### Teardown

`relish local destroy --yes` and `relish uninstall --yes` succeeded; they left context.lock

Evidence (logs, timings, candidate record): `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-qualify.mHYyNN`

## Run 2

### Setup

- Host: `Darwin 25.3.0 arm64`
- macOS 26.3.1, Apple M2 Max, 32 GiB
- Staged base URL: <https://github.com/reliaburger/reliaburger/releases/download/staging-v0.1.0-36078958881-1>
- Bootstrap: `https://reliaburger.com/install.sh` (SHA-256 `085bf57e223799d28d89ef8fae50aac3c69931513facf15784bf20c7ca3a0ef2`)
- `candidate.json` SHA-256: `f0bc876a59595b96bc8e989ae3e85a1f599fe307abe8a9b62839fc05ed19e455` (required: `f0bc876a59595b96bc8e989ae3e85a1f599fe307abe8a9b62839fc05ed19e455`)
- Command: `curl -fsSL https://reliaburger.com/install.sh | RELIABURGER_RELEASE_BASE_URL=https://github.com/reliaburger/reliaburger/releases/download/staging-v0.1.0-36078958881-1 sh -s -- --timings`
- Relish: `relish 0.1.0`
- Finished every step

### Install

Wall time from the first `curl` to a ready cluster: 125 s.

```
where the time went (116.6s in total):
  host checks          0.0s
  downloads           73.9s  829.6 MiB at 11.2 MiB/s
  VM boot             19.1s
  node setup          14.3s
  cluster checks       9.3s

every step (duration, start offset):
      0.0s +   0.0s  check host                   Done
      4.3s +   0.0s  install Lima 2.1.0           Done
     73.9s +   0.0s  download guest image         Done
     30.3s +   0.0s  download bun                 Done
     31.8s +   0.0s  download relish              Done
     16.4s +  74.0s  boot VM 1                    Done
     18.8s +  74.2s  boot VM 2                    Done
     15.4s +  74.2s  boot VM 3                    Done
      0.8s +  93.1s  install files on node 1      Done
      0.6s +  93.9s  start node 1                 Done
      1.8s +  94.5s  install files on node 2      Done
      1.8s +  94.5s  install files on node 3      Done
      0.3s +  96.3s  enrol node 3                 Done
      0.2s +  96.3s  enrol node 2                 Done
     10.7s +  96.6s  start node 2                 Done
     10.7s +  96.6s  start node 3                 Done
      0.0s + 107.3s  form council quorum          Done
      9.2s + 107.4s  run hello through ingress    Done

timings saved to /private/tmp/rbq.aPmkHh/clusters/laptop/timings.json
cluster laptop ready in 116.6s
  app: http://localhost:18080
  next: /tmp/rbq.aPmkHh/bin/relish manual tour   (the five-minute tour of this cluster)
  or: /tmp/rbq.aPmkHh/bin/relish status; /tmp/rbq.aPmkHh/bin/relish logs hello; /tmp/rbq.aPmkHh/bin/relish dashboard
  lifecycle: /tmp/rbq.aPmkHh/bin/relish local status|stop|start|destroy --name laptop
```

<details><summary>timings.json</summary>

```json
{
  "schema": 1,
  "version": "0.1.0",
  "os": "macos",
  "arch": "aarch64",
  "nodes": 3,
  "development_binaries": false,
  "succeeded": true,
  "total_seconds": 116.599050666,
  "steps": [
    {
      "label": "check host",
      "stage": "host",
      "start_seconds": 0.000018541,
      "seconds": 0.009563667,
      "outcome": "done"
    },
    {
      "label": "install Lima 2.1.0",
      "stage": "download",
      "start_seconds": 0.009754958,
      "seconds": 4.327519,
      "outcome": "done",
      "bytes": 37187062
    },
    {
      "label": "download guest image",
      "stage": "download",
      "start_seconds": 0.009844583,
      "seconds": 73.875055417,
      "outcome": "done",
      "bytes": 633581056
    },
    {
      "label": "download bun",
      "stage": "download",
      "start_seconds": 0.010190625,
      "seconds": 30.30888375,
      "outcome": "done",
      "bytes": 104598176
    },
    {
      "label": "download relish",
      "stage": "download",
      "start_seconds": 0.010192125,
      "seconds": 31.824158541,
      "outcome": "done",
      "bytes": 94558112
    },
    {
      "label": "boot VM 1",
      "stage": "boot",
      "start_seconds": 73.979032708,
      "seconds": 16.433076833,
      "outcome": "done"
    },
    {
      "label": "boot VM 2",
      "stage": "boot",
      "start_seconds": 74.232631833,
      "seconds": 18.827201875,
      "outcome": "done"
    },
    {
      "label": "boot VM 3",
      "stage": "boot",
      "start_seconds": 74.2327925,
      "seconds": 15.403356208,
      "outcome": "done"
    },
    {
      "label": "install files on node 1",
      "stage": "configure",
      "start_seconds": 93.06969725,
      "seconds": 0.838067,
      "outcome": "done"
    },
    {
      "label": "start node 1",
      "stage": "configure",
      "start_seconds": 93.926758541,
      "seconds": 0.557745167,
      "outcome": "done"
    },
    {
      "label": "install files on node 2",
      "stage": "configure",
      "start_seconds": 94.484507125,
      "seconds": 1.791279208,
      "outcome": "done"
    },
    {
      "label": "install files on node 3",
      "stage": "configure",
      "start_seconds": 94.484616916,
      "seconds": 1.788153625,
      "outcome": "done"
    },
    {
      "label": "enrol node 3",
      "stage": "configure",
      "start_seconds": 96.285848125,
      "seconds": 0.320247041,
      "outcome": "done"
    },
    {
      "label": "enrol node 2",
      "stage": "configure",
      "start_seconds": 96.29775075,
      "seconds": 0.234490791,
      "outcome": "done"
    },
    {
      "label": "start node 2",
      "stage": "configure",
      "start_seconds": 96.599728041,
      "seconds": 10.735276167,
      "outcome": "done"
    },
    {
      "label": "start node 3",
      "stage": "configure",
      "start_seconds": 96.616683541,
      "seconds": 10.718673792,
      "outcome": "done"
    },
    {
      "label": "form council quorum",
      "stage": "verify",
      "start_seconds": 107.335359416,
      "seconds": 0.030190875,
      "outcome": "done"
    },
    {
      "label": "run hello through ingress",
      "stage": "verify",
      "start_seconds": 107.365550833,
      "seconds": 9.221525667,
      "outcome": "done"
    }
  ]
}```

</details>

### Downloaded bytes

| File | SHA-256 | Candidate asset |
|---|---|---|
| `bin/relish` | `cd1ef247d8b3aff7e2db209b656b0ae39881e02870b21ebf6fb76c5ff355442f` | relish-macos-aarch64 |
| `cache/bun-v0.1.0-linux-aarch64` | `720d0c0efbfae28a69afb8385462e31656fd981ef89610207258c7c5c55d338a` | bun-linux-aarch64 |
| `cache/reliaburger-guest-ubuntu-24.04-20260911-aarch64.qcow2` | `c4a206d68d5c3794c4d1124b2ba6b88a042970839e6d1c3825b49a45e5294623` | reliaburger-guest-ubuntu-24.04-20260911-aarch64.qcow2 |
| `cache/relish-v0.1.0-linux-aarch64` | `f64142233e8ba1609a9d68e3e2defe9702e4fffd2a1a5828c2a8160891abc0b8` | relish-linux-aarch64 |

### Tour

| Command | Time | Exit |
|---|---|---|
| `relish apply -f /Users/miko/github/reliaburger/.claude/worktrees/dl/examples/kubernetes/podinfo.yaml` | 0 s | 0 |
| `relish status` | 0 s | 0 |
| `relish path frontend --to redis` | 1 s | 0 |
| `relish metrics frontend` | 0 s | 0 |

### Teardown

`relish local destroy --yes` and `relish uninstall --yes` succeeded; they left context.lock

Evidence (logs, timings, candidate record): `/var/folders/2_/8114g2p575n5_8_3x1p7484c0000gn/T//reliaburger-qualify.urqwB6`

