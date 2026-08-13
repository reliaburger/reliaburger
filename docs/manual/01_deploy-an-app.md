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
interval = 10
timeout = 5
```

ProcessGrill runs `command` directly and ignores `image`; real runtimes
(runc, Apple Container) pull `image` from a registry. Try it:

```sh
relish manual examples
relish apply examples/phase-1/proc-minimal-app.toml
relish status
```

## The everyday loop

```sh
relish apply app.toml            # deploy (or converge) everything in the file
relish apply app.toml --dry-run  # preview without an agent
relish lint app.toml             # validate only
relish logs web -f               # stream logs (--tail 20 for the last 20)
relish exec web env              # run a command inside an instance
relish top                       # workload table: state, PID, restarts (not live CPU/memory)
relish inspect web               # full detail
relish stop web                  # stop all instances
```

## Rolling deploys

```sh
relish deploy app.toml           # health-gated rolling replacement
relish history web               # what shipped when
relish rollback web              # back to the previous version
```

A failed health check pauses the roll and `rollback` restores the previous
version. `examples/phase-1/proc-restarts.toml` shows the health checker
restarting an app that goes unhealthy.

## More shapes

- Jobs: `examples/phase-1/proc-job-success.toml`,
  `proc-job-failure.toml` (watch the retries)
- Init containers: `examples/phase-1/proc-init-container.toml`
- Volumes (managed + host path): `examples/phase-1/proc-volumes.toml`
- Several apps per file: `examples/phase-1/proc-multi-app.toml`
- Batch scheduling: `relish batch examples/phase-8/batch-jobs.toml`, then
  `relish batch-status <id> --wait`
