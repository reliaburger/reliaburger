# Five-minute tour, run for real (Z6.7)

24 September 2026. Every step of the homepage tour, run on a fresh three-node
laptop cluster, timed, with its real output. This is the check the zero-to-
cluster plan asked for before the homepage copy changed.

## Setup

- Host: Apple M2 Max, 32 GiB, macOS 26.3.1.
- Cluster: `relish setup --quickstart --development-binaries <dir> --timings`
  with an isolated `RELIABURGER_HOME`, default ports (19117-19119, 18080,
  15050; nothing else was listening). Lima 2.1.0 and the pinned Ubuntu image,
  as in a release.
- Binaries: Linux aarch64 `bun` and `relish` built `--release --features ebpf`
  in the `reliaburger-test` Lima VM; the host `relish` a debug build of the same
  tree (`fix/z2c-tour` at the tour-copy commit).
- The manifest was applied from `examples/kubernetes/podinfo.yaml`, the file the
  Pages workflow publishes as `https://reliaburger.com/demo/podinfo.yaml`. The
  URL path itself is covered by `relish apply`'s own tests.
- The install line (`curl ... | sh`) wasn't run: it needs the signed release.
  `setup --quickstart` is what it runs after installing `relish`.
- Steps ran from a script (`tour.sh`), so the wall time below has no reading
  time in it. The script waits only where the tour tells a person to: for the
  apps to start, twenty seconds for metrics after the fault, and for the
  killed replica and the lost node to recover.

## Timings

Final run, warm downloads (Lima and the guest image cached):

| Step | Command | Command time | Elapsed at end |
|---|---|---|---|
| 1 | `relish setup --quickstart` | 91.2 s | 1:31 |
| 2 | `relish apply -f podinfo.yaml` | 0.5 s | 1:32 |
| 3 | `relish status` (after all 7 instances ran, 37 s) | 0.6 s | 2:09 |
| 4 | open `http://podinfo.localhost:18080` | 0.1 s | 2:09 |
| 5 | `relish trace frontend --to redis` | 0.6 s | 2:10 |
| 6 | `relish metrics frontend` (after 10 s) | 0.1 s | 2:21 |
| 7 | `relish fault delay redis 300ms --from frontend ...` | 1.3 s | 2:22 |
| 8 | `relish trace frontend --to redis --count 3` | 1.6 s | 2:24 |
| 9 | `relish metrics frontend --name http_request_duration_seconds` (after 20 s) | 0.2 s | 2:44 |
| 10 | `relish dashboard` (open, fetch charts, close) | ~9 s | 2:53 |
| 11 | `relish fault kill frontend --count 1 ...`, `relish status` until back | 0.4 s + 14 s | 3:08 |
| 12 | `relish local stop node-3`, `relish status` until 3 frontends run | 30.8 s + 53 s | 4:32 |
| 13 | `relish wtf` | 1.9 s | 4:34 |

**Total: 274 s (4 min 34 s) from `setup --quickstart` to the end of `wtf`.**

Setup, four runs on the same day:

| Run | Downloads | Setup |
|---|---|---|
| 1 (cold: Lima 35.5 MiB, guest image 590.9 MiB at ~6 MiB/s) | 101.7 s | 229.8 s |
| 2 (warm) | 1.8 s | 108.6 s |
| 3 (warm) | 1.8 s | 89.6 s |
| 4 (warm, the run above) | 1.8 s | 90.9 s |

**Does it meet five minutes?** With warm downloads, the machine time does:
4 min 34 s, with about 25 s of it spent in deliberate waits. A person reading
each step's sentence and output will take longer, realistically seven or eight
minutes. A cold first run adds the downloads, about 100 s at 6 MiB/s here, so a
first-time visitor on a similar connection sees roughly six minutes of machine
time. Five minutes holds for "the cluster and every command" on a warm cache,
not for "a first-time reader".

## Output

Trimmed where marked. Node names are the cluster's (`rb-a40f6e7244cc-N`);
`node-3` in the tour selects the third.

### 1. Setup

```
[ ok ] check host                       0.0s
development binaries selected explicitly; this run does not qualify a published release
[ ok ] install Lima 2.1.0               0.1s  installed
[ ok ] use development binaries         0.0s
[ ok ] download guest image             1.8s  cached
[ ok ] boot VM 1                       39.6s
[ ok ] boot VM 3                       44.3s
[ ok ] boot VM 2                       53.6s
[ ok ] install files on node 1          1.9s
[ ok ] start node 1                     0.8s
[ ok ] install files on node 2          4.0s
[ ok ] install files on node 3          4.0s
[ ok ] enrol node 2                     0.3s
[ ok ] enrol node 3                     0.6s
[ ok ] start node 2                    10.7s
[ ok ] start node 3                    15.8s
[ ok ] form council quorum              0.0s
  app hello: committed to the cluster
[ ok ] run hello through ingress       11.7s
where the time went (90.9s in total):
  host checks          0.0s
  downloads            1.8s
  VM boot             54.1s
  node setup          23.1s
  cluster checks      11.7s
cluster laptop ready in 90.9s
```

### 2. `relish apply -f podinfo.yaml`

```
Converted:
  + Deployment/frontend → [app.frontend]
  + Deployment/backend → [app.backend]
  + Deployment/redis → [app.redis]
  + Deployment/loadgen → [app.loadgen]

Approximated (review recommended):
  ~ Deployment/frontend — livenessProbe is not imported (only readinessProbe maps to a health check)
  ~ Service/frontend — port 80 forwards to container port 9898; Reliaburger has no port mapping, so clients must connect to frontend:9898 (ingress is unaffected)
  ~ Deployment/backend — livenessProbe is not imported (only readinessProbe maps to a health check)
  ~ Service/backend — port 9999 (grpc) dropped; an app exposes one port
  ~ Deployment/backend — an app exposes one port (9898); container port(s) 9999 (grpc) are not published or routed, though the process can still listen on them
  ~ Deployment/redis — livenessProbe is not imported (only readinessProbe maps to a health check)
  ~ Deployment/redis — readinessProbe runs a command (redis-cli ping); only httpGet probes import, so this app has no health check. Add a [health] block with an HTTP path

  app backend: committed to the cluster
  app frontend: committed to the cluster
  app loadgen: committed to the cluster
  app redis: committed to the cluster
applied 4 app(s); the scheduler places them now (watch with `relish status`)
```

### 3. `relish status`

All seven instances ran 37 s after the apply returned.

```
NODE                     INSTANCE             APP             NAMESPACE    STATE      PID        RESTARTS
rb-a40f6e7244cc-1        default__backend-0   backend         default      running    2843       0
rb-a40f6e7244cc-1        default__frontend-0  frontend        default      running    2982       0
rb-a40f6e7244cc-1        default__hello-0     hello           default      running    2415       0
rb-a40f6e7244cc-1        default__loadgen-0   loadgen         default      running    2516       0
rb-a40f6e7244cc-1        default__redis-0     redis           default      running    3229       0
rb-a40f6e7244cc-2        default__frontend-0  frontend        default      running    2440       0
rb-a40f6e7244cc-3        default__frontend-0  frontend        default      running    2434       0
```

### 4. Open `http://podinfo.localhost:18080`

`curl -H 'Accept: application/json'`, then three more requests' `hostname`:

```
{
  "hostname": "lima-rb-a40f6e7244cc-1",
  "version": "6.15.0",
  "message": "greetings from podinfo v6.15.0",
  ...
}
  "hostname": "lima-rb-a40f6e7244cc-2",
  "hostname": "lima-rb-a40f6e7244cc-3",
  "hostname": "lima-rb-a40f6e7244cc-1",
```

### 5. `relish trace frontend --to redis`

Run immediately after `status` first showed redis running, the trace failed:
redis (which has no importable health check) wasn't in the service map yet.
The step 8 trace, 13 s later, found redis healthy, and so did a re-run after
the tour (by then node-1's frontend had been restarted, hence its cgroup). The
tour's "give it half a minute" covers this. The re-run:

```
Trace default/frontend -> default/redis:6379 from node rb-a40f6e7244cc-1
  1. DNS query [PASS; observed]
     redis.default.internal -> 127.128.202.174 (resolver 192.168.104.14)
  2. Service and eBPF state [PASS; observed]
     userspace service map: VIP 127.128.202.174, 1 of 1 backends healthy
       backend default__redis-0 at 10.254.116.6:6379 (healthy)
     the VIP sends every connect to default__redis-0 at 10.254.116.6:6379
     live backend_map: 1 entries, 1 healthy
       kernel backend 10.254.116.6:6379 (healthy)
  3. Firewall state [PASS; observed]
     live maps: source cgroup 6727, source namespace Some(1455585218), destination namespace 1455585218, action Some(1)
  4. Active faults [PASS; observed]
     no fault acts on this path
  5. TCP probe [PASS; observed]
     1/1 connects to 127.128.202.174:6379 succeeded (connect time min 0 ms, median 0 ms, 10 ms clock)
Overall: PASS (1/1 connects, median connect 0.0 ms)
```

### 6. `relish metrics frontend`

47 metrics; the ones the tour talks about:

```
METRIC                                      TYPE       SERIES  INSTANCES  VALUE
http_request_duration_seconds               histogram      21          3  mean 389µs
http_requests_total                         counter         9          3  9.60/s
process_cpu_percent                         gauge           3          3  0
process_resident_memory_bytes               gauge           3          3  123.3M
up                                          gauge           3          3  3
```

Per replica, before the fault (`--name http_request_duration_seconds --since 2m`):

```
INSTANCE             NODE               SERIES       MEAN     OBS/S  TREND
default__frontend-0  rb-a40f6e7244cc-1       7      249µs      3.70  ▁
default__frontend-0  rb-a40f6e7244cc-2       7      413µs      2.50  ▁█
default__frontend-0  rb-a40f6e7244cc-3       7      523µs      3.40  ▁█
```

### 7. `relish fault delay redis 300ms --from frontend --duration 2m --acknowledge`

```
Fault injected: delay 300ms from frontend on redis on node rb-a40f6e7244cc-1 (id: 1, expires in 119s)
Fault injected: delay 300ms from frontend on redis on node rb-a40f6e7244cc-2 (id: 1, expires in 119s)
Fault injected: delay 300ms from frontend on redis on node rb-a40f6e7244cc-3 (id: 1, expires in 119s)
```

### 8. `relish trace frontend --to redis --count 3`

Exit status 2 (DEGRADED).

```
  4. Active faults [DEGRADED; observed]
     fault 1: delay 300ms from frontend (118s left)
     live netem on the source's eth0: delay 300ms
     reason: fault 1 (delay 300ms from frontend) is active on this path
  5. TCP probe [PASS; observed]
     3/3 connects to 127.128.202.174:6379 succeeded (connect time min 300 ms, median 300 ms, 10 ms clock)
Overall: DEGRADED (3/3 connects, median connect 300.0 ms)
  because fault 1 (delay 300ms from frontend) is active on this path
```

(Steps 1-3 as in step 5.)

### 9. `relish metrics frontend --name http_request_duration_seconds`

Twenty seconds after the fault:

```
default/frontend http_request_duration_seconds (histogram, last 2m)
INSTANCE             NODE               SERIES       MEAN     OBS/S  TREND
default__frontend-0  rb-a40f6e7244cc-1       7    386.4ms      1.40  ▁▆█
default__frontend-0  rb-a40f6e7244cc-2       7      226µs      1.20  ▁▆█▃
default__frontend-0  rb-a40f6e7244cc-3       7    116.0ms      1.30  ▁▁▁█
```

`MEAN` is the last scrape interval's mean, so it moves with which requests
landed in it: node-2's last interval happened to hold no cache reads, while its
trend shows the jump. A cache read through the ingress took 0.90 s during the
fault (three samples: 0.904, 0.904, 0.905 s, taken in the previous run on the
same data path), and 1.3 to 2.5 ms without it.

### 10. `relish dashboard`

Started with `--no-open`; the app page and two of its chart endpoints fetched
through the dashboard's session:

```
dashboard: http://127.0.0.1:63303/_reliaburger/open/<token>
read-only browser session; Ctrl-C to stop
app page 200
CPU Usage
Memory Usage
Requests/s
Mean Latency
9 points; [('default__frontend-0 on rb-a40f6e7244cc-1', [3.7, 2.3, 1.4]), ('default__frontend-0 on rb-a40f6e7244cc-2', [2.6, 1.2, 1.2]), ('default__frontend-0 on rb-a40f6e7244cc-3', [3.4, 1.5, 1.3])]
9 points; [('default__frontend-0 on rb-a40f6e7244cc-1', [0.0, 0.262, 0.386]), ('default__frontend-0 on rb-a40f6e7244cc-2', [0.001, 0.0, 0.0]), ('default__frontend-0 on rb-a40f6e7244cc-3', [0.001, 0.0, 0.116])]
```

(The last three points of each line: requests/s fall and mean latency rises
when the delay lands.)

### 11. `relish fault kill frontend --count 1 --acknowledge`, `relish status`

```
Fault injected: kill 1 on frontend on node rb-a40f6e7244cc-1 (id: 2, expires in 0s)
```

Two seconds later (trimmed to the frontends):

```
rb-a40f6e7244cc-1        default__frontend-0  frontend        default      pending    -          1
rb-a40f6e7244cc-2        default__frontend-0  frontend        default      running    2440       0
rb-a40f6e7244cc-3        default__frontend-0  frontend        default      running    2434       0
```

Back 14.5 s after the kill:

```
rb-a40f6e7244cc-1        default__frontend-0  frontend        default      running    3939       1
rb-a40f6e7244cc-2        default__frontend-0  frontend        default      running    2440       0
rb-a40f6e7244cc-3        default__frontend-0  frontend        default      running    2434       0
```

### 12. `relish local stop node-3`, `relish status`

`stopped rb-a40f6e7244cc-3` after 30.8 s (a graceful VM stop). Node-3 was the
council leader. Three frontends ran again 53 s later:

```
NODE                     INSTANCE             APP             NAMESPACE    STATE      PID        RESTARTS
rb-a40f6e7244cc-1        default__backend-0   backend         default      running    2843       0
rb-a40f6e7244cc-1        default__frontend-0  frontend        default      stopped    -          1
rb-a40f6e7244cc-1        default__frontend-g1-0 frontend        default      running    4243       0
rb-a40f6e7244cc-1        default__frontend-g1-1 frontend        default      running    4380       0
rb-a40f6e7244cc-1        default__hello-0     hello           default      running    2415       0
rb-a40f6e7244cc-1        default__loadgen-0   loadgen         default      running    2516       0
rb-a40f6e7244cc-1        default__redis-0     redis           default      running    3229       0
rb-a40f6e7244cc-2        default__frontend-0  frontend        default      running    2440       0
```

Node-2's frontend never moved. Node-1 went from one frontend to two, which is
a rolling redeploy on that node (see "Still open" below), so its original
replica shows as stopped while the two new ones serve.

### 13. `relish wtf`

Exit status 2 (warnings). This run's output also carried a CPU-throttling
warning for `loadgen`; its limit was raised to 500m afterwards and a re-run
showed only the council warning:

```
Reliaburger diagnosis: laptop (2 nodes)

WARNING (1)
  [council-member-down] 1 of 3 council members did not answer (council)
    quorum holds with 2 of 3; 1 more failure(s) would lose it
    next: bring the missing node back, or replace it: `relish nodes`, `relish local start <node>` on a laptop cluster

OK (10)
  [alerts] no relevant alerts are firing
  [certificates] all observed certificates are currently valid with healthy automatic rotation
  [cpu-throttling] no application accumulated throttled CPU time
  [crashloops] no applications have three timestamped restarts in 15 minutes (...)
  [deploys] no deploy has been active for more than 15 minutes (...)
  [disks] all observed storage domains are below 85% usage
  [faults] no active Smoker faults
  [nodes] all 2 nodes alive and answering
  [registry] Pickle is reachable and its known layers meet the redundancy target
  [services] all deployed services have a healthy backend

Summary: 0 critical, 1 warning, 0 unknown, 10 OK
```

`relish local start node-3` afterwards brought the node back in 11.6 s, and
node-1's pending rollout finished 20 s later.

## Bugs found and fixed

The first run didn't get past step 3. Each fix has its own commit and test.

1. **The agent loop blocked on image pulls.** runc holds an instance's
   lifecycle lock for its whole create, image pull included, and the loop asked
   the runtime for every instance's PID, cgroup and fault-target PIDs. Two nodes
   stalled for 35 s, `relish status` timed out ("agent status timed out"), their
   reports went stale and the leader moved all three frontends to node-1. Fixed:
   the loop doesn't ask the runtime about `Pending`/`Preparing` instances.
2. **Rolling deploys failed on the leader's first answer.** A producer release
   answered 202 (waiting for other nodes' withdrawal receipts) failed the
   deploy, and the orchestrator retried with a new generation; the frontend
   passed generation 30. Fixed: a typed pending answer, retried for up to 30 s.
3. **Withdrawals nobody could confirm.** A new backend's first catalogue entry
   has no runtime execution; its replacement with one was recorded as a
   withdrawal that matched every execution at that address. No node could ever
   confirm it, so every producer release on that node waited forever (a killed
   frontend sat in `pending`). Fixed: learning a backend's execution isn't a
   withdrawal.
4. **Metrics merged the replicas.** All three frontends are
   `default__frontend-0` on their own nodes; `relish metrics` reported one
   instance and the dashboard drew one line. Fixed: grouped by instance and node.
5. **Losing a node moved every replica.** The leader re-planned the whole app,
   and a new leader counted a not-yet-reported node's replica as lost. Fixed:
   surviving placements stay; unheard-from live nodes keep theirs.
6. **`wtf` was all OK with a node down.** Fixed: a council member missing while
   quorum holds is a warning.
7. **`apply` ended with "deployed 4 instance(s):" and an empty list** on a
   cluster. Fixed: it says how many apps it applied and to watch `status`.
8. **The load generator aliased with the VIP's rotation.** Three calls per loop
   against three frontends sent every cache read to one replica; the latency
   step showed one frontend unaffected by the fault. Fixed: four calls per loop
   (the new one exercises frontend → backend). Its CPU limit also rose from 100m
   to 500m after `wtf` flagged it as throttled.

## Still open

- **A stopped node blocks address releases.** Every node must confirm an
  endpoint withdrawal before its producer may reuse the address, and a stopped
  node can't. While node-3 was down, node-1's rolling redeploy (step 12) kept
  retrying its retirement; traffic was unaffected because the new replicas were
  already serving. It finished once node-3 came back. This is the protocol's
  safety rule (a partitioned node could still route to the old address), but a
  node that's known to be stopped could be discharged sooner.
- **A replica-count change is a rolling redeploy on that node**, not "start one
  more", so gaining a replica restarts the node's existing one.
- **Trace right after `status` can fail** for redis, which has no importable
  health check, until its backend reaches the service map a few seconds later.
- **`MEAN` in `relish metrics --name` is one scrape interval's mean**, so with a
  few requests a second it jumps about. The trend column is the honest view.
