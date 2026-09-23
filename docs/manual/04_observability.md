# Observability

Metrics (Mayo), logs (Ketchup), events and dashboards ship in the binary.
Nothing to deploy, nothing to scrape-config.

## Logs

Captured stdout/stderr per app, on the node that ran it. You ask any node and
it gathers from every node that runs the app:

```sh
relish logs web                  # everything captured
relish logs web --tail 20 -f     # last 20 per replica, then follow
relish logs web --grep error --since 1h
relish logs web --json-field level=warn
```

`-f` follows every replica on every node and prefixes each line with its node
and instance, like `[rb-4f2a9c1e07b3-2 default__web-0] GET /healthz 200`. It
picks up replicas scheduled onto new nodes as they appear. If a node goes
away mid-stream, you get a `warning:` on stderr and the rest keep streaming.

Retention and export are config (`[logs]`): old files age out after
`retention_days`; `export_path` ships Parquet files to a local path or object
store (`s3://`, `gs://`). Exported archives answer SQL:

```sh
relish logs-export --dest ./archive
relish logs-search ./archive "SELECT count(*) FROM logs WHERE line LIKE '%error%'"
```

To export a local store directly, including a custom store while its agent is
stopped, use `relish logs-export --source /path/to/parquet --dest ./archive`.
If files copy but the checkpoint cannot be saved, the command exits non-zero
and explains that a later export may repeat them.

## Metrics

System, per-app and Prometheus-endpoint metrics are collected on every node:

The TUI and the web dashboard (Brioche) chart live CPU and memory. The CLI
`relish top` is a one-shot table of every workload on every node: node, app,
namespace, state, PID, restarts, and the latest CPU and memory sample each
node's collector took (`process_cpu_percent`, `process_memory_bytes`, every
few seconds). A `-` means no sample yet. A node that doesn't answer becomes a
`warning:` line rather than an error:

```sh
relish top                       # every node's workloads, with CPU and memory
```

Alert rules evaluate in the agent; `[[alerts.destinations]]` webhooks (with
optional HMAC signing) deliver them. Council members hold cluster-wide
rollups so one node can answer for the fleet.

## Events and dashboards

```sh
relish                           # the terminal dashboard (TUI)
relish dashboard                 # authenticated, read-only browser session
```

The TUI shows apps, nodes, jobs, routes, live logs and events on WebSockets;
press `?` inside for keys. The same data drives the web dashboard (Brioche)
at <http://127.0.0.1:9117/>.
