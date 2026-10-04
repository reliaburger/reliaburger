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

CPU follows Kubernetes: a bare number is cores (`cpu = "2"`, `cpu = "0.5-2"`)
and the `m` suffix is millicores (`"250m"` is a quarter of a core). Memory takes
`Ki`/`Mi`/`Gi`/`Ti`, and a bare number is bytes. A single value sets the request
and the limit to the same thing.

Names (apps, jobs, namespaces) are lowercase DNS labels. Secrets go in `env`
encrypted; see `security`. Ingress, firewall and egress rules are in
`networking`, metrics scraping in `observability`.

## Autoscaling

An `autoscale` block lets the leader move `replicas` between `min` and `max`:

```toml
[app.api.autoscale]
metric = "cpu"            # or "memory" (needs a memory request)
target = "70%"            # of each replica's request
min = 2
max = 10
```

`min` must be at least 1. There's no scale-to-zero: CPU and memory come from
running replicas, so an app at zero would have nothing to scale back up on, and
`relish apply` refuses `min = 0`. To park an app at zero by hand, use
`relish stop`.

## Namespace quotas

A `[namespace]` block gives a namespace a budget:

```toml
[namespace.shop]
cpu = "4"                 # summed requests of every replica
memory = "8Gi"
max_apps = 10
max_replicas = 30
```

The scheduler checks the budget when it places an app, not when you apply it.
`relish apply` accepts an app that doesn't fit; it just isn't placed, and
running apps are never evicted to make room. The app says why everywhere you'd
look:

```text
$ relish status
no workloads running

big (namespace shop) is not placed, blocked: namespace "shop" would exceed CPU quota: 3000+2000 > 4000m

$ relish inspect big
App: big (namespace shop)
  Replicas:  2 desired, 0 running
  Blocked:   namespace "shop" would exceed CPU quota: 3000+2000 > 4000m
```

The dashboard marks the app `blocked` with the same reason on its page, and
`relish wtf` raises a `quota-blocked` warning. The reason lives in the
council, so any node gives the same answer. Raise the budget, or shrink or
delete other apps in the namespace, and the next scheduling pass (a few seconds
later) places the app and clears the reason.

## The everyday loop

```sh
relish apply app.toml            # deploy (or converge) everything in the file
relish apply app.toml --dry-run  # preview; no-agent output states its offline assumption
relish lint app.toml             # validate only
relish logs web -f               # stream logs from every node (--tail 20 for the last 20)
relish exec web env              # run a command inside an instance, on whichever node runs it
relish top                       # every workload on every node, with CPU and memory
relish inspect web               # every instance on every node, desired vs running
relish stop web                  # scale to zero; `relish apply` starts it again
relish delete web                # remove the app from the cluster
```

`apply` and `deploy` check the file's syntax and field values locally. In a cluster, the leader checks permission/build namespace references against both the file and namespaces already created. You don't need to repeat a namespace declaration, which could replace its existing budget. `relish lint` works offline, so it requires those references to be declared in the file. Applying a build declaration validates its namespace; use `relish build` to execute the build.

In a cluster, `relish stop` and `relish delete` return as soon as the council
has recorded the change. Each node then retires its instances on its own, and
a node that's unreachable does so when it comes back. Run `relish status` (or
`relish inspect web`) to watch the instances go.

Commands that take an app name also take `--namespace` (default `default`).

## Instance names

Every replica gets a name you'll see in `status`, `inspect`, logs and metrics:
`default__web-0`, `default__web-1`, and so on, namespace first. The number is
the replica's ordinal, and the leader hands them out across the whole cluster,
so three replicas on three nodes are `-0`, `-1` and `-2` and no two share a
name. A replica keeps its ordinal for as long as it lives. If its node dies,
the replacement on another node takes the same name. Scaling up adds the
lowest free ordinals, scaling down retires the highest, and a daemon set's
replica keeps its number while its node stays eligible.

A rolling or blue-green deploy names the new instances after their generation
and keeps the ordinal: `default__web-1` becomes `default__web-g1-1`. A
standalone node with no cluster numbers its replicas from 0.

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
compares complete desired specifications, including replicas, environment,
resources, namespace quotas and permissions. Namespace-qualified workloads have
separate identities. Incomplete live evidence is shown as `?` (`unknown` in JSON),
never as unchanged. With no reachable agent, the output states that creates are
an offline assumption; JSON includes `comparison_available: false`. If a live
agent answers but cannot supply the comparison, the command fails.

A compiled manifest currently keys apps, jobs and builds by bare name. If two
resources of the same kind have the same name in different namespaces,
compilation fails rather than dropping one. Apply those manifests separately
or give their resources distinct names. Duplicate definitions in one namespace
use the later file in sorted order and produce a warning.

GitOps resolves its watched tree with these same defaults and directory rules.
The configured watch directory is the root; only directories below it contribute
namespaces, and defaults outside that root are not inherited. GitOps refuses
duplicate resource definitions, while `relish compile` warns about overrides
within one namespace. Parse, read and namespace-identity errors refuse the whole
tree in both paths.

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

A changed `cpu` or `memory` request rolls in place too, on every node that
still has room for it once the old instance's share is counted as freed. A
replica whose node no longer has room moves to one that does. A changed
`placement.required` moves the replicas on nodes that no longer match and
leaves the rest where they are.

An app with a managed volume always rolls stop-first: Bun stops the old
instance before it starts the new one, whatever `strategy` and `max_surge`
say, because both would write the same volume directory. Expect a moment
of unavailability on every redeploy of such an app. Host-path volumes don't
change the rollout.

```sh
relish history web               # what shipped when, on every node
relish rollback web              # back to the previous version
relish cancel-deploy <OPERATION_ID>
```

`relish history` asks every node, because each records its own rollout: you
get one row per node per deploy, with a NODE column. A node that doesn't answer
prints `warning: history incomplete: …` on stderr.

`relish rollback` reads the same cluster-wide history, from whichever node you
ask. The previous version is the newest one that differs from what the app
runs now, so the copies of the current deploy that every node recorded don't
count. A rollback changes the version and keeps the scale: an app running
three replicas still runs three, and a deploy that only changed `replicas`
isn't a version to roll back to.

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
  Batch jobs must be non-scheduled and have no `run_before` declarations. Use
  ordinary apply for cron schedules and jobs that gate apps in the same manifest.

Repeated batches may reuse a logical job label; each gets a distinct execution
identity. Status and logs retain the original namespace and label for token
scope. Select an explicit instance to read one run, especially when a label also
names an older opaque execution. The cluster retains execution ownership after
terminal progress records expire, and each runner retains replay proof after
retirement. This history is finite: the cluster index is limited to 131,072
entries or 32 MiB, and each runner's checkpoint to 16 MiB. Full history refuses
new admissions while preserving existing runs and their replay fences. Further
admissions then require a fresh cluster.

A batch admission response of 503 can mean its acknowledgement was lost after
publication started. That original owned attempt may still run. Retrying the
same internal dispatch preserves the attempt and cannot launch a second one.
An ownership metadata timeout admits no work. OCI executions whose container
resource absence is unproven retain their full replay record, using more of
the finite node history than compact retirement proofs.

Directory defaults support `image`, `memory`, `cpu`, `[env]` and `[deploy]`. Child directories override individual fields; environment and deployment tables merge by key, and explicit workload values win. Common fields apply to jobs too, while deployment strategies apply to apps. Unknown defaults keys fail compilation.

For a batch log follow, select the execution's instance ID. A cluster reader
checks the committed allocation and follows that worker, even when an ordinary
app has the same submitted label. It refuses a missing or unadvertised worker,
or an allocation whose progress record has already been pruned, before opening
an SSE or WebSocket stream. Stored log queries still use the original logical
label; retained replay ownership does not itself supply a remote follow route.
Predictable node policy rejection happens before the whole group's checkpoint,
so correcting a rejected member does not leave healthy jobs permanently fenced.
