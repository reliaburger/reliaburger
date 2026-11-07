<!-- Generated from docs/website/compare/k3s.html by `make agent-docs`: edit the page, not this file. -->

PROJECT COMPARISON · 15 OCTOBER 2026

# Reliaburger and k3s

Reliaburger combines an app-centred configuration with built-in logs, metrics, a push registry, network diagnosis and fault injection. That shared operating model is its attraction for small application teams. k3s brings Kubernetes APIs and a packaged infrastructure stack to teams that need them.

Basis: [Integrated platform](https://github.com/reliaburger/reliaburger/blob/v0.2.2/README.md) · [Diagnostics](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/07_diagnostics.md) · [Push registry](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/11_images-and-volumes.md) · [k3s packaging](https://docs.k3s.io/). The recommendation is our judgement.

**Versions matter.** Reliaburger statements below describe the [0.2.2 release](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.2), which the public installer installs, re-checked on 21 October 2026. k3s reference: [v1.37.1+k3s1](https://github.com/k3s-io/k3s/releases/tag/v1.37.1%2Bk3s1). This is a source and documentation review, not a deployment benchmark. [Method and limits](https://reliaburger.com/compare/methodology.html#method).

## Choose the operating model

### Choose Reliaburger for integrated operations

Relish exposes application diagnosis, network-path inspection and bounded fault injection alongside deployment. Bun includes its own registry, metrics, logs and GitOps. Teams whose applications fit the model can use one set of app names and commands across deployment, observation and failure exercises.

Sources: [Diagnostics](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/07_diagnostics.md) · [Chaos](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/05_chaos.md) · [Built-in subsystems](https://github.com/reliaburger/reliaburger/blob/v0.2.2/README.md).

### Choose k3s for Kubernetes contracts

Our recommendation: choose k3s when you want Kubernetes workload semantics and extension APIs with an integrated installation. Its documented embedded-etcd HA and upgrade paths are concrete capabilities. Reliaburger's YAML conversion is lossy and its pre-1.0 state formats do not migrate.

Sources: [HA topology](https://docs.k3s.io/datastore/ha-embedded) · [Upgrade procedure](https://docs.k3s.io/upgrades/manual) · [Import limits](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/09_kubernetes.md) · [Compatibility policy](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/releasing.md#compatibility-before-100).

k3s packages Kubernetes with containerd, Flannel, CoreDNS, Traefik, ServiceLB and local-path provisioning. Both projects therefore provide integrated components; the choice includes the application model and operating tools you want to use.

Sources: [k3s components](https://docs.k3s.io/) · [Reliaburger components](https://github.com/reliaburger/reliaburger/blob/v0.2.2/README.md).

## Same deployment problem, different model

Bundled does not mean the same behaviour
| Concern            | k3s                                                                                                                                                                   | Reliaburger                                                                                                                                                                                                                                          |
| ------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Nodes              | Servers run the control plane; agents do not. Both normally run workloads. [Node roles](https://docs.k3s.io/architecture)                                             | Bun runs on each node, with a council holding cluster state. [Cluster model](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/06_under-the-hood.md)                                                                                |
| Workloads          | Kubernetes Deployments, Pods, Services and associated APIs. [Kubernetes API access](https://docs.k3s.io/cluster-access)                                               | TOML apps/jobs; selected Kubernetes kinds are translated, not served through a Kubernetes API. [Translation contract](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/09_kubernetes.md)                                           |
| Network / ingress  | Flannel and Traefik are bundled choices; ServiceLB uses available host ports. [Networking defaults](https://docs.k3s.io/networking/networking-services)               | eBPF TCP service discovery and Wrapper ingress belong to Bun. Public ACME issuance is not provided. [Networking and TLS](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/03_networking.md)                                        |
| Image distribution | Optional Spegel mirrors images already in participating nodes; it is not a general push registry. [Mirror contract](https://docs.k3s.io/installation/registry-mirror) | Pickle accepts OCI pushes and replicates layers; durability has a window before replication completes. [Registry contract](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/11_images-and-volumes.md#image-durability-and-cleanup) |

k3s' bundled local-path provisioner also uses storage on individual nodes. Neither its default storage nor Reliaburger's local managed volumes provides replicated application data. [Local-path scope](https://github.com/rancher/local-path-provisioner) · [Reliaburger volume locality](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/11_images-and-volumes.md#volumes).

A StatefulSet, sidecar or PVC does not become equivalent just because Relish accepts YAML. Inspect the migration report. Kubernetes ordering and volume APIs remain relevant on k3s; Reliaburger drops or approximates those parts of the input.

Sources: [StatefulSet contract](https://kubernetes.io/docs/concepts/workloads/controllers/statefulset/) · [Exact conversion losses](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/09_kubernetes.md).

## Start a cluster, then choose its topology

**k3s single server:** use a supported Linux host with systemd or OpenRC, sudo/root access and the documented network prerequisites. This version-pinned installer creates one server, not an HA cluster; it writes kubeconfig and includes kubectl.

Sources: [Host requirements](https://docs.k3s.io/installation/requirements) · [Installer and kubeconfig](https://docs.k3s.io/quick-start) · [Reviewed release](https://github.com/k3s-io/k3s/releases/tag/v1.37.1%2Bk3s1).

K3S · SINGLE LINUX SERVER sh

```
curl -sfL https://get.k3s.io | \
  INSTALL_K3S_VERSION='v1.37.1+k3s1' sh -
sudo k3s kubectl get nodes
```

To add an agent, use the same installer with `K3S_URL=https://SERVER:6443` and the server's node token as `K3S_TOKEN`, replacing SERVER with a reachable address. Embedded-etcd HA is a different setup: initialise with `--cluster-init`, then join additional servers. Plan at least three servers and a registration address; SQLite single-server state is not HA.

Sources: [Agent registration](https://docs.k3s.io/quick-start) · [Embedded-etcd installation](https://docs.k3s.io/datastore/ha-embedded).

**Reliaburger laptop trial:** the public installer below creates a three-node Linux-VM cluster using released 0.2.2. It needs macOS on Apple silicon, or Linux (x86\_64 or arm64) with QEMU/KVM, plus roughly 8 GiB free memory and 15 GiB disk. For physical Linux hosts, follow its separate rootful-runc/eBPF guide. These different topologies cannot support an installation-speed ranking.

Sources: [Laptop setup](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/quickstart.md) · [Installer pin](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/website/install.sh) · [Existing Linux hosts](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/linux-servers.md).

RELIABURGER · RELEASED LAPTOP TRIAL sh

```
curl -fsSL https://reliaburger.com/install.sh | RELIABURGER_VERSION=v0.2.2 sh -s -- --laptop
relish nodes
relish council status
```

## The daily command path

Assume a healthy cluster and a disposable nginx workload called `web` in `default`. Run k3s commands on its server with administrative access; run Relish with an authenticated context. These examples create stateless replicas only. They do not publish an internet endpoint.

Sources: [Client access](https://docs.k3s.io/cluster-access) · [Workload commands](https://kubernetes.io/docs/reference/kubectl/quick-reference/) · [Relish app commands](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md).

The Relish file adds an HTTP health check. Add a Kubernetes `readinessProbe` to the generated workload for a comparable readiness gate. [Kubernetes probes](https://kubernetes.io/docs/tasks/configure-pod-container/configure-liveness-readiness-startup-probes/) · [Relish health checks](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md#health-checks).

K3S · KUBERNETES WORKLOAD sh

```
sudo k3s kubectl create deployment web \
  --image=nginx:alpine --replicas=2
sudo k3s kubectl rollout status deployment/web
sudo k3s kubectl logs deployment/web
sudo k3s kubectl exec deployment/web -- nginx -t
```

RELIABURGER · SAVE AS web.toml toml

```
[app.web]
image = "nginx:alpine"
port = 80
replicas = 2

[app.web.health]
path = "/"
```

RELIABURGER · APPLY AND INSPECT sh

```
relish lint web.toml
relish apply web.toml
relish inspect web
relish logs web
relish exec web nginx -t
```

Common intentions and their command forms
| Task                 | k3s                                                                                    | Reliaburger                                                                                                                                 |
| -------------------- | -------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| Declarative apply    | `sudo k3s kubectl apply -f web.yaml`, with your reviewed Kubernetes manifest.          | `relish apply web.toml`, or review conversion with `relish apply -f web.yaml --dry-run`; since 0.2.2, `relish lint web.yaml` checks it too. |
| Change replica count | `sudo k3s kubectl scale deployment/web --replicas=3`; keep declarative config in sync. | Edit `replicas` in TOML, then apply it.                                                                                                     |
| Diagnosis            | `sudo k3s kubectl describe deployment web` and `sudo k3s kubectl get events`.          | `relish wtf` correlates findings; `relish resolve web` lists discovery backends.                                                            |
| Cleanup              | `sudo k3s kubectl delete deployment web`.                                              | `relish delete web`.                                                                                                                        |

Sources: [kubectl operations](https://kubernetes.io/docs/reference/kubectl/quick-reference/) · [Bundled client](https://docs.k3s.io/cluster-access) · [YAML dry run](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/09_kubernetes.md) · [Diagnostics](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/07_diagnostics.md) · [Lifecycle commands](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md).

Relish's integrated fault commands are useful for a deliberate failure exercise, but require node opt-in and suitable credentials. For example, `relish fault kill web --count 1 --reason comparison-trial --acknowledge` targets an app replica subject to its safety rails. It is not evidence that either orchestrator recovers faster.

Sources: [Fault permissions and constraints](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/05_chaos.md).

## Two separate upgrade jobs

### Application versions

k3s uses Kubernetes Deployment updates: change the Pod template, watch `rollout status`, and use `rollout undo` when needed. Relish accepts an updated app specification and provides `relish history web` and `relish rollback web`. Since 0.2.1, Reliaburger serialises a changed specification across nodes, while each node enforces its own deployment budget. Managed-volume apps roll stop-first, so that case has an interruption.

Sources: [Deployment updates](https://kubernetes.io/docs/concepts/workloads/controllers/deployment/) · [Rollout and volume rules](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md).

### Platform binaries and state

For k3s, upgrade servers before agents, preserve configuration and obey Kubernetes version-skew limits. Re-running the installer with a chosen version is supported; the script does not automatically cordon or drain. Existing containers continue running during a k3s restart, but that is not a promise of uninterrupted API availability.

Sources: [Manual upgrade contract](https://docs.k3s.io/upgrades/manual) · [Version-skew limits](https://kubernetes.io/releases/version-skew-policy/).

The following is a staging command for an existing default-config server on a supported predecessor version. It downloads the reviewed target without starting it. Custom installations must retain their existing environment/arguments or configuration file. Complete the documented upgrade sequence afterwards. Automated upgrades instead use the additional system-upgrade-controller and Plan resources.

Sources: [Version pin and skip-start](https://docs.k3s.io/upgrades/manual) · [Configuration persistence](https://docs.k3s.io/installation/configuration) · [Automated upgrade controller](https://docs.k3s.io/upgrades/automated).

K3S · STAGE AN UPGRADE, DO NOT START IT sh

```
curl -sfL https://get.k3s.io | \
  INSTALL_K3S_VERSION='v1.37.1+k3s1' \
  INSTALL_K3S_SKIP_START=true sh -
```

Reliaburger has integrated `upgrade check`, `plan`, `start` and rollback commands, but only for matching protocol/state formats. A format change means recreating the cluster before 1.0, and 0.2.2 is one: a 0.2.1 cluster can't roll to it. Treat this as a present operational disadvantage, even though the same-format rollout machinery ships.

Sources: [Upgrade commands](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/12_operations.md) · [Compatibility restriction](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/releasing.md#compatibility-before-100).

## Where Reliaburger fits

**Learning curve, qualitative:** k3s removes installation choices by bundling defaults; it retains Kubernetes workload concepts. Relish reduces the number of objects in the example, but introduces its own configuration, diagnostics and storage contracts. We have not timed novice onboarding or incident response.

Sources: [Packaging choices](https://docs.k3s.io/) · [Kubernetes API](https://docs.k3s.io/cluster-access) · [App model](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md) · [Storage model](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/11_images-and-volumes.md).

**Keep application operations together.** Reliaburger is a useful choice when you want deployment, image pushes, cluster diagnostics and controlled fault exercises in the same toolset. The examples above show the app model and commands you will use. Prefer k3s when retaining Kubernetes workload APIs and extension compatibility is a requirement.

Sources: [Image pushes](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/11_images-and-volumes.md) · [Diagnostics](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/07_diagnostics.md) · [Fault exercises](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/05_chaos.md) · [Kubernetes API access](https://docs.k3s.io/cluster-access).

Backup comparisons also need the right boundary: a k3s datastore snapshot protects cluster state, not the contents of application volumes. Reliaburger likewise distinguishes council recovery from local volume snapshots. Neither feature alone establishes a complete application disaster-recovery plan.

Sources: [Datastore backup scope](https://docs.k3s.io/datastore/backup-restore) · [Council recovery](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/12_operations.md) · [Volume snapshots](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/11_images-and-volumes.md#snapshots).

## Evidence and limits

Version reference: [k3s v1.37.1+k3s1](https://github.com/k3s-io/k3s/releases/tag/v1.37.1%2Bk3s1). Official documentation was checked on 15 October 2026. Reliaburger claims pin the [v0.2.2](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.2) tag, the release the installer installs, re-checked on 21 October 2026. No cluster was deployed for this comparison; resource overhead, failure recovery and throughput remain unmeasured.

For workload API, persistent-storage and compatibility details shared with upstream, read the [Kubernetes comparison](https://reliaburger.com/compare/kubernetes.html). The [comparison methodology](https://reliaburger.com/compare/methodology.html#method) explains the evidence boundary.

---

The Markdown copy of <https://reliaburger.com/compare/k3s.html>.
