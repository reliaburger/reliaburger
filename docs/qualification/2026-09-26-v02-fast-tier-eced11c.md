# Sustained soak (V02): FAIL

26 September 2026. Fast tier, compressed schedule, 1 h 48 min of soak on a three-node quickstart cluster.

**Fast tier: not clean.** Fix what failed and run the fast tier again before a final-tier run.

## Verdict

FAIL, with no data lost: the writer's file checks never failed, and Redis's stored counter (read directly, 2691) was ahead of every value the soak acknowledged. What worked that failed on 2e93b67: upgrades were accepted with the operator's signature (#221), rollback replaced a paused run (#226), the chaos pause settled (#221), and the `relish test` pulse passed 11 of 15 (5 of 15 before). Every failure has a fix, all merged before the next candidate (main at 1117dbb).

## Candidate and host

| | |
|---|---|
| Staged base URL | <https://github.com/reliaburger/reliaburger/releases/download/staging-v0.1.0-36243131542-1> |
| `candidate.json` SHA-256 | `a94a24c1d29004dbaffe935c7fbca71ccc72aec7b0398ad95e338f82f0bba70b` |
| Running versions | v0.1.0 x3 |
| Soak build | `bun-v0.1.0-soak.2`, SHA-256 `b7d97ef290f955910614203f0ec4aa9241a29f28b05bbb3dbf00cb092b219831` |
| Host | macOS 26.3.1, Apple M2 Max, 32 GiB |
| Lima | limactl version 2.1.0 |
| Guest | Ubuntu 24.04.5 LTS, kernel 6.8.0-139-generic |
| RELIABURGER_HOME | `/tmp/rbq.7bISHH` |
| Evidence | `/var/tmp/reliaburger-v02.SoFgSN` |

## Configuration deviations

- node.toml `[testing] allowed_operations` adds `provision_isolated_workloads` (quickstart allows only inject_workload_faults and alter_node_state), so the catalogue pulse can lease test namespaces
- node.toml `[logs]`/`[metrics]` export to file:///var/lib/reliaburger/soak-export every 60 s with max_storage_mb = 8, `[storage.snapshots]` every 900 s (compressed: 120 s), retain 4, uploaded to the same directory
- node.toml `[ingress] tls_cert/tls_key` point at an operator pair from a soak CA, 45-minute leaves rotated by the harness
- node.toml `[node.labels] soak-volume` = "writer" on node 2 and "redis" on node 3, to pin the volume apps
- node.toml `[upgrades] external_signing_key` = a throwaway operator key the harness made for this run (`ed25519:WQhGQ51NYxR6WWcChDaJ8tZQ9Jjssscr6a9/6+ZTKyE=`); the soak build's `.sig` gets that key's external signature beside the release one (D2)
- node.toml `[security] leaf_lifetime_override_secs = 3600` (D1)

## Timeline

- Started 2026-09-26 13:39:26 UTC, finished 2026-09-26 15:27:28 UTC
- Cycles completed: 6
- Teardown: `relish local destroy --yes` and `relish uninstall --yes` succeeded

## Faults and slots

| Class | Runs | Settled/ok | Failed | Skipped | Settle (median / max) |
|---|---|---|---|---|---|
| fault:bun-kill-follower | 6 | 6 | 0 | 0 | 32 s / 56 s |
| fault:bun-kill-leader | 6 | 6 | 0 | 0 | 40 s / 44 s |
| fault:chaos-delay | 2 | 4 | 0 | 0 | 18 s / 18 s |
| fault:chaos-dns | 1 | 2 | 0 | 0 | 17 s / 17 s |
| fault:chaos-drop | 1 | 2 | 0 | 0 | 19 s / 19 s |
| fault:chaos-kill | 2 | 4 | 0 | 0 | 32 s / 32 s |
| fault:chaos-partition | 2 | 4 | 0 | 0 | 18 s / 18 s |
| fault:chaos-pause | 2 | 4 | 0 | 0 | 19 s / 19 s |
| fault:chaos-scenario | 2 | 2 | 2 | 0 | 34 s / 34 s |
| fault:deploy-kill | 3 | 3 | 0 | 0 | 55 s / 75 s |
| fault:power-off | 3 | 3 | 0 | 0 | 32 s / 36 s |
| pulse | 1 | 0 | 1 | 0 | n/a |
| pulse:settle | 0 | 1 | 0 | 0 | 31 s / 31 s |
| special:all-off | 1 | 1 | 0 | 0 | 52 s / 52 s |
| special:graceful-stop | 1 | 1 | 0 | 0 | 41 s / 41 s |
| special:quorum-back | 1 | 1 | 0 | 0 | n/a |
| special:quorum-loss | 1 | 1 | 0 | 0 | 53 s / 53 s |
| storage:offline-export | 3 | 3 | 0 | 0 | n/a |
| storage:registry-push | 6 | 0 | 0 | 0 | n/a |
| tls:invalid-expired | 3 | 3 | 0 | 0 | n/a |
| tls:invalid-torn | 6 | 6 | 0 | 0 | n/a |
| tls:reload | 38 | 38 | 0 | 0 | n/a |
| upgrade:rollback | 3 | 2 | 1 | 0 | n/a |
| upgrade:settle | 0 | 3 | 0 | 0 | 56 s / 78 s |
| upgrade:upgrade | 3 | 2 | 1 | 0 | n/a |

## Renewals and rotations

| Measure | Observed | Required | Verdict |
|---|---|---|---|
| workload identity rotations (soak-identity, per node) | 8 | ≥ 2 | PASS |
| node leaf renewals (per node) | 4 | ≥ 2 | PASS |
| operator ingress reloads (all nodes) | 38 | ≥ 20 | PASS |

## Data

- Volume writer: highest ACK 57496 in the log view; the writer file checks (writer-gap, writer-regression) decide data loss, and `*-log-order` failures are about the order the log view returned lines in
- Redis counter: highest INCR 5987 in the log view
- Export rb-537da6e326af-1: 165 source files, 161 at the destination
- Export rb-537da6e326af-2: 215 source files, 214 at the destination
- Export rb-537da6e326af-3: 147 source files, 143 at the destination
- Registry images pushed and re-verified: 7
- Checks evaluated: 189 (heavy 20, light 68, settle 101)
- Last observed progress: redis-counter 5987, writer-ack 57496

## Resource trends

| Node | Measure | First | Last | Max |
|---|---|---|---|---|
| rb-537da6e326af-1 | disk_kb data | 178704 | 30992 | 608052 |
| rb-537da6e326af-1 | disk_kb images | 316508 | 419084 | 419084 |
| rb-537da6e326af-1 | disk_kb logs | 64 | 3572 | 3572 |
| rb-537da6e326af-1 | disk_kb metrics | 92 | 3228 | 3228 |
| rb-537da6e326af-1 | disk_kb soak-export | 28 | 6816 | 6816 |
| rb-537da6e326af-1 | disk_kb volumes | 224 | 36 | 252 |
| rb-537da6e326af-1 | fd | 103 | 344 | 491 |
| rb-537da6e326af-1 | rss_kb | 391980 | 498892 | 796420 |
| rb-537da6e326af-2 | disk_kb data | 360984 | 547908 | 552724 |
| rb-537da6e326af-2 | disk_kb images | 316472 | 316596 | 316596 |
| rb-537da6e326af-2 | disk_kb logs | 36 | 5812 | 5812 |
| rb-537da6e326af-2 | disk_kb metrics | 80 | 3348 | 3348 |
| rb-537da6e326af-2 | disk_kb soak-export | 24 | 9100 | 9100 |
| rb-537da6e326af-2 | disk_kb volumes | 16856 | 18204 | 18204 |
| rb-537da6e326af-2 | fd | 70 | 89 | 185 |
| rb-537da6e326af-2 | rss_kb | 227352 | 516148 | 671944 |
| rb-537da6e326af-3 | disk_kb data | 231148 | 240340 | 698468 |
| rb-537da6e326af-3 | disk_kb images | 316508 | 320708 | 320708 |
| rb-537da6e326af-3 | disk_kb logs | 32 | 504 | 504 |
| rb-537da6e326af-3 | disk_kb metrics | 44 | 1352 | 1352 |
| rb-537da6e326af-3 | disk_kb soak-export | 24 | 2292 | 2292 |
| rb-537da6e326af-3 | disk_kb volumes | 16760 | 17216 | 17216 |
| rb-537da6e326af-3 | fd | 57 | 73 | 78 |
| rb-537da6e326af-3 | rss_kb | 183084 | 322036 | 374156 |

## Failures

| # | First seen | Check | Symptom | Class | Cause | Action |
|---|---|---|---|---|---|---|
| 1 | 13:53:09 | pulse | relish test exited 1; see pulse-1790430482.json | product + test fixture | a stopped app kept its ingress route (from #211); busybox fixtures ignored SIGTERM, so every stop sat out the grace on the agent's command loop and status/cleanup timed out | fixed: #232, #237, #239 |
| 2 | 14:07:16 | upgrade-upgrade | versions after 600 s: 0.1.0 0.1.0 0.1.0-soak.2  (wanted 0.1.0-soak.2) | product | one failed binary fetch (registry on the leader restarting after its bun was killed) paused the rolling upgrade | fixed: #233 |
| 3 | 14:20:18 | chaos-scenario | `dead_worker_node_has_workloads_rescheduled` timed out at 600 s | test case | the chaos workload served `/etc`, which has no `hostname` in the pinned image, so the case never got past its first wait | fixed: #236 |
| 4 | 14:23:23 | settle-check | fail: redis-log-order: log view ends at 782, below the 2504 an earlier check saw (line order in the log view, not a read of the stored counter); | product | the log query asked only the node the app runs on now; the Redis client had moved and back | fixed: #235 |
| 5 | 14:42:34 | upgrade | upgrade rollback failed | harness | rollback sent before the leader marked the completed run Completed (409 already in progress) | fixed: #234 |
| 6 | 14:52:39 | upgrade-rollback | versions after 600 s: 0.1.0-soak.2 0.1.0-soak.2 0.1.0-soak.2  (wanted 0.1.0) | harness | follows from 5: the rollback never ran | fixed: #234 |
| 7 | 14:56:43 | settle-check | fail: writer-log-order: log view ends at 40314, below the 40640 an earlier check saw (line order in the log view; the writer file checks decide data loss);fail: redis-log-order: log view ends at 3544, below the 4202 an earlier check saw (line order in the log view, not a read of the stored counter); | check | every VM powered off: stdout not yet synced was lost from the log view; the writer's file and Redis's counter lost nothing | fixed: #238 |
| 8 | 15:24:56 | chaos-scenario | `dead_worker_node_has_workloads_rescheduled` timed out at 600 s | test case | same as 3 | fixed: #236 |

Evidence: `/var/tmp/reliaburger-v02.SoFgSN` (events.jsonl, snapshots/, failures/<n>/).
