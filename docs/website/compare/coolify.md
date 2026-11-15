<!-- Generated from docs/website/compare/coolify.html by `make agent-docs`: edit the page, not this file. -->

APPLICATION DELIVERY / COOLIFY

# Reliaburger and Coolify

Deploy and diagnose a cluster through one command line. Reliaburger brings scheduling, identity, discovery, logs and incident tools into its node agent, so the same application names carry from deployment into investigation. Coolify offers a browser-led path from Git repository to a public web application.

Basis: [Coolify deployment workflow](https://coolify.io/docs/applications/deployments/overview) and [Reliaburger architecture](https://github.com/reliaburger/reliaburger/blob/v0.2.2/README.md#whats-in-the-binary).

**Which Reliaburger.** The public installer installs [0.2.2](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.2), and the feature analysis below describes that release, re-checked on 21 October 2026. Coolify is pinned to [4.4.6](https://github.com/coollabsio/coolify/releases/tag/v4.4.6), with current official documentation for workflows.

## Where each wins

### Reliaburger: the integrated cluster

The Bun node agent bundles Raft, scheduling, registry caching, network discovery, metrics and logs. Its CLI adds network-path diagnosis and controlled fault injection. Operators get a shared application model across these tasks and fewer separately operated platform components. [Architecture](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/06_under-the-hood.md) · [diagnostics](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/07_diagnostics.md) · [chaos](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/05_chaos.md).

### Coolify: the application product

Git sources, preview environments, automatic public TLS and engine-aware database backups form an immediately useful delivery workflow. These are concrete advantages over assembling equivalent workflows around Reliaburger. [Sources](https://coolify.io/docs/applications/sources/overview) · [previews](https://coolify.io/docs/applications/deployments/preview-deployments) · [TLS](https://coolify.io/docs/core/networking/domains) · [backups](https://coolify.io/docs/databases/backups).

Coolify's additional deployment servers are standalone destinations: an external load balancer and shared application state remain your responsibility. Its documented multi-server application flow excludes Compose applications and resources with persistent storage. This is materially different from Reliaburger scheduling replicas within one cluster. [Coolify limits](https://coolify.io/docs/core/infrastructure/scaling/multi-server-deployments) · [Reliaburger cluster model](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/02_cluster-basics.md).

## Translate the concepts

| Concern                | Coolify                                                                                                                                                           | Reliaburger                                                                                                                                                                                                                                                                                                     |
| ---------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Application definition | Resource settings, source and build method; a Compose application keeps a Compose definition. [Guide](https://coolify.io/docs/applications/builds/docker-compose) | TOML app or supported Kubernetes YAML converted at admission. [Import contract](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/09_kubernetes.md)                                                                                                                                            |
| Organization           | Project and environment containing resources. [Application model](https://coolify.io/docs/applications/what-is-an-application)                                    | Namespaces, scoped credentials and resource quotas; not an exact project/environment hierarchy. [Access](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/10_security.md) · [quotas](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md#namespace-quotas) |
| Execution target       | Connected Docker server and destination. [Server model](https://coolify.io/docs/core/infrastructure/scaling/multi-server-deployments)                             | Cluster placement based on requests and placement rules. [App fields](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md#the-rest-of-an-app)                                                                                                                                |
| Browser interface      | Deployment and configuration controls. [Workflow](https://coolify.io/docs/applications/deployments/overview)                                                      | Authenticated, read-only dashboard; mutations use CLI/API. [Dashboard contract](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/04_observability.md)                                                                                                                                         |

**Learning curve, our assessment:** developers familiar with hosted deployment platforms should recognize Coolify's resource pages sooner. Reliaburger asks them to learn manifests, namespaces and quorum, but gives operators a consistent command surface for those concepts. This is an interface-based judgment, not a timed usability study. [Coolify flow](https://coolify.io/docs/applications/deployments/overview) · [cluster concepts](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/02_cluster-basics.md).

## Install the platform

COOLIFY root shell · Linux server

```
curl -fsSL https://cdn.coollabs.io/coolify/install.sh | bash
```

RELIABURGER laptop quickstart

```
curl -fsSL https://reliaburger.com/install.sh | RELIABURGER_VERSION=v0.2.2 sh -s -- --laptop
relish status
relish nodes
```

Run the Coolify script as root on a supported Linux server. Open the printed dashboard URL, create the administrator and save the generated installation secrets. Its documented minimum is 2 CPU cores, 2 GB RAM and 10 GB disk, before application capacity. [Official installation](https://coolify.io/docs/start-with-self-hosted).

Reliaburger's default install builds a three-node Linux VM cluster on Apple silicon macOS, or Linux with QEMU/KVM, requiring roughly 8 GiB free memory and 15 GiB disk. That is a laptop demonstration with three nodes, so comparing these requirements as equivalent overhead would be misleading. Production container clusters require rootful Linux runc and eBPF. [Quickstart](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/quickstart.md) · [runtime limits](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/README.md#scope-and-limits).

## Deploy, inspect and recover

### Coolify's browser workflow

1.  Connect a public repository, deploy key or GitHub App; choose the build method and configure the application. [Source access](https://coolify.io/docs/applications/sources/overview).
2.  Select Deploy, then inspect the queued operation under Deployment Logs. Set a domain and redeploy to activate its route; HTTPS uses the configured proxy's certificate automation. [Deployments](https://coolify.io/docs/applications/deployments/overview) · [domains](https://coolify.io/docs/core/networking/domains).
3.  Enable Preview Deployments with separate preview variables, then verify creation and cleanup against repository events. A database's Backups page separately configures scheduled dumps and optional S3 storage. [Previews](https://coolify.io/docs/applications/deployments/preview-deployments) · [database backups](https://coolify.io/docs/databases/backups).

### Reliaburger's command workflow

With an authored `web.toml` containing `app.web` and a reachable cluster, these commands validate, apply, inspect and diagnose it. `apply` accepting cluster state is distinct from every replica becoming healthy. [Deployment contract](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md) · [diagnostics](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/07_diagnostics.md).

RELIABURGER existing web.toml

```
relish lint web.toml
relish apply web.toml
relish inspect web
relish logs web
relish wtf --app web
relish dashboard
```

The advantage is cluster-aware explanation in the same tool: placement refusal, missing evidence and network-path problems are surfaced explicitly. Coolify's advantage is fewer bespoke steps between repository, preview URL and database backup. Neither interface removes the need to test the application's actual behavior. [Diagnostic evidence](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/07_diagnostics.md) · [preview lifecycle](https://coolify.io/docs/applications/deployments/preview-deployments).

## Application releases versus platform upgrades

**Coolify applications:** eligible non-Compose deployments can start a replacement, wait for health and retain the old container if the new one fails. Compose uses its own reconciliation; published host ports and fixed container names also prevent overlap. A rollback selects a retained old image but uses current runtime settings and does not reverse database migrations. [Replacement rules](https://coolify.io/docs/applications/deployments/rolling-updates) · [rollback semantics](https://coolify.io/docs/applications/deployments/rollbacks).

**Reliaburger applications:** the leader advances a changed spec one node at a time after that node becomes healthy. Surge/unavailable limits are per node. The council retains successful versions for rollback across restarts; failed rollouts remain halted until a changed spec or rollback. Managed volumes force stop-first replacement. These rollout rules arrived in 0.2.1. [Pinned rollout contract](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md#rolling-deploys).

APP RELEASE Reliaburger

```
relish deploy web.toml
relish history web
relish rollback web
```

PLATFORM CHECK Reliaburger

```
relish upgrade check
relish upgrade status
```

**Coolify itself:** Settings → Updates controls instance updates. They replace the control plane, not managed applications or operating systems; active deployments can fail during the update. **Bun itself:** compatible updates roll workers first and council members individually, with signatures and automatic crash-loop reversion. Protocol/state mismatches require a fresh cluster; a 0.2.1 cluster can't upgrade in place to 0.2.2. [Coolify update boundary](https://coolify.io/docs/core/instance-management/update) · [Bun compatibility and upgrade rules](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/12_operations.md#upgrading-bun).

## Where Reliaburger fits

**Choose Reliaburger for an integrated cluster workflow:** place replicas across nodes, inspect why an application is unhealthy, trace its network path and test a controlled failure with the same toolset. Our assessment is that this fits operators who value cluster behavior and evidence-rich diagnosis. Coolify fits teams whose priority is a self-service browser workflow for builds, previews and domains. [Reliaburger diagnostics](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/07_diagnostics.md) · [controlled faults](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/05_chaos.md) · [Coolify workflow](https://coolify.io/docs/applications/deployments/overview).

Coolify operates a web application with PostgreSQL and Redis alongside Docker and its proxy. Reliaburger embeds more functions, but remains pre-1.0, with rootful/eBPF cluster requirements and no built-in ACME. The operational choice is between maintaining composed services and accepting a young integrated platform's constraints. [Coolify services](https://github.com/coollabsio/coolify/blob/f55efc859dfcbbf79274c4907b3df89f9767c11d/docker-compose.yml) · [Reliaburger status](https://github.com/reliaburger/reliaburger/blob/v0.2.2/README.md) · [TLS limits](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/03_networking.md).

Backups need precision: current Coolify supports eligible application storage-mount archives as well as engine-aware dumps. Reliaburger provides Btrfs snapshots and separate council backups; a council backup excludes application volumes. None of those facts proves a successful restore of your database. [Storage archives](https://coolify.io/docs/core/persistent-storage/storage-mounts/backups) · [volume snapshots](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/11_images-and-volumes.md) · [council recovery](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/12_operations.md#backing-up-the-council).

## Evidence and limits

Reviewed [Coolify's deployment implementation](https://github.com/coollabsio/coolify/blob/f55efc859dfcbbf79274c4907b3df89f9767c11d/app/Jobs/ApplicationDeploymentJob.php), [database backup job](https://github.com/coollabsio/coolify/blob/f55efc859dfcbbf79274c4907b3df89f9767c11d/app/Jobs/DatabaseBackupJob.php), [volume backup job](https://github.com/coollabsio/coolify/blob/f55efc859dfcbbf79274c4907b3df89f9767c11d/app/Jobs/VolumeBackupJob.php) and the pinned Reliaburger manuals linked above. Current documentation can change independently of a tagged release; implementation claims use the stated versions. No comparative uptime, throughput, memory or recovery-time result was measured.

---

The Markdown copy of <https://reliaburger.com/compare/coolify>.
