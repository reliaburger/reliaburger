<!-- Generated from docs/website/compare/uncloud.html by `make agent-docs`: edit the page, not this file. -->

COMPARISON · REVIEWED 15 OCTOBER 2026

# Reliaburger and Uncloud

Reliaburger keeps reconciling application placement after deployment, with metrics, GitOps and failure diagnostics in the same toolchain. Uncloud offers a Compose workflow with WireGuard and automatic HTTPS. The key choice is how much ongoing cluster operation you want the platform to handle.

## The verdict

**Which Reliaburger.** Installation examples and capability comparisons both describe the [0.2.2 release](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.2), which the public installer installs, re-checked on 21 October 2026. Its node-by-node rollouts and `RolloutFailed` handling arrived in 0.2.1. [Deployment contract](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md#rolling-deploys) · [Compatibility policy](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/releasing.md#compatibility-before-100).

### Where Reliaburger leads

Reliaburger continuously reconciles desired app placement and replaces eligible stateless replicas after node loss. It also brings a broader operational toolkit: metrics, alert rules, GitOps, network diagnosis and fault injection. Uncloud deliberately leaves failed-machine replacement and redeployment to the operator. [Cluster behaviour](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/02_cluster-basics.md) · [Included tools](https://github.com/reliaburger/reliaburger/blob/v0.2.2/README.md#whats-in-the-binary) · [Uncloud failure model](https://github.com/psviderski/uncloud/blob/v0.21.0/website/landing/index.html#L789-L805).

### Where Uncloud leads

Uncloud has the shorter documented path from an existing Compose application to several Docker hosts with private WireGuard networking and public HTTPS. Its Caddy integration provisions certificates, and its CLI can build and transfer images directly to machines. That is a practical advantage for a small web application. [Architecture and workflow](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/1-overview.md) · [Ingress](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/3-concepts/2-ingress/1-overview.md).

We review [Uncloud 0.21.0](https://github.com/psviderski/uncloud/releases/tag/v0.21.0), not its nightly build. Neither system automatically makes local database data durable across node loss. Keep stateful storage and application replication in the decision, rather than treating surviving web replicas as proof of database availability. [Volume support](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/8-compose-file-reference/1-support-matrix.md) · [Reliaburger data locality](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/11_images-and-volumes.md#volumes).

## The same application, different control models

| What you mean          | Uncloud                                                                                                                                                                                                                           | Reliaburger                                                                                                                                                                                                                       |
| ---------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A runnable application | A Compose service becomes a replicated or global service. The CLI creates a concrete deployment plan. [Deployment guide](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/4-guides/1-deployments/1-deploy-app.md). | An app specification declares replicas, placement and health; agents reconcile it. [App guide](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md).                                           |
| A machine              | Docker and `uncloudd` on a Linux host; SSH can manage the cluster through one machine. [Machine initialisation](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/9-cli-reference/uc_machine_init.md).              | Rootful runc and `bun` on Linux; some nodes form a Raft council. [Server guide](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/linux-servers.md).                                                                    |
| Cross-host traffic     | WireGuard mesh, container addresses and DNS; NAT traversal helps connect different networks. [Networking model](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/1-overview.md).                                   | eBPF maps service VIPs to node endpoints, without an overlay. Host endpoints must be reachable. [Network requirements](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/linux-servers.md#network-and-firewall-matrix). |
| Public ingress         | Caddy, automatic HTTPS and optional managed `uncld.dev` DNS. [Ingress](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/3-concepts/2-ingress/1-overview.md).                                                       | Built-in proxy with cluster-CA or supplied certificates; ACME is still deferred. [TLS modes](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/03_networking.md#ingress).                                        |
| An unavailable node    | Healthy replicas continue; no automatic rescheduling onto survivors. [Official FAQ](https://github.com/psviderski/uncloud/blob/v0.21.0/website/landing/index.html#L789-L805).                                                     | Stateless work can be replaced; managed-volume apps wait for their data-bearing node. [Stateful exception](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/11_images-and-volumes.md#volumes).                  |

## Install on machines you can reach

Uncloud’s quickstart assumes an Ubuntu or Debian server with a public IP and key-based SSH, as root or a user with passwordless sudo. The CLI runs on your workstation. `machine init` installs Docker and the daemon unless told otherwise, configures the machine and deploys Caddy. Replace the example addresses below with your own hosts. [Prerequisites and setup](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/2-getting-started/2-deploy-demo-app.md).

UNCLOUD · WORKSTATION AND TWO SERVERS

```
brew install psviderski/tap/uncloud
uc version
uc machine init --version v0.21.0 root@server-one
uc machine add --version v0.21.0 root@server-two
uc machine ls
```

RELIABURGER · RELEASED LAPTOP CLUSTER

```
curl -fsSL https://reliaburger.com/install.sh | RELIABURGER_VERSION=v0.2.2 sh -s -- --laptop
relish nodes
relish council status
```

Commands: [Uncloud CLI installation](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/2-getting-started/1-install-cli.md), [init](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/9-cli-reference/uc_machine_init.md), [add](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/9-cli-reference/uc_machine_add.md); [Reliaburger quickstart](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/quickstart.md). Homebrew tracks its current formula; check `uc version`, or obtain the pinned CLI from the [0.21.0 release](https://github.com/psviderski/uncloud/releases/tag/v0.21.0).

These commands provision different things. Reliaburger’s installer creates a three-node cluster in managed Linux VMs, on an Apple silicon Mac or a Linux host with QEMU/KVM, requiring approximately 8 GiB free memory and 15 GiB disk. That convenience is valuable for an isolated experiment. Deploying it onto existing servers is a separate procedure with runc, eBPF and firewall prerequisites. Uncloud begins with your own remote machines and supplies the overlay network. [Laptop requirements](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/quickstart.md) · [Existing servers](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/linux-servers.md) · [Uncloud network](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/1-overview.md).

## Deploy and inspect a web service

For Uncloud, this small `compose.yaml` publishes Nginx through its managed HTTPS domain. It assumes the cluster’s DNS reservation and publicly reachable Caddy succeeded. A custom domain needs DNS pointing to suitable ingress machines. `x-ports` is an Uncloud extension, not a portable Compose field. [Port extension](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/8-compose-file-reference/2-extensions.md#x-ports) · [Publishing services](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/3-concepts/2-ingress/2-publishing-services.md).

UNCLOUD · COMPOSE.YAML

```
services:
  web:
    image: nginx:1.28-alpine
    deploy:
      replicas: 2
    x-ports:
      - 80/https
```

The corresponding Reliaburger application is a TOML app with `image`, `port`, `replicas` and an optional `ingress` block. HTTPS currently needs a supplied certificate or trust in the cluster CA, so this is not an equivalent automatic-public-certificate workflow. [App specification](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md) · [Ingress contract](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/03_networking.md#ingress).

UNCLOUD · FROM THE COMPOSE DIRECTORY

```
uc deploy
uc ps
uc logs -f web
uc scale web 3
# Edit the image tag, then:
uc deploy
```

RELIABURGER · WITH A VALID WEB.TOML

```
relish lint web.toml
relish apply web.toml --dry-run
relish apply web.toml
relish inspect web
relish logs web
# Set replicas = 3, then:
relish apply web.toml
```

Commands: [deploy](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/9-cli-reference/uc_deploy.md), [ps](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/9-cli-reference/uc_ps.md), [logs](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/9-cli-reference/uc_logs.md), [scale](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/9-cli-reference/uc_scale.md); [Reliaburger deployment](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md) and [logs](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/04_observability.md). Uncloud asks you to approve its deployment plan; `--yes` opts into unattended execution.

Compose familiarity is a real benefit, but compatibility is a subset. In 0.21.0, custom `networks`, `dns` and `labels` are unsupported; ports and dependencies have restrictions. All containers share the cluster network. Review the support matrix before moving a Compose stack, especially if its security model depends on separate networks. Reliaburger does not accept Compose; its Kubernetes importer has its own documented losses. [Exact Compose matrix](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/8-compose-file-reference/1-support-matrix.md) · [Accepted manifest formats](https://github.com/reliaburger/reliaburger/blob/v0.2.2/src/relish/manifest.rs) · [Kubernetes translation](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/09_kubernetes.md).

## Updates, partial failures and daemon versions

**Application update:** `uc deploy` replaces containers sequentially with health monitoring. Its default start-first order changes to stop-first for host-port conflicts and a single replica using a volume. A failed replacement is stopped; for stop-first, the old container restarts. Earlier successful replacements remain on the new version. Re-running deploy retries the remaining work. This is automatic recovery of the failed replacement, not atomic rollback of the whole service. [0.21.0 rolling deployment contract](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/4-guides/1-deployments/4-rolling-deployments.md).

Reliaburger also gates rollout progress on health, releases node shares in sequence and records `RolloutFailed` rather than repeatedly retrying a bad specification. It keeps previous successful versions for explicit `relish rollback web`. Managed-volume apps still roll stop-first. These rollout safety fixes arrived in 0.2.1. [Rollout contract](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md#rolling-deploys).

**Orchestrator update:** upgrading `uc` on your laptop, upgrading `uncloudd` on servers, and changing Caddy are separate operations. `brew upgrade uncloud` updates the CLI. The daemon installer selects `UNCLOUD_VERSION` and replaces a differing installed binary; inspect the release and apply it on each host. `uc caddy deploy` updates the proxy. Despite its name, `uc machine update` changes machine metadata and endpoints, not daemon software. [CLI updates](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/2-getting-started/1-install-cli.md) · [Daemon installer](https://github.com/psviderski/uncloud/blob/v0.21.0/scripts/install.sh) · [Caddy updates](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/9-cli-reference/uc_caddy_deploy.md) · [Machine metadata](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/9-cli-reference/uc_machine_update.md).

Reliaburger provides cluster-wide signed binary upgrade planning and execution, with compatibility checks. However, pre-1.0 format changes require cluster recreation, and 0.2.2 is one: a 0.2.1 cluster can't roll to it; a rolling upgrade command does not override that policy. We found no basis for claiming either project offers an effortless upgrade across every future version. [Upgrade operations](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/12_operations.md) · [Compatibility boundary](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/releasing.md#compatibility-before-100).

## Where Reliaburger fits

**Keep operating after the deploy.** Reliaburger combines desired-state repair with GitOps, metrics, a registry and fault rehearsal. Its diagnostic commands let you investigate placement and service paths through the same CLI used to deploy. That is a concrete advantage when ongoing cluster operation matters alongside the initial release. Plan around node-local storage, supplied public certificates and the documented pre-1.0 compatibility limits. [Integrated features](https://github.com/reliaburger/reliaburger/blob/v0.2.2/README.md#whats-in-the-binary) · [Diagnostics](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/07_diagnostics.md) · [Storage](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/11_images-and-volumes.md) · [TLS](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/03_networking.md) · [Release policy](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/releasing.md).

**Choose Uncloud** for Compose-oriented web deployment on a few machines, particularly when WireGuard and automatic HTTPS remove work you would otherwise do yourself. Its execution plans make changes visible. Its imperative model also leaves you responsible for deciding when to repair placement after machine loss. Those properties belong together. [Design choices](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/1-overview.md) · [Failure model](https://github.com/psviderski/uncloud/blob/v0.21.0/website/landing/index.html#L789-L805).

**Learning curve, our assessment:** Docker users should recognise more of Uncloud’s service, image and volume vocabulary. Reliaburger asks them to learn TOML apps and council behaviour, then gives them a common place to inspect and test the platform. We have no timed onboarding study, so this is a qualitative judgement about concepts and steps, not a measured productivity claim. [Uncloud concepts](https://github.com/psviderski/uncloud/blob/v0.21.0/website/docs/1-overview.md) · [Reliaburger concepts](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/02_cluster-basics.md).

Both projects use Apache-2.0. [Uncloud licence](https://github.com/psviderski/uncloud/blob/v0.21.0/LICENSE) · [Reliaburger licence](https://github.com/reliaburger/reliaburger/blob/v0.2.2/LICENSE).

## Evidence and limits

Uncloud citations are pinned to [v0.21.0](https://github.com/psviderski/uncloud/tree/v0.21.0); Reliaburger citations to the [v0.2.2](https://github.com/reliaburger/reliaburger/tree/v0.2.2/) tag, the release the installer installs, re-checked on 21 October 2026. We prioritised the versioned deployment guide and code over the older README sentence saying automatic rollback was still forthcoming.

This is a documentation and source comparison. No matched performance, outage-recovery or penetration test was run. A WireGuard mesh is a connectivity mechanism, not evidence by itself of tenant isolation; a rolling deployment feature is not proof that every stateful application can update without downtime. See the [shared review method](https://reliaburger.com/compare/methodology#method).

---

The Markdown copy of <https://reliaburger.com/compare/uncloud>.
