# Sustained soak (V02): FAIL

15 October 2026. fast tier, compressed schedule, 1 h 30 min of soak on a three-node quickstart cluster.

**Fast tier: not clean.** Fix what failed and run the fast tier again before a final-tier run.

## Candidate and host

| | |
|---|---|
| Staged base URL | <https://github.com/reliaburger/reliaburger/releases/download/staging-v0.2.0-38674308323-1> |
| `candidate.json` SHA-256 | `a8188b1cb13a5ec798b05a5d338ccb6fa26d39253d465820d49f1ea8012a3e0a` |
| Running versions | v0.2.0 x3 |
| Soak build | `bun-v0.2.0-soak.1`, SHA-256 `8c6fb16b4e5c41db265201565fe3280fbd2726e15c132055c460fcdc35549d39` |
| Walks roll back to | 0.2.0 |
| Host | macOS 26.3.1, Apple M2 Max, 32 GiB |
| Lima | limactl version 2.1.0 |
| Guest | Ubuntu 24.04.5 LTS, kernel 6.8.0-139-generic |
| RELIABURGER_HOME | `/tmp/rbq.Qc3gTU` |
| Evidence | `/var/tmp/reliaburger-v02.gQKlwO` |

## Configuration deviations

- node.toml `[testing] allowed_operations` adds `provision_isolated_workloads` (quickstart allows only inject_workload_faults and alter_node_state), so the catalogue pulse can lease test namespaces
- node.toml `[logs]`/`[metrics]` export to file:///var/lib/reliaburger/soak-export every 60 s with max_storage_mb = 8, `[storage.snapshots]` every 900 s (compressed: 120 s), retain 4, uploaded to the same directory
- node.toml `[ingress] tls_cert/tls_key` point at an operator pair from a soak CA, 45-minute leaves rotated by the harness
- Bun runs with auto/mixed; host jobs opt in to a single pinned BusyBox copy under /var/lib/reliaburger/soak-jobs, mount_isolation=false. The independent verifier listens on each private VM address, TCP 8189, with a separate run token
- node.toml `[node.labels] soak-volume` = "writer" on node 2 and "redis" on node 3, to pin the volume apps
- node.toml `[upgrades] external_signing_key` = a throwaway operator key the harness made for this run (`ed25519:bV4bdZFGGWoGZ957fttwGbDinjcekOYXPLrz4+MvNt0=`); the soak build's `.sig` gets that key's external signature beside the release one (D2)
- node.toml `[security] leaf_lifetime_override_secs = 3600` (D1)

## Timeline

- Started 2026-10-15 06:25:18 UTC, finished 2026-10-15 07:55:36 UTC
- Cycles completed: 7
- Teardown: `relish local destroy --yes` and `relish uninstall --yes` succeeded

## Faults and slots

| Class | Runs | Settled/ok | Failed | Skipped | Settle (median / max) |
|---|---|---|---|---|---|
| fault:bun-kill-follower | 7 | 7 | 0 | 0 | 35 s / 65 s |
| fault:bun-kill-leader | 7 | 7 | 0 | 0 | 34 s / 41 s |
| fault:chaos-delay | 2 | 4 | 0 | 0 | 31 s / 31 s |
| fault:chaos-dns | 2 | 4 | 0 | 0 | 20 s / 20 s |
| fault:chaos-drop | 2 | 4 | 0 | 0 | 23 s / 23 s |
| fault:chaos-kill | 2 | 4 | 0 | 0 | 52 s / 52 s |
| fault:chaos-partition | 2 | 4 | 0 | 0 | 22 s / 22 s |
| fault:chaos-pause | 2 | 4 | 0 | 0 | 21 s / 21 s |
| fault:chaos-scenario | 2 | 4 | 0 | 0 | 35 s / 35 s |
| fault:deploy-kill | 4 | 4 | 0 | 0 | 40 s / 56 s |
| fault:node-delay | 3 | 3 | 0 | 0 | 35 s / 35 s |
| fault:node-disk-full | 2 | 2 | 0 | 0 | 20 s / 20 s |
| fault:node-slow-fsync | 2 | 2 | 0 | 0 | 44 s / 44 s |
| fault:power-off | 4 | 4 | 0 | 0 | 51 s / 113 s |
| pulse | 1 | 1 | 0 | 0 | n/a |
| pulse:settle | 0 | 1 | 0 | 0 | 21 s / 21 s |
| special:all-off | 1 | 1 | 0 | 0 | 70 s / 70 s |
| special:graceful-stop | 1 | 1 | 0 | 0 | 57 s / 57 s |
| special:quorum-back | 1 | 1 | 0 | 0 | n/a |
| special:quorum-loss | 1 | 1 | 0 | 0 | 50 s / 50 s |
| storage:offline-export | 3 | 3 | 0 | 0 | n/a |
| storage:registry-push | 7 | 0 | 0 | 0 | n/a |
| tls:invalid-expired | 3 | 3 | 0 | 0 | n/a |
| tls:invalid-torn | 6 | 6 | 0 | 0 | n/a |
| tls:reload | 43 | 43 | 0 | 0 | n/a |
| upgrade:rollback | 3 | 3 | 0 | 0 | n/a |
| upgrade:settle | 0 | 3 | 0 | 0 | 68 s / 81 s |
| upgrade:upgrade | 3 | 3 | 0 | 0 | n/a |

## Renewals and rotations

| Measure | Observed | Required | Verdict |
|---|---|---|---|
| workload identity rotations (soak-identity, per node) | 13 | ≥ 2 | PASS |
| node leaf renewals (per node) | 3 | ≥ 2 | PASS |
| operator ingress reloads (all nodes) | 43 | ≥ 17 | PASS |
| mixed job campaign and positive drain | 0 missing/failing checks | 0 | PASS |

## Jobs

| Runtime | Accepted successes | Retries | Last accepted progress |
|---|---|---|---|
| runc | 6452 | 2 | 1792050803 |
| shared-runc | 6775168 | 2 | 1792050803 |
| process | 19505002 | 1 | 1792050803 |

The bounded campaign shares the app soak's fault schedule. These counts are reliability evidence, not a saturation rate or a 100-million-per-day qualification.

- Independent verifier execution attempts: 24
- Explicit test-operator acknowledgements for known replay-safe fenced fixtures: 157

- rb-6643e3611537-1: latest complete job storage (KiB) {'instances': 3776, 'job-state': 8, 'task-arrays': 32860}; free 5206952 KiB; live/peak owners 0/31; peak storage {'instances': 33732, 'job-state': 8, 'task-arrays': 54632}
- rb-6643e3611537-2: latest complete job storage (KiB) {'instances': 3180, 'job-state': 8, 'task-arrays': 37124}; free 6316764 KiB; live/peak owners 0/31; peak storage {'instances': 25580, 'job-state': 8, 'task-arrays': 57292}
- rb-6643e3611537-3: latest complete job storage (KiB) {'instances': 12252, 'job-state': 8, 'task-arrays': 35424}; free 6288812 KiB; live/peak owners 0/30; peak storage {'instances': 32784, 'job-state': 8, 'task-arrays': 56896}

Complete disk inventories are recorded. Global retention bounds and the 12-hour job qualification remain #668; this gate cannot certify an unimplemented retention policy.

## Job qualification (D18-C)

Load: target; 3 nodes; accepted 4,962.5/s against a 1390.0/s target: PASS.

The September plan set its targets for one 1M-task batch on a quiet cluster. A soak runs for hours with restarts, upgrades and apps, so its storage, Bun memory and Raft targets are restated (maintainer decision, 15 October 2026, [#668](https://github.com/reliaburger/reliaburger/issues/668)). Bun's steady state is the median RSS of each process once it has been seen for 10 minutes; the gated memory checks are unchanged (`leak-rss`, and Mayo's 70% `memory_high` alert through `wtf`).

| Measure (recorded, not gated) | Observed | Target |
|---|---|---|
| Wall time of a 1,048,576-task array (median / max of 24) | 285 s / 874 s | ≤ 600 s |
| p99 start lag, pooled executors | 32 ms (bucket upper bound) | < 1,000 ms |
| Raft entries per second, all cluster writes | 8.68 | recorded only; the batch target counts only task-array writes |
| Raft state of one run, largest seen | 6,305 B | ≤ 256 KiB per 1M tasks |
| Bun steady-state RSS per process, worst node against its target | rb-6643e3611537-1: 382.24 MiB steady, 412.95 MiB peak | ≤ 512 MiB (≤ 512 MiB on node 1, which serves the API) |
| Bun CPU, busiest node | 0.43 cores | ≤ 1 core (leader batch control) |
| Executor helper CPU, busiest node | 0.03 cores | ≤ 0.5 core |
| Node task-array storage per 1M accepted tasks (peaks summed) | 6.27 MiB | ≤ 64 MiB, deleted after retirement |
| Retained log volume, all nodes | 9.24 MB | ≤ 50 MB |

- Bun RSS on rb-6643e3611537-1 (serves the API, target ≤ 512 MiB): pid 8133: 259.23 MiB peak (seen for under 10 minutes); pid 182563: 165.73 MiB peak (seen for under 10 minutes); pid 417436: 375.66 MiB steady, 375.66 MiB peak; pid 2220036: 260.52 MiB peak (seen for under 10 minutes); pid 671: 412.95 MiB peak (seen for under 10 minutes); pid 636696: 382.24 MiB steady, 382.24 MiB peak; pid 1960245: 199.08 MiB peak (seen for under 10 minutes); pid 1960245: 279.92 MiB peak (seen for under 10 minutes); pid 677: 402.26 MiB peak (seen for under 10 minutes); pid 455203: 316.49 MiB steady, 316.49 MiB peak; pid 1995956: 165.32 MiB peak (seen for under 10 minutes); pid 1995956: 272.50 MiB peak (seen for under 10 minutes); pid 3750754: 237.22 MiB peak (seen for under 10 minutes); pid 674: 266.02 MiB peak (seen for under 10 minutes)
- Bun RSS on rb-6643e3611537-2 (node, target ≤ 384 MiB): pid 2273: 250.55 MiB steady, 302.05 MiB peak; pid 2121382: 146.57 MiB peak (seen for under 10 minutes); pid 2121382: 187.99 MiB peak (seen for under 10 minutes); pid 671: 180.92 MiB peak (seen for under 10 minutes); pid 423790: 264.48 MiB peak (seen for under 10 minutes); pid 669: 216.63 MiB peak (seen for under 10 minutes); pid 671: 136.07 MiB peak (seen for under 10 minutes); pid 152335: 212.36 MiB peak (seen for under 10 minutes); pid 152335: 155.91 MiB peak (seen for under 10 minutes); pid 677: 144.01 MiB peak (seen for under 10 minutes); pid 211598: 196.51 MiB peak (seen for under 10 minutes); pid 675: 149.24 MiB peak (seen for under 10 minutes); pid 713433: 203.57 MiB peak (seen for under 10 minutes); pid 713433: 158.55 MiB peak (seen for under 10 minutes); pid 2476384: 181.96 MiB steady, 181.96 MiB peak
- Bun RSS on rb-6643e3611537-3 (node, target ≤ 384 MiB): pid 2137: 118.02 MiB peak (seen for under 10 minutes); pid 3593: 133.43 MiB peak (seen for under 10 minutes); pid 670: 150.77 MiB peak (seen for under 10 minutes); pid 1154158: 147.61 MiB peak (seen for under 10 minutes); pid 1154158: 158.06 MiB peak (seen for under 10 minutes); pid 675: 147.46 MiB peak (seen for under 10 minutes); pid 158902: 169.44 MiB steady, 169.44 MiB peak; pid 672: 150.85 MiB peak (seen for under 10 minutes); pid 673: 160.65 MiB peak (seen for under 10 minutes); pid 660: 158.27 MiB peak (seen for under 10 minutes); pid 722217: 162.15 MiB peak (seen for under 10 minutes); pid 674: 161.84 MiB peak (seen for under 10 minutes); pid 1765325: 193.72 MiB peak (seen for under 10 minutes)

- runc retry causes: exit_code 1, launch 1, signal 0, timeout 0, unknown 0, unrecorded 0
- shared-runc retry causes: exit_code 1, launch 1, signal 0, timeout 0, unknown 0, unrecorded 0
- process retry causes: exit_code 1, launch 0, signal 0, timeout 0, unknown 0, unrecorded 0
- Audited executions: 24; operator replay decisions: 157

## Data

- Volume writer: highest ACK 46625 in the log view; the writer file checks (writer-lost, writer-regression) decide data loss, and `*-log-order` failures are about the order the log view returned lines in
- Redis counter: highest INCR 4578 in the log view
- Export rb-6643e3611537-1: 137 source files, 128 at the destination
- Export rb-6643e3611537-2: 187 source files, 187 at the destination
- Export rb-6643e3611537-3: 178 source files, 175 at the destination
- Registry images pushed and re-verified: 8
- Agent-loop turns over the 1 s budget: rb-6643e3611537-1 4, rb-6643e3611537-2 11, rb-6643e3611537-3 0
- Guest clock steps by Lima's guest agent: rb-6643e3611537-1 4, rb-6643e3611537-2 6, rb-6643e3611537-3 7
- Checks evaluated: 261 (heavy 23, light 87, settle 151)
- Last observed progress: redis-counter 4578, writer-ack 46625

## Resource trends

| Node | Measure | First | Last | Max |
|---|---|---|---|---|
| rb-6643e3611537-1 | disk_kb data | 1370668 | 967584 | 1517428 |
| rb-6643e3611537-1 | disk_kb images | 328964 | 444696 | 444696 |
| rb-6643e3611537-1 | disk_kb logs | 44 | 2468 | 2468 |
| rb-6643e3611537-1 | disk_kb metrics | 88 | 5164 | 5164 |
| rb-6643e3611537-1 | disk_kb soak-export | 4 | 7304 | 7304 |
| rb-6643e3611537-1 | disk_kb volumes | 200 | 36 | 228 |
| rb-6643e3611537-1 | fd | 114 | 274 | 444 |
| rb-6643e3611537-1 | rss_kb | 148720 | 272404 | 422856 |
| rb-6643e3611537-2 | disk_kb data | 68612 | 292064 | 334736 |
| rb-6643e3611537-2 | disk_kb images | 158956 | 326024 | 326024 |
| rb-6643e3611537-2 | disk_kb logs | 28 | 1252 | 1252 |
| rb-6643e3611537-2 | disk_kb metrics | 48 | 5432 | 5432 |
| rb-6643e3611537-2 | disk_kb soak-export | 4 | 7440 | 7440 |
| rb-6643e3611537-2 | disk_kb volumes | 16736 | 17628 | 17628 |
| rb-6643e3611537-2 | fd | 59 | 113 | 314 |
| rb-6643e3611537-2 | rss_kb | 109424 | 186328 | 309296 |
| rb-6643e3611537-3 | disk_kb data | 231244 | 581580 | 673396 |
| rb-6643e3611537-3 | disk_kb images | 316524 | 319848 | 319848 |
| rb-6643e3611537-3 | disk_kb logs | 28 | 5300 | 5300 |
| rb-6643e3611537-3 | disk_kb metrics | 40 | 5784 | 5784 |
| rb-6643e3611537-3 | disk_kb soak-export | 4 | 15112 | 15112 |
| rb-6643e3611537-3 | disk_kb volumes | 16760 | 17224 | 17224 |
| rb-6643e3611537-3 | fd | 63 | 117 | 205 |
| rb-6643e3611537-3 | rss_kb | 110188 | 198372 | 198372 |

## Failures

| # | First seen | Check | Symptom | Class | Cause | Action |
|---|---|---|---|---|---|---|
| 1 | 07:04:07 | settle-check | fail: agent-loop-turn rb-6643e3611537-1: 4 follow_up turn(s) over the 1 s budget, the worst between 1 and 2.5 s; | unclassified | open | open |
| 2 | 07:43:53 | settle-check | fail: agent-loop-turn rb-6643e3611537-2: 11 follow_up turn(s) over the 1 s budget, the worst between 5 and 10 s; | unclassified | open | open |

Evidence: `/var/tmp/reliaburger-v02.gQKlwO` (events.jsonl, snapshots/, failures/<n>/).
