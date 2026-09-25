# Deploy an app

Workloads are TOML. An app is a long-running service; a job runs to
completion (retried up to 3 times with backoff).

```toml
[app.web]
image = "proc-grill:image-ignored"
command = ["target/debug/testapp", "--mode", "healthy", "--port", "8080"]
port = 8080

[app.web.health]
path = "/healthz"
interval = 10             # seconds between probes (default 10)
timeout = 5               # seconds per probe (default 5)
```

ProcessGrill runs `command` directly and ignores `image`; runc pulls `image`
from a registry and runs it the way Kubernetes would (`command` replaces the
image's entrypoint, `args` its default arguments). Try it from a source
checkout, with `bun --runtime process` running:

```sh
relish apply examples/phase-1/proc-minimal-app.toml
relish status
```

Outside a checkout, `relish manual examples` writes every example config into
`./examples/`. The `container-*` ones run real images on runc.

## The rest of an app

Everything else is optional:

```toml
[app.api]
image = "ghcr.io/example/api:1.4.2"
port = 8080
replicas = 3              # or "*" for one per node
cpu = "250m-1000m"        # request-limit
memory = "256Mi-512Mi"
namespace = "shop"        # default: "default"
env = { LOG_LEVEL = "info" }

[app.api.placement]
required = ["zone=eu-west-1a"]  # node labels that must match
preferred = ["ssd=true"]

[[app.api.volumes]]
path = "/data"            # a managed volume; see `images-and-volumes`
```

Names (apps, jobs, namespaces) are lowercase DNS labels. Secrets go in `env`
encrypted; see `security`. Ingress, firewall and egress rules are in
`networking`, metrics scraping in `observability`.

## The everyday loop

```sh
relish apply app.toml            # deploy (or converge) everything in the file
relish apply app.toml --dry-run  # preview; exits 0 even with no agent
relish lint app.toml             # validate only
relish logs web -f               # stream logs from every node (--tail 20 for the last 20)
relish exec web env              # run a command inside an instance
relish top                       # every workload on every node, with CPU and memory
relish inspect web               # full detail
relish stop web                  # stop all instances
```

Commands that take an app name also take `--namespace` (default `default`).

## Many files

`relish apply` takes one file (or, with `-f`, a Kubernetes manifest or an
`https://` URL). For a tree of configs, compile it first:

```sh
relish compile config/ > all.toml   # merge, apply _defaults.toml, derive namespaces
relish diff old.toml all.toml       # structural diff between two configs
relish fmt all.toml --check         # canonical ordering; drop --check to rewrite
relish apply all.toml
```

`compile` walks the directory recursively. Each subdirectory's name becomes
the namespace of the apps inside it, and a `_defaults.toml` fills in fields its
apps leave unset. `diff` compares files, not the live cluster; `apply --dry-run`
shows what would change on the cluster.

## Rolling deploys

Apply a changed app and Bun replaces its instances one at a time, waiting for
each new one to pass its health check. `relish deploy app.toml` makes the
same request and reports it as a deploy.

```toml
[app.web.deploy]
strategy = "rolling"      # or "blue-green"
max_surge = 1             # default 1
max_unavailable = 0       # default 0
health_timeout = "60s"    # default 60s
drain_timeout = "30s"     # default 30s
auto_rollback = true      # default true
```

```sh
relish history web               # what shipped when
relish rollback web              # back to the previous version
relish cancel-deploy <OPERATION_ID>
```

A new instance that doesn't turn healthy within `health_timeout` fails the
deploy, and with `auto_rollback` (the default) Bun restores the previous
version. With `auto_rollback = false` the deploy stops where it failed.
`cancel-deploy` takes the operation id that `apply` prints and waits for the
in-flight step to finish. `examples/phase-1/proc-restarts.toml` shows the
health checker restarting an app that goes unhealthy.

## Jobs and cron

```toml
[job.migrate]
image = "ghcr.io/example/api:1.4.2"
command = ["./migrate", "up"]
run_before = ["app.api"]  # finish before the api starts

[job.report]
image = "ghcr.io/example/report:2"
schedule = "0 3 * * *"    # cron, UTC
```

A failed job retries up to three times. A job whose exit Bun couldn't observe
(say, the node crashed) is `unknown`, and an ordinary apply won't rerun it,
because it may already have done its work. Check, then ask explicitly:

```sh
relish apply jobs.toml --rerun-jobs
```

Cron doesn't catch up: firings missed while a node was down are skipped.

## More shapes

- Jobs: `examples/phase-1/proc-job-success.toml`,
  `proc-job-failure.toml` (watch the retries)
- Init containers: `examples/phase-1/proc-init-container.toml`
- Volumes (managed + host path): `examples/phase-1/proc-volumes.toml`
- Several apps per file: `examples/phase-1/proc-multi-app.toml`
- Batch scheduling: `relish batch examples/phase-8/batch-jobs.toml`, then
  `relish batch-status <ID> --wait`
