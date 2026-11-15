<!-- Generated from docs/website/compare/nomad.html by `make agent-docs`: edit the page, not this file. -->

COMPARISON · REVIEWED 15 OCTOBER 2026

# Reliaburger and Nomad

Reliaburger brings deployment, a registry, ingress and incident diagnosis into one toolchain. For a small team, that means fewer platform services to assemble. Nomad offers broader workload controls and a documented upgrade path for long-lived clusters.

## The verdict

**Which Reliaburger.** Installation examples and capability comparisons both describe the [0.2.2 release](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.2), which the public installer installs, re-checked on 21 October 2026. Its node-by-node rollouts and `RolloutFailed` handling arrived in 0.2.1. [Deployment contract](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md#rolling-deploys) · [Compatibility policy](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/releasing.md#compatibility-before-100).

### Where Reliaburger leads

Reliaburger puts a registry, image builds, ingress, GitOps, metrics, log export and fault injection into its agent. For a small team starting without those services, that is less platform assembly. Its `path`, `wtf` and live test commands give incidents a common interface. [Included components](https://github.com/reliaburger/reliaburger/blob/v0.2.2/README.md#whats-in-the-binary) · [Diagnostics](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/07_diagnostics.md).

### Where Nomad leads

Nomad is the stronger choice when you need task groups, multiple runtime drivers, CSI storage, canary promotion and a documented in-place upgrade path. These are concrete capabilities, not a claim that it wins an unperformed reliability benchmark. [Workload model](https://developer.hashicorp.com/nomad/docs/glossary) · [CSI](https://developer.hashicorp.com/nomad/docs/architecture/storage/csi) · [Deployment strategies](https://developer.hashicorp.com/nomad/docs/job-specification/update) · [Upgrades](https://developer.hashicorp.com/nomad/docs/upgrade).

This compares [Nomad 2.0.7](https://github.com/hashicorp/nomad/releases/tag/v2.0.7) with the Reliaburger release above. Nomad Community and Enterprise differ; a feature marked Enterprise in its documentation should not be counted as a free Community capability. [Edition boundaries](https://developer.hashicorp.com/nomad/docs/enterprise).

## Translate the concepts

| What you mean              | Nomad                                                                                                                                                                                       | Reliaburger                                                                                                                                                                                                                                                  |
| -------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| A long-running application | A service job contains task groups; each group contains one or more tasks. [Job specification](https://developer.hashicorp.com/nomad/docs/job-specification/job).                           | An `[app.web]` block combines image, replicas, resources, health and ingress. [App model](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md).                                                                           |
| One scheduled instance     | An allocation maps a task group to a client. Co-located tasks share a lifecycle. [Glossary](https://developer.hashicorp.com/nomad/docs/glossary).                                           | An app instance runs on a node. There is no equivalent general multi-container task group; Kubernetes sidecars are dropped on import. [Translation limits](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/09_kubernetes.md#what-doesnt). |
| Control and execution      | Servers hold consensus state and schedule; clients execute allocations. [Architecture](https://developer.hashicorp.com/nomad/docs/architecture).                                            | Every node runs `bun`; a subset forms the Raft council. [Architecture](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/06_under-the-hood.md).                                                                                             |
| Service discovery          | Native discovery is available; Consul is an optional provider with additional capabilities. [Discovery providers](https://developer.hashicorp.com/nomad/docs/networking/service-discovery). | Built-in DNS and eBPF virtual IP routing. Nodes still need network reachability. [Networking](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/03_networking.md).                                                                          |
| Persistent storage         | Host volumes or CSI plugins; the chosen backend determines durability. [Volume contract](https://developer.hashicorp.com/nomad/docs/job-specification/volume).                              | Node-local volumes, Btrfs snapshots and optional off-node uploads; no distributed volume service. [Volumes](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/11_images-and-volumes.md#volumes).                                            |

## Install a sandbox, then plan the cluster

Nomad’s macOS CLI installation is below; its [installation guide](https://developer.hashicorp.com/nomad/docs/install) also covers Linux packages and signed binary downloads. The development agent is a disposable single-process sandbox. For container experiments, use a Linux host with Docker installed. It is not a secure production cluster. [Quickstart](https://developer.hashicorp.com/nomad/docs/quickstart).

NOMAD · MACOS CLI

```
brew tap hashicorp/tap
brew install hashicorp/tap/nomad
nomad version
```

NOMAD · LINUX SANDBOX, AFTER INSTALL

```
sudo nomad agent -dev
```

Production work adds server/client configuration, durable state, connectivity and security configuration. Do not assume Consul or Vault is mandatory for every Nomad deployment: native service discovery and Nomad Variables cover some needs. [Roles](https://developer.hashicorp.com/nomad/docs/architecture) · [Discovery](https://developer.hashicorp.com/nomad/docs/networking/service-discovery) · [Variables](https://developer.hashicorp.com/nomad/docs/concepts/variables).

RELIABURGER · RELEASED LAPTOP CLUSTER

```
curl -fsSL https://reliaburger.com/install.sh | RELIABURGER_VERSION=v0.2.2 sh -s -- --laptop
relish nodes
relish council status
```

That installer creates three managed Linux VMs, rather than installing only a CLI. It needs macOS on Apple silicon, or Linux (x86\_64 or arm64) with QEMU/KVM, roughly 8 GiB free memory and 15 GiB disk. Existing servers use the separate rootful-runc/eBPF deployment guide. A one-line laptop demonstration should not be mistaken for a one-line production deployment. [Quickstart](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/quickstart.md) · [Linux servers](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/linux-servers.md).

## Run, inspect and change an application

Assume an authenticated context, available capacity, a valid `web.nomad.hcl` defining job `web`, group `api`, task `server`; and a Reliaburger `web.toml` defining app `web`. These are command equivalents, not interchangeable manifests.

NOMAD · JOB AND ALLOCATION WORKFLOW

```
nomad job validate web.nomad.hcl
nomad job plan web.nomad.hcl
nomad job run web.nomad.hcl
nomad job status web
nomad alloc logs -job web server
nomad job scale web api 3
nomad job history web
```

RELIABURGER · APP WORKFLOW

```
relish lint web.toml
relish apply web.toml --dry-run
relish apply web.toml
relish inspect web
relish logs web
# Set replicas = 3 in web.toml, then:
relish apply web.toml
relish history web
```

Commands: [Nomad job reference](https://developer.hashicorp.com/nomad/commands/job), [allocation logs](https://developer.hashicorp.com/nomad/commands/alloc/logs), [scale](https://developer.hashicorp.com/nomad/commands/job/scale); [Reliaburger deploy manual](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md) and [observability](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/04_observability.md). Nomad’s `-job` log selector chooses an allocation; it is not a cluster-wide merged log query.

Nomad’s plan actually invokes the scheduler and returns a modify index that `job run -check-index` can use to detect intervening edits. Exit status 1 can mean a valid plan changes allocations, so CI must distinguish it from an error. Reliaburger’s dry-run previews desired-spec changes; it does not promise an equivalent scheduling simulation. [Plan semantics](https://developer.hashicorp.com/nomad/commands/job/plan) · [Reliaburger planner](https://github.com/reliaburger/reliaburger/blob/v0.2.2/src/relish/plan.rs).

For an incident, Reliaburger adds `relish wtf` and `relish path web --to db`. Nomad exposes allocation events, task logs, checks and execution through its CLI and UI; that is a different diagnostic workflow, not an absence of observability. [Integrated diagnosis](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/07_diagnostics.md) · [Allocation inspection](https://developer.hashicorp.com/nomad/commands/alloc).

## Application releases and orchestrator upgrades

**Application update:** edit the image and run the same plan/apply workflow. Nomad’s update policy supports health gates, canaries, promotion and automatic reversion. Since 0.2.1, Reliaburger releases changed node shares in sequence, stops failed specifications with `RolloutFailed`, restores old capacity before retiring healthy replacements, and keeps successful rollback versions in Raft. [Nomad update policy](https://developer.hashicorp.com/nomad/docs/job-specification/update) · [Reliaburger rollouts](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md#rolling-deploys).

To deliberately return an application to a previous version, choose a recorded Nomad version, then use `nomad job revert web VERSION`; Reliaburger uses `relish rollback web`. Neither command reverses an application’s database migration. [Nomad revert](https://developer.hashicorp.com/nomad/commands/job/revert) · [Reliaburger rollback](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md).

**Orchestrator update:** Nomad documents incremental server-first upgrades, health checks, then client updates. Supported version boundaries matter, and downgrades are not supported as a general inverse. Reliaburger has signed rolling binary replacement and compatibility checks, but pre-1.0 incompatible formats require a fresh cluster. Moving from 0.2.1 to 0.2.2 is such a break, as 0.1.6 to 0.2.0 and 0.2.0 to 0.2.1 were. This makes Nomad’s upgrade continuity a real advantage for a long-lived deployment. [Nomad upgrade procedure](https://developer.hashicorp.com/nomad/docs/upgrade) · [Reliaburger format boundary](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/releasing.md#compatibility-before-100).

## Where Reliaburger fits

**One toolchain for deployment and incidents.** Reliaburger combines its registry, image signing, logs, tests and fault commands so a small team can deploy and rehearse failures without assembling each service separately. Its app configuration and diagnostics share the same CLI. That integration is the reason to evaluate it; scale targets and remaining release qualification are documented separately. [Included components](https://github.com/reliaburger/reliaburger/blob/v0.2.2/README.md#whats-in-the-binary) · [Diagnostics](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/07_diagnostics.md) · [Fault tools](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/05_chaos.md) · [Measured results versus targets](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/whitepaper.md#2-design-goals).

**Choose Nomad** when co-located task groups, CSI-backed storage, selectable runtime drivers and deployment controls are requirements. Treat its ecosystem as work you can choose to integrate: for example, HashiCorp documents external ingress and telemetry integrations, while Reliaburger supplies basic ingress, storage of metrics and alert webhooks itself. [CSI](https://developer.hashicorp.com/nomad/docs/architecture/storage/csi) · [Delivery controls](https://developer.hashicorp.com/nomad/docs/job-specification/update) · [Ingress integration](https://developer.hashicorp.com/nomad/docs/networking/load-balancer/traefik) · [Monitoring](https://developer.hashicorp.com/nomad/docs/monitor) · [Reliaburger components](https://github.com/reliaburger/reliaburger/blob/v0.2.2/README.md#whats-in-the-binary).

**Learning curve, our assessment:** Nomad introduces jobs, groups, tasks, allocations and evaluations, then the services you integrate. Reliaburger’s app block and common CLI reduce the first layer, but you still learn quorum, health checks, data locality and upgrades. Neither removes distributed-systems responsibilities. [Nomad concepts](https://developer.hashicorp.com/nomad/docs/glossary) · [Reliaburger concepts](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/02_cluster-basics.md).

Licensing also differs: Reliaburger uses Apache-2.0; Nomad 2.0.7’s source carries the Business Source License with an additional-use grant. Read the terms for your intended use rather than treating the licences as equivalent. [Reliaburger licence](https://github.com/reliaburger/reliaburger/blob/v0.2.2/LICENSE) · [Nomad licence](https://github.com/hashicorp/nomad/blob/v2.0.7/LICENSE).

## Evidence and limits

Baseline: [Nomad 2.0.7 release](https://github.com/hashicorp/nomad/releases/tag/v2.0.7), its official 2.0.x documentation, and the [Reliaburger 0.2.2 source](https://github.com/reliaburger/reliaburger/tree/v0.2.2/), the release the installer installs. Citations beside claims identify the supporting contract or implementation. Documentation URLs may advance; the Reliaburger and licence links are pinned.

We reviewed documentation and source, and re-checked the Reliaburger side for 0.2.2 on 21 October 2026. We did not install both systems on identical machines or measure failure recovery, memory, cost or learning time. Verdicts concern the supported workflows described here. See the [shared review method](https://reliaburger.com/compare/methodology#method) for the wider comparison.

---

The Markdown copy of <https://reliaburger.com/compare/nomad>.
