# Reliaburger: The Batteries-Included Container Orchestrator

**A whitepaper on the design, architecture, and vision for Reliaburger, a radically simplified container orchestration platform written in Rust.**

**Version 0.2.0, October 2026**

---

## The Reliaburger Manifesto

We believe that:

1. Running workloads in production should not require a PhD in distributed systems.
2. The tools you use to deploy your code should be simpler than the code itself.
3. Batteries should be included, not sold separately.
4. A cluster of machines should be as easy to manage as a single machine.
5. Every node starts equal. Roles are earned dynamically, not assigned statically.
6. Your apps should keep running even when the control plane has a bad day.
7. The state of your cluster should be recoverable from the cluster itself.
8. Configuration should be measured in lines, not pages.
9. The default experience should be production-ready, not a starting point for further assembly.
10. Open source means open source. No bait-and-switch. No relicensing.

---

## Table of Contents

- [1. Vision & Problem Statement](#1-vision--problem-statement)
- [2. Design Goals](#2-design-goals)
- [3. Design Principles](#3-design-principles)
- [4. Naming Conventions](#4-naming-conventions)
- [5. Core Concepts](#5-core-concepts)
- [6. Node Configuration (Getting Started)](#6-node-configuration-getting-started)
- [7. Cluster Architecture](#7-cluster-architecture)
- [8. Leader Election & State Management](#8-leader-election--state-management)
- [9. Networking](#9-networking)
- [10. Service Discovery (Onion)](#10-service-discovery-onion)
- [11. Security](#11-security)
- [12. Built-In Image Registry (Pickle)](#12-built-in-image-registry-pickle)
- [13. Deployments & Rollouts](#13-deployments--rollouts)
- [14. GitOps](#14-gitops)
- [15. Observability](#15-observability)
- [16. Debugging & Tooling (Relish)](#16-debugging--tooling-relish)
- [17. Process Workloads (Non-Container)](#17-process-workloads-non-container)
- [18. Fault Injection (Smoker)](#18-fault-injection-smoker)
- [19. Testing & Benchmarks](#19-testing--benchmarks)
- [20. Self-Upgrades](#20-self-upgrades)
- [21. Multi-Cluster (Franchise)](#21-multi-cluster-franchise)
- [22. What's Deliberately Not Included](#22-whats-deliberately-not-included)
- [23. Comparison Matrix](#23-comparison-matrix)
- [24. Licensing](#24-licensing)
- [25. Questions & Answers](#25-questions--answers)

---

## 1. Vision & Problem Statement

### The Complexity Crisis

Kubernetes has become the de facto standard for container orchestration, commanding over 80% market share. Yet its own users consistently identify complexity as their biggest challenge. Industry surveys paint a clear picture:

- 4 in 5 production Kubernetes users say it is more complex than any other technology they use (Spectro Cloud, 2022).
- 48% of users struggle to choose which infrastructure components to use, up from 29% the previous year (Spectro Cloud, 2024).
- 57% of users cite the steep learning curve as their top challenge (Civo Developer Survey).
- 57% of respondents report their Kubernetes infrastructure consists of more than 11 distinct software elements (Spectro Cloud, 2024).
- 91% of enterprises running Kubernetes have over 1,000 employees, so smaller teams are disproportionately burdened by the complexity.

Before deploying a single container to production on Kubernetes, you typically need to install and configure:

- A Kubernetes distribution
- A CNI plugin (Flannel, Calico, or Cilium)
- An ingress controller (nginx, Traefik, or HAProxy)
- cert-manager for TLS certificates
- Prometheus for metrics
- Grafana for dashboards
- Loki or Elasticsearch for logs
- ArgoCD or Flux for GitOps
- A container registry (Harbor, Artifactory, or a cloud provider's offering)

That's eight separate systems (on top of Kubernetes itself) to install, configure, monitor, and keep compatible with each other.

### Lightweight Distributions Don't Solve This

K3s, k0s, and MicroK8s make Kubernetes smaller. They reduce binary size and resource footprint. But they maintain full API compatibility with upstream Kubernetes, which means they inherit all of its conceptual complexity. You still need to understand Pods, Deployments, ReplicaSets, Services, Endpoints, Ingresses, ConfigMaps, Secrets, PersistentVolumeClaims, StatefulSets, DaemonSets, CRDs, RBAC, NetworkPolicies, and dozens of other resource types. The YAML is the same. The learning curve is the same. The ecosystem sprawl is the same.

### The Opportunity

There's a large and growing market of teams who need more than Docker Compose (which doesn't do multi-node or rolling deploys) but far less than Kubernetes. These teams typically have 2-200 nodes, run web applications and APIs behind load balancers, and want their containers running reliably without dedicating a full-time platform engineering team to the task.

Reliaburger is built for these teams. It's one node agent (`bun`) and one CLI (`relish`), and the agent includes everything you need to run containers in production: scheduling, networking, ingress with built-in TLS (from the cluster's own CA or your certificate; automatic ACME certificates are planned, [#369](https://github.com/reliaburger/reliaburger/issues/369)), service discovery, a container image registry, log collection, metrics and dashboards, GitOps, and a web UI. No plugins. No ecosystem to assemble. No YAML to wrestle.

The architecture is designed for 10,000 nodes to provide headroom, but the primary audience is teams with 2-200 nodes who benefit from a single, well-tested architecture rather than separate solutions for small and large clusters.

---

## 2. Design Goals

Reliaburger is designed to meet the following quantitative targets. These engineering constraints inform every architectural decision in this document. A target isn't a result: the last column says which ones a release has measured, and links the record in [`qualification/`](qualification/). Everything marked "Target (unmeasured)" is a design goal no release has checked yet.

| Target | Value | Measured |
|--------|-------|----------|
| Maximum nodes per cluster | 10,000 | Target (unmeasured). Only simulated gossip membership has run at this size (`tests/gossip_10k.rs`); every node still reports to the leader directly (§8.1) |
| Maximum apps per node | 500 | Target (unmeasured) |
| Job scheduling throughput | 100,000,000 jobs per day (~1,160 per second sustained) | Target (unqualified). Per-job overhead of each runtime on one small VM: [timed job scenarios](qualification/2026-10-09-timed-job-scenarios/README.md); the sustained gate is [#668](https://github.com/reliaburger/reliaburger/issues/668) |
| Leader election time | < 5 seconds | Target (unmeasured) |
| Time to full operability after leader loss | < 20 seconds (5s election + 15s learning period) | Target (unmeasured) |
| State reconstruction time (cold start, full cluster) | < 30 seconds | Target (unmeasured) |
| Time from bare metal to first deploy | < 5 minutes | Laptop quickstart only: 178 s and 282 s from the first `curl` to a ready three-node cluster on Apple silicon ([record](qualification/2026-09-27-v0.1.0-release-closure.md#how-it-ended)); bare-metal Linux unmeasured |
| Cold start (new node joins and accepts work) | < 60 seconds | Target (unmeasured) |
| Image distribution to N total copies (default 2: the pusher plus one peer) | < 30 seconds per layer (replicated asynchronously by the leader's heal loop) | Target (unmeasured) |
| Idle node memory | No target | 63 MiB RSS for a single-node cluster, 48 MiB standalone, on macOS before the release candidate ([record](qualification/2026-10-11-idle-node-memory.md), Q15); Linux unmeasured |
| GPU scheduling | First-class resource, whole-device allocation | Planned ([#359](https://github.com/reliaburger/reliaburger/issues/359)); not in the binary |
| Minimum kernel version | Linux 5.8+ (for eBPF CO-RE, BTF, BPF_LINK support and the boot-time clock helper) | Requirement |
| cgroup version | v2 (required for eBPF service discovery and resource isolation) | Requirement |

> **Note on leader election time:** The < 5 second target measures the Raft election itself. The subsequent learning period (up to 15 seconds) runs before the new leader places new work. Total time to full operability after leader loss is < 20 seconds. The data plane is unaffected throughout.

**10,000 nodes per cluster** demands that the coordination layer never require all-to-all communication. The two-layer architecture (Mustard gossip for all nodes, Raft for a small council) is a direct consequence of this target. Raft doesn't scale beyond single digits; gossip scales to tens of thousands.

**500 apps per node** demands that the Bun agent, Onion eBPF service map, Ketchup log collector, and Mayo metrics store all have sub-millisecond per-app overhead. Rust's zero-cost abstractions and lack of garbage collection pauses make this achievable in a way that a garbage-collected runtime would struggle to match at the tail.

**100 million jobs per day** demands a scheduler (Meat) that can make placement decisions at high throughput without becoming a bottleneck. At a sustained rate, this is over a thousand jobs dispatched per second. The design implications (batch scheduling, delegated execution, and asynchronous reporting) are detailed in [design/scheduler-meat.md](design/scheduler-meat.md).

**GPU as a first-class schedulable resource** means that AI/ML workloads aren't a day-two feature. This is a target, not current behaviour: no release allocates a GPU. Today the Bun agent detects NVIDIA GPUs at startup (it probes `/dev/nvidia0` and parses `nvidia-smi`, so no vendor library is linked into the binary) and refuses a workload that asks for more cards than the node has, or any card when GPU support is off. A request it does accept starts without a device, because nothing passes `/dev/nvidia*` into the container yet. The count doesn't reach the cluster scheduler either, so Meat sees zero GPUs on every node and a clustered app with `gpu = 1` is never placed. Refusing `gpu > 0` everywhere until passthrough lands is [#701](https://github.com/reliaburger/reliaburger/issues/701). Propagating GPU capacity (backlog item F01) and passing the `/dev/nvidia*` devices into the container's OCI spec (see [design/agent-bun.md](design/agent-bun.md) §5.4) are both planned ([#359](https://github.com/reliaburger/reliaburger/issues/359)); the [roadmap](roadmap.md#releases-after-010) lists GPU scheduling under "Later", after 0.4.0.

**Recovery from the loss of any single node, the leader, or the entire council** means that none of these failures interrupts the data plane. Applications continue running when the control plane is unavailable. Losing a node, the leader or a minority of council voters heals without an operator. Losing the whole council is the exception: an operator restores the desired state from a sealed backup (§8.3), because that recovery throws away Raft history and a human should choose which backup wins.

---

## 3. Design Principles

### 3.1 One Agent, Batteries Included

Reliaburger ships as two binaries: `bun`, the node agent, and `relish`, the CLI. Every component of a node (the scheduler, ingress proxy, image registry, metrics store, log collector, GitOps engine, eBPF service discovery, and web UI) is compiled into `bun`. There's no separate control-plane binary and no plugin to pick, and the two binaries come from one release, so there's no compatibility matrix between components.

The binaries aren't static. Release builds link glibc (`*-linux-gnu`), and a Linux node needs a few host packages: `runc`, `uidmap`, `iptables`, `iproute2`, `nftables` and `btrfs-progs` (the [Linux servers guide](linux-servers.md#1-prerequisites-and-network-requirements) installs them). Static musl builds are planned with the 0.3.0 appliance ([#401](https://github.com/reliaburger/reliaburger/issues/401)).

### 3.2 Apps, Not Pods

The unit of deployment is an **App**, not a Pod/Deployment/Service triple. At its simplest, an App is an image, a replica count, and a port: three lines of TOML. A single App resource replaces the Kubernetes concepts of Deployment, ReplicaSet, Service, Ingress, HorizontalPodAutoscaler, DaemonSet, and node affinity rules. Optional inline sections (placement constraints, autoscaling, firewall, identity, deploy strategy) add capabilities that would otherwise require separate Kubernetes resources, but the minimal definition stays minimal.

### 3.3 Convention Over Configuration

Sensible defaults everywhere, with security-sensitive choices left explicit. An ingress route is plain HTTP unless it selects `tls = "cluster"` (the cluster's Ingress CA) or `tls = "explicit"` (an operator-supplied certificate and key). Automatic public certificate provisioning is a post-v1 feature: `auto` and `acme` are rejected rather than quietly weakening the route. Health checks use sane timeouts. Rolling deploys are the default strategy. Log collection is on by default. Reliaburger collects metrics automatically. Configuration is for overriding defaults, not for bootstrapping basic functionality.

### 3.4 Homogeneous Nodes

Every node in the cluster runs the same binary with the same capabilities. There's no separate "server" binary, no "agent" binary, and no installation-time role assignment. Nodes are *structurally* homogeneous. They are *functionally* differentiated at runtime: the cluster elects leaders, selects council members based on stability, and assigns coordination duties dynamically. Operators can tune node behaviour (disabling ingress on GPU-heavy compute nodes, reserving resources for council duties via `node.toml`) without this constituting a distinct "node type." The same binary, the same join process, and any node can assume any role if the cluster needs it to. Adding capacity means adding machines, not deciding what kind of machine to add.

### 3.5 Configuration at Scale

A single TOML file works for small deployments, but teams with dozens of apps need structure. Reliaburger supports both models: single-file mode (everything in one file) and directory mode (split across multiple `.toml` files with a `_defaults.toml` for shared values). `relish fmt` formats files and `relish lint` validates them.

### 3.6 Familiar but Not Compatible

Reliaburger uses TOML for configuration, chosen for its unambiguous type system (no YAML "Norway problem" where `NO` becomes a boolean), no significant whitespace, and alignment with the Rust ecosystem (`Cargo.toml`). It draws on concepts familiar to anyone who has used Docker, Kubernetes, or Nomad. But it isn't API-compatible with any of them. Full compatibility would inherit the complexity that Reliaburger exists to eliminate. Migration tooling bridges the gap in both directions: `relish import` converts Kubernetes YAML into Reliaburger TOML (with warnings for unsupported features), and `relish export` generates standard Kubernetes manifests from Reliaburger configuration. These are migration tools, not runtime interfaces.

### 3.7 Written in Rust

The entire system is implemented in Rust. This provides memory safety without garbage collection, predictable low-latency performance, small binary size, and minimal resource overhead, all qualities that matter for an always-on infrastructure component, especially in resource-constrained and edge environments.

---

## 4. Naming Conventions

Every component in the Reliaburger system is named after a burger part.

| Component | Name | Role |
|-----------|------|------|
| Project | **Reliaburger** | The product |
| Agent/daemon | **Bun** | Runs on every node; holds everything together |
| Scheduler | **Meat** | The core scheduling engine |
| Container runtime interface | **Grill** | Where containers get cooked |
| Image registry | **Pickle** | Preserved image layers, distributed across the cluster |
| Gossip protocol layer | **Mustard** | Spreads information everywhere |
| Log collector | **Ketchup** | Captures and stores application logs |
| Metrics store / TSDB | **Mayo** | Smooth, rich metrics coating everything |
| GitOps engine | **Lettuce** | Fresh layers from git |
| Security / mTLS / identity | **Sesame** | Seeds on top; "open sesame" for access |
| Service discovery (eBPF) | **Onion** | Invisible layers beneath the surface |
| Web UI | **Brioche** | The fancy presentation layer |
| Ingress / reverse proxy | **Wrapper** | The external-facing boundary |
| CLI + terminal UI | **Relish** | How you interact with the system |
| Fault injection engine | **Smoker** | Adds smoke to the grill to stress-test the meat |

---

## 5. Core Concepts

Reliaburger defines seven workload resource types, plus `[build.*]` for in-cluster image builds (§5.2). Every production workload can be expressed using a combination of these types.

### 5.1 App

An App is a long-running process with replicas, replacing the seven Kubernetes resource types described in Section 3.2.

```toml
[app.web]
image = "myapp:v1.4.2"
replicas = 3
port = 8080
memory = "128Mi-512Mi"
cpu = "100m-500m"

[app.web.health]
path = "/healthz"

[app.web.ingress]
host = "myapp.com"
tls = "cluster"  # certificate from the cluster's Ingress CA; omit for plain HTTP
```

**Replica modes:** `replicas = 3` places exactly 3 instances across available nodes. `replicas = "*"` runs one instance on every node (daemon mode). An App with a volume becomes stateful. GPU allocation (`gpu = 1`) is planned ([#359](https://github.com/reliaburger/reliaburger/issues/359)): no release passes a device into the container yet (§2). An App with `exec` instead of `image` runs a host binary as a process workload (see Section 17).

**Autoscaling:** Apps can declare an autoscale block to adjust replicas based on observed metrics:

```toml
[app.web.autoscale]
min = 2
max = 10
metric = "cpu"
target = "70%"
```

`metric` is `"cpu"` or `"memory"`, and `target` is utilisation of each replica's request, the Kubernetes HPA convention (an app with no CPU request is measured against one core; memory scaling requires a memory request). `min` must be at least 1: there's no scale-to-zero, because an app with no replicas reports no CPU or memory to scale back up on. The leader makes scaling decisions locally based on the per-instance CPU and memory Mayo records. The Lettuce GitOps engine treats autoscaler adjustments as runtime overrides (see Section 14).

**Init containers:** Apps support init containers via an `[[app.web.init]]` block that runs before the main container starts, meant for database migrations, config generation, or dependency checks. Today an init step runs its command in the app's image, without the app's environment, secrets or volumes; honouring its own `image` and giving it the app's env and mounts is [#701](https://github.com/reliaburger/reliaburger/issues/701).

**Placement constraints:** Nodes carry labels (configured in `node.toml`). Apps can use `required` labels (hard constraints) and `preferred` labels (soft constraints) to control placement.

```toml
[app.web.placement]
required = ["region=us-east"]
preferred = ["ssd=true"]
```

> For scheduling algorithm details, see [design/scheduler-meat.md](design/scheduler-meat.md).

### 5.2 Job

A Job is a run-to-completion task, either on-demand or scheduled, replacing Kubernetes Job and CronJob. Jobs can run inside a container or as a host process (`exec`/`script`).

```toml
[job.db-migrate]
image = "myapp:v1.4.2"
command = ["npm", "run", "migrate"]
run_before = ["app.api"]

[job.cleanup]
image = "cleanup:latest"
schedule = "0 3 * * *"
```

**Common execution lifecycle (0.2.0 development):** A definition describes work;
a run fixes its revision and trigger. A singleton is an indexed run with count
one. Manual jobs, arrays, cron and deployment hooks share placement, node-local
CPU/memory admission, owned attempts, cancellation and durable results. The
leader grants chunks of indexes; workers start each attempt when its actual
request fits beside applications. A chunk of 1,000 isn't 1,000 simultaneously
reserved jobs. Mixed manifests group distinct resource profiles.

Cron claims a UTC minute and creates its run in one durable transaction. Default
overlap is forbidden and missed minutes are skipped; clock rollback and leader
changes can't replay claimed occurrences. Deployment intent pins hook results
and gates app publication on accepted success. Standalone operation persists
the same state before acknowledging work.

Known failures follow the run's attempt policy. Ordinary jobs and hooks keep an
unknown outcome owned until a user acknowledges replay; bulk work can opt into
at-least-once retry. Neither policy promises exactly-once external effects.
Encrypted templates and authorised scripts retain namespace and host-execution
checks. Bounded summaries, accepted rates, backlog, duration histograms and
selected result pages replace listing millions of jobs. Each definition keeps
its last ten outcomes after the runs' detail is pruned, every node exports job
counters and duration histograms labelled only by namespace, definition,
profile and node, and the leader records one event per run (never per task),
sending failed and unknown-outcome runs to the alert webhooks. The daily throughput
claim still needs sustained qualification, including container launch cost,
resource demand, storage and service quality. See the [job manual](manual/14_batch-jobs.md).

**Image builds:** `relish build` builds container images from `[build.*]` declarations and pushes them to Pickle. A `destination` field (a `pickle://` reference) scopes registry access. This is a manual build path; Lettuce does not dispatch build jobs or inject `${GIT_SHA}` into their tags.

> For batch scheduling and build job details, see [design/scheduler-meat.md](design/scheduler-meat.md) and [design/registry-pickle.md](design/registry-pickle.md).

### 5.3 Secret

Reliaburger stores secrets encrypted in git using asymmetric encryption (age). The cluster's public key is distributed freely; only the cluster holds the private key. Developers can encrypt secrets offline without cluster access.

```toml
[app.api.env]
DATABASE_URL = "ENC[AGE:YWdlLWVuY3J5cHRpb24...]"
NODE_ENV = "production"
```

`relish secret rotate` handles key rotation of the cluster-wide keypair with graceful transition periods (both old and new ciphertexts decrypt during the overlap).

**Namespace-scoped keypairs:** `secret_key = true` on a `[namespace.*]` gives that namespace its own age keypair. The leader generates it, re-seals the namespace's existing secrets to it, and from then on its values decrypt only with the namespace's keys. Every node still holds the cluster master key that wraps those keys, so the boundary holds against other tenants' tokens and workloads, not against a compromised node (see the [security manual](manual/10_security.md)).

> For encryption implementation details, see [design/security-sesame.md](design/security-sesame.md).

### 5.4 ConfigFile

A free-text file injected into a specific path inside the container. Unlike environment variables, ConfigFiles can contain arbitrary content (NGINX configs, application YAML, TLS certificates). Declared inline with `content`, or with `source` naming a file on the node, which the node bind-mounts. A `source` is gated like a host-path volume (§5.5): the node must list it under `[storage] allowed_host_paths`, and a token with a `[permission]` block needs `host-exec`.

```toml
[[app.nginx.config_file]]
path = "/etc/nginx/nginx.conf"
content = """
worker_processes auto;
events { worker_connections 1024; }
http { server { listen 80; location / { proxy_pass http://web.internal:8080; } } }
"""
```

### 5.5 Volume

Local persistent storage attached to an App. Volumes survive container restarts but are tied to the physical node.

```toml
[app.redis]
image = "redis:7-alpine"
port = 6379

[[app.redis.volumes]]
path = "/data"
size = "10Gi"
```

**Volumes are local by design.** Reliaburger doesn't include distributed storage. For data that must survive node loss: (1) use a managed database service, (2) use an application that handles its own replication, or (3) configure volume snapshots with off-node upload. A `relish wtf` warning for `replicas = 1` Apps whose volumes have no snapshot schedule is planned.

When a volume is created, a node running as root enforces `size` with a Btrfs subvolume quota, or a loop-mounted sparse file on ext4/xfs. Elsewhere `size` only logs a warning, and a changed `size` on an existing volume is ignored; refusing both is [#701](https://github.com/reliaburger/reliaburger/issues/701). Built-in volume snapshots use copy-on-write filesystem snapshots, and Bun's snapshot loop on each node uploads them (Q9).

A volume can instead name a host directory with `source`, as a config file can name a host file. Because every rootful container on a node shares one user-namespace id range, a host path would let one tenant mount another's managed volume, so host paths are gated twice: each node refuses any `source` not under a prefix in its `[storage] allowed_host_paths` (empty by default) and always refuses its own storage and security directories, and a token with a `[permission]` block needs `host-exec` to deploy one. The node rechecks the path for every container it starts, rolling replacements included, and mounts the resolved path it approved rather than the one you wrote, so a symlink swapped in after the check can't redirect it. One race remains: runc mounts that path by name later (after the image pull, and on every restart), so someone who can write under a listed prefix can still replace a directory on it with a symlink in between. Only list prefixes that tenants can't write to.

### 5.6 Permission

Simplified access control. Permissions define who can perform which actions.

```toml
[permission.deployer]
actions = ["deploy", "scale", "logs", "metrics"]
apps = ["web", "api"]
```

Valid actions: `deploy`, `scale`, `logs`, `metrics`, `exec`, `host-exec`, `fault`, `admin`, `secret-read`, `secret-write`. `secret-read` is accepted but grants nothing yet, because no API route returns a decrypted secret. The `host-exec` and `admin` actions are required for process workloads and cluster administration respectively; `host-exec` also covers apps that mount a host path with `source`. Snapshots and builds need `deploy` on their app, chaos faults need `fault`, and test leases need `admin`.

### 5.7 Namespace

Optional workload isolation with resource quotas (CPU, memory, GPU, app count, replica count). Reliaburger uses a single default namespace unless you explicitly create others. CPU, memory and GPU budgets cover jobs as well as apps: the leader charges every job attempt it grants against them, after the namespace's apps. Every namespace also gets its own share of the cluster's job capacity (64 active bulk runs and 64 job definitions), and cron fires and deployment hooks keep reserved headroom that bulk work can't use.

```toml
[namespace.team-backend]
cpu = "8000m"
memory = "16Gi"
gpu = 2
max_apps = 50
max_replicas = 200
```

---

## 6. Node Configuration (Getting Started)

You configure each node's Bun agent via a single TOML file (`node.toml`). A node can join an existing cluster with a two-line configuration:

```toml
[cluster]
join = ["10.0.1.5:9443"]
```

The first node starts from the configuration and PKI generated by `relish
init`. The command writes files; it doesn't start Bun for you.

**First boot experience:**

```sh
# Build, generate a secure one-node cluster, then start it on Linux.
$ cargo build --bins --features ebpf
$ sudo target/debug/relish init cluster --cluster-name prod --node-id node-01
$ sudo target/debug/bun --cluster --runtime runc --config cluster/reliaburger.toml

# In another terminal, create the first administrator and deploy the generated app.
$ export RELIABURGER_TOKEN="$(target/debug/relish \
    --ca-cert cluster/identity/root-ca.crt \
    token create --name first-admin --role admin)"
$ target/debug/relish --ca-cert cluster/identity/root-ca.crt apply cluster/app.toml
$ target/debug/relish --ca-cert cluster/identity/root-ca.crt status
```

The API is HTTPS on `127.0.0.1:9117`; ports 9443, 9444 and 9445 are gossip,
Raft and reporting, not the operator API. On Apple Silicon, use `--runtime
apple` as your ordinary user instead of the `sudo ... --runtime runc` line.
This bootstraps a real but non-resilient one-voter cluster. Run as root on
Linux with `runc` installed, `relish init` turns on `[ebpf]` and `[dns]`, so
namespace isolation holds from the first deploy; Bun refuses to start a
rootful runc node whose eBPF data path fails to load.

Adding a node is a separate two-part operation. Put its node-specific config
and cluster master key on the machine, including the existing node's gossip
address in `[cluster].join`. An authenticated administrator mints one
short-lived token per node against the current leader:

```sh
$ JOIN_TOKEN="$(relish --endpoint https://10.0.1.5:9117 \
    --ca-cert cluster/identity/root-ca.crt \
    join-token create --ttl 15m)"
```

The council stores only its hash and expiry. Relish prints the plaintext once;
the token enrols exactly one node. Then enrol that node's identity against the
leader's **API** address:

```sh
$ relish join --token "$JOIN_TOKEN" --node-id node-02 \
    --identity-dir node-02/identity \
    --ca-fingerprint sha256:<ROOT_CA_FINGERPRINT> \
    https://10.0.1.5:9117
```

`relish join` writes the certificate bundle. It doesn't rewrite node config,
start Bun or claim that the node has joined gossip. Start that node with
`bun --cluster --config node-02/reliaburger.toml` after enrolment. The
fingerprint check turns the initial certificate exchange from blind trust into
an out-of-band verified hand-off. If leadership changes, retry creation or
enrolment against the new leader. A follower refuses the write rather than
returning a token that Raft never committed.

> For the full `node.toml` reference, storage path design, and resource configuration, see [design/agent-bun.md](design/agent-bun.md).

### Dev cluster

Getting started with Kubernetes locally means choosing between minikube, kind, k3d, Docker Desktop's built-in cluster, or Rancher Desktop. Each has trade-offs, none gives you a real multi-node cluster, and you still need to install a CNI plugin, ingress controller, and metrics stack before you can do anything useful.

Reliaburger gives you a real multi-node cluster with a single command:

```bash
$ relish dev create --nodes 3
```

This spins up 3 Ubuntu VMs (via Lima), builds `bun` and `relish` from your current source tree in a separate Lima build VM, installs them, starts Bun as root with runc on each node, configures gossip networking and Raft consensus, and hands you a working cluster. Real VMs, real networking, real containers, running the binaries you just built. It's for working on Reliaburger itself: the build is a debug build, and the cluster's internal transports are plaintext inside the Lima network. Everyone else wants the quickstart, which installs the signed release.

```bash
$ limactl shell reliaburger-1 relish nodes
NODE                 ADDRESS              STATE      COUNCIL  LEADER
reliaburger-1        192.168.105.2:9443   alive      yes      yes
reliaburger-2        192.168.105.3:9443   alive      yes      -
reliaburger-3        192.168.105.4:9443   alive      yes      -

$ relish dev destroy
```

The dev cluster runs the same code as production, with no reduced-functionality variant. Run `relish` on a node (`limactl shell` or `relish dev shell reliaburger-1`): the CLI refuses a plain `http://` endpoint anywhere but loopback, and a remote cluster takes `--endpoint https://<node>:9117 --ca-cert <root-ca.crt>`.

---

## 7. Cluster Architecture

### 7.1 Homogeneous Nodes

Every node runs the same binary: **Bun**. There's no distinction between control plane and worker nodes. Nodes take on additional responsibilities dynamically.

```
┌──────────────────────────────────────────────────────────┐
│                   RELIABURGER CLUSTER                    │
│                                                          │
│  ┌────────┐  ┌────────┐  ┌────────┐  ┌────────┐          │
│  │ Node 1 │  │ Node 2 │  │ Node 3 │  │ Node 4 │  ...     │
│  │  Bun   │  │  Bun   │  │  Bun★  │  │  Bun   │          │
│  │  +apps │  │  +apps │  │  +apps │  │  +apps │          │
│  └───┬────┘  └───┬────┘  └───┬────┘  └───┬────┘          │
│      │           │           │           │               │
│      └───── Mustard gossip mesh ─────────┘               │
│                                                          │
│  ★ = current leader                                      │
│  Raft council: [Node 1, Node 3, Node 4]                  │
│                                                          │
│  Every Bun instance runs:                                │
│   • Container management (Grill)                         │
│   • eBPF service discovery (Onion)                       │
│   • Metrics collector (Mayo)                             │
│   • Log collector (Ketchup)                              │
│   • Image registry node (Pickle)                         │
│   • Gossip participant (Mustard)                         │
│   • Ingress proxy (Wrapper)                              │
│   • Web UI (Brioche)                                     │
│                                                          │
│  The leader additionally runs:                           │
│   • Scheduler (Meat)                                     │
│   • GitOps engine (Lettuce)                              │
│                                                          │
│  Council members additionally run:                       │
│   • API server (reads from local Raft state)             │
│   • Metrics aggregation                                  │
└──────────────────────────────────────────────────────────┘
```

### 7.2 Leader and Council

The leader is an ordinary node that additionally runs the Meat scheduler. Leadership is lightweight: the leader runs application workloads alongside its scheduling duties. Any council member serves API read requests; writes go to the leader via Raft.

```
Read:  User → any node → nearest council member → response (from local state)
Write: User → any node → leader → Raft commit → response
UI:    User → any node → Brioche (local) + API calls (routed as above)
```

---

## 8. Leader Election & State Management

### 8.1 Two-Layer Architecture

Reliaburger uses two distinct protocols for cluster coordination:

**Layer 1: Mustard (Gossip), all nodes.** Based on SWIM, handles membership discovery, failure detection and leader identity broadcast. SWIM is designed to scale to tens of thousands of nodes with O(log N) convergence and O(1) per-node overhead; we've simulated 10,000 members, not run them. Carrying a small per-node resource summary on gossip is planned ([#364](https://github.com/reliaburger/reliaburger/issues/364)); nothing fills it in today.

**Layer 2: Raft, small dynamic council.** A council of typically 3-7 nodes runs Raft consensus for leader election and desired-state replication. The cluster picks council members automatically: a candidate must have been alive in gossip for a few seconds, and among those it prefers a zone the council doesn't cover yet, then the oldest node. Resource-aware selection is planned ([#364](https://github.com/reliaburger/reliaburger/issues/364)).

Variable-size runtime state (what's running where, health, resource usage) travels separately from gossip, so every gossip message fits one 1,400-byte datagram: a probe carries as many membership updates as its header and the sender's endpoints and labels leave room for. Today every node sends its state reports straight to the leader. The design calls for a **hierarchical reporting tree**, where each node reports to an assigned council member and council members aggregate for the leader. That tree is planned ([#364](https://github.com/reliaburger/reliaburger/issues/364)), and it comes before any 10,000-node claim: a leader taking a report from every node directly won't get there.

### 8.2 Reconstructable State

When a new leader is elected, it enters a learning period, during which it collects StateReports from nodes before making scheduling decisions. The learning period ends when 95% of the nodes gossip shows alive have reported (counting the most it has seen alive during the period), or a 15-second timeout fires. Applies still commit to Raft while it learns, but the leader places nothing until the period ends, so new work waits unplaced. Unreported nodes are marked "state unknown," and the leader then schedules using only reported capacity. The data plane is completely unaffected during reconstruction.

### 8.3 Catastrophic Recovery

There are three cases, and they behave differently.

- **A minority of voters fails** (one of three, two of five). The survivors still form a quorum, so they elect a leader if they need one, and the council reconciler promotes healthy nodes to replace the dead voters before removing them. No operator is involved.
- **A majority of voters fails, but some survive.** There's no quorum, so nothing can be elected, scheduled or written; apps keep running. If the lost voters come back, quorum returns and the cluster carries on where it left off. If they're gone for good, the survivors can't regrow the council on their own (changing Raft membership needs a quorum too), so this becomes the operator-triggered recovery below. `relish council recover` runs on one stopped survivor. Before it changes anything, it reads the voter set the restored state names (the data directory's last membership, or the backup's) and asks each voter's API whether it still serves a council. It refuses, naming the voter, while any of them answers as one, and it fails closed when the state names no voters at all; `--force` overrides both. It also refuses a data directory whose fence record says another recovery already replaced it. The recovered node carries a new recovery epoch, above both the restored state's epoch and any epoch its own fence record knows, plus a random 128-bit recovery id. Every Raft RPC and every gossip datagram carries the pair. An old voter that later hears of a newer epoch, from a Raft refusal or from gossip, fences itself: it serves no Raft and refuses writes until you re-enrol it with `relish council re-enrol`. Two councils on the same epoch with different recovery ids came from two separate recoveries (two survivors recovered independently, say); each side fences itself as a split when it meets the other, and you recover one of them again to land it on a newer epoch. A restarted voter doesn't serve Raft until gossip has joined and synced and then had two anti-entropy intervals (20 seconds, `[cluster] recovery_probe_window_secs`) to show it its peers' epochs, unless every peer its membership names has advertised one sooner (a single voter, with no peers to hear from, serves once gossip is up); it logs when the window runs out without hearing from them all. Old voters that can reach only each other, never a newer-epoch node, still can't learn of the recovery, so re-enrol old voters before starting them. A fence record that fails to write (a full disk) is retried until it lands. `relish council status` and `relish wtf` compare every node's view and show a fence or a split as critical, and an unwritten fence as a warning.
- **Every voter fails at once.** There's nothing left to elect from.

Recovery from the last two cases is deliberately operator-triggered rather than automatic, because it discards the dead cluster's Raft history. A surviving node restores the desired state from the last sealed backup (or, if it was a voter, from its own snapshot and committed log), re-bootstraps a fresh single-voter Raft with a bumped recovery epoch, and the reconciler regrows the council. The cost is honest: anything written after the last backup is lost. Network partitions can't cause split-brain: Raft requires a majority quorum to elect a leader, so a minority side simply has no quorum. Nodes on the minority side operate in data-plane-only mode (apps continue running, no new deploys) until the partition heals.

> **Design note (fully-automatic recovery).** The original design called for *pre-seeded* recovery candidates chosen for zone diversity, gossiped as an encrypted list, so the highest-priority survivor could assume leadership with no operator in the loop. That remains an architecture proposal, not shipped behaviour: the operator-triggered restore above is what the binary does today (`relish council recover`, [design/gossip-mustard.md](design/gossip-mustard.md)). We chose to ship the operator-in-the-loop path first because full-council loss is the one failure where discarding history is unavoidable, and a human confirming *which* backup to restore is cheaper than getting an automated tie-break wrong.

**Partition degradation:** During a sustained partition, isolated nodes experience progressive degradation. The eBPF service map becomes stale for new cross-partition connections (immediate), certificate rotation fails (at 30 minutes), and workload certificates expire (at 1 hour, or 4 hours with the grace period). Apps with existing connections and valid certificates continue operating throughout.

**Clock synchronisation:** Reliaburger assumes nodes have reasonably synchronised clocks (NTP or equivalent). Certificate validation, token TTLs, and cron scheduling depend on clock accuracy within a few seconds.

**Backup and restore:** For disaster recovery from total cluster loss, the leader periodically exports an encrypted state-machine snapshot to external object storage (`file://`, `s3://` or `gs://`), sealed with a key derived from the cluster master key. You configure the destination and cadence under `[cluster.backup]`; the export runs automatically on the leader, no command to remember. Credentials and region come from each backend's standard environment variables. Each backup is named after the position of the state inside it (recovery epoch, Raft term, log index), so restores and retention follow the council's history rather than a leader's clock. To recover after losing the whole council, `relish council recover` restores the newest backup that unseals on a stopped survivor, skipping and listing any it can't use, or one you name with `--backup` (see §8.3 above).

**Disk exhaustion:** Council nodes monitor available disk space and proactively step down from the council when storage falls below a configurable threshold (default: 1 GB), preventing Raft from stalling on failed log writes. A pressured leader first hands leadership to a healthy voter, holding its own elections so it can't win the election that follows. The node rejoins the council automatically once disk pressure is resolved.

> For protocol details, state machine diagrams, and split-brain prevention, see [design/gossip-mustard.md](design/gossip-mustard.md).

---

## 9. Networking

### 9.1 No Overlay Network

Reliaburger doesn't use an overlay network, a CNI plugin, or any form of virtual networking. Each container runs in its own Linux network namespace with dynamic port mapping to the host. Each container gets host ports from a configurable range (default 10000-32766, about 22,700 ports per node), kept below Linux's ephemeral range so a published port never collides with an outgoing connection's source port. Bun also skips any port a host daemon already listens on. There's no virtual network, no tunnel encapsulation, and no cluster-wide IP space to manage.

This eliminates a large proportion of Kubernetes debugging pain: encapsulation overhead, MTU issues, IP exhaustion, opaque packet paths, and an entire plugin ecosystem to manage.

### 9.2 Built-In Ingress (Wrapper)

Wrapper can run on every node and handles TLS termination, host/path-based routing, health-aware load balancing, connection draining during deploys, and WebSocket support. It is opt-in in `node.toml`. The v1 TLS sources are the cluster's Ingress CA and an operator-supplied certificate/key pair. Automatic ACME/Let's Encrypt provisioning is deferred.

```toml
[app.web.ingress]
host = "myapp.com"
tls = "cluster"     # clients must trust the cluster root CA
```

There's no IngressClass, no annotations, no separate cert-manager installation. External traffic reaches the cluster via a standard TCP load balancer (or DNS round-robin) pointing to any set of nodes. Service VIPs and published host ports are TCP-only; UDP services aren't supported. Rate limits are per node, per client IP: behind a balancer that spreads one client over N nodes, that client gets up to N times the configured rate. `relish apply` refuses an ingress whose TLS mode or rate limit no node could serve, and if one reaches the cluster anyway, each node skips that route and keeps publishing the rest of the catalogue. Wrapper on each node can route traffic for any app, so you don't need application-aware routing at the load balancer layer.

> For routing, TLS modes, and connection draining details, see [design/ingress-wrapper.md](design/ingress-wrapper.md).

---

## 10. Service Discovery (Onion)

Onion resolves service names to virtual IPs, then rewrites connections to those VIPs onto live backends. It works at two points:

1. **DNS resolution (userspace):** When an app resolves an `.internal` name, a small userspace responder that Bun runs answers from the service map with a virtual IP. We originally planned to intercept DNS in the kernel, but the cgroup `sendmsg4`/`recvmsg4` eBPF hooks can rewrite socket addresses, not read or synthesise DNS packet payloads, so an in-kernel DNS server isn't possible. The responder is authoritative for `.internal`, so those names never leak to a public resolver.
2. **Connect interception (eBPF):** When an app calls `connect()` with a virtual IP, an eBPF program rewrites the destination to a healthy backend's actual `host:port`. No proxy in the data path.

```
App calls getaddrinfo("redis.payments.internal")
  → userspace responder answers with virtual IP 127.128.0.42

App calls connect(127.128.0.42, 6379)
  → eBPF rewrites to 10.0.1.5:30891 (real backend)
  → direct TCP connection, native performance
```

From an application's perspective, connecting to another service is identical to any network service: `http://web.internal:8080`, `redis://redis.payments.internal:6379`. Names are namespace-qualified (`<app>.<namespace>.internal`); a bare `<app>.internal` resolves within the caller's own namespace. Two apps of the same name in different namespaces are distinct services with distinct VIPs. The connect-rewrite layer is completely invisible.

Virtual IPs are allocated per namespace-qualified service (not per instance) from 127.128.0.0/16, within the loopback range so packets are guaranteed to be intercepted locally and never leak onto the network (~65K unique services, expandable to /10 for ~4M). The connect hook runs in the kernel, so a Bun crash doesn't break existing connections. The service map becomes stale for new connections until Bun restarts. The systemd units we ship restart Bun two seconds after it exits and set `OOMScoreAdjust=-900`, so under memory pressure the kernel kills containers before Bun. On startup Bun re-populates the eBPF maps from the leader's service catalogue.

**External DNS:** Non-`.internal` names use the host's configured resolvers: Bun reads the `nameserver` lines of `/etc/resolv.conf` at start and tries them in order, skipping its own listen address. `[dns] upstream` overrides that, and with no usable resolver Bun refuses to start. Egress allowlists implicitly permit DNS (UDP and TCP port 53) to the node's responder, so an allowlisted app can still resolve the names on its list. Without `[dns]` there is no responder, and an allowlist must name its resolver itself.

> For BPF map layouts, eBPF program details, and the `firewall_map`, see [design/discovery-onion.md](design/discovery-onion.md).

---

## 11. Security

### 11.1 Certificate Authority Hierarchy (Sesame)

Sesame generates a root CA and three intermediate CAs at cluster initialisation:

```
Root CA (signs only intermediate CAs; key sealed to the cluster key at init)
├── Node CA         — signs node certificates for inter-node mTLS
├── Workload CA     — signs workload identity certificates (SPIFFE)
└── Ingress CA      — signs certificates for tls = "cluster" ingress routes
```

A compromise of any single intermediate CA doesn't affect the others. The root CA's private key never enters the council or reaches another node, but it isn't offline yet: `relish init` writes it as `<cluster>-root-ca.age`, sealed to the cluster's own age key, next to `master.key` on the node where init ran. Root on that node can therefore recover the root key. Make your own backup with `relish ca backup` (sealed to a passphrase or your age key), check it with `relish ca verify`, and then move the sealed file off the node. A truly offline root, never written to the node, is [#709](https://github.com/reliaburger/reliaburger/issues/709). Intermediate CAs are issued with a 5-year lifetime.

`relish ca rotate --role node|workload|ingress --root-backup FILE` replaces an intermediate: the cluster makes the new key, `relish` signs its request with your root backup on your machine, and both CAs stay trusted while nodes and workloads move over (see the [security manual](manual/10_security.md#rotating-an-intermediate-ca)). `relish ca status` and `relish ca revoke`, with a 90-day expiry warning, are still planned ([#362](https://github.com/reliaburger/reliaburger/issues/362)).

**Node authentication:** Nodes join via short-lived join tokens (default 15 minutes, single-use) that embed a SHA-256 fingerprint of the cluster's root CA certificate, allowing the joining node to verify the cluster's identity before transmitting credentials. On successful join, the node receives a certificate signed by the Node CA. All subsequent communication is mTLS. TPM-based hardware attestation is **planned — not yet implemented** (the binary links no TPM library).

### 11.2 Workload Identity

Every app instance automatically receives a SPIFFE-compatible X.509 certificate and an OIDC JWT, with no configuration required from you:

- **X.509 cert** (1-hour lifetime, rotated every 30 minutes) for mTLS to services that trust the Workload CA
- **OIDC JWT** for cloud provider identity federation (AWS, GCP, Azure)

Delegated job tasks (fresh `runc`, `shared-runc` executors and host processes) don't receive an identity yet.

Worker nodes use a CSR model: they generate a keypair locally, send a CSR to their council member, and receive a signed certificate only if the workload is legitimately scheduled on that node. During council outages, a 4-hour grace period extension prevents running workloads from losing their identity.

### 11.3 Network Security

Three layers of built-in firewall:

1. **Cluster perimeter** (nftables): external traffic enters only through Wrapper
2. **Namespace isolation** (eBPF, backed by nftables): cross-namespace traffic is blocked by default, at service VIPs and at real addresses alike: a backend's container address on any port and a published host port on any node. The socket hooks hold containers, delegated job tasks and host `exec`/`script` workloads (which start inside their own cgroup) to it; a forward-path nftables table catches traffic between local containers that never meets a socket hook, such as raw sockets or a host port reached through DNAT. Processes outside Reliaburger's cgroups (host daemons, Bun itself) stay outside the policy, and a host workload running as root can leave its cgroup, so isolation keeps honest host commands in bounds rather than containing hostile ones.
3. **Cross-namespace grants** (eBPF `allow_from`): an app or job names the apps in other namespaces that may connect to it. `allow_from` only opens cross-namespace paths; apps in one namespace always reach each other. Finer control inside a namespace would reuse the same real-address mechanism, and may come later.

Isolation needs the eBPF data path. `relish init` enables it on rootful Linux runc, where a load failure stops Bun. A node that can't enforce isolation (rootless runc, ProcessGrill, macOS) refuses `allow_from`, and `relish wtf` warns when a cluster with more than one namespace has such a node.

**Egress is unrestricted by default.** An app with no `egress` block can reach any external destination. Declaring an `egress` block flips that app to deny-by-default: only the listed destinations are permitted, and everything else is blocked. Use it to contain data exfiltration from compromised containers.

```toml
[app.api.egress]
allow = ["api.stripe.com:443", "hooks.slack.com:443"]
```

### 11.4 Data at Rest

Reliaburger encrypts Raft log entries at rest with AES-256-GCM, under a key derived (HKDF-SHA256, with a fresh salt per entry) from the cluster master key. That key is stored on the node, so the encryption protects a stolen disk or a copied log file, not a node whose root account is compromised. The Raft snapshot (`snapshot.redb`, holding token hashes, wrapped keys and the desired state) isn't encrypted, and neither are Raft's vote and log-position metadata. A cluster with no master key writes its log in plaintext. The CA and age private keys inside the Raft state carry their own HKDF-wrapped encryption under the same master key. Encrypting the snapshot too, with a key sealed to a TPM or derived from node identity, is 0.3.0 appliance work ([#402](https://github.com/reliaburger/reliaburger/issues/402)).

Certificate revocation uses a CRL kept in the council's replicated state; each node refreshes its copy as revocations commit.

### 11.5 Audit Logging

Every node keeps a durable, bounded audit log of the actions it served: applies, deletes, stops, execs, rollbacks, job, array and build submissions, upgrades, decommissions, elections, snapshots, test leases, fault injection, clearing and expiry, and every change to tokens, secrets and CAs. Each record carries the authenticated principal (never a client-supplied name), the app and namespace, the source address, a timestamp and the action's own facts, and never a secret. The node writes the record as a line of JSON under its data directory and syncs it before answering, so an acknowledged action survives a restart; the log rotates in segments and drops the oldest first, within a configurable bound (64 MiB by default). A write that fails is logged and counted but never fails the operator's action. `GET /v1/audit` and `relish audit` filter by time, action, principal and namespace, and `--cluster` merges every node's log. Only an unscoped Admin may read it, never the internal service principal. Refused requests to the same routes are recorded too (a missing or unknown token, a role, permission or scope refusal), with the principal or `anonymous`, the source address, the route and the reason, rate-limited per source and principal so a flood of bad requests is summarised rather than pushing real records out of the bounded log. The log counts toward the node's disk pressure alongside job data. The in-memory event ring still shows audit records live, but no longer holds the only copy.

**Status:** the log is per node. Central retention (shipping records off the node, so a lost or decommissioned node doesn't take its history with it) is planned, not implemented, as is tamper evidence (hash-chaining records).

> For the full PKI model, API authentication, secret encryption, and threat model, see [design/security-sesame.md](design/security-sesame.md).

---

## 12. Built-In Image Registry (Pickle)

Pickle is a distributed OCI-compatible registry built into every node. When you push an image (via standard `docker push`), Pickle commits it locally and returns immediately with an `oci-replication: pending` header. Replication to peers runs afterwards, driven by a leader-only heal loop (roughly every 60 seconds) that works towards a redundancy target (default 2 total copies: the pusher plus one peer). Once distributed, layers can be downloaded from different nodes in parallel, similar to BitTorrent. In clusters too small to meet the target, the heal loop warns that full redundancy can't be reached.

Key properties:

- **Asynchronous, eventually-replicated.** A push succeeds as soon as the local commit lands. Durability across nodes follows shortly after via the heal loop, so an image pushed and then immediately lost with its origin node before the heal tick runs is not guaranteed to survive. Push latency is decoupled from replication.
- **P2P distribution.** Image fan-out scales with cluster size, not against it.
- **Pull-through cache.** External registry images (Docker Hub, GHCR) are cached on first use.
- **Image signing.** Keyless signing via workload identity, in Reliaburger's own format, and operator-key signing with `relish sign`. For upstream images, the trust policy verifies key-based cosign signatures; keyless cosign is planned ([#619](https://github.com/reliaburger/reliaburger/issues/619)).
- **Scoped access.** Build jobs can only push to explicitly declared repositories.

> For GC, signing, replication failure handling, and build job integration, see [design/registry-pickle.md](design/registry-pickle.md).

---

## 13. Deployments & Rollouts

When an App's image changes, each node running it performs a rolling deployment of its own instances: start a new instance, wait for its health check, add it to Wrapper routing, drain the old instance, stop it. Old and new versions coexist on the same node without port conflicts.

```toml
[app.web.deploy]
strategy = "rolling"      # default; or "blue-green"
max_surge = 1             # extra instances during the rollout (default 1)
max_unavailable = 0       # instances allowed down (default 0)
auto_rollback = true      # default true
```

The leader releases a spec change one node at a time. It commits the new spec to Raft, hands it to the first node (in node-id order) that doesn't run it yet, and keeps every later node on the last spec that rolled out everywhere until that node reports its share running healthy on the new one. Each node rolls its own share with the app's `[deploy]` table, so `max_surge` and `max_unavailable` bound one node's share, not the whole app. The leader reads progress from the nodes' reports, not from state of its own, so a new leader carries on where the old one stopped. A first placement and a scale change aren't gated: neither costs running capacity.

`auto_rollback` (on by default) never ends below the capacity a node started from. A node whose new instance fails its health check stops the rollout; if old instances had already retired, it restarts them on the previous spec before it retires the healthy new ones, and if the previous spec can't come back either it keeps the new ones serving and reports the rollout halted. Either way it tells the leader, which marks the app `RolloutFailed` in Raft. The leader doesn't revert the desired spec (GitOps would only apply it again); it stops handing the failed spec to nodes, and `relish status` and `relish wtf` say where and why it failed. Applying the same spec again doesn't clear the mark. A different spec does, whether it comes from `relish apply`, `relish rollback` or a git commit. The council also keeps the last ten specs that rolled out everywhere for each app, so `relish rollback` still works after every node restarts.

There's no superseding: a second deploy that reaches a node while that node's rollout of the app is still running waits for it. Fully leader-coordinated rollouts, with one surge and unavailable budget for the whole cluster held in Raft, are planned.

**Dependency ordering:** Jobs can declare `run_before = ["app.api"]` to ensure migrations complete before app instances start. The target app must be in the same apply and namespace. Cluster apply records ownership before execution and publishes dependent app revisions only after positive zero exit and durable confirmation; uncertainty or leader replacement retains the fence. A hook gets one attempt and a 600-second deadline unless it sets `max_attempts` or `task_timeout_secs`. Recurring schedules work on clusters and standalone nodes alike: once the apps publish, the leader registers each `schedule` as a durable definition and claims each matching UTC minute in Raft. An unknown outcome is never replayed or overtaken automatically: cron skips that job's occurrences until an operator acknowledges the exact grant with `relish batch replay`, and `--rerun-jobs` doesn't clear it.

> For the deploy state machine, connection draining protocol, and autoscaling, see [design/deployments.md](design/deployments.md).

---

## 14. GitOps

### 14.1 Built-In Sync Engine (Lettuce)

Lettuce is Reliaburger's built-in GitOps engine, replacing ArgoCD and Flux with a sync loop compiled directly into the Bun binary.

CLI compilation and Lettuce share the resolver for inherited `_defaults.toml` values and directory-derived namespaces. Lettuce resolves the configured watch directory from the verified Git commit; defaults outside that directory are not inherited. Malformed input or an unrepresentable namespace collision refuses the whole sync before desired-state changes. GitOps refuses duplicate resource definitions, while CLI compilation reports deterministic overrides within a namespace.

Lettuce reconciles apps, namespaces and permissions. Any job declaration, including a scheduled job or a `run_before` migration, refuses the entire validated commit before desired-state changes. The failed sync leaves the previous applied SHA intact. Execute migration jobs and their dependent apps together through `relish apply`, or submit batch work through `relish batch`. GitOps job execution awaits a durable revision-to-run identity and dispatch path.

**Configuration:**

```toml
[gitops]
repo = "git@github.com:myorg/infra.git"
branch = "main"
path = "production/"
poll_interval_secs = 30
require_signed_commits = true              # optional: reject unsigned commits
trusted_signing_keys = [                   # GPG or SSH key fingerprints
  "SHA256:abc123...",
  "SHA256:def456...",
]
```

Or webhook-triggered for instant deploys on push. Lettuce validates webhook payloads using HMAC-SHA256 signatures (configured via `[gitops] webhook_secret`). Only POST requests with a valid signature are processed. The webhook endpoint (`POST /v1/gitops/webhook`) is served on the node's API port, which is HTTPS unless the node runs a development plaintext config. Rate limiting (default: 10 triggers per minute) prevents abuse. When `require_signed_commits` is enabled, Lettuce still verifies the commit referenced by the webhook before applying it.

When `require_signed_commits` is enabled, Lettuce only applies commits that are signed by a key in the `trusted_signing_keys` list. Unsigned or untrusted commits are refused: the sync fails, the previous commit stays applied, and the sync status shows why. No alert fires yet. This is particularly important for configurations that include inline `script` fields (Section 17), since anyone with git write access could otherwise inject arbitrary commands into the cluster.

**Sync behaviour:**

1. Lettuce watches the configured git repository.
2. On each poll (or webhook trigger), it pulls the latest commit.
3. It compares the desired state in the repository against the actual state of the cluster.
4. It computes a diff and applies only the changes: namespaces first, then permissions, then apps, and removals in the reverse order. A change the cluster refuses doesn't block the others; the sync records a partial result naming each refused change, and the applied commit moves only once everything has applied.
5. The Brioche UI and `relish gitops status` show the sync status, the last applied commit and a history of syncs. There's no preview of pending changes yet.

**Ownership:** Lettuce prunes only what it wrote. Raft holds the set of apps, namespaces and permissions a sync created, changed or adopted, updated in the same entry as the write itself, and the diff removes only resources in that set. GitOps and `relish apply` therefore coexist: turning GitOps on in a working cluster, or committing an empty tree, leaves manually applied resources alone. A manual `relish apply` of a GitOps-managed resource warns that the next sync reverts it, and `relish delete` or `relish rollback` of one is refused without `--force`. A running test's `rbtest-*` namespace never enters the diff, and only the test lease's own cleanup can delete what's in it.

**Plaintext secrets:** an `env` value whose key looks secret (`PASSWORD`, `SECRET`, `TOKEN`, `CREDENTIAL`, or ending in `_KEY`) but isn't `ENC[AGE:...]` is applied with a warning in the log, the sync status and `relish wtf`. Lettuce warns rather than refuses, so existing repositories keep working.

**Autoscaler interaction:** Lettuce treats the `replicas` field in git as the *base* replica count. Autoscaler adjustments are runtime overrides stored in Raft, not in git. Lettuce compares the `replicas` field independently from other fields: only a change to the `replicas` value itself in git resets the runtime override. This prevents Lettuce from fighting the autoscaler during traffic spikes. After a leader election, the new leader carries forward autoscaler overrides from Raft and re-evaluates scaling decisions once the learning period completes and fresh Mayo metrics are available.

**Coordinator:** The Raft leader runs the sync loop, because only the leader can write desired state; when leadership moves, the new leader's loop takes over. The GitOps configuration lives in Raft, so the new leader follows the same repository, branch and path from the committed sync state. A node's `[gitops]` in `node.toml` only seeds Raft when the cluster has no configuration; a node whose section differs reports it in `relish wtf` and is ignored. `relish gitops set` (admin only, audited) changes it. The repository URL's credentials and the webhook secret are sealed with a key derived from the cluster master key before they reach Raft. Each sync records the Raft term it read desired state in, and the state machine refuses its writes in any other term, so a leader that lost and regained leadership mid-sync can't apply a stale diff.

> For sync loop internals and coordinator election, see [design/gitops-lettuce.md](design/gitops-lettuce.md).

---

## 15. Observability

Reliaburger includes a complete observability stack with zero configuration:

**Metrics (Mayo):** Per-node store (Parquet, queried with SQL) that collects node CPU, memory, disk and network, plus per-app CPU and memory. Jobs get their own series: attempt, success, failure and retry counters, duration histograms, and CPU and memory for running tasks read from their cgroups, labelled only by namespace, definition, profile and node. Apps that declare `metrics = {}` have their Prometheus `/metrics` endpoint scraped by the node running each instance. Data is kept at one resolution for `retention_days` (7 by default) and then pruned; tiered downsampling is planned. Council members aggregate one-minute rollups for cluster-wide dashboards. Alerts are fixed threshold rules; PromQL-style custom alerts are planned (see [design/metrics-mayo.md](design/metrics-mayo.md)). Five default alerts ship active out of the box, all on node-level metrics: node CPU above 90 %, node memory above 85 % and above 70 %, node disk above 80 %, and node CPU below 5 % for half an hour.

**Logs (Ketchup):** Captures stdout/stderr from every container. Structured storage with timestamp-based indexing and zstd compression. `relish logs --json-field key=value` filters JSON lines on the client, after the server has returned the lines you asked for; server-side field queries are planned. Each node exports its log files to a directory, `s3://` or `gs://` on a schedule.

**Dashboards (Brioche):** Built-in web UI compiled into the Bun binary, served on every node. Cluster overview, app detail (metrics, logs, deploy history), node detail, ingress overview, and GitOps status.

> For TSDB internals, scraping, aggregation, and alert configuration, see [design/metrics-mayo.md](design/metrics-mayo.md), [design/logs-ketchup.md](design/logs-ketchup.md), and [design/ui-brioche.md](design/ui-brioche.md).

---

## 16. Debugging & Tooling (Relish)

Relish is the CLI and interactive terminal UI for Reliaburger. Running `relish` with no arguments launches a full-screen TUI (similar to k9s) showing apps, nodes, jobs, events, logs, routes, and search views.

| Command | Purpose | K8s equivalent |
|---------|---------|----------------|
| `relish` (no args) | Interactive terminal UI | k9s |
| `relish status` | One-line cluster summary | `kubectl cluster-info` + `kubectl get nodes` |
| `relish compile <path>` | Resolve config to final form | (none — Helm template + kustomize build) |
| `relish lint <path>` | Validate config files | `kubectl --dry-run=client` (barely) |
| `relish fmt <path>` | Format and sort TOML files | (none) |
| `relish plan <path>` *(planned)* | Preview changes before apply | `terraform plan` (K8s has nothing equivalent) |
| `relish diff <a> <b>` | Structural diff between two configs (not live cluster drift) | `kubectl diff` (limited) |
| `relish apply <path>` | Apply configuration | `kubectl apply` |
| `relish run --batch <name>` | Run a task array from an image or allowlisted host binary | `kubectl create job` (one at a time) |
| `relish batch` | Submit, watch, cancel and read results of task arrays and mixed manifests | (none) |
| `relish jobs` | Job run summaries and rates, or schedule definitions | `kubectl get jobs,cronjobs` |
| `relish deploy <path>` | Trigger a rolling deploy from a config file | `kubectl set image` |
| `relish events` *(planned)* | Streaming event log | `kubectl get events` (1h expiry) |
| `relish logs <app>` | Stream/search logs | `kubectl logs` + `stern` |
| `relish path <app> --to <app>` | Connectivity diagnosis | (none — manual iptables/DNS debugging) |
| `relish inspect <resource>` | Deep resource inspection | `kubectl describe` |
| `relish top` | Workloads with their latest CPU and memory, running array tasks included | `kubectl top` (requires metrics-server) |
| `relish metrics <app>` | An app's own Prometheus metrics, scraped by its nodes | (none — requires Prometheus) |
| `relish dashboard` | Read-only web dashboard through your authenticated CLI | `kubectl proxy` + a dashboard install |
| `relish wtf` | Automated health check | (none — requires runbooks + Prometheus alerts) |
| `relish exec` | Run a command inside a running container | `kubectl exec` |
| `relish exec --debug` *(planned)* | Debug container (separate identity) | `kubectl debug` |
| `relish resolve <name>` | Query eBPF service map | `kubectl get endpoints` + `nslookup` |
| `relish firewall <app>` *(planned)* | Show effective firewall rules | `kubectl get networkpolicy` + CNI tools |
| `relish history <app>` | Deploy history for an app | `kubectl rollout history` |
| `relish rollback <app>` | One-command rollback | `kubectl rollout undo` |
| `relish scale <app> <n>` *(planned)* | Set replica count | `kubectl scale` |
| `relish secret encrypt` | Encrypt a secret value | (none — requires Sealed Secrets) |
| `relish fault <type> <app>` | Inject a fault | (none — requires Chaos Mesh / Litmus) |
| `relish test` | Run integration test suite | (none) |
| `relish bench` | Run performance benchmarks | (none) |
| `relish upgrade start` | Rolling cluster upgrade | `kubeadm upgrade` (multi-step) |
| `relish ca backup` / `ca verify` | Keep and check your own sealed copy of the root CA | (none — manual process) |
| `relish ca rotate` | Rotate an intermediate CA, signed with your root backup | (none — manual process) |
| `relish ca status` *(planned)* | Show CA hierarchy and expiry | (none) |
| `relish ca revoke` *(planned)* | Revoke a compromised certificate | (none — manual process) |
| `relish token` | Create, list and revoke API tokens | `kubectl create token` + RBAC |
| `relish sign` | Sign a Pickle-hosted image with your own key | `cosign sign` |
| `relish setup` | Guided setup, or a laptop cluster with `--quickstart` | `kind create cluster` / `minikube start` |
| `relish local <status\|start\|stop\|destroy>` | Manage the laptop cluster setup made | `kind` / `minikube` commands |
| `relish init` | Bootstrap a new cluster | `kubeadm init` |
| `relish join` | Join a node to the cluster | `kubeadm join` |
| `relish drain <node>` *(planned)* | Safely evacuate a node | `kubectl drain` |
| `relish council recover` | Restore a lost council from a sealed backup | `etcdctl snapshot restore` |
| `relish secret rotate` | Rotate encryption keys | (none — requires Sealed Secrets re-encrypt) |
| `relish snapshot create\|list\|restore\|delete` | Manage an app's local volume snapshots | (none — requires CSI snapshotter) |
| `relish import -f <k8s-yaml>` | Convert K8s manifests to Reliaburger TOML | (none) |
| `relish export -f <file>` | Generate K8s manifests from a Reliaburger config | (none) |

> For TUI mockups, command details, and debug container behaviour, see [design/cli-relish.md](design/cli-relish.md).

---

## 17. Process Workloads (Non-Container)

Reliaburger provides a middle ground between full container workloads and unmanaged system processes: **run a binary from the host's filesystem under the same scheduling, health checks, metrics, and log capture as a container app.** Process Apps use `exec` instead of `image`; process Jobs use `exec` or `script` for inline shell scripts.

What the current binary actually enforces is a policy layer, not a sandbox:

- **Deny-by-default binary allowlist** (`[process_workloads] allowed_binaries` in `node.toml`). An empty or absent allowlist refuses every `exec`/`script` workload, so host execution is off on any node the operator hasn't explicitly opened up. Inline `script` workloads run through `/bin/sh`, which must itself be allowlisted.
- **Signed-commit gating.** Inline scripts applied via Lettuce require signed commits (Section 14). When applied directly via `relish apply`, TOML files containing `script` fields require `host-exec` permission.

On rootful Linux, process batches use a bounded native executor pool. Each command starts as a fresh host process inside its resource-profile cgroup, with CPU, memory, swap and PID limits applied before execution. A durable owner holds the executor subtree until retirement. These jobs run as Bun's user and retain access to the host; they are trusted workloads, not a security sandbox. Every host backend passes only a short allowlist of Bun's environment (`PATH`, `HOME`, locale and a few more) plus the job's own variables.

Process apps get the supervisor's per-process CPU and memory samples. Process
batch tasks don't run under the supervisor, so each node measures their
executor cgroups (or, without cgroups, their processes) per job definition and
profile instead: Mayo stores those as job series and `relish top` shows one
row per definition with its running task count.

Process applications, ordinary process jobs and every host workload on platforms without the native backend still use the process owner directly. Explicit job resource limits are refused when that backend cannot enforce them. **Status: further kernel isolation is planned.** There is no dedicated `burger` user, seccomp profile or network/PID/mount namespace for host workloads. Mount-namespace isolation is explicitly refused when `mount_isolation` is requested. Linux turns `[process_workloads] mount_isolation` on by default, so until host jobs can run under it ([#714](https://github.com/reliaburger/reliaburger/issues/714)), every node that should run host jobs needs `mount_isolation = false` in its config. A standalone node refuses a host job without it; in a cluster the leader fails the run once no node can take it (see the [job manual](manual/14_batch-jobs.md)).

> For the full isolation model, security controls, and when-to-use-what guidance, see [design/agent-bun.md](design/agent-bun.md).

---

## 18. Fault Injection (Smoker)

Smoker is Reliaburger's built-in chaos engineering system. Because Onion's eBPF programs already intercept every `connect()` call, Bun's DNS responder answers every `.internal` name, and Bun already manages cgroups for every container, fault injection is a natural extension of existing infrastructure. No new binaries, processes, or sidecars.

```bash
relish fault delay redis 200ms --acknowledge       # tc netem on each caller's interface
relish fault drop api 10% --acknowledge            # fail 10% of connections
relish fault dns redis nxdomain --acknowledge      # DNS failure
relish fault cpu inference 50% --acknowledge       # CPU stress in cgroup
relish fault kill web --count 1 --acknowledge      # kill one replica
relish fault scenario chaos/scenario.toml --acknowledge # scripted multi-step scenario
```

`relish fault delay` has shipped; `relish fault bandwidth` parses but Bun
refuses it until it's implemented
([#366](https://github.com/reliaburger/reliaburger/issues/366)).
Safety rails require an authenticated role, the matching server-owned
`[testing].allowed_operations` grant and explicit acknowledgement. They also
enforce duration limits (default 10 min) and blast-radius protection
(quorum, replica and leader guards). A fault never outlives the Bun that
injected it: after a crash, kill or upgrade, the new Bun empties the eBPF
fault maps (pinned ones included), rewrites each adopted instance's CPU,
memory and disk-I/O limits from its recorded specification, resumes paused
processes and removes netem delays, so `relish fault list` and the kernel
agree there's nothing active. A scripted scenario validates every step before
it injects any and exits non-zero if a step is refused. Admin doesn't
override a disabled operation, and Bun attributes audit events to the
authenticated credential rather than a client-supplied name.

In 0.2.0 workload faults target apps only: the replica guard refuses a kill or
pause aimed at a job, and array or shared-executor attempts aren't Smoker
targets. Jobs are tested under node-level faults instead (Bun killed on a
worker or the leader, delayed traffic between nodes, a full disk, slow
fsync), which is how the release's job qualification breaks them. Job-aware
chaos is planned for 0.2.2.

| | Chaos Mesh (K8s) | Litmus (K8s) | Gremlin (SaaS) | **Smoker** |
|---|---|---|---|---|
| **Installation** | CRDs + operator + RBAC | CRDs + operator + agent | SaaS agent | **Built-in** |
| **Network faults** | tc + iptables | tc + iptables | Agent-based | **eBPF connect hook (drop, partition), DNS responder (nxdomain), tc netem (delay)** |
| **Overhead when idle** | iptables rules present | iptables rules | Agent running | **One empty BPF map lookup per connect** |
| **Blast radius protection** | Manual | Manual | Basic | **Automatic** |

> For eBPF implementation, resource faults, and scripted scenarios, see [design/chaos-smoker.md](design/chaos-smoker.md).

---

## 19. Testing & Benchmarks

Reliaburger compiles its test suite into the binary. `relish test` runs 39
integration tests across 13 subsystems (scheduling, service discovery,
deployments, health checks, secrets, firewall, workload identity, ingress,
volumes, process workloads, jobs, image registry, cluster coordination).
Every case uses a server-owned workload lease and reports `Pass`, `Fail`,
`Skipped` or `Unknown`; timeouts and uncertain cleanup never become green.

`relish test --chaos` runs five serial recovery scenarios covering leader
failure, worker failure with live replicas, minority council partition,
bounded whole-node pressure, and node death during a rolling deploy. This
suite is deliberately destructive. It refuses without three nodes, fresh
primitive evidence, the required server policy, a digest-pinned runc or Apple
Container workload, and exact operator consent (`--yes` for automation).
Consent can't override server policy. Each case owns exact fault ids and
reverses them after failure, timeout or panic; unconfirmed reversal is
`Unknown`.

`relish bench` measures the data plane of a live cluster and, with `--compare`, checks the report against a previous one to catch regressions. It compares against that stored baseline, not against the design goals in Section 2, and `--disruptive` adds a leader-failure reconstruction run.
Four job scenarios (`--scenario jobs-containers`, `jobs-shared-containers`,
`jobs-host-processes` and the raw `jobs-vm-baseline`) count unique accepted
successes in a fixed window through the public job API. Each writes a strict
JSON report, and `--compare` with an earlier report of the same scenario fails
on a drop in the window rate beyond `--tolerance` (10% by default); different
settings make the comparison informational. A window rate is a regression
guard, never a daily qualification: the 100-million-a-day claim needs the
12-hour job qualification on at least three nodes, at that daily rate.

> For test output examples, benchmark methodology, and CI integration, see the design docs for each component.

---

## 20. Self-Upgrades

Since every component of a node lives in the one `bun` binary, upgrading a node means replacing that binary and restarting the Bun process. The system automates this in a rolling fashion: workers upgrade first (with configurable parallelism), then council members one at a time to maintain quorum, then the leader upgrades last (in place — a council of three or more keeps quorum through the sub-second exec bounce; openraft 0.9 has no graceful leadership transfer). Application workloads are never interrupted, because the container runtime manages containers independently of Bun. During the rolling upgrade window, old and new binaries coexist, but only when both speak the same formats. Every binary carries a cluster protocol generation and a durable state generation (`src/compatibility.rs`), and nodes refuse peers, snapshots, backups and data directories stamped with a different pair. Before 1.0.0 we don't keep backwards compatibility at all: a release that changes a wire or storage format bumps a generation, and upgrading across that bump means starting a fresh cluster, with no migration (see [compatibility before 1.0.0](releasing.md#compatibility-before-100)). So until 1.0, self-upgrade works only between builds of the same format. Every release from 0.1.0 to 0.2.0 bumped one, so no rolling upgrade between two published releases has run yet; shipping a patch release that keeps its predecessor's formats, and qualifying a real published-to-published rolling walk on it, is a goal. A compatibility promise across versions is a post-1.0 goal.

Reliaburger verifies binary integrity via dual signatures: an embedded signing key set compiled into the binary AND an external signing key configured in `node.toml`. Network upgrades require both signatures. Published release metadata carries only the release signature, so `relish upgrade start <version>` countersigns each download with the operator's key (`--external-key`) or takes a countersigned envelope (`--sig`). It picks the build for the platform each node reports rather than the one relish runs on, and a cluster that mixes architectures gets one build per platform. Before staging, each node runs the verified candidate once with `--compatibility` and refuses a binary whose formats differ or that doesn't answer within 60 seconds, a hang guard sized for macOS, which assesses every new executable before its first run. Automatic rollback triggers if a node fails to start on the new version.

> For the upgrade sequence, binary versioning, rollback procedure, and security considerations, see [design/agent-bun.md](design/agent-bun.md).

---

## 21. Multi-Cluster (Franchise)

> **Status: planned — not yet implemented in the current binary.** Nothing in this section runs today. Franchise is a forward-looking design: there is no WAN gossip ring, no `relish franchise` command, no cross-cluster service discovery, and no franchise dashboard. In fact the binary actively refuses the one hook that exists in the config schema — an app that declares `egress.allow_franchise` is refused when a node deploys it, rather than run with the rule silently unenforced (moving that refusal to config validation is [#701](https://github.com/reliaburger/reliaburger/issues/701)). The rest of this section describes the intended design, kept here so the vision is on record.

Reliaburger clusters are independent by design. Each has its own Raft consensus, its own CA hierarchy, and its own lifecycle. Franchise extends this model to give you visibility and connectivity across multiple clusters without coupling them.

Each cluster is a **franchise location**: independently operated, sharing standards and visibility. Peering is a single command:

```bash
relish franchise join https://prod-west.example.com --token <peer-token>
```

This exchanges OIDC trust bundles (Sesame already supports per-cluster SPIFFE trust domains) and joins a WAN gossip ring that is separate from the intra-cluster LAN ring. Both clusters see each other immediately.

### 21.1 WAN Gossip

Leaders exchange lightweight cluster-level metadata over WAN gossip (a Mustard extension): cluster name, leader endpoint, node count, service catalog (app names + health status), and capacity summary. This isn't node-level data or Raft state, just cluster-level summaries, converging in seconds with O(1) per-cluster overhead.

### 21.2 Cross-Cluster Service Discovery

Onion's eBPF layer resolves a new DNS zone: `name.cluster.franchise` (e.g., `redis.prod-west.franchise`). Cross-cluster traffic routes through the peer cluster's Wrapper ingress, which already terminates mTLS. There are no VPNs, no tunnels, and no direct pod-to-pod networking across clusters. Wrapper is the bridge.

Apps opt in explicitly:

```toml
[app.web.egress]
allow_franchise = ["redis.prod-west"]
```

### 21.3 Unified Dashboard and Metrics

Brioche provides a franchise overview page: all peered clusters, their health status, app counts, and capacity. Click through to any peer's detail view (Brioche proxies the request). On the CLI, `relish franchise status` shows a summary across all clusters and `relish top --franchise` shows fleet-wide resource usage. Each cluster's Mayo would expose a Prometheus remote-read API (itself planned, [#364](https://github.com/reliaburger/reliaburger/issues/364)), so Brioche could query metrics across the franchise without cross-cluster scraping.

### 21.4 Cross-Cluster Image Pull and GitOps

Peered clusters can pull images from one another's Pickle registries on demand (lazy, not eagerly replicated) using the existing OCI Distribution API over the trust relationship. For GitOps, Lettuce supports shared repositories with per-cluster watch directories. A `_defaults.toml` inside the watched tree provides shared values, and child directories can override them. Each cluster independently resolves its watched tree; defaults outside its watch root are not inherited.

### 21.5 Trust Model

Each cluster keeps its own CA hierarchy; there's no shared root CA. Peering exchanges OIDC trust bundles (see Section 11.2). Cross-cluster traffic uses Wrapper's mTLS with the peer's Ingress CA. A peered cluster going down doesn't affect your local cluster: WAN gossip marks it as unreachable, and local operations continue uninterrupted.

### 21.6 What Franchise Does Not Do

Franchise deliberately excludes cross-cluster scheduling (each cluster schedules independently), shared Raft (cluster state is sovereign), cross-cluster pod networking (no tunnels or overlay), and automatic failover (redeployment is manual or GitOps-driven). These boundaries keep clusters truly independent. Franchise adds visibility and connectivity without tight coupling.

> For WAN gossip protocol details, trust bundle exchange, and cross-cluster routing, see [design/gossip-mustard.md](design/gossip-mustard.md), [design/security-sesame.md](design/security-sesame.md), and [design/ingress-wrapper.md](design/ingress-wrapper.md).

---

## 22. What's Deliberately Not Included

The following features are intentionally excluded from Reliaburger v1. Each is a deliberate design decision, not an oversight.

| Feature | Why It's Excluded |
|---------|-------------------|
| Overlay network / CNI plugins | Per-container network namespaces with port mapping cover the common connectivity needs at a fraction of the complexity. No overlay, no CNI, no virtual network. |
| Distributed / network-attached volumes | Local volumes only. Distributed storage is a complex domain with its own failure modes. Use managed databases for data that must survive node loss, or applications that replicate internally (CockroachDB, Cassandra). |
| Custom Resource Definitions (CRDs) | CRDs are Kubernetes's extensibility mechanism and the source of enormous ecosystem complexity. Reliaburger's seven workload resource types cover the 80% use case. |
| Operators | Operators exist to manage complex stateful workloads on Kubernetes. Without CRDs, there is no operator framework, by design. |
| Service mesh (full sidecar proxy) | Namespace isolation, per-app eBPF firewall rules, and SPIFFE-based workload identity provide network segmentation and cryptographic authentication without the overhead of a sidecar proxy on every connection. Applications that need mTLS can configure it directly using the workload identity certificates — Reliaburger provides the certs, the app owns its TLS policy. |
| Helm / package manager | When the configuration is already 15 lines of TOML, a package manager for configuration templates is unnecessary. |
| External secret manager integration | The encrypted-in-git model covers the majority of use cases. Vault/AWS Secrets Manager integration is planned for v2. |
| Windows node support | Linux only in v1. |
| IPv6 | The current networking model (virtual IPs, eBPF hooks) is IPv4-only. IPv6 support is planned for v2. |
| Pod affinity / anti-affinity | Replaced by simple placement hints via node labels. |
| Pod Disruption Budgets | Reliaburger's intended drain logic respects the same constraints as rolling deploys: it never drains a node if doing so would reduce any app below `replicas - max_unavailable` healthy instances, checking all affected apps before proceeding. (The `relish drain` command itself is planned — see Section 16.) |
| Sidecars | Some use cases genuinely require co-located processes sharing a network namespace (authentication proxies, log forwarders, protocol adapters). In v1, init containers cover per-instance startup tasks and separate Apps with service discovery cover most runtime co-location needs, though at the cost of a network hop. A `sidecar` field on the App spec (co-located containers sharing the parent's network namespace and lifecycle) is planned for v2. |
| Distributed tracing backend | Applications export traces to external collectors (Jaeger, Tempo, Datadog) using standard OpenTelemetry SDKs. Workload identity (Section 11.2) provides authentication to external tracing services. No single tracing backend fits all teams, so including one would violate the "batteries-included means the default works" principle. |

### Migration Path

Reliaburger provides bidirectional Kubernetes migration tooling. Neither direction is a perfect translation (the systems have different abstractions), but the goal is to preserve as much as possible and clearly report what was lost.

**Importing from Kubernetes** (`relish import`) converts Kubernetes YAML into Reliaburger TOML. The importer correlates related resources automatically: a Deployment + Service + Ingress + HPA that Kubernetes treats as four separate objects becomes a single `[app.*]` block in TOML. Today it groups them by name: a Service with the Deployment's name, an Ingress whose backend names that Service, and an HPA whose scale target names the Deployment. Matching on label selectors, the way Kubernetes itself does, is planned ([#703](https://github.com/reliaburger/reliaburger/issues/703)). Every import produces a migration report with three sections: what was converted directly, what was approximated (review recommended), and what was dropped (no Reliaburger equivalent) with guidance on alternatives.

```bash
# From files (repeat -f for multiple manifests)
relish import -f deployment.yaml -f service.yaml

# Fail on any conversion warning (CI gate)
relish import -f deployment.yaml --strict
```

**Exporting to Kubernetes** (`relish export`) generates standard Kubernetes manifests from Reliaburger configuration. Each `[app.*]` produces a Deployment + Service, plus Ingress and HPA if the app defines ingress and autoscale blocks. Each `[job.*]` produces a Job or CronJob. Features with no Kubernetes equivalent (auto-rollback, Smoker fault rules, process workloads) are noted in the export report.

```bash
relish export -f myapp.toml > k8s-manifests.yaml
```

This is an explicit design goal: Reliaburger should never be a dead end, regardless of which direction you're moving.

---

## 23. Comparison Matrix

The Reliaburger column describes 0.2.0. Rows marked *Target* are design goals from §2 that no release has measured yet, and *Planned* rows aren't in the binary; the [scope and limits](README.md#scope-and-limits) and the [release order](roadmap.md#releases-after-010) say what ships and when the rest is due.

| | Kubernetes | k3s / k0s | Nomad | Docker Compose | **Reliaburger** |
|---|---|---|---|---|---|
| **Conceptual complexity** | Very high (50+ resource types) | Very high (same API) | Medium (~5 job types) | Low | **Low (7 workload types, plus builds)** |
| **Node types** | Control plane + workers | Server + agent | Server + client | Single host | **Homogeneous** |
| **Binary count** | Many (apiserver, scheduler, controller-manager, etcd, kubelet, kube-proxy) | 1 | 1 | 1 | **2 (`bun` agent, `relish` CLI)** |
| **Networking** | Overlay (CNI required) | Overlay (CNI required) | Host or overlay | Host (bridge) | **Per-container namespaces + port mapping (no overlay)** |
| **Ingress** | Separate install | Bundled (Traefik) | Separate | N/A | **Built-in (Wrapper)** |
| **TLS certificates** | Separate (cert-manager) | Separate | Separate | N/A | **Built-in Ingress CA or operator-supplied; ACME deferred** |
| **Metrics** | Separate (Prometheus) | Separate | Separate | N/A | **Built-in (Mayo + app scraping)** |
| **Alerting** | Separate (Alertmanager) | Separate | Separate | N/A | **Built-in (5 default alerts)** |
| **Logs** | Separate (Loki/EFK) | Separate | Built-in (alloc logs) | `docker logs` | **Built-in (Ketchup)** |
| **Dashboards** | Separate (Grafana) | Separate | Built-in (basic) | N/A | **Built-in (Brioche)** |
| **GitOps** | Separate (ArgoCD/Flux) | Separate | Separate | N/A | **Built-in (Lettuce)** |
| **Image registry** | Separate (Harbor etc.) | Separate | Separate | Docker Hub | **Built-in (Pickle)** |
| **Service discovery** | CoreDNS + kube-proxy | Same as K8s | Built-in (since 1.3) or Consul | Docker DNS | **Built-in (Onion eBPF, near-zero overhead)** |
| **Network security** | NetworkPolicy (CNI-dependent) | Same | Consul Connect | None | **Built-in eBPF + nftables (namespace isolation on VIPs and real addresses + cross-namespace `allow_from` + opt-in egress allowlists, for apps and jobs)** |
| **Image builds** | Separate (Tekton, Jenkins, external CI) | Same | Separate | `docker build` | **Built-in (build jobs → Pickle)** |
| **Terminal UI** | None (k9s is third-party) | Same | None | None | **Built-in (relish TUI)** |
| **Change planning** | `kubectl diff` (limited) | Same | Built-in (`nomad job plan`) | None | **Built-in (`relish apply --dry-run`)** |
| **Connectivity debugging** | Manual (iptables, DNS, endpoints) | Same | Manual | N/A | **Built-in (relish path)** |
| **Health diagnosis** | Manual (requires runbooks) | Same | Manual | N/A | **Built-in (relish wtf)** |
| **Fault injection** | Separate (Chaos Mesh / Litmus) | Same | Separate (Gremlin) | N/A | **Built-in (Smoker: eBPF, DNS responder, tc netem, cgroups)** |
| **Built-in test suite** | None | None | None | None | **Built-in (relish test, relish bench)** |
| **Config format** | YAML (verbose) | YAML (same) | HCL | YAML | **TOML (concise)** |
| **Max cluster size** | ~5,000 nodes | ~5,000 nodes | ~10,000 nodes | 1 host | **Target: 10,000 nodes (unmeasured)** |
| **Volume model** | Local + CSI (network) | Same as K8s | Local + CSI | Docker volumes | **Local only (by design)** |
| **Multi-node** | Yes | Yes | Yes | No | **Yes** |
| **Rolling deploys** | Yes | Yes | Yes | No | **Yes, per node (node-local auto-rollback; cluster-coordinated rollouts planned, #692)** |
| **Autoscaling** | HPA (separate config) | Same as K8s | External | No | **Built-in** |
| **Time to first deploy** | Hours to days | Minutes to 30 min | 30 min to hours | Minutes | **Target: < 5 min (unmeasured)** |
| **Written in** | Go | Go | Go | Go | **Rust** |
| **GPU scheduling** | Via device plugin (separate install) | Via device plugin | Yes (device plugins) | No | **Planned (node detects NVIDIA cards; allocation and placement are #359, §2)** |
| **Secret management** | Built-in (basic) or External (Vault, Sealed Secrets) | Same as K8s | Vault integration | Docker secrets | **Encrypted-in-git (built-in)** |
| **Workload identity** | Separate (SPIRE, cert-manager) | Same | Consul Connect | None | **Built-in (SPIFFE-compatible, auto-rotated)** |
| **Non-container jobs** | No | No | Yes (exec driver) | No | **Yes (allowlisted host exec; sandboxing planned)** |
| **Non-container apps** | No | No | Yes (exec/raw_exec drivers) | No | **Yes (allowlisted host exec; sandboxing planned)** |
| **Scheduling constraints** | nodeSelector, affinity/anti-affinity, taints/tolerations | Same | constraints, affinities | N/A | **Node labels with required/preferred (AND logic)** |
| **Daemon mode** | DaemonSet (separate resource type) | Same | system scheduler | N/A | **replicas = "*" (same App resource)** |
| **Job throughput** | Low (per-job API calls) | Low (same) | Medium | N/A | **Target: 100M+ per day (unqualified; task arrays implemented for 0.2.0)** |
| **Leader recovery** | Automatic within quorum; backup for quorum loss | Same | Automatic within quorum; backup for quorum loss | N/A | **Automatic within quorum; operator restore from a sealed backup for quorum loss (§8.3)** |
| **Multi-cluster** | Separate (Karmada / Cilium ClusterMesh / many CRDs) | Same | WAN gossip + API forwarding (Consul for service discovery) | N/A | **Planned (Franchise — WAN gossip + Wrapper ingress; not yet implemented, see §21)** |
| **K8s migration** | N/A | N/A | N/A | N/A | **Built-in (relish import/export with migration reports)** |
| **License** | Apache 2.0 | Apache 2.0 | BSL 1.1 (MPL 2.0 before August 2023) | Apache 2.0 | **Apache 2.0** |

> **Note on Docker Swarm:** Docker Swarm mode (via `docker stack deploy`) adds multi-node orchestration, rolling deploys, overlay networking, service discovery, and secret management to the Docker engine. It occupies a similar "simple orchestrator" space. However, Swarm has been in maintenance mode since 2019, receives only security patches, and lacks built-in metrics, dashboards, GitOps, a registry, or fault injection. The Docker Compose column above reflects standalone Compose without Swarm mode.

---

## 24. Licensing

Reliaburger is licensed under **Apache 2.0**, the same license as Kubernetes, k3s, Docker Engine, and Podman.

**Why Apache 2.0:**

- **Maximum adoption.** No enterprise legal review friction, no "is this really open source?" debates. Every organisation (startup, enterprise, government, cloud provider) can use, modify, and distribute Reliaburger without restriction.
- **Explicit patent grant.** Unlike MIT, Apache 2.0 includes a patent license from contributors. For infrastructure software touching networking, scheduling, and distributed consensus, this protection matters.
- **Foundation-ready.** Apache 2.0 is the standard license for CNCF projects. If Reliaburger grows to the point where foundation governance makes sense, no relicensing is needed.

**Why this matters now:**

The infrastructure tooling landscape experienced a wave of relicensing between 2018 and 2024. MongoDB switched from AGPL to SSPL (2018). Elasticsearch moved from Apache 2.0 to SSPL + Elastic License (2021). HashiCorp switched all products, including Nomad (Reliaburger's closest competitor), from MPL 2.0 to BSL 1.1 (2023). Redis adopted a dual SSPL + RSALv2 license (2024). Each change triggered community backlash, forks (OpenTofu, OpenSearch, Valkey), and lasting trust damage. Developers who invested in these tools saw their contributions locked into value creation for a single company. [IBM completed its acquisition of HashiCorp](https://newsroom.ibm.com/2025-02-27-ibm-completes-acquisition-of-hashicorp,-creates-comprehensive,-end-to-end-hybrid-cloud-platform) in February 2025, and Nomad is still under the BSL: its [`LICENSE`](https://github.com/hashicorp/nomad/blob/main/LICENSE) names IBM as licensor, with each release converting to MPL 2.0 only four years after publication, and HashiCorp's [Community Edition licence page](https://developer.hashicorp.com/nomad/docs/ce-license-support) says the same ([HashiCorp's 2023 announcement](https://www.hashicorp.com/blog/hashicorp-adopts-business-source-license)). OpenTofu continues as an independent project, and the precedent of relicensing remains a risk factor for any single-vendor project.

Reliaburger's manifesto (point #10) states: *"Open source means open source. No bait-and-switch. No relicensing."* This isn't a policy that can be quietly changed. It's a published design principle on the same level as "one agent" or "batteries included." The Apache 2.0 license is a competitive advantage: teams evaluating orchestration tools can adopt Reliaburger knowing it will never become source-available, never require a commercial license for production use, and never restrict how they deploy it.

---

## 25. Questions & Answers

This section addresses the hard questions about Reliaburger's architecture: failure modes, scaling limits, and design trade-offs that a critical reader would raise.

### Q1: Port mapping with 500 apps per node. Won't you run out of ports?

The default host port range is 10000-32766, about 22,700 ports per node, below Linux's own ephemeral range. At 500 apps per node, even with rolling deploys (where old and new versions briefly coexist), port exhaustion isn't a practical concern. Unlike proxy-based service discovery approaches, the Onion eBPF layer consumes zero OS ports. It operates entirely at the socket level in the kernel. The only ports consumed are those allocated to actual running containers.

### Q2: How do cluster-wide metrics queries work at 10,000 nodes?

Nobody has run 10,000 nodes yet, so this is the design and what exists of it. Each node rolls its own samples up every minute (min, max, sum and count per series) and council members collect those rollups, so cluster-wide dashboard queries hit the council (3-7 nodes), not every node. The rollups are per series today, still carrying labels such as `instance` and `pid`, so they shrink time resolution but not the number of series; rolling them up per app is planned ([#364](https://github.com/reliaburger/reliaburger/issues/364)). Single-app queries fan out only to the nodes running that specific app (typically 3-10 nodes). A Prometheus remote-read endpoint, for federating full-resolution data into your own Thanos or Cortex, is planned too ([#364](https://github.com/reliaburger/reliaburger/issues/364)).

### Q3: Isn't gossip carrying too much data at scale?

Not the gossip itself: Mustard carries only lightweight, bounded state (membership, failure detection and leader identity), which is what keeps SWIM's O(1) per-node overhead. Per-node resource summaries on gossip are planned but not sent ([#364](https://github.com/reliaburger/reliaburger/issues/364)). Variable-size runtime state (what's running where, image availability, job completions) goes outside gossip, and that's the part that doesn't scale yet: every node reports straight to the leader. The hierarchical reporting tree, where nodes report to an assigned council member and council members aggregate for the leader, is planned under the same issue and has to land before any 10,000-node claim.

### Q4: With encrypted-in-git secrets, how do developers encrypt without cluster access?

Reliaburger uses asymmetric encryption (age). The cluster's public key is committed to the git repository and can be distributed freely. Anyone with the public key can encrypt secrets offline, no cluster access needed. Only the cluster holds the private key for decryption. Reusing ciphertext with a replacement cluster currently requires an operator to provision the existing key material out of band; the previously proposed `relish init --import-key` command isn't implemented. Key rotation supports a graceful transition period where both old and new ciphertexts are accepted.

### Q5: When a new leader is elected, can it make wrong scheduling decisions from incomplete state?

No. The new leader enters a learning period where it accepts StateReports from nodes but doesn't make scheduling decisions. Applies still commit to Raft, but nothing is placed until the period ends. It ends when 95% of the nodes gossip shows alive have reported or a timeout expires (default 15 seconds). During this period, the data plane is completely unaffected. Apps continue running and serving traffic. Only new work waits, unplaced.

### Q6: Aren't exec (non-container) jobs a massive security risk?

They would be, without constraints. The constraint that ships today is a policy gate, not a kernel sandbox. Process workloads require an explicit `admin` or `host-exec` Permission grant, and you must configure an explicit binary allowlist in `node.toml`. An empty or absent allowlist refuses every `exec`/`script` workload, so host execution is disabled on any node the operator hasn't explicitly opened up. Inline scripts additionally require signed commits when applied via GitOps. This deny-by-default posture ensures no host binary can execute without explicit operator approval. On rootful Linux, process batches enforce CPU/memory limits through native executors. Host applications still need cgroup limits; a dedicated unprivileged `burger` user, a seccomp profile and network/PID/mount namespaces remain planned (see Section 17). So treat a process workload as a binary the operator has explicitly whitelisted running on the host, not as a locked-down sandboxed process — for anything hostile, keep it in a container.

### Q7: How does Pickle ensure image durability?

Durability is eventual, not guaranteed at push time. A push commits the image locally and returns immediately with an `oci-replication: pending` header, so the client never mistakes acceptance for full redundancy. A leader-only heal loop (running roughly every 60 seconds) then replicates layers towards a redundancy target of 2 total copies by default: the node that received the push plus one peer. There is no synchronous mode and no `push_sync` config key. This keeps push latency low and independent of cluster size, at the cost of a short window after a push where the image exists on only one node. In clusters too small to reach the target, the heal loop records that full redundancy can't be met rather than silently claiming success.

Heal counts only copies on live nodes (alive or suspect in gossip). When a node dies, every image it held drops a copy, and the next heal pass has another live node copy the layers straight from a surviving holder; the leader plans but doesn't relay. Decommissioning a node removes it from every holder list. An image whose last live copy is gone is reported as *lost* by `relish status` and `relish wtf`, and a deploy of it fails telling you to re-push it. Old images don't pile up either: a manifest that has had no tag for `gc_retain_days` expires, unless an app or job pins it, and GC reclaims its layers.

### Resource-aware delegated execution

A thousand queued jobs don't need a thousand simultaneous allocations. Suppose
an otherwise available node has eight cores and 16 GiB of memory. It can run
32 tasks requesting 250 millicores and 256 MiB each, four tasks requesting two
cores and 4 GiB each, or a mixture. Existing applications and system reservations
come out of that capacity first. A task count alone cannot describe this budget.

The delegated pipeline separates resource allocation from task selection, an
idea also used by Mesos's two-level scheduling. Meat selects eligible nodes and
records bounded chunk grants in Raft. Bun accounts for application commitments,
packs individual tasks within its shared node budget and reuses execution
machinery and cached artifacts. A chunk is a queue of work;
its size amortises dispatch and bookkeeping, not the resource footprint of
concurrent execution. Arrays encode repeated templates compactly. Mixed-profile
manifests preserve individual identities while grouping compatible work.

Workers record terminal outcomes as tasks finish, using bounded group commit,
and resend completion batches until the leader accepts them. Grant generations
and reconciliation fence stale requests and resolve uncertain ownership after
failover. Restoring older council state can rewind grant identities; workers
with existing array data persist a refusal until re-enrolled with fresh data,
preserving old results for investigation. At-least-once execution requires
business idempotency keys that survive cluster recovery. Detailed outcome
availability and retention are explicit; a missing worker isn't a zero count.

At this volume, the default operator view is a workload summary: unique success
and terminal-failure rates, retries, backlog and submission age, final-attempt
duration distributions, resource requests and refusal reasons.
Individual tasks remain addressable for diagnosis, with bounded detail queries
and exports. Histograms merge before percentiles are calculated, and metric
labels never use individual task IDs. Local FIFO admission prevents small tasks
continually overtaking a large waiting request, at the cost of idle capacity
behind that request. Namespace quotas govern app placement and job grants
alike, and each namespace has its own caps on active runs and definitions, with
headroom reserved for cron fires and deployment hooks. Fair sharing of spare
capacity between tenants (DRF) and service pre-emption remain future work;
new app deployments still require free capacity and rollout headroom.

**Implementation status:** this is the architecture implemented in
[#266](https://github.com/reliaburger/reliaburger/pull/266), following the
[resource-aware jobs plan](plans/2026-10-04-plan-delegated-jobs.md). The daily
throughput target remains unqualified until real workloads sustain at least
1,158 unique successful completions per second with headroom, concurrent apps,
failure recovery and bounded memory and storage. A short simulated burst doesn't
establish that result.

### Short commands in reusable containers

`runtime = "shared-runc"` selects bounded, built-in image executors
beneath the common job path. Containers are compatible only within a pinned
image, namespace, credentials and resource profile. Each command starts a
separate process, born inside its own limited cgroup. Private scratch and IPC
namespaces disappear with the task; descendants must retire before reuse.
Fresh containers remain the default. PID and network namespace reuse is an
explicit trade-off, rather than a new application worker protocol.

Warm executors retain their resource requests and helper overhead while idle.
They expire quickly and yield to waiting profiles, preserving application
capacity and FIFO admission. Cancellation or uncertainty retires the original
container before replacement. Rootful Linux allowlisted host batches use the same
bounded executor design with a durable native helper and a fresh process per
command. Explicit CPU/memory profiles are enforced before execution through
task cgroups; helper resources stay charged while idle. Other platforms retain
the original owned process backend and refuse unsupported explicit limits.
Host commands retain host access and Bun's user; they require trusted workloads.
Neither path removes process creation, durable outcomes or external-effect
idempotency costs. See the [execution and qualification plan](plans/2026-10-07-plan-reusable-executors-and-throughput.md).

Fast delegated jobs use bounded grant lookahead derived from verified duration
summaries, covering receipt acceptance and grant delivery without increasing
control polling. Learned grant depth is capped at sixteen chunks; actual execution
remains gated by per-node concurrency and resource admission. The trade-off is
more granted work to reconcile after worker loss, with unchanged attempt fences.

Fresh containers suit fully isolated independent work. Shared containers suit
repeatable trusted tasks whose container startup would dominate the command.
Host jobs trade isolation for speed with an explicit allowlisted binary. Jobs
select `runtime = "runc"` by default, or opt into `shared-runc` or `process`; a
process definition with an image is refused. Explicit Bun `--runtime runc`
permits containers only, while `mixed` and Linux auto-detection can enable both.

The [timed job scenarios](qualification/2026-10-09-timed-job-scenarios/README.md)
give each path the same 60 seconds, concurrency and resource profile on one small
VM, beside a raw-process reference. Every path runs a no-op command, so they
measure per-job overhead, not the throughput of real work, and the record says
what they don't establish: a universal ceiling, multi-node scaling, sustained
headroom or bounded historical metadata. One-hour soaks of each path then ran
sequentially beside the same application, with no terminal failures and every
application probe answered. Disk use grew materially during them and the
capped storage scans were incomplete, so they are saturation observations, not
a daily qualification; that remains in #668.


### AI workloads: resident models and coordinated training

Preparing a million dataset records, running a hundred training experiments
and serving inference requests require different execution contracts. The
same batch foundation can provide stable task identities, resource profiles,
bounded queues, retries and durable outcomes. Data, models and checkpoints
should remain in artifact storage, with small immutable references in the
control plane. Job throughput alone does not describe model throughput or
accelerator utilisation.

For inference, a container pool that starts a new model process for every task
would still repeat model initialisation. The proposed AI path keeps model
workers resident and feeds separately tracked requests through a built-in
engine adapter. Reliaburger would allocate and own the workers; engines such
as [vLLM](https://docs.vllm.ai/en/stable/) would manage continuous batching and
attention memory. The worker holds its resource reservation while requests
share bounded engine capacity. Scheduler chunks and model batches are separate
units. Interactive inference remains a long-lived application with latency
protection from background batches.

Independent training experiments fit the task model. Distributed training
requires group allocation, rendezvous and coordinated recovery; its workers
cannot be retried as unrelated array elements. For example,
[PyTorch's elastic launcher](https://docs.pytorch.org/docs/main/elastic/run.html#failure-modes)
can restart a worker group after failure. Reliaburger would need an explicit
group contract and application checkpoint references before supporting those
runs. Accelerator placement must consider device identity, memory and topology,
with exclusive whole-device allocation before any supported sharing scheme.

These are design extensions. Current delegated jobs refuse GPU requests, and
resident model adapters and distributed training groups are not implemented.
GPU cluster placement and container device assignment remain tracked in F01
([#359](https://github.com/reliaburger/reliaburger/issues/359)). Their validation
must measure useful records/tokens or training samples per second, queue and
serving latency, accelerator utilisation and recovery, as well as task counts.
See the [AI implementation sequence](plans/2026-10-04-plan-delegated-jobs.md#ai-training-and-inference).

### Q8: Can a single leader actually schedule 100M+ jobs per day while doing everything else?

100M jobs a day remains an unqualified design target (§2). The 0.2.0 development implementation keeps arrays compact in Raft and expands them on workers. Singleton jobs, cron and hooks create runs through the same indexed path as arrays. High-volume callers compress repeated work into arrays or mixed-profile manifests.

The leader grants chunks: "Node 7, here are your next 1,024 indexes" (the default chunk size, capped at 65,536), and a node with fast-finishing tasks can hold up to sixteen chunks ahead. Workers execute within their shared CPU/memory budget. A leader-driven HTTP sync accepts durable aggregate completions and retransmits grants; per-task ledgers and indexes stay on workers. This removes global placement and Raft writes per task. Runtime startup, resource demand, storage, result retention and application service quality still determine whether a particular cluster can sustain the daily target. The [delegated jobs report](qualification/2026-10-04-delegated-jobs/README.md) and the [timed job scenarios](qualification/2026-10-09-timed-job-scenarios/README.md) record the evidence so far; the sustained gate is [#668](https://github.com/reliaburger/reliaburger/issues/668).

### Q9: Local-only volumes with no distributed storage. How do teams not lose data?

Three layers of mitigation. First, Reliaburger recommends managed databases for critical data (the pattern most production teams already follow). Second, apps that manage their own replication (CockroachDB, Cassandra) work naturally with local volumes. Third, Reliaburger includes built-in volume snapshots: `relish snapshot create <app>` creates an instant copy-on-write snapshot on the local filesystem, and Bun's snapshot loop on each node ships it to S3/GCS on a schedule (see Section 5.5). The upload runs inside Bun, not as a separate container, and uses the object-store credentials in that node's configuration. Local snapshots provide fast rollback from application-level corruption; the upload job provides off-node durability. This covers the common case of "don't lose my Redis cache" without the complexity of a distributed storage layer.

### Q10: Will TOML configuration get unwieldy at 50+ apps?

Reliaburger supports directory-mode configuration where each app lives in its own file, merged at deploy time. A `_defaults.toml` file provides shared values (common env vars, memory limits, deploy strategy) inherited by all apps unless overridden. `relish fmt` formats files; `relish lint` validates configuration and catches common errors. The GitOps engine (Lettuce) uses the same tree resolver for defaults and directory namespaces, with its configured watch directory as the root. It refuses malformed or ambiguous trees before applying changes.

### Q11: Doesn't the eBPF approach require a modern kernel? What about older systems?

Yes, Onion requires Linux kernel 5.8 or later. This covers every actively-maintained Linux distribution as of 2026: Ubuntu 22.04+ (kernel 5.15), Debian 12+ (kernel 6.1), RHEL 9+ (kernel 5.14), Amazon Linux 2023 (kernel 6.1). Older distributions like RHEL 8 (kernel 4.18) remain under extended life support until 2029 but ship a kernel that predates the eBPF features Onion requires. The kernel requirement is a hard line where it matters: a rootful runc node with `[ebpf]` enabled (the default `relish init` writes there) refuses to start on an older kernel, with a clear error. Without eBPF, Bun still starts, but the node can't enforce namespace isolation or egress allowlists, so it refuses `allow_from` and allowlists. This is a deliberate trade-off: eBPF socket interception removes the proxy from the data path and the overlay from the network, which is worth more than supporting legacy kernel versions. It doesn't remove DNS: Bun still runs a small userspace responder for `.internal` names, because a cgroup socket hook can rewrite addresses but can't answer a query.

### Q12: The design goals mention GPU scheduling. Does it support fractional GPUs?

Not yet, and not whole GPUs either: GPU allocation is planned ([#359](https://github.com/reliaburger/reliaburger/issues/359)), and no release passes a device into a container today (§2). The plan is whole-device allocation first (`gpu = 1`, `gpu = 2`). Fractional GPU sharing (MIG partitions on NVIDIA A100/H100, or time-slicing) requires specific hardware support and driver configuration that varies significantly across GPU generations. Rather than ship a half-baked abstraction, fractional GPU support is deferred to v2, where MIG partitions can be exposed as distinct schedulable devices.

### Q13: How does multi-tenancy work? Can one team starve the cluster?

Namespaces provide resource quotas (CPU, memory, GPU, app count, and replica count budgets) that the Meat scheduler enforces when it places workloads. The check happens in the leader's scheduling pass, not at apply time: `relish apply` accepts an over-quota app and commits it to the desired state, and Meat then leaves it unplaced. Running apps are never evicted to make room. The leader records why in the council, so `relish status`, `relish inspect`, the dashboard and `relish wtf` all show the app as blocked with the quota it would break, and the reason clears on the first pass where the app fits. An apply-time rejection isn't on the roadmap yet. Jobs are held to the same budget at the moment the leader grants them work: each attempt's CPU, memory and GPU requests count against the namespace after its apps, and a run with no room waits with a visible quota-blocked reason instead of starting. Job capacity is divided too. One namespace can hold at most 64 active arrays, batches and manual runs and 64 job definitions, and 32 of the cluster's 128 active-run slots are kept for cron fires and deployment hooks, so a tenant flooding the queue can't lock out anyone else's schedules or deploys. A cron fire that still finds no room is recorded as a skipped occurrence with an event, never dropped silently. The default namespace has no quotas unless you configure them, which is appropriate for single-team clusters. Multi-team clusters should configure per-team namespaces with quotas from day one.

### Q14: What's the minimum cluster size?

Reliaburger runs on any number of nodes. A single-node cluster works for development and small production workloads: the node is simultaneously the leader, the only council member, and the only worker. A 2-node cluster gives you somewhere to reschedule apps, and its council has one voter: the council always holds the largest odd number of voters that the live nodes (and the cap of seven) allow, because an even council survives no more failures than the odd one below it. Lose the node that isn't the voter and nothing changes. Lose the voter and the survivor's apps keep running, but nothing can be written or scheduled until the voter comes back or you recover the council on the survivor. Raft can't do better with two machines: two voters would need both for a majority, so either failure would stop the control plane. High availability needs three nodes, where the council can lose any one of them and keep its quorum. Four nodes still run three voters, and five run five. The stated "2-200 nodes" target audience reflects that Reliaburger is designed for this range, not that all configurations within it are equally resilient.

### Q15: What is the base resource overhead per node?

We measured an idle node with nothing deployed, ten minutes after it started, on the 0.2.0 development build ([record](qualification/2026-10-11-idle-node-memory.md)). A single-node cluster, so the node is the leader and the only council voter, held 63 MiB of resident memory; the same binary without `--cluster` (no Raft, no gossip) held 48 MiB. Neither moved over the following five minutes; we didn't record CPU. All components share a single process (the Bun binary), so there's no per-daemon overhead to add.

Read those numbers with their limits. They come from macOS on Apple silicon, where Bun runs the process runtime with no eBPF maps, DNS responder or ingress loaded, on a busy host whose memory pressure may have pushed them down. A Linux node with runc and eBPF on hasn't been measured yet; the same script ([`scripts/release/idle_memory.py`](../scripts/release/idle_memory.py)) runs on the release candidate. Nor is idle the figure to size a node by. Once apps and jobs run, Bun's metrics, logs and job state grow: in the 0.1.6 soak, Bun's first samples were 180-583 MB with workloads deployed and its peak was 810 MB ([record](qualification/2026-10-06-v0.1.6-sustained-v02-final.md)). Budget for that on a small node.

### Q16: How does Franchise compare to Kubernetes multi-cluster tools?

Franchise is planned, not yet implemented (see Section 21). This comparison sets out the intended design against the alternatives.

| | K8s + Karmada | K8s + Cilium ClusterMesh | Nomad Federation | **Reliaburger Franchise** |
|---|---|---|---|---|
| Setup | Install Karmada control plane + CRDs | Cilium on all clusters + shared etcd | `nomad server join` | **`relish franchise join`** |
| Cross-cluster networking | Via Submariner / Cilium | Direct pod-to-pod (eBPF) | None (app-level) | **Via Wrapper ingress (no tunnels)** |
| Shared state | Karmada API server | Shared cilium-etcd | WAN gossip only | **WAN gossip only** |
| Service discovery | MCS-API CRDs | Cilium global services | Consul required | **Built-in (`name.cluster.franchise`)** |
| Dashboards | Separate (Grafana per cluster) | Separate | Nomad UI (per-region) | **Built-in (Brioche franchise view)** |
| GitOps | ArgoCD ApplicationSets | Separate | Separate | **Built-in (Lettuce per-cluster dirs)** |
| Image sharing | Harbor replication | Not included | Not included | **Built-in (Pickle cross-pull)** |
| Extra tools needed | Many | Cilium CNI on all clusters | Consul for service discovery | **None** |

The key difference: Kubernetes multi-cluster requires choosing, installing, and operating additional tools (each with its own learning curve). Franchise is built in and uses infrastructure that already exists in every Reliaburger cluster. Wrapper handles cross-cluster traffic the same way it handles external traffic.

---

*Reliaburger is an open source project licensed under Apache 2.0.*
*For more information, visit reliaburger.dev.*
