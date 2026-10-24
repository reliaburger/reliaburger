# Writing a controller

Some jobs need code. Fail a database over when its primary dies, take a
backup before an upgrade, move a replica off a node that keeps flapping. In
Kubernetes you'd write an operator: a custom resource, a controller and an
SDK. In Reliaburger a controller is an ordinary app holding an API token. It
reads the cluster through the API, decides, and applies ordinary apps. There's
no SDK and no new kind of resource, just a contract with three parts:

- an `owner` on each app the controller manages, so `relish apply` and GitOps
  leave it alone;
- a token scoped to the controller's namespace, so it can't touch anything
  else;
- documented JSON for the calls it makes, pinned by tests.

This arrived in 0.2.4. The `owner` field is new desired state, so a cluster
made with an earlier release can't roll to it; start a fresh one
(`operations` explains why).

## A controller is an app with a token

Give the controller a token of its own, scoped to its namespace:

```sh
relish token create --name db-failover --role deployer --namespaces pg
```

A `deployer` scoped to `pg` can apply, stop, delete and roll back apps in
`pg`, and nothing outside it. It can't write namespaces, permissions or
policies, which need an unscoped admin. Its reads are scoped the same way:
status, desired apps and events show `pg` only. Narrow it further with a
`[permission]` block (see `security`).

Run the controller wherever it can reach a node's API: as an app in the
cluster, or on a host. Hand it the token the way you'd hand any app a
secret, encrypted with `relish secret encrypt`. It reads the same variables
`relish` does: `RELIABURGER_ENDPOINT`, `RELIABURGER_TOKEN` and, for
`https://`, `RELIABURGER_CA_CERT`.

## Owners

A controller claims an app by applying it with its token's name as the
`owner`:

```toml
[app.db-a]
image = "postgres:17"
namespace = "pg"
owner = "db-failover"
```

From then on the app is the controller's. The rules:

| Who | Does what | What happens |
|-----|-----------|--------------|
| Anyone | Applies an app declaring `owner = "X"` | Allowed only with token `X` or an admin token; otherwise 403 |
| Anyone | Applies an app owned by `X`, declaring `owner = "X"` | Written |
| Anyone | Applies an app owned by `X`, declaring no owner | Skipped, with a note; the rest of the file applies |
| Anyone | Applies an app owned by `X`, declaring `owner = "Y"` | 409: release it first |
| Token `X` or an admin | Stops, deletes, rolls back or releases it | Allowed |
| Anyone else | Stops, deletes, rolls back or releases it | 409, even with `--force` |
| GitOps | Syncs a repository that declares, changes or drops it | Leaves it alone: no update, adoption or prune |
| GitOps | Syncs a repository that declares an `owner` | Refuses that app, named in the sync's partial result |

The skip is what stops the fight. Re-apply the directory a template came from,
and the apps a controller has since taken over stay as the controller left
them:

```console
$ relish apply -f pg/
app 'db-a' in namespace 'pg' is owned by 'db-failover'; skipped, so this apply doesn't fight it
```

To take an app back, release it. The owner's token or an admin can:

```sh
relish release db-a --namespace pg
```

A released app is an ordinary app again: the next `relish apply` or GitOps
sync writes it. Claiming or releasing changes who manages the app, not what
runs, so neither restarts an instance or starts a rollout. A rollback keeps
the app's current owner.

A policy's `default` can't fill in `owner`: claiming an app is for the
controller's token or an admin, and a default would claim it for everyone who
applies. Policies see the owner as `app.owner`, so a platform team can, for example,
insist that only the database controller writes apps in `pg`:

```toml
[policy.pg-belongs-to-the-controller]
match = "app.namespace == 'pg'"
deny = "!has(app.owner) || app.owner != 'db-failover'"
message = "apps in pg are managed by db-failover"
```

A controller's writes go through the same apply path as everyone's, so they
are judged by the cluster's policies and recorded in the audit log under the
controller's token. `relish policy audit` judges an owned app as the
controller's token too, since that's who writes it next.

## The calls a controller makes

| Call | What it's for |
|------|---------------|
| `POST /v1/apply` | Write apps: a TOML body, the same files `relish apply` takes |
| `POST /v1/stop/{app}/{namespace}` | Scale an app to zero until the next apply |
| `POST /v1/delete/{app}/{namespace}` | Remove an app |
| `POST /v1/release/{app}/{namespace}` | Clear an app's owner |
| `GET /v1/diagnostics/apps` | Every app the token can see: owner, replicas, placements |
| `GET /v1/status?cluster=true` | Every instance the token can see, and its state |
| `GET /v1/events` | Recent events; with `?follow=true`, a live stream |

Every call takes `Authorization: Bearer <token>`. A refusal is a 4xx status
with a reason, usually as `{"error": "..."}` JSON and sometimes as plain text,
so decide on the status code and log the body. The ones a controller should
expect:

| Status | Means |
|--------|-------|
| 400 | The body didn't parse or validate |
| 401 | No token, or one the cluster doesn't know |
| 403 | The token's role, scope or permission doesn't allow it, or it declared an owner it can't speak for |
| 409 | Someone else owns the app, or (for a release) nobody does |
| 422 | A policy denied the apply: `{"error", "policy_denials", "policy_warnings"}` |
| 503 | No council leader right now; try again |

### Applying

`POST /v1/apply` takes a TOML manifest and answers `200` with a stream of
server-sent events, one JSON object per `data:` line. Read it to the end:
the outcome is the last event.

```json
{"type": "Progress", "message": "app db-a: committed to the cluster"}
{"type": "Complete", "created": 1, "instances": []}
```

| `type` | Fields | Means |
|--------|--------|-------|
| `Progress` | `message` | A step: a write committed, a skipped app, a policy warning, a bound image |
| `Complete` | `created`, `instances` | Done; `created` counts the writes committed |
| `Error` | `message` | Stopped here; earlier writes stay committed |

Applying the same spec again changes nothing and starts no rollout, so a
controller can apply what it wants on every check.

### Stop, delete and release

Each answers `202` with `{"status": "stopping"}`, `{"status": "deleting"}` or
`{"status": "released"}`. Deleting and stopping are asynchronous: the nodes
retire the instances afterwards.

### Desired apps

`GET /v1/diagnostics/apps` answers a JSON array, one object per app the
token can see, sorted by namespace and name:

```json
[{"app": "db-a", "namespace": "pg", "desired_replicas": 1, "scheduled_replicas": 1,
  "placements": {"node-2": 1}, "service_port": 5432, "owner": "db-failover"}]
```

| Field | Means |
|-------|-------|
| `app`, `namespace` | The app |
| `desired_replicas` | Replicas the spec asks for (one per node for `"*"`) |
| `scheduled_replicas` | Replicas the scheduler has placed |
| `placements` | Placed replicas per node name |
| `service_port` | The app's `port`, or `null` |
| `owner` | The token that owns it; absent when nobody does |
| `blocked` | Present when the namespace quota has no room |
| `unschedulable` | Present when only some replicas found a node |
| `volume_home_away` | Present when the node holding its volume is out of the cluster |
| `volume_homes` | The nodes holding its managed volumes; absent when it has none |
| `rollout_failed` | Present when the current spec failed to roll out |

### Instances

`GET /v1/status?cluster=true` answers a JSON array, one object per instance
on every node:

```json
[{"node": "node-2", "id": "db-a-0", "app_name": "db-a", "namespace": "pg",
  "state": "running", "restart_count": 0, "host_port": 31002, "pid": 4242}]
```

| Field | Means |
|-------|-------|
| `node` | The node it runs on |
| `id` | The instance |
| `app_name`, `namespace` | Its app |
| `state` | `pending`, `preparing`, `initialising`, `starting`, `health-wait`, `running`, `unhealthy`, `stopping`, `stopped` or `failed`. `running` means it passes its health check |
| `restart_count` | How often it restarted |
| `host_port`, `pid` | Its published port and process, or `null` |
| `exit_code`, `reason` | Present for a stopped or failed instance, when known |
| `runtime_unknown` | Present and `true` when the runtime's evidence was late, so a `null` is unknown |

A node that doesn't answer fails the whole call with a 503 rather than
leaving its instances out, so a controller never mistakes a quiet node for
dead instances.

### Events

`GET /v1/events` answers the newest events across the cluster, oldest first.
`?limit=N` (default 100), `?app=NAME` and `?severity=warning` narrow it.

```json
{"events": [{"sequence": 41, "timestamp": 1792000000, "kind": "health",
  "severity": "warning", "app": "db-a", "namespace": "pg", "node": "node-2",
  "message": "db-a-0 failed its health check"}],
 "warnings": []}
```

| Field | Means |
|-------|-------|
| `sequence` | Increases per node; a restarted node starts again |
| `timestamp` | Unix seconds |
| `kind` | `deploy`, `restart`, `health`, `stop`, `job-completed`, `job-failed`, `job-skipped`, `job-cancelled`, `job-unknown`, `alert`, `fault`, `token`, `secret`, `identity`, `operation`, `refusal` or `placement` |
| `severity` | `info`, `warning` or `critical` |
| `app`, `namespace`, `node` | What it's about, or `null` |
| `action`, `principal`, `details` | Present on audited actions: what was done, by which credential, and its facts |
| `message` | For people, not for parsing |
| `warnings` | One line per node whose events are missing |

`?follow=true` streams new events as they happen, from every node, each as a
`data:` line holding one event object. A scoped token sees only events about
apps and namespaces in its scope, in both forms; events that name no
namespace, such as a token being created, are for unscoped tokens.

Treat events as a hint to look again, not as the truth. A stream can drop
while a node restarts, and an event can arrive twice. Re-read desired apps and
instances on a timer too, and decide from those.

## How stable is this?

Every shape on this page is pinned by a snapshot test
(`src/bun/api/contract_tests.rs`), so a change to one is deliberate and lands
with a change to this page. Before 1.0, any release may still change them, as
it may any other format. Ignore fields you don't recognise: new ones appear
without notice.

## An example

`examples/controller/main.rs` is a small controller that uses nothing but this
page. It keeps one of two apps running, a cold-standby pair pinned to
different racks (`examples/controller/pair.toml`). Only the active app exists.
When it has had no running instance for a few checks in a row, the
controller applies the standby and deletes the app that failed. Which app is
active lives in the cluster, so the controller can restart at any time; its
only memory is the count of missed checks.

It checks every five seconds, and `CONTROLLER_THRESHOLD` (3 by default) sets
how many misses in a row make a failover. Keep the product longer than it
takes your cluster to place and start the app. Otherwise a member that's
still starting counts as failed, and the controller swaps the pair back and
forth without either one ever running.

```sh
relish token create --name db-failover --role deployer --namespaces pg
```

Then, with the printed token:

```sh
RELIABURGER_TOKEN=rbrg_... CONTROLLER_NAME=db-failover CONTROLLER_NAMESPACE=pg \
  CONTROLLER_MANIFEST=examples/controller/pair.toml cargo run --example controller
```

The pair can also come from a template. `examples/controller/postgres-pair.toml`
declares a `[template.postgres-member]` and a `[use.*]` for each member. Given
a manifest with `[use.*]` tables, the controller applies only the active
member's `use` and lets the cluster expand it, passing its token's name as the
`owner` parameter and its namespace as `namespace`. So a template it drives
takes those two parameters, and each `use` names its app with `name`:

```sh
RELIABURGER_TOKEN=rbrg_... CONTROLLER_NAME=db-failover CONTROLLER_NAMESPACE=pg \
  CONTROLLER_MANIFEST=examples/controller/postgres-pair.toml cargo run --example controller
```

The app it writes is an ordinary app with the template's name in its
provenance, judged by the cluster's policies like any other.

The 0.2.4 exit test ([#1126](https://github.com/reliaburger/reliaburger/issues/1126))
runs this controller and this template on a three-node cluster with the
platform policies in force, kills the node the active member runs on, and
checks that the controller brings up the standby on another rack. It's
deliberately simple, though. The standby starts with no data. A real database
failover also streams the data to the standby, promotes it and fences the old
primary, and that work belongs to the database and the controller you write,
not to the contract.
