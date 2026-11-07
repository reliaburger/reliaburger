<!-- Generated from docs/website/compare/docker-compose.html by `make agent-docs`: edit the page, not this file. -->

LOCAL DEVELOPMENT / DOCKER COMPOSE

# Reliaburger and Docker Compose

Take application operations beyond one Docker host. Reliaburger combines placement across nodes, cluster identity, service discovery and incident investigation in one platform. Compose remains a straightforward way to define and run a stack on one host; Reliaburger adds the shared operational model for a cluster.

Basis: [Compose's development workflow](https://docs.docker.com/compose/gettingstarted/), [production scope](https://docs.docker.com/compose/how-tos/production/) and [Reliaburger architecture](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/06_under-the-hood.md). Suitability is our qualitative assessment.

**Version boundary.** Reliaburger's public installer installs [0.2.2](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.2), and the feature analysis describes that release, re-checked on 21 October 2026. Compose commands follow current official documentation, reviewed 15 October 2026. This page compares ordinary `docker compose` operation; [Swarm](https://docs.docker.com/engine/swarm/) is a separate clustering mode.

## Where each wins

### Reliaburger: one operational cluster

Bun combines Raft, scheduling, discovery, registry caching, metrics and logs. Placement and failure decisions operate over cluster nodes, and `relish wtf` explains available evidence. That is a concrete capability advantage when the requirement spans machines. [Architecture](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/06_under-the-hood.md) · [cluster behavior](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/02_cluster-basics.md) · [diagnostics](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/07_diagnostics.md).

### Compose: a small deployment unit

One YAML definition combines services, networks and volumes. Watch can synchronize source or rebuild containers during development, while production overrides reuse the base definition. These documented workflows make Compose a compelling development and single-server choice. [Getting started](https://docs.docker.com/compose/gettingstarted/) · [Watch](https://docs.docker.com/compose/how-tos/file-watch/) · [production overrides](https://docs.docker.com/compose/how-tos/production/).

Docker restart policies recover containers on their Docker daemon; they do not create another host to replace a failed machine. Pointing Compose at a remote daemon changes the destination, not its scheduling model. Docker's own production guide directs cluster deployments toward Swarm. [Restart policies](https://docs.docker.com/engine/containers/start-containers-automatically/) · [remote host and clustering](https://docs.docker.com/compose/how-tos/production/).

## Translate the concepts

| Concern           | Compose                                                                                                                                          | Reliaburger                                                                                                                                                                                                                                                                     |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Definition        | A project groups YAML services, networks and volumes. [Model](https://docs.docker.com/compose/gettingstarted/)                                   | TOML apps/jobs and namespaces; supported Kubernetes YAML can be imported. [Apps](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md) · [import limits](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/09_kubernetes.md) |
| Service discovery | Containers on a project network resolve service names through Docker DNS. [Networking](https://docs.docker.com/compose/how-tos/networking/)      | Cluster DNS and stable service addresses backed by eBPF routing. [Networking](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/03_networking.md)                                                                                                              |
| Readiness         | `depends_on: service_healthy` gates dependent startup on a health check. [Startup order](https://docs.docker.com/compose/how-tos/startup-order/) | Health checks feed replica state and rollout progression; this is not a direct dependency-graph equivalent. [Health contract](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md)                                                           |
| Secrets           | Explicit grants mount secret files into eligible service containers. [Secrets](https://docs.docker.com/compose/how-tos/use-secrets/)             | Encrypted secrets and namespace-scoped access within the cluster identity model. [Security](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/10_security.md)                                                                                                  |

**Learning curve, our assessment:** an existing Compose user can keep familiar YAML and Docker tools. Moving to Reliaburger means learning manifests, placement and quorum; the payoff is a common interface for cluster diagnostics. This is a conceptual comparison, not measured onboarding time. [Compose tutorial](https://docs.docker.com/compose/gettingstarted/) · [cluster concepts](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/02_cluster-basics.md).

## Install the platform

Docker Desktop includes Engine, CLI and Compose. On Linux with Engine, CLI and Docker's package repository already configured, the Debian/Ubuntu plugin commands below install Compose. Installing the plugin alone does not install those prerequisites. [Installation choices](https://docs.docker.com/compose/install/) · [Linux instructions](https://docs.docker.com/compose/install/linux/).

COMPOSE configured Debian/Ubuntu host

```
sudo apt-get update
sudo apt-get install docker-compose-plugin
docker compose version
```

RELIABURGER laptop quickstart

```
curl -fsSL https://reliaburger.com/install.sh | RELIABURGER_VERSION=v0.2.2 sh -s -- --laptop
relish status
relish nodes
```

Reliaburger's default installer creates three Linux VMs using Apple's Virtualization.framework through Lima on Apple silicon macOS, or QEMU/KVM through Lima on Linux, with roughly 8 GiB free memory and 15 GiB disk required. It exercises cluster behavior; that is a different installation scope from adding Compose to an existing daemon. Production Reliaburger container clusters use rootful Linux runc and eBPF. [Quickstart](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/quickstart.md) · [runtime limits](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/README.md#scope-and-limits).

## Validate, run and investigate

These examples assume an existing `compose.yaml` with service `web`, or `web.toml` with app `web`. Compose's `config --quiet` validates configuration; `up --wait` waits for services to be running or healthy. Reliaburger's `apply` commits desired state asynchronously, so inspect health afterward. [Config](https://docs.docker.com/reference/cli/docker/compose/config/) · [up](https://docs.docker.com/reference/cli/docker/compose/up/) · [Reliaburger deployment](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md).

COMPOSE project directory

```
docker compose config --quiet
docker compose up -d --wait
docker compose logs --tail 50 web
docker compose exec web sh
```

RELIABURGER reachable cluster

```
relish lint web.toml
relish apply web.toml
relish inspect web
relish logs web --tail 50
relish wtf --app web
```

Compose's shell example needs an image containing `sh`; logs and exec are service-oriented commands. Reliaburger's diagnostic command investigates placement and network paths instead of being a shell equivalent. Its authenticated dashboard is read-only. [Compose logs](https://docs.docker.com/reference/cli/docker/compose/logs/) · [exec](https://docs.docker.com/reference/cli/docker/compose/exec/) · [diagnostics](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/07_diagnostics.md) · [dashboard](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/04_observability.md).

Reliaburger also makes the resolved deployment configuration inspectable: `relish compile config/ > all.toml` combines manifests, inherited defaults and derived namespaces into one file before admission. Compose provides `docker compose config` for its resolved service model. Both give the reader a concrete configuration to review. [Reliaburger compilation](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md#many-files) · [Compose configuration](https://docs.docker.com/reference/cli/docker/compose/config/).

For development, configure `develop.watch`, then use `docker compose up --watch` for its file-sync/build loop. Watch is aimed at editing and testing source locally, while the deployment commands above apply a workload definition. [Watch setup and supported actions](https://docs.docker.com/compose/how-tos/file-watch/).

## Applications and the platform have different upgrades

**Compose application:** after changing the image reference, pull and reconcile it. Pull alone does not start a container; `up` recreates changed services. This is not a documented cluster rolling-release controller, and `--wait` does not promise uninterrupted traffic. [Pull](https://docs.docker.com/reference/cli/docker/compose/pull/) · [recreation and waiting](https://docs.docker.com/reference/cli/docker/compose/up/).

APP RELEASE Compose, image already edited

```
docker compose pull web
docker compose up -d --wait web
```

APP RELEASE Reliaburger, manifest edited

```
relish deploy web.toml
relish history web
relish rollback web
```

**Reliaburger:** since 0.2.1, changed-spec rollouts advance one node at a time, after health succeeds on that node. Surge/unavailable budgets are local to each node; managed volumes force stop-first. Successful versions are replicated for rollback across restarts, while failed rollouts halt further nodes. This is a stronger cluster release contract, not evidence of faster recovery. [Pinned rollout and rollback semantics](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md#rolling-deploys).

**Platform:** updating the Compose package uses the Linux installation commands above; manual binary installations need manual updates. Docker Engine/Desktop has its own lifecycle. Reliaburger offers `relish upgrade check` and coordinated agent upgrades only across compatible protocol/state formats. 0.2.2 changes those formats, so a 0.2.1 cluster needs a fresh start rather than an upgrade. [Plugin updates](https://docs.docker.com/compose/install/linux/) · [installation components](https://docs.docker.com/compose/install/) · [Bun upgrade rules](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/12_operations.md#upgrading-bun).

## Where Reliaburger fits

Compose's local volumes persist beyond a container, but backups and remote storage need an explicit operational design. Reliaburger has Btrfs snapshots and council backups; council backups exclude application volumes. Neither platform makes a database restore correct merely by copying files. [Docker volume lifecycle and backup procedures](https://docs.docker.com/engine/storage/volumes/) · [Reliaburger volumes](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/11_images-and-volumes.md) · [council recovery](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/12_operations.md#backing-up-the-council).

**Choose Reliaburger when the operational unit is a cluster:** its integrated identity, discovery and diagnostics let you work with applications across nodes through one platform. Its pre-1.0 status and rootful/eBPF requirements still matter. For a small stack that only needs one host, Compose has a narrower set of concepts to operate. This suitability assessment follows the documented scope. [Reliaburger status](https://github.com/reliaburger/reliaburger/blob/v0.2.2/README.md) · [scope](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/README.md#scope-and-limits) · [Compose production model](https://docs.docker.com/compose/how-tos/production/).

## Evidence and limits

This comparison uses Docker's official command references and operational guides, alongside Reliaburger's pinned manuals. Claims about Compose do not silently include Swarm, third-party proxies or backup operators. No workload was deployed for this review; performance, resource overhead and recovery-time comparisons remain unmeasured. Current Docker documentation may change after the review date.

---

The Markdown copy of <https://reliaburger.com/compare/docker-compose.html>.
