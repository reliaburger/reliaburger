# Coming from Kubernetes

You don't have to rewrite your manifests to try Reliaburger. `relish apply`
takes Kubernetes YAML as it is, converts it in memory and tells you what it had
to change:

```sh
relish apply -f deploy/podinfo.yaml
relish apply -f https://reliaburger.com/demo/podinfo.yaml
relish apply -f deploy/podinfo.yaml --dry-run
```

A file counts as Kubernetes when it has `apiVersion:` and `kind:` lines, or
is a JSON object with `"apiVersion"` and `"kind"`; anything else is read as
Reliaburger TOML. URLs must be `https://` (redirects included), answer within
30 seconds and stay under 1 MiB. The migration report goes to stderr before
anything is applied.

`relish lint`, `relish deploy`, `relish diff` and `relish compile` read a
Kubernetes file the same way, so you can check or compare one before you apply
it:

```sh
relish lint deploy/podinfo.yaml
relish diff deploy/podinfo.yaml
```

## What converts

Eleven kinds: Deployment, StatefulSet, DaemonSet, Service, Ingress, ConfigMap,
Secret, Job, CronJob, HorizontalPodAutoscaler and Namespace. Anything else
(NetworkPolicy, PersistentVolumeClaim, CRDs) lands in the report's Dropped list
and isn't applied.

| Kubernetes | Becomes |
|------------|---------|
| Deployment, StatefulSet | `[app.NAME]` (StatefulSet loses ordering and stable network ids) |
| DaemonSet | `[app.NAME]` with `replicas = "*"` |
| Service | merged into the workload its `selector` selects: its first port, via `targetPort` |
| Ingress | the app its backend Service selected: `ingress` |
| HorizontalPodAutoscaler | the app its `scaleTargetRef` names: `autoscale` |
| ConfigMap | flattened into `env` when a workload reads it with `envFrom` |
| Job, CronJob | `[job.NAME]`, with the cron `schedule`, `backoffLimit` as `max_attempts` and `activeDeadlineSeconds` as `task_timeout_secs` |
| Namespace | `[namespace.NAME]` |

From the pod template it takes the first container's image, `command`, `args`,
`workingDir`, plain `env` values, CPU and memory requests and limits,
`nodeSelector` (as `placement.required`), init containers, `runAsUser` and
`runAsGroup`, and an `httpGet` readiness probe as the health check. The
`prometheus.io/scrape`, `port` and `path` annotations turn on metrics scraping.
Its stop hooks come across too: `terminationGracePeriodSeconds` becomes
`stop_grace`, `lifecycle.stopSignal` becomes `stop_signal`, and an `exec`
`lifecycle.preStop` becomes `pre_stop`. A grace of 0 is imported as 1 s and
one over an hour as an hour, with a warning. Kubernetes counts preStop inside
the grace, while Reliaburger gives `pre_stop` its own bound
(`pre_stop_timeout`, 30 s by default) before the grace starts, so an imported
`pre_stop` gets a warning saying so. `relish export` maps the same three back;
`pre_stop_timeout` has no Kubernetes field and is listed as unsupported.

Services attach the way Kubernetes routes them: a Service belongs to every
workload in its namespace whose pod template labels include all of its
`selector`, whatever the two are called. An app answers only to its own name,
so a Service called `web-http` that selects Deployment `web` gets a warning
telling clients to call `web` instead. A Service that selects nothing, or
several workloads, is reported too. One with no selector at all falls back to
the workload of the same name, with a warning, since Kubernetes would route it
nowhere.

## What doesn't

The report warns about everything it approximates or drops. The usual ones:

- sidecars: only the first container is imported;
- pod volumes and `fsGroup`: declare Reliaburger volumes instead. A `hostPath`
  volume gets a warning of its own, because its replacement, a hand-written
  `source`, needs the `host-exec` permission and a `[storage]
  allowed_host_paths` entry on every node that runs it (see
  `images-and-volumes`);
- `tolerations`, `affinity` and `topologySpreadConstraints`;
- `livenessProbe`, and `exec`, `tcpSocket` or gRPC readiness probes;
- `lifecycle.postStart`, a `sleep`, `httpGet` or `tcpSocket` preStop (a node
  already withdraws and drains an instance before it signals it), and a
  `stopSignal` Reliaburger doesn't accept, such as `SIGKILL`;
- `env` from `valueFrom`: set the value, or encrypt it with
  `relish secret encrypt` (see `security`);
- `envFrom`: a ConfigMap in the same input and namespace is copied into `env`
  (explicit `env` entries win, as in Kubernetes), which is reported because
  later changes to the ConfigMap no longer reach the app. A Secret, or a
  ConfigMap that isn't in the input, is named in a warning;
- ConfigMaps nothing reads with `envFrom`, and every Secret: move their values
  into `env` or `config_file`;
- a Job's `completions`, `parallelism`, `completionMode` and
  `podFailurePolicy`: a job runs once per trigger. A Job-wide
  `activeDeadlineSeconds` becomes a per-attempt timeout, and a `backoffLimit`
  above 9 is capped at 10 attempts;
- in a job's pod: `workingDir`, `runAsUser`, `runAsGroup`, `nodeSelector` and
  init containers;
- a CronJob's `suspend`, `concurrencyPolicy`, `timeZone` (schedules run in
  UTC) and `startingDeadlineSeconds`;
- extra Service ports and port remapping.

Kubernetes names are unique per namespace, so the importer keys everything by
namespace and name: a Service, Ingress, HPA or ConfigMap only ever matches
workloads in its own namespace. Two apps or jobs with the same name in
different namespaces can't share a TOML key, so the second becomes
`[app.NAMESPACE-NAME]` or `[job.NAMESPACE-NAME]`, with a warning. Report lines
for anything outside `default` read `Kind/namespace/name`.

## Keep the TOML

`relish import` writes the converted config to stdout, so you can review it,
commit it and apply it like any other file:

```sh
relish import -f deployment.yaml -f service.yaml > app.toml
relish import -f deployment.yaml --strict      # exit 1 if anything is approximated or dropped
```

`--strict` is meant for CI: it fails when the report has any warning or any
dropped resource, and the error says how many of each.

`relish export -f app.toml` goes the other way, printing Deployments (or
DaemonSets), Services, Ingresses, HPAs, Jobs, CronJobs and Namespaces as
multi-document YAML. Containers keep their `command`, `args`, `workingDir`
and `runAsUser`/`runAsGroup`; a job's `max_attempts` becomes `backoffLimit` and
`task_timeout_secs` the pod's `activeDeadlineSeconds`. A scheduled job's
`overlap` becomes the CronJob's `concurrencyPolicy` (`allow` → `Allow`,
`forbid` → `Forbid`); an omitted `overlap` still exports as `Forbid`, because
that's Reliaburger's default and Kubernetes' is `Allow`. It reports what it
can't express: firewall and egress rules on apps and jobs, process workloads,
`run_before`, a job's `pids` and `replay_unknown`, and for now health checks,
metrics, volumes, init containers, config files and placement. An encrypted env
value exports as the placeholder `ENCRYPTED_VALUE`, and each one is listed as
needing a Kubernetes Secret. A `runtime = "process"` job produces no Job at
all, since Kubernetes needs an image. A `runtime = "shared-runc"` job becomes
an ordinary Job, which starts a fresh Pod for every attempt.

`relish export --strict` is the same CI gate in the other direction: it exits
1 when the report lists anything unsupported or not exported yet, and the
error says how many of each.

```sh
relish export -f app.toml > k8s.yaml
relish export -f app.toml --strict             # exit 1 if anything isn't exported
```
