# Sustained soak (V02): FAIL

15 October 2026. fast tier, compressed schedule, 1 h 32 min of soak on a three-node quickstart cluster.

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
| RELIABURGER_HOME | `/tmp/rbq.vqVlou` |
| Evidence | `/var/tmp/reliaburger-v02.TT9vq8` |

## Configuration deviations

- node.toml `[testing] allowed_operations` adds `provision_isolated_workloads` (quickstart allows only inject_workload_faults and alter_node_state), so the catalogue pulse can lease test namespaces
- node.toml `[logs]`/`[metrics]` export to file:///var/lib/reliaburger/soak-export every 60 s with max_storage_mb = 8, `[storage.snapshots]` every 900 s (compressed: 120 s), retain 4, uploaded to the same directory
- node.toml `[ingress] tls_cert/tls_key` point at an operator pair from a soak CA, 45-minute leaves rotated by the harness
- Bun runs with auto/mixed; host jobs opt in to a single pinned BusyBox copy under /var/lib/reliaburger/soak-jobs, mount_isolation=false. The independent verifier listens on each private VM address, TCP 8189, with a separate run token
- node.toml `[node.labels] soak-volume` = "writer" on node 2 and "redis" on node 3, to pin the volume apps
- node.toml `[upgrades] external_signing_key` = a throwaway operator key the harness made for this run (`ed25519:a3rszNZTGHeYWsIFOSGzJEYMMnTiKMZ+4Q4z4wOK1K4=`); the soak build's `.sig` gets that key's external signature beside the release one (D2)
- node.toml `[security] leaf_lifetime_override_secs = 3600` (D1)

## Timeline

- Started 2026-10-15 16:11:37 UTC, finished 2026-10-15 17:43:44 UTC
- Cycles completed: 7
- Teardown: `relish local destroy --yes` and `relish uninstall --yes` succeeded

## Faults and slots

| Class | Runs | Settled/ok | Failed | Skipped | Settle (median / max) |
|---|---|---|---|---|---|
| fault:bun-kill-follower | 7 | 7 | 0 | 0 | 37 s / 52 s |
| fault:bun-kill-leader | 7 | 7 | 0 | 0 | 34 s / 54 s |
| fault:chaos-delay | 2 | 4 | 0 | 0 | 32 s / 32 s |
| fault:chaos-dns | 2 | 4 | 0 | 0 | 20 s / 20 s |
| fault:chaos-drop | 2 | 4 | 0 | 0 | 21 s / 21 s |
| fault:chaos-kill | 2 | 4 | 0 | 0 | 66 s / 66 s |
| fault:chaos-partition | 2 | 4 | 0 | 0 | 22 s / 22 s |
| fault:chaos-pause | 2 | 4 | 0 | 0 | 22 s / 22 s |
| fault:chaos-scenario | 2 | 4 | 0 | 0 | 54 s / 54 s |
| fault:deploy-kill | 4 | 4 | 0 | 0 | 37 s / 43 s |
| fault:node-delay | 3 | 3 | 0 | 0 | 20 s / 37 s |
| fault:node-disk-full | 2 | 2 | 0 | 0 | 29 s / 29 s |
| fault:node-slow-fsync | 2 | 2 | 0 | 0 | 21 s / 21 s |
| fault:power-off | 4 | 4 | 0 | 0 | 37 s / 42 s |
| pulse | 1 | 1 | 0 | 0 | n/a |
| pulse:settle | 0 | 1 | 0 | 0 | 25 s / 25 s |
| special:all-off | 1 | 1 | 0 | 0 | 77 s / 77 s |
| special:graceful-stop | 1 | 1 | 0 | 0 | 83 s / 83 s |
| special:quorum-back | 1 | 1 | 0 | 0 | n/a |
| special:quorum-loss | 1 | 1 | 0 | 0 | 52 s / 52 s |
| storage:offline-export | 3 | 3 | 0 | 0 | n/a |
| storage:registry-push | 7 | 0 | 0 | 0 | n/a |
| tls:invalid-expired | 3 | 3 | 0 | 0 | n/a |
| tls:invalid-torn | 6 | 6 | 0 | 0 | n/a |
| tls:reload | 41 | 41 | 0 | 0 | n/a |
| upgrade:rollback | 3 | 3 | 0 | 0 | n/a |
| upgrade:settle | 0 | 3 | 0 | 0 | 81 s / 85 s |
| upgrade:upgrade | 3 | 3 | 0 | 0 | n/a |

## Renewals and rotations

| Measure | Observed | Required | Verdict |
|---|---|---|---|
| workload identity rotations (soak-identity, per node) | 12 | ≥ 2 | PASS |
| node leaf renewals (per node) | 3 | ≥ 2 | PASS |
| operator ingress reloads (all nodes) | 41 | ≥ 17 | PASS |
| mixed job campaign and positive drain | 0 missing/failing checks | 0 | PASS |

## Jobs

| Runtime | Accepted successes | Retries | Last accepted progress |
|---|---|---|---|
| runc | 6066 | 1 | 1792085982 |
| shared-runc | 6358159 | 2 | 1792085982 |
| process | 19280546 | 1 | 1792085982 |

The bounded campaign shares the app soak's fault schedule. These counts are reliability evidence, not a saturation rate or a 100-million-per-day qualification.

- Independent verifier execution attempts: 24
- Explicit test-operator acknowledgements for known replay-safe fenced fixtures: 168

- rb-668588ef706d-1: latest complete job storage (KiB) {'instances': 4800, 'job-state': 8, 'task-arrays': 38272}; free 5197528 KiB; live/peak owners 0/32; peak storage {'instances': 37640, 'job-state': 8, 'task-arrays': 60316}
- rb-668588ef706d-2: latest complete job storage (KiB) {'instances': 10916, 'job-state': 8, 'task-arrays': 41956}; free 6338112 KiB; live/peak owners 0/30; peak storage {'instances': 29356, 'job-state': 8, 'task-arrays': 60884}
- rb-668588ef706d-3: latest complete job storage (KiB) {'instances': 2324, 'job-state': 8, 'task-arrays': 44816}; free 6293488 KiB; live/peak owners 0/31; peak storage {'instances': 26140, 'job-state': 8, 'task-arrays': 61444}

Complete disk inventories are recorded. Global retention bounds and the 12-hour job qualification remain #668; this gate cannot certify an unimplemented retention policy.

## Job qualification (D18-C)

Load: target; 3 nodes; accepted 4,842.3/s against a 1390.0/s target: PASS.

The September plan set its targets for one 1M-task batch on a quiet cluster. A soak runs for hours with restarts, upgrades and apps, so its storage, Bun memory and Raft targets are restated (maintainer decision, 15 October 2026, [#668](https://github.com/reliaburger/reliaburger/issues/668)). Bun's steady state is the median RSS of each process once it has been seen for 10 minutes; the gated memory checks are unchanged (`leak-rss`, and Mayo's 70% `memory_high` alert through `wtf`).

| Measure (recorded, not gated) | Observed | Target |
|---|---|---|
| Wall time of a 1,048,576-task array (median / max of 24) | 284 s / 937 s | ≤ 600 s |
| p99 start lag, pooled executors | 32 ms (bucket upper bound) | < 1,000 ms |
| Raft entries per second, all cluster writes | 8.62 | recorded only; the batch target counts only task-array writes |
| Raft state of one run, largest seen | 6,335 B | ≤ 256 KiB per 1M tasks |
| Bun steady-state RSS per process, worst node against its target | rb-668588ef706d-1: 387.21 MiB steady, 406.03 MiB peak | ≤ 512 MiB (≤ 512 MiB on node 1, which serves the API) |
| Bun CPU, busiest node | 0.41 cores | ≤ 1 core (leader batch control) |
| Executor helper CPU, busiest node | 0.03 cores | ≤ 0.5 core |
| Node task-array storage per 1M accepted tasks (peaks summed) | 6.96 MiB | ≤ 64 MiB, deleted after retirement |
| Retained log volume, all nodes | 9.73 MB | ≤ 50 MB |

- Bun RSS on rb-668588ef706d-1 (serves the API, target ≤ 512 MiB): pid 7647: 251.04 MiB peak (seen for under 10 minutes); pid 157999: 387.21 MiB steady, 406.03 MiB peak; pid 1970305: 274.91 MiB peak (seen for under 10 minutes); pid 670: 398.66 MiB peak (seen for under 10 minutes); pid 739925: 310.21 MiB steady, 323.48 MiB peak; pid 2163612: 163.05 MiB peak (seen for under 10 minutes); pid 2163612: 291.08 MiB peak (seen for under 10 minutes); pid 674: 371.41 MiB steady, 371.54 MiB peak; pid 1924116: 172.37 MiB peak (seen for under 10 minutes); pid 1924116: 299.30 MiB peak (seen for under 10 minutes); pid 3733128: 254.74 MiB peak (seen for under 10 minutes); pid 675: 278.16 MiB peak (seen for under 10 minutes)
- Bun RSS on rb-668588ef706d-2 (node, target ≤ 384 MiB): pid 2284: 186.71 MiB peak (seen for under 10 minutes); pid 403870: 245.52 MiB peak (seen for under 10 minutes); pid 1946696: 144.36 MiB peak (seen for under 10 minutes); pid 1946696: 204.53 MiB peak (seen for under 10 minutes); pid 668: 186.32 MiB peak (seen for under 10 minutes); pid 471404: 252.81 MiB peak (seen for under 10 minutes); pid 672: 186.88 MiB peak (seen for under 10 minutes); pid 300823: 208.33 MiB peak (seen for under 10 minutes); pid 300823: 151.32 MiB peak (seen for under 10 minutes); pid 672: 196.57 MiB peak (seen for under 10 minutes); pid 435824: 188.93 MiB peak (seen for under 10 minutes); pid 675: 200.84 MiB peak (seen for under 10 minutes); pid 675: 164.02 MiB peak (seen for under 10 minutes); pid 2471959: 212.23 MiB peak (seen for under 10 minutes); pid 2996730: 221.60 MiB peak (seen for under 10 minutes)
- Bun RSS on rb-668588ef706d-3 (node, target ≤ 384 MiB): pid 2141: 116.04 MiB peak (seen for under 10 minutes); pid 3500: 133.46 MiB peak (seen for under 10 minutes); pid 667: 149.49 MiB peak (seen for under 10 minutes); pid 971451: 140.62 MiB peak (seen for under 10 minutes); pid 971451: 153.27 MiB peak (seen for under 10 minutes); pid 678: 138.47 MiB peak (seen for under 10 minutes); pid 245652: 150.54 MiB peak (seen for under 10 minutes); pid 675: 149.99 MiB peak (seen for under 10 minutes); pid 674: 147.59 MiB peak (seen for under 10 minutes); pid 662: 157.17 MiB peak (seen for under 10 minutes); pid 675: 148.61 MiB peak (seen for under 10 minutes); pid 197973: 144.05 MiB peak (seen for under 10 minutes); pid 672900: 149.11 MiB peak (seen for under 10 minutes); pid 1712127: 149.95 MiB peak (seen for under 10 minutes); pid 673: 162.85 MiB steady, 200.17 MiB peak

- runc retry causes: exit_code 1, launch 0, signal 0, timeout 0, unknown 0, unrecorded 0
- shared-runc retry causes: exit_code 1, launch 1, signal 0, timeout 0, unknown 0, unrecorded 0
- process retry causes: exit_code 1, launch 0, signal 0, timeout 0, unknown 0, unrecorded 0
- Audited executions: 24; operator replay decisions: 168

## Data

- Volume writer: highest ACK 47123 in the log view; the writer file checks (writer-lost, writer-regression) decide data loss, and `*-log-order` failures are about the order the log view returned lines in
- Redis counter: highest INCR 4510 in the log view
- Export rb-668588ef706d-1: 174 source files, 165 at the destination
- Export rb-668588ef706d-2: 192 source files, 190 at the destination
- Export rb-668588ef706d-3: 147 source files, 145 at the destination
- Registry images pushed and re-verified: 8
- Agent-loop turns over the 1 s budget: rb-668588ef706d-1 0, rb-668588ef706d-2 0, rb-668588ef706d-3 0
- Guest clock steps by Lima's guest agent: rb-668588ef706d-1 3, rb-668588ef706d-2 4, rb-668588ef706d-3 7
- Checks evaluated: 253 (heavy 21, light 88, settle 144)
- Last observed progress: redis-counter 4510, writer-ack 47123

## Resource trends

| Node | Measure | First | Last | Max |
|---|---|---|---|---|
| rb-668588ef706d-1 | disk_kb data | 1370628 | 969592 | 1520612 |
| rb-668588ef706d-1 | disk_kb images | 328964 | 444696 | 444696 |
| rb-668588ef706d-1 | disk_kb logs | 36 | 5464 | 5464 |
| rb-668588ef706d-1 | disk_kb metrics | 84 | 5844 | 5844 |
| rb-668588ef706d-1 | disk_kb soak-export | 4 | 11052 | 11052 |
| rb-668588ef706d-1 | disk_kb volumes | 200 | 36 | 228 |
| rb-668588ef706d-1 | fd | 119 | 271 | 440 |
| rb-668588ef706d-1 | rss_kb | 147488 | 284836 | 415772 |
| rb-668588ef706d-2 | disk_kb data | 68612 | 427024 | 454284 |
| rb-668588ef706d-2 | disk_kb images | 105892 | 329088 | 329088 |
| rb-668588ef706d-2 | disk_kb logs | 28 | 3028 | 3028 |
| rb-668588ef706d-2 | disk_kb metrics | 48 | 5700 | 5700 |
| rb-668588ef706d-2 | disk_kb soak-export | 4 | 8576 | 8576 |
| rb-668588ef706d-2 | disk_kb volumes | 16736 | 17996 | 17996 |
| rb-668588ef706d-2 | fd | 62 | 113 | 286 |
| rb-668588ef706d-2 | rss_kb | 105868 | 198808 | 258880 |
| rb-668588ef706d-3 | disk_kb data | 231244 | 463544 | 611988 |
| rb-668588ef706d-3 | disk_kb images | 316644 | 316792 | 316792 |
| rb-668588ef706d-3 | disk_kb logs | 28 | 1012 | 1012 |
| rb-668588ef706d-3 | disk_kb metrics | 40 | 4396 | 4396 |
| rb-668588ef706d-3 | disk_kb soak-export | 4 | 6248 | 6248 |
| rb-668588ef706d-3 | disk_kb volumes | 16760 | 17156 | 17156 |
| rb-668588ef706d-3 | fd | 59 | 106 | 202 |
| rb-668588ef706d-3 | rss_kb | 111356 | 167612 | 204976 |

## Failures

| # | First seen | Check | Symptom | Class | Cause | Action |
|---|---|---|---|---|---|---|
| 1 | 16:37:07 | heavy-check | fail: job-inventory rb-668588ef706d-2: default__executor-db56b28214b34dd3911c69e5b13d044f-host-15: resources without an owner record; default__executor-db56b28214b34dd3911c69e5b13d044f-host-11: resources without an owner record; default__executor-db56b28214b34dd3911c69e5b13d044f-host-0: resources wi | unclassified | open | open |

Evidence: `/var/tmp/reliaburger-v02.TT9vq8` (events.jsonl, snapshots/, failures/<n>/).
