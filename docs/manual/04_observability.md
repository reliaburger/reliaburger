# Observability

Metrics (Mayo), logs (Ketchup), events and dashboards ship in the binary.
Nothing to deploy, nothing to scrape-config.

## Logs

Captured stdout/stderr per app, on the node that ran it:

```sh
relish logs web                  # everything captured
relish logs web --tail 20 -f     # last 20, then follow
relish logs web --grep error --since 1h
relish logs web --json-field level=warn
```

Retention and export are config (`[logs]`): old files age out after
`retention_days`; `export_path` ships Parquet files to a local path or object
store (`s3://`, `gs://`). Exported archives answer SQL:

```sh
relish logs-export --dest ./archive
relish logs-search ./archive "SELECT count(*) FROM logs WHERE line LIKE '%error%'"
```

## Metrics

System, per-app and Prometheus-endpoint metrics are collected on every node:

```sh
relish top                       # live CPU/memory per app
```

Alert rules evaluate in the agent; `[[alerts.destinations]]` webhooks (with
optional HMAC signing) deliver them. Council members hold cluster-wide
rollups so one node can answer for the fleet.

## Events and dashboards

```sh
relish                           # the terminal dashboard (TUI)
```

The TUI shows apps, nodes, jobs, routes, live logs and events on WebSockets;
press `?` inside for keys. The same data drives the web dashboard (Brioche)
at <http://127.0.0.1:9117/>.
