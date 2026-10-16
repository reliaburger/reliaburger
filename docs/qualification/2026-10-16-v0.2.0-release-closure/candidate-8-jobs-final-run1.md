# Sustained soak (V02): FAIL

15 October 2026. final tier, full schedule, 5 h 59 min of soak on a three-node quickstart cluster.

**Final tier: not clean. The V02 gate does not pass.** A product fix means a fresh fast-tier run, then a fresh final-tier run.

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
| RELIABURGER_HOME | `/tmp/rbq.IBMxgi` |
| Evidence | `/var/tmp/reliaburger-v02.YHafvM` |

## Configuration deviations

- node.toml `[testing] allowed_operations` adds `provision_isolated_workloads` (quickstart allows only inject_workload_faults and alter_node_state), so the catalogue pulse can lease test namespaces
- node.toml `[logs]`/`[metrics]` export to file:///var/lib/reliaburger/soak-export every 60 s with max_storage_mb = 8, `[storage.snapshots]` every 900 s (compressed: 120 s), retain 4, uploaded to the same directory
- node.toml `[ingress] tls_cert/tls_key` point at an operator pair from a soak CA, 45-minute leaves rotated by the harness
- Bun runs with auto/mixed; host jobs opt in to a single pinned BusyBox copy under /var/lib/reliaburger/soak-jobs, mount_isolation=false. The independent verifier listens on each private VM address, TCP 8189, with a separate run token
- node.toml `[node.labels] soak-volume` = "writer" on node 2 and "redis" on node 3, to pin the volume apps
- node.toml `[upgrades] external_signing_key` = a throwaway operator key the harness made for this run (`ed25519:AgNzU2/sWXruKYTV82DP5fKR1qfDmufqfyZrevLrX2M=`); the soak build's `.sig` gets that key's external signature beside the release one (D2)
- node.toml `[security] leaf_lifetime_override_secs = 3600` (D1)

## Timeline

- Started 2026-10-15 19:56:26 UTC, finished 2026-10-16 01:56:20 UTC
- Cycles completed: 5
- Teardown: `relish local destroy --yes` and `relish uninstall --yes` succeeded

## Faults and slots

| Class | Runs | Settled/ok | Failed | Skipped | Settle (median / max) |
|---|---|---|---|---|---|
| fault:bun-kill-follower | 6 | 6 | 0 | 0 | 42 s / 62 s |
| fault:bun-kill-leader | 6 | 6 | 0 | 0 | 39 s / 44 s |
| fault:chaos-delay | 1 | 2 | 0 | 0 | 21 s / 21 s |
| fault:chaos-drop | 1 | 2 | 0 | 0 | 24 s / 24 s |
| fault:chaos-kill | 1 | 2 | 0 | 0 | 44 s / 44 s |
| fault:chaos-partition | 1 | 2 | 0 | 0 | 22 s / 22 s |
| fault:chaos-pause | 1 | 2 | 0 | 0 | 25 s / 25 s |
| fault:chaos-scenario | 1 | 2 | 0 | 0 | 37 s / 37 s |
| fault:deploy-kill | 3 | 3 | 0 | 0 | 40 s / 43 s |
| fault:node-delay | 2 | 2 | 0 | 0 | 23 s / 23 s |
| fault:node-disk-full | 2 | 2 | 0 | 0 | 25 s / 25 s |
| fault:node-slow-fsync | 2 | 2 | 0 | 0 | 25 s / 25 s |
| fault:power-off | 3 | 3 | 0 | 0 | 43 s / 62 s |
| pulse | 6 | 6 | 0 | 0 | n/a |
| pulse:settle | 0 | 6 | 0 | 0 | 24 s / 26 s |
| special:quorum-back | 1 | 1 | 0 | 0 | n/a |
| special:quorum-loss | 1 | 1 | 0 | 0 | 59 s / 59 s |
| storage:offline-export | 1 | 1 | 0 | 0 | n/a |
| storage:registry-push | 6 | 0 | 0 | 0 | n/a |
| tls:invalid-expired | 6 | 6 | 0 | 0 | n/a |
| tls:invalid-torn | 9 | 9 | 0 | 0 | n/a |
| tls:reload | 68 | 68 | 0 | 0 | n/a |
| upgrade:rollback | 3 | 3 | 0 | 0 | n/a |
| upgrade:settle | 0 | 3 | 0 | 0 | 67 s / 70 s |
| upgrade:upgrade | 3 | 3 | 0 | 0 | n/a |

## Renewals and rotations

| Measure | Observed | Required | Verdict |
|---|---|---|---|
| workload identity rotations (soak-identity, per node) | 15 | ≥ 10 | PASS |
| node leaf renewals (per node) | 12 | ≥ 10 | PASS |
| operator ingress reloads (all nodes) | 68 | ≥ 22 | PASS |
| mixed job campaign and positive drain | 5 missing/failing checks | 0 | FAIL |

## Jobs

| Runtime | Accepted successes | Retries | Last accepted progress |
|---|---|---|---|
| runc | 27844 | 9 | 1792115731 |
| shared-runc | 28721638 | 15 | 1792115746 |
| process | 81483500 | 1 | 1792115746 |

The bounded campaign shares the app soak's fault schedule. These counts are reliability evidence, not a saturation rate or a 100-million-per-day qualification.

- job campaign did not positively drain before teardown
- job campaign requested drain before the final two-minute interval
- rb-5de239cdf01a-1 lacks a complete empty owner inventory after campaign drain
- rb-5de239cdf01a-2 lacks a complete empty owner inventory after campaign drain
- rb-5de239cdf01a-3 lacks a complete empty owner inventory after campaign drain
- Independent verifier execution attempts: 24
- Explicit test-operator acknowledgements for known replay-safe fenced fixtures: 149

- rb-5de239cdf01a-1: latest complete job storage (KiB) {'instances': 104900, 'job-state': 8, 'task-arrays': 50164}; free 4535104 KiB; live/peak owners 30/31; peak storage {'instances': 104900, 'job-state': 8, 'task-arrays': 57508}
- rb-5de239cdf01a-2: latest complete job storage (KiB) {'instances': 45128, 'job-state': 8, 'task-arrays': 41604}; free 6113200 KiB; live/peak owners 30/32; peak storage {'instances': 74352, 'job-state': 8, 'task-arrays': 62808}
- rb-5de239cdf01a-3: latest complete job storage (KiB) {'instances': 24220, 'job-state': 8, 'task-arrays': 54136}; free 6122696 KiB; live/peak owners 30/32; peak storage {'instances': 24288, 'job-state': 8, 'task-arrays': 64212}

Complete disk inventories are recorded. Global retention bounds and the 12-hour job qualification remain #668; this gate cannot certify an unimplemented retention policy.

## Job qualification (D18-C)

Load: target; 3 nodes; accepted 5,101.0/s against a 1390.0/s target: PASS.

The September plan set its targets for one 1M-task batch on a quiet cluster. A soak runs for hours with restarts, upgrades and apps, so its storage, Bun memory and Raft targets are restated (maintainer decision, 15 October 2026, [#668](https://github.com/reliaburger/reliaburger/issues/668)). Bun's steady state is the median RSS of each process once it has been seen for 10 minutes; the gated memory checks are unchanged (`leak-rss`, and Mayo's 70% `memory_high` alert through `wtf`).

| Measure (recorded, not gated) | Observed | Target |
|---|---|---|
| Wall time of a 1,048,576-task array (median / max of 64) | 275 s / 1,172 s | ≤ 600 s |
| p99 start lag, pooled executors | 32 ms (bucket upper bound) | < 1,000 ms |
| Raft entries per second, all cluster writes | 8.60 | recorded only; the batch target counts only task-array writes |
| Raft state of one run, largest seen | 6,624 B | ≤ 256 KiB per 1M tasks |
| Bun steady-state RSS per process, worst node against its target | rb-5de239cdf01a-2: 263.83 MiB steady, 263.84 MiB peak | ≤ 384 MiB (≤ 512 MiB on node 1, which serves the API) |
| Bun CPU, busiest node | 0.38 cores | ≤ 1 core (leader batch control) |
| Executor helper CPU, busiest node | 0.01 cores | ≤ 0.5 core |
| Node task-array storage per 1M accepted tasks (peaks summed) | 1.63 MiB | ≤ 64 MiB, deleted after retirement |
| Retained log volume, all nodes | 19.58 MB | ≤ 50 MB |

- Bun RSS on rb-5de239cdf01a-1 (serves the API, target ≤ 512 MiB): pid 8009: 269.89 MiB peak (seen for under 10 minutes); pid 1344034: 270.20 MiB steady, 381.01 MiB peak; pid 3833475: 230.08 MiB steady, 234.79 MiB peak; pid 3833475: 227.46 MiB steady, 268.38 MiB peak; pid 1658942: 264.33 MiB steady, 267.84 MiB peak; pid 2529926: 206.71 MiB steady, 227.88 MiB peak; pid 2529926: 223.09 MiB steady, 249.01 MiB peak; pid 2087608: 194.57 MiB peak (seen for under 10 minutes); pid 3411574: 181.96 MiB peak (seen for under 10 minutes); pid 86313: 290.16 MiB steady, 290.16 MiB peak; pid 1829632: 189.97 MiB peak (seen for under 10 minutes); pid 1829632: 226.23 MiB steady, 271.02 MiB peak; pid 2593583: 188.58 MiB peak (seen for under 10 minutes); pid 3314864: 193.68 MiB peak (seen for under 10 minutes)
- Bun RSS on rb-5de239cdf01a-2 (node, target ≤ 384 MiB): pid 2370: 117.09 MiB peak (seen for under 10 minutes); pid 4783: 199.89 MiB steady, 199.96 MiB peak; pid 2890487: 145.22 MiB peak (seen for under 10 minutes); pid 670: 215.38 MiB steady, 216.85 MiB peak; pid 670: 193.47 MiB steady, 207.51 MiB peak; pid 126899: 206.48 MiB steady, 229.54 MiB peak; pid 126899: 178.20 MiB steady, 178.20 MiB peak; pid 113271: 200.71 MiB steady, 211.79 MiB peak; pid 671: 263.83 MiB steady, 263.84 MiB peak; pid 671: 194.82 MiB steady, 195.44 MiB peak
- Bun RSS on rb-5de239cdf01a-3 (node, target ≤ 384 MiB): pid 2155: 157.69 MiB steady, 214.32 MiB peak; pid 2043446: 184.30 MiB peak (seen for under 10 minutes); pid 3635460: 191.54 MiB steady, 224.15 MiB peak; pid 2246801: 183.32 MiB steady, 213.01 MiB peak; pid 673: 190.28 MiB steady, 227.59 MiB peak; pid 1432472: 171.77 MiB peak (seen for under 10 minutes); pid 1432472: 188.23 MiB steady, 249.34 MiB peak; pid 1743429: 174.26 MiB peak (seen for under 10 minutes); pid 3266006: 177.27 MiB peak (seen for under 10 minutes); pid 671: 150.96 MiB steady, 154.69 MiB peak; pid 669: 135.72 MiB peak (seen for under 10 minutes); pid 163172: 143.88 MiB steady, 147.71 MiB peak; pid 163172: 163.43 MiB steady, 165.81 MiB peak

- runc retry causes: exit_code 1, launch 7, signal 0, timeout 1, unknown 0, unrecorded 0
- shared-runc retry causes: exit_code 1, launch 3, signal 0, timeout 11, unknown 0, unrecorded 0
- process retry causes: exit_code 1, launch 0, signal 0, timeout 0, unknown 0, unrecorded 0
- Audited executions: 24; operator replay decisions: 149

## Data

- Volume writer: highest ACK 190969 in the log view; the writer file checks (writer-lost, writer-regression) decide data loss, and `*-log-order` failures are about the order the log view returned lines in
- Redis counter: highest INCR 20311 in the log view
- Export rb-5de239cdf01a-1: 193 source files, 590 at the destination
- Export rb-5de239cdf01a-2: 319 source files, 711 at the destination
- Export rb-5de239cdf01a-3: 257 source files, 453 at the destination
- Registry images pushed and re-verified: 7
- Agent-loop turns over the 1 s budget: rb-5de239cdf01a-1 0, rb-5de239cdf01a-2 0, rb-5de239cdf01a-3 0
- Guest clock steps by Lima's guest agent: rb-5de239cdf01a-1 3, rb-5de239cdf01a-2 5, rb-5de239cdf01a-3 4
- Checks evaluated: 764 (heavy 73, light 590, settle 101)
- Last observed progress: redis-counter 20311, writer-ack 190969

## Resource trends

| Node | Measure | First | Last | Max |
|---|---|---|---|---|
| rb-5de239cdf01a-1 | disk_kb data | 1370656 | 1102648 | 1664512 |
| rb-5de239cdf01a-1 | disk_kb images | 328964 | 452948 | 452948 |
| rb-5de239cdf01a-1 | disk_kb logs | 40 | 8364 | 9024 |
| rb-5de239cdf01a-1 | disk_kb metrics | 96 | 8324 | 9144 |
| rb-5de239cdf01a-1 | disk_kb soak-export | 4 | 50828 | 50828 |
| rb-5de239cdf01a-1 | disk_kb volumes | 200 | 36 | 252 |
| rb-5de239cdf01a-1 | fd | 129 | 241 | 358 |
| rb-5de239cdf01a-1 | rss_kb | 153404 | 198328 | 390156 |
| rb-5de239cdf01a-2 | disk_kb data | 68616 | 517124 | 527032 |
| rb-5de239cdf01a-2 | disk_kb images | 158956 | 334304 | 334304 |
| rb-5de239cdf01a-2 | disk_kb logs | 28 | 8728 | 9008 |
| rb-5de239cdf01a-2 | disk_kb metrics | 44 | 8552 | 9072 |
| rb-5de239cdf01a-2 | disk_kb soak-export | 4 | 46152 | 46152 |
| rb-5de239cdf01a-2 | disk_kb volumes | 16736 | 21672 | 21672 |
| rb-5de239cdf01a-2 | fd | 60 | 170 | 369 |
| rb-5de239cdf01a-2 | rss_kb | 102272 | 200132 | 270168 |
| rb-5de239cdf01a-3 | disk_kb data | 231244 | 481768 | 604976 |
| rb-5de239cdf01a-3 | disk_kb images | 316524 | 323976 | 323976 |
| rb-5de239cdf01a-3 | disk_kb logs | 28 | 1088 | 1088 |
| rb-5de239cdf01a-3 | disk_kb metrics | 48 | 8580 | 8944 |
| rb-5de239cdf01a-3 | disk_kb soak-export | 4 | 27104 | 27104 |
| rb-5de239cdf01a-3 | disk_kb volumes | 16760 | 18344 | 18344 |
| rb-5de239cdf01a-3 | fd | 63 | 183 | 226 |
| rb-5de239cdf01a-3 | rss_kb | 102496 | 169788 | 255320 |

## Failures

| # | First seen | Check | Symptom | Class | Cause | Action |
|---|---|---|---|---|---|---|
| 1 | 23:40:52 | heavy-check | fail: wtf-warning cluster: alerts-firing for 604 s: memory_high: Memory usage above 70% for 10 minutes; | unclassified | open | open |
| 2 | 00:32:05 | light-check | fail: redis-log-order: log view ends at 11825, below the 15856 an earlier check saw (line order in the log view, not a read of the stored counter); | unclassified | open | open |
| 3 | 00:34:42 | heavy-check | fail: bun-restart rb-5de239cdf01a-1: 1 systemd restart(s) of bun without a harness kill; | unclassified | open | open |
| 4 | 01:45:04 | heavy-check | fail: wtf-warning cluster: alerts-firing for 604 s: memory_high: Memory usage above 70% for 10 minutes; | unclassified | open | open |
| 5 | 01:50:29 | heavy-check | fail: bun-restart rb-5de239cdf01a-1: 1 systemd restart(s) of bun without a harness kill; | unclassified | open | open |

## Notes

- Job controller was still running at teardown; positive drain must be present to pass
- The harness stopped early (exit 1) during 5:settle

Evidence: `/var/tmp/reliaburger-v02.YHafvM` (events.jsonl, snapshots/, failures/<n>/).
