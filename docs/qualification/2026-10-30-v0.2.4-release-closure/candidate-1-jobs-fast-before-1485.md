# Sustained soak (V02): FAIL

29 October 2026. fast tier, compressed schedule, 1 h 04 min of soak on a three-node quickstart cluster.

**Fast tier: not clean.** Fix what failed and run the fast tier again before a final-tier run.

## Candidate and host

| | |
|---|---|
| Staged base URL | <https://github.com/reliaburger/reliaburger/releases/download/staging-v0.2.4-40870779000-1> |
| `candidate.json` SHA-256 | `340eaeba513c9740f058e51684713654f3df7c392ea8f19993ae1b5d12727d09` |
| Running versions | v0.2.4 x3 |
| Soak build | `bun-v0.2.4-soak.1`, SHA-256 `50d279fccf1b1abb1d6593b57f50f889f7aee6bb4f45acba30d218a004abf716` |
| Walks roll back to | 0.2.4 |
| Host | macOS 26.3.1, Apple M2 Max, 32 GiB |
| Lima | limactl version 2.2.1 |
| Guest | Ubuntu 24.04.5 LTS, kernel 6.8.0-139-generic |
| RELIABURGER_HOME | `/tmp/rbq.pB836R` |
| Evidence | `/var/tmp/reliaburger-v02.LHBWaw` |

## Configuration deviations

- node.toml `[testing] allowed_operations` adds `provision_isolated_workloads` (quickstart allows only inject_workload_faults and alter_node_state), so the catalogue pulse can lease test namespaces
- node.toml `[logs]`/`[metrics]` export to file:///var/lib/reliaburger/soak-export every 60 s with max_storage_mb = 8 (the harness deletes a metrics export 30 minutes after its source has gone), `[storage.snapshots]` every 900 s (compressed: 120 s), retain 4, uploaded to the same directory
- node.toml `[ingress] tls_cert/tls_key` point at an operator pair from a soak CA, 45-minute leaves rotated by the harness
- Bun runs with auto/mixed; host jobs opt in to a single pinned BusyBox copy under /var/lib/reliaburger/soak-jobs, mount_isolation=false. The independent verifier listens on each private VM address, TCP 8189, with a separate run token
- node.toml `[node.labels] soak-volume` = "writer" on node 2 and "redis" on node 3, to pin the volume apps
- node.toml `[upgrades] external_signing_key` = a throwaway operator key the harness made for this run (`ed25519:vZzHPX6Acr2PPf4uUwIIe+pc0tnYcLZ9tBmDUqzc3nc=`); the soak build's `.sig` gets that key's external signature beside the release one (D2)
- node.toml `[security] leaf_lifetime_override_secs = 3600` (D1)

## Timeline

- Started 2026-10-29 15:32:55 UTC, finished 2026-10-29 16:37:00 UTC
- Cycles completed: 5
- Fault and node choices: 27, from seed 515451119 (`--seed 515451119` repeats them)
- Teardown: `relish local destroy --yes` and `relish uninstall --yes` succeeded

## Faults and slots

| Class | Runs | Settled/ok | Failed | Skipped | Settle (median / max) |
|---|---|---|---|---|---|
| fault:bun-kill-follower | 5 | 5 | 0 | 0 | 34 s / 35 s |
| fault:bun-kill-leader | 5 | 5 | 0 | 0 | 35 s / 39 s |
| fault:chaos-cpu | 1 | 2 | 0 | 0 | 18 s / 18 s |
| fault:chaos-delay | 1 | 2 | 0 | 0 | 17 s / 17 s |
| fault:chaos-dns | 1 | 2 | 0 | 0 | 18 s / 18 s |
| fault:chaos-drop | 1 | 2 | 0 | 0 | 22 s / 22 s |
| fault:chaos-kill | 2 | 4 | 0 | 0 | 60 s / 60 s |
| fault:chaos-memory | 1 | 2 | 0 | 0 | 18 s / 18 s |
| fault:chaos-partition | 1 | 2 | 0 | 0 | 17 s / 17 s |
| fault:chaos-pause | 1 | 2 | 0 | 0 | 18 s / 18 s |
| fault:chaos-scenario | 1 | 2 | 0 | 0 | 31 s / 31 s |
| fault:deploy-kill | 3 | 3 | 0 | 0 | 35 s / 40 s |
| fault:node-delay | 2 | 2 | 0 | 0 | 32 s / 32 s |
| fault:node-disk-full | 2 | 2 | 0 | 0 | 18 s / 18 s |
| fault:node-slow-fsync | 1 | 1 | 0 | 0 | 18 s / 18 s |
| fault:power-off | 3 | 3 | 0 | 0 | 32 s / 45 s |
| pulse | 1 | 1 | 0 | 0 | n/a |
| pulse:settle | 0 | 1 | 0 | 0 | 24 s / 24 s |
| special:all-off | 1 | 1 | 0 | 0 | 47 s / 47 s |
| special:graceful-stop | 1 | 1 | 0 | 0 | 42 s / 42 s |
| special:quorum-back | 1 | 1 | 0 | 0 | n/a |
| special:quorum-loss | 1 | 1 | 0 | 0 | 32 s / 32 s |
| storage:offline-export | 2 | 2 | 0 | 0 | n/a |
| storage:registry-push | 5 | 0 | 0 | 0 | n/a |
| tls:invalid-expired | 3 | 3 | 0 | 0 | n/a |
| tls:invalid-torn | 3 | 3 | 0 | 0 | n/a |
| tls:reload | 29 | 29 | 0 | 0 | n/a |
| upgrade:rollback | 2 | 2 | 0 | 0 | n/a |
| upgrade:settle | 0 | 2 | 0 | 0 | 62 s / 62 s |
| upgrade:upgrade | 2 | 2 | 0 | 0 | n/a |

## Execution inventory (recorded, not gated)

What the run completed, from its events and kept bytes, against what its schedule owes. A fault counts once it was injected and the cluster settled after it.

| Item | Required | Completed | Verdict |
|---|---|---|---|
| candidate.json bytes match the qualified digest | 1 | 1 | ok |
| app workloads settled at the baseline | 1 | 1 | ok |
| upgrade walk (to the soak build and back) | 2 | 2 | ok |
| fault:bun-kill-follower | 1 | 5 | ok |
| fault:bun-kill-leader | 1 | 5 | ok |
| fault:deploy-kill | 1 | 3 | ok |
| fault:power-off | 1 | 3 | ok |
| fault:chaos-kill | 1 | 2 | ok |
| fault:chaos-delay | 1 | 1 | ok |
| fault:chaos-partition | 1 | 1 | ok |
| fault:chaos-scenario | 1 | 1 | ok |
| fault:chaos-pause | 1 | 1 | ok |
| fault:chaos-drop | 1 | 1 | ok |
| fault:chaos-dns | 1 | 1 | ok |
| fault:chaos-cpu | 1 | 1 | ok |
| fault:node-delay | 1 | 2 | ok |
| fault:node-disk-full | 1 | 2 | ok |
| fault:node-slow-fsync | 1 | 1 | ok |
| special:graceful-stop | 1 | 1 | ok |
| special:quorum-loss | 1 | 1 | ok |
| special:all-off | 1 | 1 | ok |
| catalogue pulse | 1 | 1 | ok |
| offline export | 2 | 2 | ok |
| job campaign drained in its final two minutes | 1 | 1 | ok |

## Observed fault effects

What each injected fault was seen to do, read back from the node.

| Fault | Target | Observed |
|---|---|---|
| effect:bun-kill-follower | rb-8571059eb74e-3 | MainPID 2236 -> 3884 |
| effect:bun-kill-leader | rb-8571059eb74e-1 | MainPID 9557 -> 250838 |
| effect:deploy-kill | rb-8571059eb74e-3 | MainPID 3884 -> 618393 |
| effect:power-off | rb-8571059eb74e-2 | Lima status Stopped |
| effect:node-delay | rb-8571059eb74e-1 | netem 8001: root refcnt 2 limit 1000 delay 200ms  50ms  |
| effect:bun-kill-follower | rb-8571059eb74e-1 | MainPID 250838 -> 2065480 |
| effect:bun-kill-leader | rb-8571059eb74e-2 | MainPID 678 -> 1423872 |
| effect:node-disk-full | rb-8571059eb74e-2 | available 67063808 bytes  |
| effect:bun-kill-follower | rb-8571059eb74e-1 | MainPID 675 -> 164020 |
| effect:bun-kill-leader | rb-8571059eb74e-3 | MainPID 675 -> 495494 |
| effect:deploy-kill | rb-8571059eb74e-2 | MainPID 675 -> 836582 |
| effect:power-off | rb-8571059eb74e-2 | Lima status Stopped |
| effect:node-slow-fsync | rb-8571059eb74e-3 | IOWriteBandwidthMax=/dev/vda 1000000 IOWriteIOPSMax=/dev/vda 20  |
| effect:bun-kill-follower | rb-8571059eb74e-3 | MainPID 676 -> 126909 |
| effect:bun-kill-leader | rb-8571059eb74e-1 | MainPID 164020 -> 2722302 |
| effect:node-delay | rb-8571059eb74e-2 | netem 8001: root refcnt 2 limit 1000 delay 200ms  50ms  |
| effect:bun-kill-follower | rb-8571059eb74e-1 | MainPID 674 -> 180420 |
| effect:bun-kill-leader | rb-8571059eb74e-2 | MainPID 678 -> 503024 |
| effect:deploy-kill | rb-8571059eb74e-1 | MainPID 180420 -> 866297 |
| effect:power-off | rb-8571059eb74e-3 | Lima status Stopped |
| effect:node-disk-full | rb-8571059eb74e-3 | available 67117056 bytes  |

## Renewals and rotations

| Measure | Observed | Required | Verdict |
|---|---|---|---|
| workload identity rotations (soak-identity, per node) | 7 | ≥ 1 | PASS |
| node leaf renewals (per node) | 2 | ≥ 1 | PASS |
| operator ingress reloads (all nodes) | 29 | ≥ 11 | PASS |
| mixed job campaign and positive drain | 0 missing/failing checks | 0 | PASS |

## Latency SLOs

What a user should feel on a quiet three-node cluster (#1400). Every sample is taken outside fault windows and through a host forward that works; each p99 is the upper bound of its histogram bucket.

| SLO | p99 | Threshold | Samples | Mode | Verdict |
|---|---|---|---|---|---|
| API read p99 | 150 ms | ≤ 1,000 ms | 234 | record only | PASS |
| API write p99 | 150 ms | ≤ 2,000 ms | 7 | record only | PASS |
| App start p99 | 1 ms | ≤ 5,000 ms | 21 | record only | PASS |
| Rolling update p99 | 30,000 ms | ≤ 30,000 ms | 7 | record only | PASS |
| Job task start p99 | 2,048 ms | ≤ 256 ms | 20 | gated | FAIL |

- API read p99: `GET /v1/status?cluster=true` and `GET /v1/cluster/nodes` on every node, at every light check
- API write p99: `relish apply` of the soak workloads, from the call to the commit
- App start p99: each node's deploy of a new `soak-identity` instance, from its start to the instance running
- Rolling update p99: from `relish apply` returning to every node's `soak-identity` running the new spec
- Job task start p99: a job task's start lag on a pooled executor, from the job campaign's buckets, in runs no fault window overlapped
- Record only from 28 October 2026 (#1400): no soak before 0.2.4 measured it. The threshold is provisional; the first 0.2.4 fast and final records set the gate.
- **SLO breached: job-start: job task start p99 2,048 ms is over the 256 ms SLO**

## Jobs

| Runtime | Accepted successes | Retries | Last accepted progress |
|---|---|---|---|
| runc | 10594 | 3 | 1793291809 |
| shared-runc | 6639124 | 1 | 1793291806 |
| process | 18170818 | 1 | 1793291806 |

The bounded campaign shares the app soak's fault schedule. These counts are reliability evidence, not a saturation rate or a 100-million-per-day qualification.

- Independent verifier execution attempts: 24
- Explicit test-operator acknowledgements for known replay-safe fenced fixtures: 112

- rb-8571059eb74e-1: latest complete job storage (KiB) {'instances': 3156, 'job-state': 8, 'task-arrays': 15556}; free 5180628 KiB; live/peak owners 0/30; peak storage {'instances': 24740, 'job-state': 8, 'task-arrays': 40340}
- rb-8571059eb74e-2: latest complete job storage (KiB) {'instances': 896, 'job-state': 8, 'task-arrays': 15952}; free 6297216 KiB; live/peak owners 0/32; peak storage {'instances': 24244, 'job-state': 8, 'task-arrays': 40712}
- rb-8571059eb74e-3: latest complete job storage (KiB) {'instances': 432, 'job-state': 8, 'task-arrays': 15500}; free 6327548 KiB; live/peak owners 0/32; peak storage {'instances': 24136, 'job-state': 8, 'task-arrays': 43248}

Complete disk inventories are recorded. Global retention bounds and the 12-hour job qualification remain #668; this gate cannot certify an unimplemented retention policy.

## Job qualification (D18-C)

Load: target; 3 nodes; accepted 6,467.0/s against a 1390.0/s target: PASS.

The September plan set its targets for one 1M-task batch on a quiet cluster. A soak runs for hours with restarts, upgrades and apps, so its storage, Bun memory and Raft targets are restated (maintainer decision, 15 October 2026, [#668](https://github.com/reliaburger/reliaburger/issues/668)). Bun's steady state is the median RSS of each process once it has been seen for 10 minutes; the gated memory checks are unchanged (`leak-rss`, and Mayo's 70% `memory_high` alert through `wtf`).

| Measure (recorded, not gated) | Observed | Target |
|---|---|---|
| Wall time of a 1,048,576-task array (median / max of 23) | 211 s / 670 s | ≤ 600 s |
| p99 start lag, pooled executors | 32 ms (bucket upper bound) | < 1,000 ms |
| Raft entries per second, all cluster writes | 5.50 | recorded only; the batch target counts only task-array writes |
| Raft state of one run, largest seen | 10,010 B | ≤ 256 KiB per 1M tasks |
| Bun steady-state RSS per process, worst node against its target | rb-8571059eb74e-3: 158.20 MiB steady, 189.74 MiB peak | ≤ 384 MiB (≤ 512 MiB on node 1, which serves the API) |
| Bun CPU, busiest node | 0.29 cores | ≤ 1 core (leader batch control) |
| Executor helper CPU, busiest node | 0.09 cores | ≤ 0.5 core |
| Node task-array storage per 1M accepted tasks (peaks summed) | 4.89 MiB | ≤ 64 MiB, deleted after retirement |
| Retained log volume, all nodes | 6.33 MB | ≤ 50 MB |

- Bun RSS on rb-8571059eb74e-1 (serves the API, target ≤ 512 MiB): pid 9557: 197.45 MiB peak (seen for under 10 minutes); pid 250838: 199.85 MiB peak (seen for under 10 minutes); pid 2065480: 178.85 MiB peak (seen for under 10 minutes); pid 2065480: 186.23 MiB peak (seen for under 10 minutes); pid 675: 170.54 MiB peak (seen for under 10 minutes); pid 164020: 193.11 MiB steady, 193.67 MiB peak; pid 2722302: 184.41 MiB peak (seen for under 10 minutes); pid 2722302: 188.95 MiB peak (seen for under 10 minutes); pid 674: 189.72 MiB peak (seen for under 10 minutes); pid 180420: 185.41 MiB peak (seen for under 10 minutes); pid 866297: 188.04 MiB peak (seen for under 10 minutes)
- Bun RSS on rb-8571059eb74e-2 (node, target ≤ 384 MiB): pid 2523: 163.32 MiB peak (seen for under 10 minutes); pid 678: 197.18 MiB peak (seen for under 10 minutes); pid 1423872: 141.20 MiB peak (seen for under 10 minutes); pid 1890488: 168.17 MiB peak (seen for under 10 minutes); pid 675: 174.56 MiB peak (seen for under 10 minutes); pid 836582: 143.22 MiB peak (seen for under 10 minutes); pid 683: 159.52 MiB peak (seen for under 10 minutes); pid 676: 169.39 MiB peak (seen for under 10 minutes); pid 676: 163.36 MiB peak (seen for under 10 minutes); pid 678: 188.04 MiB peak (seen for under 10 minutes); pid 503024: 188.47 MiB peak (seen for under 10 minutes)
- Bun RSS on rb-8571059eb74e-3 (node, target ≤ 384 MiB): pid 2236: 123.37 MiB peak (seen for under 10 minutes); pid 3884: 135.77 MiB peak (seen for under 10 minutes); pid 618393: 158.20 MiB steady, 158.20 MiB peak; pid 618393: 162.95 MiB peak (seen for under 10 minutes); pid 675: 159.23 MiB peak (seen for under 10 minutes); pid 495494: 189.74 MiB peak (seen for under 10 minutes); pid 676: 138.05 MiB peak (seen for under 10 minutes); pid 126909: 153.63 MiB peak (seen for under 10 minutes); pid 675: 150.25 MiB peak (seen for under 10 minutes); pid 678: 160.85 MiB peak (seen for under 10 minutes); pid 663: 153.77 MiB peak (seen for under 10 minutes)

- runc retry causes: exit_code 1, launch 1, signal 0, timeout 0, unknown 1, unrecorded 0
- shared-runc retry causes: exit_code 1, launch 0, signal 0, timeout 0, unknown 0, unrecorded 0
- process retry causes: exit_code 1, launch 0, signal 0, timeout 0, unknown 0, unrecorded 0
- Audited executions: 24; operator replay decisions: 112
- Job controller API fallbacks: rb-8571059eb74e-1 missed 219, rb-8571059eb74e-2 missed 2; rb-8571059eb74e-2 answered 217, rb-8571059eb74e-3 answered 2 (requests a node didn't answer, and the node that took them instead)

## Data

- Volume writer: highest ACK 31587 in the log view; the writer file checks (writer-lost, writer-regression) decide data loss, and `*-log-order` failures are about the order the log view returned lines in
- Redis counter: highest INCR 3203 in the log view
- Export rb-8571059eb74e-1: 124 source files, 122 at the destination
- Export rb-8571059eb74e-2: 128 source files, 124 at the destination
- Export rb-8571059eb74e-3: 101 source files, 95 at the destination
- Registry images pushed and re-verified: 6
- Agent-loop turns over the 1 s budget: rb-8571059eb74e-1 0, rb-8571059eb74e-2 0, rb-8571059eb74e-3 0
- Soak policies checked in force: 119 times, policy generation 36 at the last check
- Guest clock steps by Lima's guest agent: rb-8571059eb74e-1 3, rb-8571059eb74e-2 5, rb-8571059eb74e-3 3
- Checks evaluated: 178 (heavy 16, light 59, settle 103)
- Last observed progress: redis-counter 3203, writer-ack 31587

## Council history (linearizability)

Every test-lease grant, renew, read and release the history client made, checked against a model of the council's leases. A write with no answer may or may not have happened; the check allows both.

- Operations checked: 21138 of 21138, in 2 namespaces; 1386 writes had no answer
- Verdict: linearizable

## Leak-checker exposure (recorded, not gated)

How long each leak checker could judge each node's Bun. `leak-rss` arms after 60 minutes in one process and `leak-fd` after 50; a restart, kill or upgrade exec starts the warm-up again. A final needs each armed for 60 minutes on every node, with one unbroken window of 30 (#1062); its quiet tail gives them that.

| Node | Checker | Armed | Longest armed window | Evaluated samples | Processes armed | Verdict |
|---|---|---|---|---|---|---|
| rb-8571059eb74e-1 | leak-rss | 0 min | 0 min | 0 | 0 | short |
| rb-8571059eb74e-1 | leak-fd | 0 min | 0 min | 0 | 0 | short |
| rb-8571059eb74e-2 | leak-rss | 0 min | 0 min | 0 | 0 | short |
| rb-8571059eb74e-2 | leak-fd | 0 min | 0 min | 0 | 0 | short |
| rb-8571059eb74e-3 | leak-rss | 0 min | 0 min | 0 | 0 | short |
| rb-8571059eb74e-3 | leak-fd | 0 min | 0 min | 0 | 0 | short |

## Resource trends

| Node | Measure | First | Last | Max |
|---|---|---|---|---|
| rb-8571059eb74e-1 | disk_kb data | 1382816 | 1306136 | 1510100 |
| rb-8571059eb74e-1 | disk_kb images | 342456 | 461428 | 461428 |
| rb-8571059eb74e-1 | disk_kb logs | 44 | 3688 | 3688 |
| rb-8571059eb74e-1 | disk_kb metrics | 56 | 4564 | 4564 |
| rb-8571059eb74e-1 | disk_kb soak-export | 4 | 8112 | 8112 |
| rb-8571059eb74e-1 | disk_kb volumes | 224 | 204 | 252 |
| rb-8571059eb74e-1 | fd | 111 | 129 | 235 |
| rb-8571059eb74e-1 | rss_anon_kb | 79920 | 114756 | 126676 |
| rb-8571059eb74e-1 | rss_file_kb | 72664 | 77792 | 77968 |
| rb-8571059eb74e-1 | rss_kb | 152584 | 192548 | 204644 |
| rb-8571059eb74e-2 | disk_kb data | 71796 | 279024 | 492896 |
| rb-8571059eb74e-2 | disk_kb images | 175368 | 342448 | 342448 |
| rb-8571059eb74e-2 | disk_kb logs | 32 | 1668 | 1668 |
| rb-8571059eb74e-2 | disk_kb metrics | 60 | 3000 | 3000 |
| rb-8571059eb74e-2 | disk_kb soak-export | 4 | 4836 | 4836 |
| rb-8571059eb74e-2 | disk_kb volumes | 16760 | 17388 | 17388 |
| rb-8571059eb74e-2 | fd | 68 | 104 | 196 |
| rb-8571059eb74e-2 | rss_anon_kb | 42872 | 115264 | 124860 |
| rb-8571059eb74e-2 | rss_file_kb | 71068 | 77728 | 77728 |
| rb-8571059eb74e-2 | rss_kb | 113940 | 192992 | 201912 |
| rb-8571059eb74e-3 | disk_kb data | 234544 | 208612 | 595408 |
| rb-8571059eb74e-3 | disk_kb images | 339124 | 342508 | 342508 |
| rb-8571059eb74e-3 | disk_kb logs | 28 | 824 | 824 |
| rb-8571059eb74e-3 | disk_kb metrics | 60 | 3292 | 3292 |
| rb-8571059eb74e-3 | disk_kb soak-export | 4 | 4724 | 4724 |
| rb-8571059eb74e-3 | disk_kb volumes | 16784 | 17024 | 17076 |
| rb-8571059eb74e-3 | fd | 75 | 90 | 194 |
| rb-8571059eb74e-3 | rss_anon_kb | 48020 | 81040 | 116384 |
| rb-8571059eb74e-3 | rss_file_kb | 70752 | 76416 | 77908 |
| rb-8571059eb74e-3 | rss_kb | 118772 | 157456 | 194292 |

## Charts

Time runs from the start of the soak. The marks under each chart are its faults, upgrade walks, specials and recorded failures; the dashed lines are targets.

![Bun resident memory per node](jobs-fast-charts/rss.svg)

![Bun open file descriptors per node](jobs-fast-charts/fds.svg)

![Bun CPU per node](jobs-fast-charts/cpu.svg)

![Accepted jobs a second](jobs-fast-charts/jobs.svg)

## Failures

None.

Evidence: `/var/tmp/reliaburger-v02.LHBWaw` (events.jsonl, snapshots/, failures/<n>/).
