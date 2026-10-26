# Deploy an app

Workloads are TOML. An app is a long-running service; a job runs to
completion (retried up to 3 times with backoff).

```toml
[app.web]
exec = "target/debug/testapp"
command = ["--mode", "healthy", "--port", "8080"]
port = 8080

[app.web.health]
path = "/healthz"
interval = 10             # seconds between probes (default 10)
timeout = 5               # seconds per probe (default 5)
```

An app runs either a host binary (`exec`, or an inline `script`) or an
`image`. A host app runs on the node itself, and only if the node allows the
binary in `[process_workloads] allowed_binaries`. An image app needs a
container runtime: runc pulls `image` from a registry and runs it the way
Kubernetes would (`command` replaces the image's entrypoint, `args` its default
arguments). The process runtime (`bun --runtime process`) runs host apps only
and refuses an image app with `needs a container runtime`. Try it from a
source checkout, with Bun started from the repository root:

```sh
bun --runtime process --config examples/node/process-runtime.toml
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
`networking`, metrics scraping in `observability`. Every `[app.*]` field is
listed in `reference`.

## Health checks

```toml
[app.api]
image = "ghcr.io/example/api:1.4.2"

[app.api.health]
path = "/healthz"
port = 8081               # default: the app's `port`
interval = 10             # seconds between probes (default 10)
timeout = 5               # seconds per probe (default 5), at most interval
threshold_unhealthy = 3   # failed probes before Unhealthy (default 3)
threshold_healthy = 1     # passing probes before Running (default 1)
initial_delay = 0         # seconds before the first probe (default 0)
```

`health.port` works without an app `port`, so an app can probe a separate
admin listener. `relish apply` refuses a health check with neither, a zero
interval, timeout or threshold, and a `timeout` longer than the
`interval`. A new instance that never passes is given up after 60 failed
probes (about ten minutes at the default interval) and restarted.

## Init containers

```toml
[app.api]
image = "ghcr.io/example/api:1.4.2"

[[app.api.init]]
image = "ghcr.io/example/migrate:2.1"   # default: the app's image
command = ["migrate", "up"]
```

Init containers run one after another before the app starts, each to
completion; a failure (or 300 seconds without finishing) fails the instance.
Each runs its own `image` (pulled, trust-checked and pinned to a digest like
the app's), or the app's when it has none. It gets the app's environment,
with secrets decrypted, and the app's volume and config-file mounts, so a
migration can read `DATABASE_URL` and a config generator can write a volume.
The volume stays the app's: an init container doesn't take it over, so run it
as root (most images do) or as the app's user.

An `exec` or `script` app's init container that names no image runs its
command on the host, like the app. The node's `[process_workloads]
allowed_binaries` must name the command's first word by its full path, or the
deploy is refused, and the scheduler only places the app where it does:

```toml
[app.worker]
exec = "/usr/bin/python3"
command = ["worker.py"]

[[app.worker.init]]
command = ["/bin/sh", "-c", "mkdir -p /tmp/worker"]   # /bin/sh must be allowed
```

`relish lint` and `relish apply` warn about a host init whose command starts
with a bare name such as `echo`, and suggest the full path. It's a warning,
not a refusal: a node that allowlists the bare name word for word runs it,
looking it up on its `PATH`.

```text
warning: app default/worker: init container command "echo" is a bare name, but it runs on the host, where allowed_binaries names binaries by full path; use the full path (`command -v echo` on the node prints it)
```

## Config files

```toml
[app.api]
image = "ghcr.io/example/api:1.4.2"

[[app.api.config_file]]
path = "/etc/api/config.yaml"
content = """
log_level: info
"""
```

Each instance gets its own read-only copy, so two files with the same name in
different directories don't clash, and a rollout never changes the file under
an instance that's already running. The copy goes when the instance does.
`source` instead of `content` mounts a file from the node itself; see
`images-and-volumes`.

## What a field can't do

`relish apply` refuses a field it can't honour, rather than run something
different from the config:

- `gpu` above 0: GPU workloads aren't supported yet
  ([#359](https://github.com/reliaburger/reliaburger/issues/359)).
- `egress.allow_franchise`: cross-cluster egress doesn't exist yet.
- `volumes`, `config_file`, `run_as_user` and `run_as_group` on an `exec` or
  `script` app: a process workload runs on the host, with no mounts and no
  user switch.
- A volume `size` the node can't enforce (see `images-and-volumes`), or a
  changed `size` on a volume that already exists. Those fail the deploy on
  that node.

## Autoscaling

An `autoscale` block lets the leader move `replicas` between `min` and `max`:

```toml
[app.api]
image = "ghcr.io/example/api:1.4.2"

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

The same budget covers jobs. The leader charges each job attempt it lets start
against the namespace's `cpu`, `memory` and GPU budget, after the namespace's
apps, so a job never displaces an app that fits. A job with no room waits
instead of failing, and `relish batch-status ID` says why (`waiting for quota:
namespace "shop" would exceed CPU quota: …`). Running attempts are never
stopped; new ones start as old ones finish. `max_apps` and `max_replicas`
count apps only. See "Execution and capacity limits" in the batch jobs chapter.

A `[namespace]` block can also give the namespace its own secret key with
`secret_key = true`, so its encrypted values don't decrypt anywhere else. See
"A key per namespace" in the security chapter.

## When some replicas don't fit

The scheduler places every replica it can find a node for. Ask for three when
the cluster only has room for two, and you get two, plus a reason that stays
until the third finds a home:

```text
$ relish status
...

web (namespace default) is unschedulable: placed 2 of 3 replicas: no node that can run it has room

$ relish inspect web
App: web (namespace default)
  Replicas:  3 desired, 2 running
  Placed:    node-1 1, node-2 1
  Unplaced:  placed 2 of 3 replicas: no node that can run it has room
```

The reason is the first rule no node passes, in the order the scheduler checks
them: no node is ready, no ready node has the app's `required` labels, none can
enforce its egress or DNS needs or run its host binary, or none has room.
`relish wtf` raises an `unschedulable` warning with the same reason, and the
leader logs it and records a `placement` event once when it starts and once
when it clears. Add a node, free some room, or relax the constraint, and the
next scheduling pass places the rest.

An `exec` or `script` app only goes to nodes that would run it: the node's
`[process_workloads] allowed_binaries` must name the `exec` path (`/bin/sh` for
a script), and `mount_isolation` must be off. A node whose allowlist stops
covering the binary loses the replica to one that still does.

## When the binary isn't there

The allowlist says a node may run a binary. It doesn't say the binary is
installed. Bun checks just before it launches a process app: if the `exec`
path, or the first word of `command`, doesn't exist or isn't executable, the
deploy fails at once with the reason, and the instance goes to `failed`:

```text
$ relish apply ghost.toml
  deploying app ghost (replicas: 1)
  creating instance default__ghost-0
error: API error (status 500): instance default__ghost-0 cannot launch: binary not found: /usr/local/bin/ghost-server

$ relish status
...

default__ghost-0 (ghost, namespace default) failed on node-1: binary not found: /usr/local/bin/ghost-server
```

A bare name like `bun` is looked up on the app's `PATH`, the way a shell would.
A directory, or a file without an execute bit, fails with `binary not
executable` instead. `relish wtf` raises a critical `launch-failed` finding
with the same reason. The same goes for a binary deleted while the deploy is
starting it, and for a running app whose binary is deleted: the app keeps
running, but when it exits, its restart fails at once with the reason instead
of using up its restart attempts. Waiting won't fix this one, so nothing
waits: install the binary, or fix the spec and apply it again. In a cluster,
the node's placement reconciler tries the deploy again on its usual backoff
(five seconds, doubling to five minutes), so installing the binary is enough
on its own.

## When a node stops reporting

A node can stay alive in gossip while its reports stop, say because its
report worker wedged. Its replicas may still be serving, so the leader leaves
them for `[reporting_tree] stale_node_grace_secs` (five minutes by default)
after the reports go stale, then places them elsewhere. Daemon sets get the
same grace. An app with a managed volume never moves off its node this way;
see `images-and-volumes`.

## The everyday loop

```sh
relish apply app.toml            # deploy (or converge) everything in the file
relish apply app.toml --dry-run  # preview; no-agent output states its offline assumption
relish lint app.toml             # validate only
relish logs web -f               # stream logs from every node (--tail 20 for the last 20)
relish exec web env              # run a command once inside an instance; exits with its exit code
relish events --app web          # the cluster's recent events for one app
relish top                       # every workload on every node, with CPU and memory
relish inspect web               # every instance on every node, desired vs running
relish stop web                  # scale to zero; `relish apply` starts it again
relish delete web                # remove the app from the cluster
```

`relish exec` runs its command once, with no stdin and no terminal, so
`relish exec web sh` exits straight away: name the command you want, as in
`relish exec web ls /data`. Relish prints the output and exits with the
command's exit code.

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
relish fmt all.toml --check         # canonical ordering, comments kept; drop --check to rewrite
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

In a cluster the change reaches one node at a time. The leader hands the new
spec to the first node (by node id) that doesn't run it yet and keeps the other
nodes on the previous version until that node's replicas are all running and
healthy on the new one; then the next node goes. Each node rolls its own share
of the replicas with the app's `[deploy]` table, so `max_surge` and
`max_unavailable` count that node's instances, not the whole app's. An app
with one replica per node therefore keeps every other replica serving while
one node rolls. A brand-new app, a node getting its first replica and a change
to `replicas` alone don't wait.

```toml
[app.web]
image = "nginx:alpine"

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

Each node keeps its last 50 deploys per app.

In a cluster, `relish rollback` takes its target from the council, which keeps
the last ten versions of each app that rolled out healthy on every node, so it
works from whichever node you ask and after every node has restarted. The
previous version is the newest one there that differs from what the app runs
now. Without one (a single node, or an app that never finished a rollout) it
falls back to the cluster-wide history. A rollback changes the version and
keeps the scale: an app running three replicas still runs three, and a deploy
that only changed `replicas` isn't a version to roll back to.

A new instance that doesn't turn healthy within `health_timeout` fails the
deploy. With `auto_rollback` (the default) the node puts the previous version
back without ever running fewer replicas than it started with: if old
instances had already retired, it restarts them on the previous spec first and
only then stops the new ones. If even the previous spec won't start, it keeps
the healthy new instances serving and reports the rollout halted. With
`auto_rollback = false` the deploy stops where it failed, both versions
running.

In a cluster a failed rollout also marks the app `RolloutFailed`. The leader
keeps the desired spec as you applied it but stops handing it to nodes: the
nodes that already run it keep it, the rest stay on the previous version, and
nothing retries it. `relish status` prints the node and the reason:

```text
web (namespace default) RolloutFailed on node-2: default__web-g3-0 failed its health check; apply a different spec or roll back
```

`relish wtf` reports it as a warning. Applying the same spec again (a GitOps
re-sync, say) doesn't clear it, since it would only fail the same way, and
`relish apply` says so:

```text
warning: app 'web' in namespace 'default' is RolloutFailed on node-2 (default__web-g3-0 failed its health check); the same spec again isn't retried. Change the spec, or run relish rollback
```

Any change to the spec clears the mark and starts a new rollout: a fixed image,
`relish rollback web`, or even just a different `replicas`. A scale change
releases the same version node by node again, so if it's still broken it fails
again and is marked again.

The node reports the failure to the leader with the cluster's service token.
A plaintext development cluster with no master key has no service token, and
until its first API token exists nobody holds a credential at all, so there
the leader takes the report without one. It takes it only for a node the app
is placed on, and once any credential exists (a master key or a first token)
it requires the service token again. If `bun` logs
`the leader refused the rollout failure of default/web` with a 401 or 403,
the cluster has a token but its nodes have no master key to sign with: set
`[security] master_key_path` on every node.

`cancel-deploy` takes the operation id that `apply` prints and waits for the
in-flight step to finish. It works on a single node only: a cluster `apply`
returns once the council has recorded the spec, and prints no operation id,
because each node runs its own deploy later. `examples/phase-1/proc-restarts.toml`
shows the health checker restarting an app that goes unhealthy.

To watch all of this happen, run the rollout demo from a checkout. It starts a
one-node cluster on the process runtime, applies
`examples/phase-1/proc-rollout.toml` as v1, rolls it to v2, rolls it back, then
applies a v3 that fails its health check and shows it rolled back and marked
`RolloutFailed`. Every step checks what the app really serves:

```sh
scripts/demo/rollout.sh
```

With `DEMO_PLAINTEXT=1` in its environment the demo runs the same steps on a
plaintext cluster with no master key and no token, and checks the node stops
retrying v3 once it's marked.

The demo measures what it costs as it goes. Before each step it prints Bun's
latest memory and CPU, and its workloads', on one line, and it ends with each
measure's average and peak:

```text
    resources: bun 109.5 MiB, 66.7 % CPU · 2 workloads 23.8 MiB, 1.0 % CPU

==> Resource use: 25 samples over 24 s on demo-node
                     average           peak
  bun CPU               70.4 %         94.6 %
  bun memory           109.7 MiB      117.3 MiB
  ...
```

CPU is a percentage of one core. The numbers come from the operating system
(`/proc` on Linux, `ps` on macOS), sampled once a second, and anything it
can't measure is left out rather than guessed: network traffic isn't
measured, because a one-node demo talks to itself over loopback. Workloads
are everything Bun started: each app, the owner process that keeps it
running, and a GitOps sync's `git`. Those numbers are from a debug build on a
busy laptop, which is what the demo builds by default. For numbers you'd
quote, point `BUN` at a release build (`cargo build --release --bin bun`). Set
`DEMO_RESOURCES_OUT` to a directory to keep the samples as `resources.csv`,
with `cpu.svg` and `memory.svg` charts that mark each step on the time axis,
or `DEMO_RESOURCES=0` to turn the measuring off. Every demo in
`scripts/demo/` does the same, and so does the [five-minute
tour](08_five-minute-tour.md)'s script.

`examples/phase-1/container-rollout.toml` is the same story with real
containers and `strategy = "blue-green"`, for a cluster with runc.

`relish lint` and `apply` refuse a `[deploy]` table they can't honour: a
`strategy` other than `rolling` or `blue-green`, and a `drain_timeout` or
`health_timeout` that isn't a whole number of seconds, minutes or hours
(`"90s"`, `"5m"`, `"1h"`; not `"1m30s"` or `"500ms"`).

## Jobs and cron

```toml
[job.migrate]
image = "ghcr.io/example/api:1.4.2"
command = ["./migrate", "up"]
run_before = ["app.api"]  # finish before the api starts

[app.api]
image = "ghcr.io/example/api:1.4.2"

[job.report]
image = "ghcr.io/example/report:2"
schedule = "0 3 * * *"    # cron, UTC
```

A `run_before` migration must name an app in the same manifest and effective
namespace. The example's migration needs an `[app.api]` declaration too. Apply
migrations and their dependent apps together. Cluster apply waits for a
positively observed, durably recorded zero exit before publishing the new app
revision. A failed migration leaves the old revision in place and does not
retry automatically. A corrected apply can start a new generation after a
known failure is settled.

The apply is a durable deployment operation. If the client disconnects or the
leader changes, the new leader resumes it from the same runs. Cancellation is
checked after the migration settles and immediately before the new revision is
published; a publication already submitted to Raft may still commit. A write
timeout returns uncertainty rather than a refusal, and never authorises an
automatic rerun.

The cron example works on a cluster as well as a standalone node. Once the
apply publishes its apps, the leader registers each schedule as a durable
definition and claims every matching UTC minute itself, so a leader change
neither loses nor repeats an occurrence. An app or job whose name an unsettled
deployment still owns is refused until that deployment settles, and apply
refuses an overlapping app or job while an earlier run of it still has an
uncertain outcome. Leadership changes or recovery preserve that fence, and
nothing replays an uncertain migration on its own: after checking its side
effects, acknowledge it with `relish batch replay` (see [batch
jobs](14_batch-jobs.md)).

A failed ordinary job gets four attempts in all, unless it sets `max_attempts`.
A job whose exit Bun couldn't observe (say, the node crashed) is `unknown`, and
nothing reruns it automatically, because it may already have done its work.
Cron skips that job's occurrences until the outcome is settled. Check what the
job did, then acknowledge the exact grant the status shows:

```sh
relish --output json batch-status 42
relish batch replay 42 --node worker-2 --grant-digest DIGEST --acknowledge-side-effects
```

`relish apply jobs.toml --rerun-jobs` is a different thing: it starts a fresh
apply operation, so jobs that already succeeded run again. It doesn't clear an
unknown outcome; while one remains, the cluster refuses the new run with
"acknowledge replay before another admission".

Cron doesn't catch up: firings missed while the cluster had no leader are
skipped. The policy fields (`max_attempts`, `task_timeout_secs`, `overlap` and
`replay_unknown`) and the cron dialect are in [Batch jobs](14_batch-jobs.md).

## Task arrays

When the same program needs to run thousands or millions of times, submit one
template and a count, with `{index}` wherever the task's number goes.

```sh
relish run --batch render --count 100000 --image renderer:v1 --cpu 250m-500m --memory 256Mi-512Mi -- /renderer --frame {index}
relish batch watch 12
relish batch results 12 --failed --limit 20
relish batch logs 12 --index 4071
relish batch cancel 12
```

Use the ID returned by submission. Each task has its own retry, timeout and
stable identity. Applications and actual batch attempts share CPU and memory
requests; a queued chunk does not consume its full task count's resources.
Image tasks use rootful Linux isolation and runtime limits. Host tasks require
an allowlisted binary, disabled mount isolation and the owned process runtime;
that backend refuses explicit resource limits it cannot enforce.

Mixed-profile manifests, rates, duration distributions, indexed pages,
retention and at-least-once effects are covered in `relish manual batch`.
Only failed tasks retain bounded output. Detail stays on workers and can be
lost with a worker's disk; accepted aggregate counts remain replicated.

## More shapes

- Jobs: `examples/phase-1/proc-job-success.toml`,
  `proc-job-failure.toml` (watch the retries)
- Init containers: `examples/phase-1/proc-init-container.toml`
- Volumes (managed + host path): `examples/phase-1/container-volumes.toml`
  (volumes need a container runtime)
- Several apps per file: `examples/phase-1/proc-multi-app.toml`
- Rolling deploys and rollback: `examples/phase-1/proc-rollout.toml`
  (`scripts/demo/rollout.sh` runs it), `container-rollout.toml` (blue-green)
- An encrypted secret: `examples/phase-1/container-secrets.toml`
- A migration that gates its app: `examples/phase-8/proc-run-before.toml`
- A host binary as an app: `examples/phase-8/proc-exec-app.toml`
- Batch scheduling: `relish batch examples/phase-8/container-batch-jobs.toml`,
  then `relish batch-status <ID> --wait`
  Batch jobs must be non-scheduled and have no `run_before` declarations. Use
  ordinary apply for cron schedules and jobs that gate apps in the same manifest.

Every job, whether it comes from `relish apply`, `relish batch` or a cron
schedule, runs through the same lifecycle: the leader records it as a run and
grants its tasks to nodes with room. Repeated batches may reuse a job name;
each submission gets its own run ID. Select `--instance run-ID` to read or
follow one run's logs; a follow goes to the node that holds the run's task.

Apps and jobs share each node's capacity. A node keeps one execution budget for
its CPU and memory requests: app replicas, running task attempts and reusable
executors all lease from it, and a queued task holds nothing until it starts.
Each node's state report carries what its task attempts and executors hold,
apart from its app requests, and the leader adds both together before it places
a replica. So a node that jobs have filled looks full, and the leader puts the
replica somewhere else. If jobs take the room after a replica was placed, the
node can't start it: its agent reports the refusal, and the leader moves the
replica to another node instead of waiting there. Running attempts are never
stopped to make room for an app.

Directory defaults support `image`, `memory`, `cpu`, `[env]` and `[deploy]`. Child directories override individual fields; environment and deployment tables merge by key, and explicit workload values win. Common fields apply to jobs too, while deployment strategies apply to apps. Unknown defaults keys fail compilation.
