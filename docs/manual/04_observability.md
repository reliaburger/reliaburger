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

Retention and export are node config:

```toml
[logs]
retention_days = 7                # default
export_path = "s3://bucket/logs/" # optional: a local path, file://, s3:// or gs://
export_interval_secs = 3600       # default
```

Exported Parquet archives answer SQL:

```sh
relish logs-export --dest ./archive
relish logs-search ./archive "SELECT count(*) FROM logs WHERE line LIKE '%error%'"
```

To export a local store directly, including a custom store while its agent is
stopped, use `relish logs-export --source /path/to/parquet --dest ./archive`.
If files copy but the checkpoint cannot be saved, the command exits non-zero
and explains that a later export may repeat them.

## Metrics

Every node collects its own system metrics and each workload's CPU and memory
every 10 seconds, and keeps them for 7 days (`[metrics] retention_days`).
The TUI and the web dashboard (Brioche) chart live CPU and memory. The CLI
`relish top` is a one-shot table of every workload on every node: node, app,
namespace, state, PID, restarts, and the latest CPU and memory sample each
node's collector took (`process_cpu_percent`, `process_memory_bytes`). A `-`
means no sample yet. A node that doesn't answer becomes a
`warning:` line rather than an error:

```sh
relish top                       # every node's workloads, with CPU and memory
```

### Your app's own metrics

An app that serves Prometheus text declares it, and each node scrapes its own
instances every 10 seconds. You don't install Prometheus, and the metrics port
needn't be published:

```toml
[app.web]
port = 8080
metrics = {}                     # http://<instance>:8080/metrics
# metrics = { port = 9797, path = "/prom" }
```

Kubernetes manifests get this from their `prometheus.io/scrape`, `port` and
`path` pod annotations. `relish metrics` reads what was scraped, through
whichever node you talk to:

```sh
relish metrics web                           # every metric, one number each
relish metrics web --name http_requests_total   # per instance, with a trend
relish metrics web --name http_request_duration_seconds --since 1h
```

```text
METRIC                         TYPE       SERIES  INSTANCES  VALUE
http_request_duration_seconds  histogram       2          2  mean 11.9ms
http_requests_total            counter         4          2  8.40/s
up                             gauge           2          2  2
```

A counter (`_total`) shows its per-second rate, a histogram its mean
observation, anything else its latest value, each added up across instances.
With `--name` you get one line per instance, a rate for counters and a
sparkline; a histogram named by its base shows mean latency per instance.
`up` is 1 while an instance's last scrape worked and 0 when it failed. The
app's page in the web dashboard charts the same data, one line per instance.

There's no PromQL. You read metrics through `relish metrics`, the dashboards
and the API. For something that isn't an app on the cluster, such as a node
exporter, list fixed URLs as `[[metrics.scrape_targets]]` with a `job` and a
`url`; they're scraped every 30 seconds.

### Alerts

Each node evaluates five built-in rules against its own metrics every 30
seconds: CPU above 90% for 5 minutes and memory above 85% for 2 minutes
(critical); memory above 70% for 10 minutes, disk above 80% for 5 minutes and
CPU below 5% for 30 minutes (warning). You can't define your own rules yet.
Webhooks deliver them, optionally signed with HMAC-SHA256:

```toml
[[alerts.destinations]]
type = "webhook"
url = "https://hooks.example.com/reliaburger"
severity = ["critical"]           # empty: every severity
secret = "shared-hmac-secret"     # optional
```

Council members hold cluster-wide rollups, so one node can answer for the
fleet.

## Events and dashboards

```sh
relish                           # the terminal dashboard (TUI)
relish dashboard                 # authenticated, read-only browser session
```

The TUI shows apps, nodes, jobs, routes, live logs and events; press `?`
inside for keys. It needs a terminal of at least 80×24.

`relish dashboard` serves the web dashboard (Brioche) on a loopback port and
opens a one-time link to it. It forwards read-only requests using the CLI's own
token and CA, so you don't install a certificate in your browser or paste a
token. Ctrl-C closes it; `--no-open` prints the link instead. You can also open
a node's API address in a browser directly (<http://127.0.0.1:9117/> on a
source build): once the cluster has tokens, it asks you to paste one and gives
you a read-only session for 12 hours.
