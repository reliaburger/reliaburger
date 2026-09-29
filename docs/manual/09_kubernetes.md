# Coming from Kubernetes

You don't have to rewrite your manifests to try Reliaburger. `relish apply`
takes Kubernetes YAML as it is, converts it in memory and tells you what it had
to change:

```sh
relish apply -f deploy/podinfo.yaml
relish apply -f https://reliaburger.com/demo/podinfo.yaml
relish apply -f deploy/podinfo.yaml --dry-run
```

A file counts as Kubernetes when it has `apiVersion:` and `kind:` lines;
anything else is read as Reliaburger TOML. URLs must be `https://` (redirects
included), answer within 30 seconds and stay under 1 MiB. The migration report
goes to stderr before anything is applied.

## What converts

Eleven kinds: Deployment, StatefulSet, DaemonSet, Service, Ingress, ConfigMap,
Secret, Job, CronJob, HorizontalPodAutoscaler and Namespace. Anything else
(NetworkPolicy, PersistentVolumeClaim, CRDs) lands in the report's Dropped list
and isn't applied.

| Kubernetes | Becomes |
|------------|---------|
| Deployment, StatefulSet | `[app.NAME]` (StatefulSet loses ordering and stable network ids) |
| DaemonSet | `[app.NAME]` with `replicas = "*"` |
| Service | merged into the workload of the same name: its first port, via `targetPort` |
| Ingress | the matching app's `ingress` |
| HorizontalPodAutoscaler | the matching app's `autoscale` |
| Job, CronJob | `[job.NAME]`, with the cron `schedule` |
| Namespace | `[namespace.NAME]` |

From the pod template it takes the first container's image, `command`, `args`,
`workingDir`, plain `env` values, CPU and memory requests and limits,
`nodeSelector` (as `placement.required`), init containers, `runAsUser` and
`runAsGroup`, and an `httpGet` readiness probe as the health check. The
`prometheus.io/scrape`, `port` and `path` annotations turn on metrics scraping.

## What doesn't

The report warns about everything it approximates or drops. The usual ones:

- sidecars: only the first container is imported;
- pod volumes and `fsGroup`: declare Reliaburger volumes instead;
- `livenessProbe`, and `exec`, `tcpSocket` or gRPC readiness probes;
- `env` from `valueFrom`: set the value, or encrypt it with
  `relish secret encrypt` (see `security`);
- ConfigMaps and Secrets: not wired into workloads, so move their values into
  `env` or `config_file`;
- a CronJob's `suspend`, extra Service ports and port remapping.

Two apps with the same name in different namespaces can't share a TOML key, so
the second becomes `[app.NAMESPACE-NAME]`, with a warning.

## Keep the TOML

`relish import` writes the converted config to stdout, so you can review it,
commit it and apply it like any other file:

```sh
relish import -f deployment.yaml -f service.yaml > app.toml
relish import -f deployment.yaml --strict      # non-zero exit on any warning
```

`relish export -f app.toml` goes the other way, printing Deployments (or
DaemonSets), Services, Ingresses, HPAs, Jobs, CronJobs and Namespaces as
multi-document YAML. It reports what it can't express: firewall and egress
rules, process workloads, `run_before`, and for now health checks, metrics,
volumes, init containers, config files and placement.
