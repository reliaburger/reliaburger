# Reliaburger: The Batteries-Included Container Orchestrator

**A whitepaper on the design, architecture, and vision for Reliaburger, a radically simplified container orchestration platform written in Rust.**

**Version 0.1 — February 2026**

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

Reliaburger is built for these teams. It's a single binary that includes everything you need to run containers in production: scheduling, networking, ingress with automatic TLS, service discovery, a container image registry, log collection, metrics and dashboards, GitOps, and a web UI. No plugins. No ecosystem to assemble. No YAML to wrestle.

The architecture is designed for 10,000 nodes to provide headroom, but the primary audience is teams with 2-200 nodes who benefit from a single, well-tested architecture rather than separate solutions for small and large clusters.

---

## 2. Design Goals

Reliaburger is designed to meet the following quantitative targets. These engineering constraints inform every architectural decision in this document.

| Target | Value |
|--------|-------|
| Maximum nodes per cluster | 10,000 |
| Maximum apps per node | 500 |
| Job scheduling throughput | 100,000,000 jobs per day (~1,160 per second sustained) |
| Leader election time | < 5 seconds |
| Time to full operability after leader loss | < 20 seconds (5s election + 15s learning period) |
| State reconstruction time (cold start, full cluster) | < 30 seconds |
| Time from bare metal to first deploy | < 5 minutes |
| Cold start (new node joins and accepts work) | < 60 seconds |
| Image distribution to N redundancy peers (default N=2) | < 30 seconds per layer after push |
| GPU scheduling | First-class resource, whole-device allocation |
| Minimum kernel version | Linux 5.7+ (for eBPF CO-RE, BTF, and BPF_LINK support) |
| cgroup version | v2 (required for eBPF service discovery and resource isolation) |

> **Note on leader election time:** The < 5 second target measures the Raft election itself. The subsequent learning period (up to 15 seconds) runs before the new leader accepts deploys. Total time to full operability after leader loss is < 20 seconds. The data plane is unaffected throughout.

**10,000 nodes per cluster** demands that the coordination layer never require all-to-all communication. The two-layer architecture (Mustard gossip for all nodes, Raft for a small council) is a direct consequence of this target. Raft doesn't scale beyond single digits; gossip scales to tens of thousands.

**500 apps per node** demands that the Bun agent, Onion eBPF service map, Ketchup log collector, and Mayo metrics store all have sub-millisecond per-app overhead. Rust's zero-cost abstractions and lack of garbage collection pauses make this achievable in a way that a garbage-collected runtime would struggle to match at the tail.

**100 million jobs per day** demands a scheduler (Patty) that can make placement decisions at high throughput without becoming a bottleneck. At a sustained rate, this is over a thousand jobs dispatched per second. The design implications (batch scheduling, delegated execution, and asynchronous reporting) are detailed in [design/scheduler-patty.md](design/scheduler-patty.md).

**GPU as a first-class schedulable resource** means that AI/ML workloads aren't a day-two feature. The Bun agent detects GPUs at startup via NVML and reports them as schedulable resources alongside CPU and memory.

**Automatic recovery from the loss of any single node, the leader, or the entire council** means that none of these failures interrupts the data plane. Applications continue running when the control plane is unavailable. Surviving nodes can reconstruct the full state. The cluster self-heals without operator intervention.

---

## 3. Design Principles

### 3.1 One Binary, Batteries Included

Reliaburger ships as a single statically-linked binary. Every component (the scheduler, ingress proxy, image registry, metrics store, log collector, GitOps engine, eBPF service discovery, and web UI) is compiled into one executable. There's nothing to install beyond a container runtime, no dependencies to manage, and no compatibility matrix to maintain.

### 3.2 Apps, Not Pods

The unit of deployment is an **App**, not a Pod/Deployment/Service triple. At its simplest, an App is an image, a replica count, and a port: three lines of TOML. A single App resource replaces the Kubernetes concepts of Deployment, ReplicaSet, Service, Ingress, HorizontalPodAutoscaler, DaemonSet, and node affinity rules. Optional inline sections (placement constraints, autoscaling, firewall, identity, deploy strategy) add capabilities that would otherwise require separate Kubernetes resources, but the minimal definition stays minimal.

### 3.3 Convention Over Configuration

Sensible defaults everywhere. TLS is automatic: when an App declares an ingress block, TLS defaults to `"auto"` without needing to be specified (Let's Encrypt for public services when the cluster has internet access, or the cluster's Ingress CA for air-gapped environments). Health checks use sane timeouts. Rolling deploys are the default strategy. Log collection is on by default. Reliaburger collects metrics automatically. Configuration is for overriding defaults, not for bootstrapping basic functionality.

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
| Scheduler | **Patty** | The core scheduling engine |
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

Reliaburger defines exactly seven resource types. Every production workload can be expressed using a combination of these types.

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
# tls defaults to "auto" — no need to specify
```

**Replica modes:** `replicas = 3` places exactly 3 instances across available nodes. `replicas = "*"` runs one instance on every node (daemon mode). An App with a volume becomes stateful. An App with `gpu = 1` gets a GPU. An App with `exec` instead of `image` runs a host binary as a process workload (see Section 17).

**Autoscaling:** Apps can declare an autoscale block to adjust replicas based on observed metrics:

```toml
[app.web.autoscale]
min = 2
max = 10
metric = "cpu"
target = "70%"
```

The leader makes scaling decisions locally based on Mayo metrics. The Lettuce GitOps engine treats autoscaler adjustments as runtime overrides (see Section 14).

**Init containers:** Apps support init containers via an `[[app.web.init]]` block that runs before the main container starts, used for database migrations, config generation, or dependency checks.

**Placement constraints:** Nodes carry labels (configured in `node.toml`). Apps can use `required` labels (hard constraints) and `preferred` labels (soft constraints) to control placement.

```toml
[app.web.placement]
required = ["region=us-east"]
preferred = ["ssd=true"]
```

> For scheduling algorithm details, see [design/scheduler-patty.md](design/scheduler-patty.md).

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

**High-throughput batch scheduling:** At 100M jobs/day, Patty allocates job batches to nodes rather than scheduling individual jobs. Nodes execute and report completions asynchronously. The Raft log records only batch-level decisions.

**Build jobs:** Jobs can build container images and push them to the Pickle registry via the `pickle://` scheme. Build jobs require a `build_push_to` field that scopes registry access. Lettuce injects `${GIT_SHA}` for tag synchronisation.

> For batch scheduling and build job details, see [design/scheduler-patty.md](design/scheduler-patty.md) and [design/registry-pickle.md](design/registry-pickle.md).

### 5.3 Secret

Reliaburger stores secrets encrypted in git using asymmetric encryption (age). The cluster's public key is distributed freely; only the cluster holds the private key. Developers can encrypt secrets offline without cluster access.

```toml
[app.api.env]
DATABASE_URL = "ENC[AGE:YWdlLWVuY3J5cHRpb24...]"
NODE_ENV = "production"
```

Namespace-scoped keypairs are available for multi-tenant clusters where blast radius matters. `relish secret rotate` handles key rotation with graceful transition periods.

> For encryption implementation details, see [design/security-sesame.md](design/security-sesame.md).

### 5.4 ConfigFile

A free-text file injected into a specific path inside the container. Unlike environment variables, ConfigFiles can contain arbitrary content (NGINX configs, application YAML, TLS certificates). Declared inline with `content` or referenced from git with `source`.

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
volume = { path = "/data", size = "10Gi" }
```

**Volumes are local by design.** Reliaburger doesn't include distributed storage. For data that must survive node loss: (1) use a managed database service, (2) use an application that handles its own replication, or (3) configure volume snapshots with off-node upload. `relish wtf` warns about `replicas = 1` Apps with volumes that lack a snapshot schedule.

Reliaburger enforces volume size via Btrfs subvolume quotas or loop-mounted sparse files on ext4/xfs. Built-in volume snapshots use copy-on-write filesystem snapshots with scheduled upload jobs.

### 5.6 Permission

Simplified access control. Permissions define who can perform which actions.

```toml
[permission.deployer]
actions = ["deploy", "scale", "logs", "metrics"]
apps = ["web", "api"]
```

Valid actions: `deploy`, `scale`, `logs`, `metrics`, `exec`, `host-exec`, `admin`, `secret-read`, `secret-write`. The `host-exec` and `admin` actions are required for process workloads and cluster administration respectively.

### 5.7 Namespace

Optional workload isolation with resource quotas (CPU, memory, GPU, app count, replica count). Reliaburger uses a single default namespace unless you explicitly create others.

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

For the first node, no configuration file is required. Just run `relish init`.

**First boot experience:**

```bash
# Create a new cluster
$ relish init
✓ Generated root CA and intermediate CAs
✓ Started Bun agent. This node is the leader.
  Join token: rbgr_join_1_eyJhbGciOi...
  Dashboard: https://10.0.1.5:9443

# Add a node
$ relish join --token rbgr_join_1_eyJhbGciOi... 10.0.1.5:9443
✓ Joined cluster (3 nodes total). Ready to accept workloads.

# Deploy an app
$ relish apply -f myapp.toml
✓ All health checks passing. https://myapp.com → ready
```

Three commands from bare metal to a running, TLS-secured production application (assuming the `relish` binary and `containerd` are installed).

> For the full `node.toml` reference, storage path design, and resource configuration, see [design/agent-bun.md](design/agent-bun.md).

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
│   • Scheduler (Patty)                                    │
│                                                          │
│  Council members additionally run:                       │
│   • API server (reads from local Raft state)             │
│   • GitOps engine (Lettuce, on elected coordinator)      │
│   • Metrics aggregation                                  │
└──────────────────────────────────────────────────────────┘
```

### 7.2 Leader and Council

The leader is an ordinary node that additionally runs the Patty scheduler. Leadership is lightweight: the leader runs application workloads alongside its scheduling duties. Any council member serves API read requests; writes go to the leader via Raft.

For large clusters with heavy API read load, additional non-council nodes can serve as **read replicas**, maintaining a read-only Raft state follower without participating in consensus.

```
Read:  User → any node → nearest council member → response (from local state)
Write: User → any node → leader → Raft commit → response
UI:    User → any node → Brioche (local) + API calls (routed as above)
```

---

## 8. Leader Election & State Management

### 8.1 Two-Layer Architecture

Reliaburger uses two distinct protocols for cluster coordination:

**Layer 1: Mustard (Gossip), all nodes.** Based on SWIM, handles membership discovery, failure detection, leader identity broadcast, and per-node resource summaries (a few hundred bytes per node). Scales to tens of thousands of nodes with O(log N) convergence and O(1) per-node overhead.

**Layer 2: Raft, small dynamic council.** A council of typically 3-7 nodes runs Raft consensus for leader election and desired-state replication. Council members are selected automatically based on stability, resource availability, and zone diversity.

A third **hierarchical reporting tree** carries variable-size runtime state (what's running where, health, resource usage). Each node reports to its assigned council member; council members aggregate for the leader. This separation ensures gossip messages stay constant-size at 10,000 nodes.

### 8.2 Reconstructable State

When a new leader is elected, it enters a learning period, during which it collects StateReports from all nodes before making scheduling decisions. The learning period ends when 95% of known nodes have reported or a 15-second timeout fires. Unreported nodes are marked "state unknown," and the leader begins accepting deploys using only reported capacity. The data plane is completely unaffected during reconstruction.

### 8.3 Catastrophic Recovery

Pre-seeded recovery candidates (stable nodes outside the council, selected for diversity) enable deterministic recovery when the entire council is lost. The highest-priority surviving candidate assumes leadership without an election. An encrypted candidate list in gossip prevents attackers from targeting recovery nodes. Network partitions can't cause split-brain: Raft requires a majority quorum to elect a leader, and the catastrophic recovery path activates only after a configurable timeout with no quorum detected. Nodes on the minority side of a partition operate in data-plane-only mode (apps continue running, no new deploys) until the partition heals.

**Partition degradation:** During a sustained partition, isolated nodes experience progressive degradation. The eBPF service map becomes stale for new cross-partition connections (immediate), certificate rotation fails (at 30 minutes), and workload certificates expire (at 1 hour, or 4 hours with the grace period). Apps with existing connections and valid certificates continue operating throughout.

**Clock synchronisation:** Reliaburger assumes nodes have reasonably synchronised clocks (NTP or equivalent). Certificate validation, token TTLs, and cron scheduling depend on clock accuracy within a few seconds.

**Backup and restore:** For disaster recovery from total cluster loss, you should back up the Raft state snapshot and CA key material to an external location. `relish backup` exports an encrypted snapshot suitable for off-cluster storage.

**Disk exhaustion:** Council nodes monitor available disk space and proactively step down from the council when storage falls below a configurable threshold (default: 1 GB), preventing Raft from stalling on failed log writes. The node rejoins the council automatically once disk pressure is resolved.

> For protocol details, state machine diagrams, and split-brain prevention, see [design/gossip-mustard.md](design/gossip-mustard.md).

---

## 9. Networking

### 9.1 No Overlay Network

Reliaburger doesn't use an overlay network, a CNI plugin, or any form of virtual networking. Each container runs in its own Linux network namespace with dynamic port mapping to the host. Each container gets host ports from a configurable range (default 10000-60000, providing 50,000 available ports per node). There's no virtual network, no tunnel encapsulation, and no cluster-wide IP space to manage.

This eliminates a large proportion of Kubernetes debugging pain: encapsulation overhead, MTU issues, IP exhaustion, opaque packet paths, and an entire plugin ecosystem to manage.

### 9.2 Built-In Ingress (Wrapper)

Wrapper runs on every node by default and handles TLS termination (automatic via Let's Encrypt or the cluster's Ingress CA), host/path-based routing, health-aware load balancing, connection draining during deploys, and WebSocket support.

```toml
[app.web.ingress]
host = "myapp.com"
tls = "acme"        # or "cluster" for air-gapped environments
```

There's no IngressClass, no annotations, no separate cert-manager installation. External traffic reaches the cluster via a standard TCP/UDP load balancer (or DNS round-robin) pointing to any set of nodes. Wrapper on each node can route traffic for any app, so you don't need application-aware routing at the load balancer layer.

> For routing, TLS modes, and connection draining details, see [design/ingress-wrapper.md](design/ingress-wrapper.md).

---

## 10. Service Discovery (Onion)

Onion is an eBPF program that intercepts the network stack at two points at the socket level:

1. **DNS interception:** When an app resolves a `.internal` name, the eBPF program responds directly from an in-kernel service map with a virtual IP. No DNS server process.
2. **Connect interception:** When an app calls `connect()` with a virtual IP, the eBPF program rewrites the destination to a healthy backend's actual `host:port`. No proxy in the data path.

```
App calls getaddrinfo("redis.internal")
  → eBPF responds with virtual IP 127.128.0.42

App calls connect(127.128.0.42, 6379)
  → eBPF rewrites to 10.0.1.5:30891 (real backend)
  → direct TCP connection, native performance
```

From an application's perspective, connecting to another service is identical to any network service: `http://web.internal:8080`, `redis://redis.internal:6379`. The eBPF layer is completely invisible.

Virtual IPs are allocated per service name (not per instance) from 127.128.0.0/16, within the loopback range so packets are guaranteed to be intercepted locally and never leak onto the network (~65K unique service names, expandable to /10 for ~4M). Because the eBPF program runs in the kernel, a Bun crash doesn't break existing connections. However, the service map becomes stale for new connections until Bun restarts. Systemd manages Bun with automatic restart (default: 5-second delay), `OOMScoreAdjust=-900` to protect it from the OOM killer, and re-populates the eBPF maps from the reporting tree on startup.

**External DNS:** Non-`.internal` names use the host's configured resolvers. Egress allowlists implicitly permit DNS (UDP/TCP port 53) to the host's configured nameservers.

> For BPF map layouts, eBPF program details, and the `firewall_map`, see [design/discovery-onion.md](design/discovery-onion.md).

---

## 11. Security

### 11.1 Certificate Authority Hierarchy (Sesame)

Sesame generates a root CA and three intermediate CAs at cluster initialisation:

```
Root CA (offline after init, signs only intermediate CAs)
├── Node CA         — signs node certificates for inter-node mTLS
├── Workload CA     — signs workload identity certificates (SPIFFE)
└── Ingress CA      — signs certificates for tls = "cluster" ingress routes
```

A compromise of any single intermediate CA doesn't affect the others. Reliaburger deletes the root CA private key from all cluster nodes after signing the intermediates; it exists only in a sealed backup. Intermediate CAs are issued with a 5-year lifetime. Rotation requires the sealed root CA backup, performed via `relish ca rotate`. `relish ca status` warns when intermediates are within 90 days of expiry.

**Node authentication:** Nodes join via short-lived join tokens (default 15 minutes, single-use) that embed a SHA-256 fingerprint of the cluster's root CA certificate, allowing the joining node to verify the cluster's identity before transmitting credentials. On successful join, the node receives a certificate signed by the Node CA. All subsequent communication is mTLS. Optional TPM attestation provides hardware-based identity verification.

### 11.2 Workload Identity

Every workload automatically receives a SPIFFE-compatible X.509 certificate and an OIDC JWT, with no configuration required from you:

- **X.509 cert** (1-hour lifetime, rotated every 30 minutes) for mTLS to services that trust the Workload CA
- **OIDC JWT** for cloud provider identity federation (AWS, GCP, Azure)

Worker nodes use a CSR model: they generate a keypair locally, send a CSR to their council member, and receive a signed certificate only if the workload is legitimately scheduled on that node. During council outages, a 4-hour grace period extension prevents running workloads from losing their identity.

### 11.3 Network Security

Three layers of built-in firewall:

1. **Cluster perimeter** (nftables): external traffic enters only through Wrapper
2. **Namespace isolation** (eBPF): cross-namespace traffic blocked by default
3. **Per-app firewall** (eBPF `allow_from`): fine-grained ingress control within a namespace

**Egress is deny-all by default.** Apps that need external access must declare an `egress` block, preventing data exfiltration from compromised containers.

```toml
[app.api.egress]
allow = ["api.stripe.com:443", "hooks.slack.com:443"]
```

### 11.4 Data at Rest

Reliaburger encrypts Raft log data with AES-256-GCM. The encryption key is derived via HKDF from the node's identity and sealed to the TPM when available. Certificate revocation uses a CRL distributed via the reporting tree.

### 11.5 Audit Logging

Reliaburger logs all modifying API operations with identity, source IP, and timestamp. Audit logs are stored via Ketchup and queryable via `relish history`.

> For the full PKI model, API authentication, secret encryption, and threat model, see [design/security-sesame.md](design/security-sesame.md).

---

## 12. Built-In Image Registry (Pickle)

Pickle is a distributed OCI-compatible registry built into every node. When you push an image (via standard `docker push`), Pickle stores it locally, synchronously replicates it to N peer nodes (default N=2) for durability, then makes it available for P2P distribution across the cluster. In clusters with fewer than N+1 nodes, Pickle replicates to all available peers and warns that the full durability target isn't met. Different layers can be downloaded from different nodes in parallel, similar to BitTorrent.

Key properties:

- **Synchronous replication.** A successful push guarantees the image survives any single node failure.
- **P2P distribution.** Image fan-out scales with cluster size, not against it.
- **Pull-through cache.** External registry images (Docker Hub, GHCR) are cached on first use.
- **Image signing.** Keyless signing via workload identity, cosign-compatible.
- **Scoped access.** Build jobs can only push to explicitly declared repositories.

> For GC, signing, replication failure handling, and build job integration, see [design/registry-pickle.md](design/registry-pickle.md).

---

## 13. Deployments & Rollouts

When an App's image changes, Reliaburger performs a rolling deployment: start a new instance, wait for health check, add to Wrapper routing, drain the old instance, stop it. Old and new versions coexist on the same node without port conflicts.

```toml
[app.web.deploy]
strategy = "rolling"      # default
max_unavailable = 1        # max instances down during rollout
auto_rollback = true       # revert on health check failure
```

Reliaburger persists deploy state in Raft. If the leader fails mid-deploy, the new leader resumes the rolling update from the last committed step after the learning period. If you submit a new deploy while a rollout is in progress, it supersedes the in-progress rollout: in-flight instances are drained and replaced with the newest version directly, skipping the intermediate target.

**Dependency ordering:** Jobs can declare `run_before = ["app.api"]` to ensure migrations complete before app instances start.

> For the deploy state machine, connection draining protocol, and autoscaling, see [design/deployments.md](design/deployments.md).

---

## 14. GitOps

### 14.1 Built-In Sync Engine (Lettuce)

Lettuce is Reliaburger's built-in GitOps engine, replacing ArgoCD and Flux with a sync loop compiled directly into the Bun binary.

**Configuration:**

```toml
[gitops]
repo = "git@github.com:myorg/infra.git"
branch = "main"
path = "production/"
poll_interval = "30s"
require_signed_commits = true              # optional: reject unsigned commits
trusted_signing_keys = [                   # GPG or SSH key fingerprints
  "SHA256:abc123...",
  "SHA256:def456...",
]
```

Or webhook-triggered for instant deploys on push. Lettuce validates webhook payloads using HMAC-SHA256 signatures (configured via `[gitops] webhook_secret`). Only POST requests with a valid signature are processed. The webhook endpoint is served on the cluster API port and requires TLS. Rate limiting (default: 10 triggers per minute) prevents abuse. When `require_signed_commits` is enabled, Lettuce still verifies the commit referenced by the webhook before applying it.

When `require_signed_commits` is enabled, Lettuce only applies commits that are signed by a key in the `trusted_signing_keys` list. Unsigned or untrusted commits are rejected and an alert fires. This is particularly important for configurations that include inline `script` fields (Section 17), since anyone with git write access could otherwise inject arbitrary commands into the cluster.

**Sync behaviour:**

1. Lettuce watches the configured git repository.
2. On each poll (or webhook trigger), it pulls the latest commit.
3. It compares the desired state in the repository against the actual state of the cluster.
4. It computes a diff and applies only the changes.
5. The Brioche UI shows the sync status, the last applied commit, a preview of pending changes, and a history of all syncs.

**Autoscaler interaction:** Lettuce treats the `replicas` field in git as the *base* replica count. Autoscaler adjustments are runtime overrides stored in Raft, not in git. Lettuce compares the `replicas` field independently from other fields: only a change to the `replicas` value itself in git resets the runtime override. This prevents Lettuce from fighting the autoscaler during traffic spikes. After a leader election, the new leader carries forward autoscaler overrides from Raft and re-evaluates scaling decisions once the learning period completes and fresh Mayo metrics are available.

**Coordinator:** The Lettuce coordinator is a council member selected by the leader. If it fails, the leader selects a replacement; the new coordinator resumes from the last committed sync state in Raft.

> For sync loop internals and coordinator election, see [design/gitops-lettuce.md](design/gitops-lettuce.md).

---

## 15. Observability

Reliaburger includes a complete observability stack with zero configuration:

**Metrics (Mayo):** Per-node TSDB that automatically collects CPU, memory, network, and GPU metrics per app and node. Auto-detects Prometheus `/metrics` endpoints on your apps. 3-tier retention (10s resolution for 24h, 1min for 7d, 1h for 90d, configurable). Hierarchical aggregation via council members enables cluster-wide dashboards at 10,000 nodes. Custom alerts use a PromQL-compatible subset. Five default alerts ship active out of the box (CPU throttle, OOM, memory pressure, disk filling, CPU idle).

**Logs (Ketchup):** Captures stdout/stderr from every container. Structured storage with timestamp-based indexing, JSON auto-detection for field queries, and zstd compression. Export to S3 or external destinations on a schedule.

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
| `relish plan <path>` | Preview changes before apply | `terraform plan` (K8s has nothing equivalent) |
| `relish diff <path>` | Detect cluster drift | `kubectl diff` (limited) |
| `relish apply <path>` | Apply configuration | `kubectl apply` |
| `relish deploy <app> <image>` | Quick image update | `kubectl set image` |
| `relish events` | Streaming event log | `kubectl get events` (1h expiry) |
| `relish logs <app>` | Stream/search logs | `kubectl logs` + `stern` |
| `relish trace <app> --to <app>` | Connectivity diagnosis | (none — manual iptables/DNS debugging) |
| `relish inspect <resource>` | Deep resource inspection | `kubectl describe` |
| `relish top` | Live resource usage | `kubectl top` (requires metrics-server) |
| `relish wtf` | Automated health check | (none — requires runbooks + Prometheus alerts) |
| `relish exec` | Container/host shell | `kubectl exec` + `kubectl debug` |
| `relish exec --debug` | Debug container (separate identity) | `kubectl debug` |
| `relish resolve <name>` | Query eBPF service map | `kubectl get endpoints` + `nslookup` |
| `relish firewall <app>` | Show effective firewall rules | `kubectl get networkpolicy` + CNI tools |
| `relish history <app>` | Full audit trail | (none — requires external audit logging) |
| `relish rollback <app>` | One-command rollback | `kubectl rollout undo` |
| `relish scale <app> <n>` | Set replica count | `kubectl scale` |
| `relish secret encrypt` | Encrypt a secret value | (none — requires Sealed Secrets) |
| `relish fault <type> <app>` | Inject a fault | (none — requires Chaos Mesh / Litmus) |
| `relish test` | Run integration test suite | (none) |
| `relish bench` | Run performance benchmarks | (none) |
| `relish upgrade start` | Rolling cluster upgrade | `kubeadm upgrade` (multi-step) |
| `relish ca status` | Show CA hierarchy and expiry | (none) |
| `relish ca rotate` | Rotate intermediate CA certificates | (none — manual process) |
| `relish ca revoke` | Revoke a compromised certificate | (none — manual process) |
| `relish init` | Bootstrap a new cluster | `kubeadm init` |
| `relish join` | Join a node to the cluster | `kubeadm join` |
| `relish drain <node>` | Safely evacuate a node | `kubectl drain` |
| `relish backup` | Export encrypted cluster snapshot | `etcdctl snapshot save` |
| `relish secret rotate` | Rotate encryption keys | (none — requires Sealed Secrets re-encrypt) |
| `relish volume snapshot` | Snapshot a local volume | (none — requires CSI snapshotter) |
| `relish import -f <k8s-yaml>` | Convert K8s manifests to Reliaburger TOML | (none) |
| `relish export --format kubernetes` | Generate K8s manifests from config | (none) |

> For TUI mockups, command details, and debug container behaviour, see [design/cli-relish.md](design/cli-relish.md).

---

## 17. Process Workloads (Non-Container)

Reliaburger provides a middle ground between full container workloads and unmanaged system processes: **run a binary from the host's filesystem inside the same isolation primitives as a container app.** Process workloads get a cgroup (CPU, memory, GPU limits), a network namespace (Onion eBPF service discovery and firewall rules), a PID namespace, and Ketchup log capture, but they run a binary already on the host rather than one from a container image.

Process Apps use `exec` instead of `image`; process Jobs use `exec` or `script` for inline shell scripts. Both receive the same scheduling, health checks, metrics, and service discovery as container workloads. Reliaburger enforces security via a required binary allowlist in `node.toml`, a dedicated unprivileged user (`burger`), a restrictive seccomp profile, and a restricted mount namespace. Inline scripts applied via Lettuce automatically require signed commits. When applied directly via `relish apply`, TOML files containing `script` fields require `host-exec` permission, and the binary must be in the node's allowlist.

> For the full isolation model, security controls, and when-to-use-what guidance, see [design/agent-bun.md](design/agent-bun.md).

---

## 18. Fault Injection (Smoker)

Smoker is Reliaburger's built-in chaos engineering system. Because Onion's eBPF programs already intercept every DNS resolution and `connect()` call, and Bun already manages cgroups for every container, fault injection is a natural extension of existing infrastructure. No new binaries, processes, or sidecars.

```bash
relish fault delay redis 200ms          # add 200ms latency
relish fault drop api 10%               # fail 10% of connections
relish fault dns redis nxdomain         # DNS failure
relish fault cpu inference 50%          # CPU stress in cgroup
relish fault kill web-3                 # kill an instance
relish fault run chaos/scenario.toml    # scripted multi-step scenario
```

Safety rails enforce permission requirements, duration limits (default 10 min), no persistence across restarts, and blast radius protection (quorum, replica, and leader guards).

| | Chaos Mesh (K8s) | Litmus (K8s) | Gremlin (SaaS) | **Smoker** |
|---|---|---|---|---|
| **Installation** | CRDs + operator + RBAC | CRDs + operator + agent | SaaS agent | **Built-in** |
| **Network faults** | tc + iptables | tc + iptables | Agent-based | **eBPF socket-level** |
| **Overhead when idle** | iptables rules present | iptables rules | Agent running | **Near-zero (empty BPF map lookup)** |
| **Blast radius protection** | Manual | Manual | Basic | **Automatic** |

> For eBPF implementation, resource faults, and scripted scenarios, see [design/chaos-smoker.md](design/chaos-smoker.md).

---

## 19. Testing & Benchmarks

Reliaburger compiles its test and benchmark suite into the binary. `relish test` runs 39 integration tests across 13 subsystems (scheduling, service discovery, deployments, health checks, secrets, firewall, workload identity, ingress, volumes, process workloads, jobs, image registry, cluster coordination). Tests are independent, idempotent, and safe to run against production clusters.

`relish test --chaos` combines the integration suite with Smoker fault injection to verify recovery from leader failure, node failure, network partitions, and resource exhaustion.

`relish bench` measures performance against the design goals (Section 2) and produces a report with regression detection when compared against a baseline.

> For test output examples, benchmark methodology, and CI integration, see the design docs for each component.

---

## 20. Self-Upgrades

Since Reliaburger is a single binary, upgrading means replacing the binary and restarting the Bun process. The system automates this in a rolling fashion: workers upgrade first (with configurable parallelism), then council members one at a time to maintain quorum, then the leader transfers leadership and upgrades last. Application workloads are never interrupted, because the container runtime manages containers independently of Bun. During the rolling upgrade window, old and new binaries coexist; protocol version negotiation ensures backward compatibility within one major version.

Reliaburger verifies binary integrity via dual signatures: an embedded signing key set compiled into the binary AND an external signing key configured in `node.toml`. Network upgrades require both signatures. Automatic rollback triggers if a node fails to start on the new version.

> For the upgrade sequence, binary versioning, rollback procedure, and security considerations, see [design/agent-bun.md](design/agent-bun.md).

---

## 21. Multi-Cluster (Franchise)

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

Brioche provides a franchise overview page: all peered clusters, their health status, app counts, and capacity. Click through to any peer's detail view (Brioche proxies the request). On the CLI, `relish franchise status` shows a summary across all clusters and `relish top --franchise` shows fleet-wide resource usage. Each cluster's Mayo exposes a Prometheus remote-read API, so Brioche can query metrics across the franchise without cross-cluster scraping.

### 21.4 Cross-Cluster Image Pull and GitOps

Peered clusters can pull images from one another's Pickle registries on demand (lazy, not eagerly replicated) using the existing OCI Distribution API over the trust relationship. For GitOps, Lettuce supports shared repositories with per-cluster directories: a `_defaults.toml` provides shared configuration, and per-cluster directories override as needed. Each cluster's Lettuce independently syncs its own directory.

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
| Custom Resource Definitions (CRDs) | CRDs are Kubernetes's extensibility mechanism and the source of enormous ecosystem complexity. Reliaburger's 7 resource types cover the 80% use case. |
| Operators | Operators exist to manage complex stateful workloads on Kubernetes. Without CRDs, there is no operator framework, by design. |
| Service mesh (full sidecar proxy) | Namespace isolation, per-app eBPF firewall rules, and SPIFFE-based workload identity provide network segmentation and cryptographic authentication without the overhead of a sidecar proxy on every connection. Applications that need mTLS can configure it directly using the workload identity certificates — Reliaburger provides the certs, the app owns its TLS policy. |
| Helm / package manager | When the configuration is already 15 lines of TOML, a package manager for configuration templates is unnecessary. |
| External secret manager integration | The encrypted-in-git model covers the majority of use cases. Vault/AWS Secrets Manager integration is planned for v2. |
| Windows node support | Linux only in v1. |
| IPv6 | The current networking model (virtual IPs, eBPF hooks) is IPv4-only. IPv6 support is planned for v2. |
| Pod affinity / anti-affinity | Replaced by simple placement hints via node labels. |
| Pod Disruption Budgets | Reliaburger's drain logic respects the same constraints as rolling deploys: it never drains a node if doing so would reduce any app below `replicas - max_unavailable` healthy instances. `relish drain` checks all affected apps before proceeding. |
| Sidecars | Some use cases genuinely require co-located processes sharing a network namespace (authentication proxies, log forwarders, protocol adapters). In v1, init containers cover per-instance startup tasks and separate Apps with service discovery cover most runtime co-location needs, though at the cost of a network hop. A `sidecar` field on the App spec (co-located containers sharing the parent's network namespace and lifecycle) is planned for v2. |
| Distributed tracing backend | Applications export traces to external collectors (Jaeger, Tempo, Datadog) using standard OpenTelemetry SDKs. Workload identity (Section 11.2) provides authentication to external tracing services. No single tracing backend fits all teams, so including one would violate the "batteries-included means the default works" principle. |

### Migration Path

Reliaburger provides bidirectional Kubernetes migration tooling. Neither direction is a perfect translation (the systems have different abstractions), but the goal is to preserve as much as possible and clearly report what was lost.

**Importing from Kubernetes** (`relish import`) converts Kubernetes YAML into Reliaburger TOML. The importer correlates related resources automatically: a Deployment + Service + Ingress + HPA that Kubernetes treats as four separate objects becomes a single `[app.*]` block in TOML. It uses the same matching logic Kubernetes itself uses (label selectors, backend references, and scale target refs) to group resources. Every import produces a migration report with three sections: what was converted directly, what was approximated (review recommended), and what was dropped (no Reliaburger equivalent) with guidance on alternatives.

```bash
# From files
relish import -f deployment.yaml -f service.yaml --output-dir ./reliaburger/

# From a live cluster
relish import --from-cluster --kubeconfig ~/.kube/config --namespace myapp

# Dry run (migration report only)
kubectl get all -o yaml | relish import -f - --dry-run
```

**Exporting to Kubernetes** (`relish export`) generates standard Kubernetes manifests from Reliaburger configuration. Each `[app.*]` produces a Deployment + Service, plus Ingress and HPA if the app defines ingress and autoscale blocks. Each `[job.*]` produces a Job or CronJob. Features with no Kubernetes equivalent (auto-rollback, Smoker fault rules, process workloads) are noted in the export report.

```bash
relish export --format kubernetes -f myapp.toml > k8s-manifests.yaml
```

This is an explicit design goal: Reliaburger should never be a dead end, regardless of which direction you're moving.

---

## 23. Comparison Matrix

| | Kubernetes | k3s / k0s | Nomad | Docker Compose | **Reliaburger** |
|---|---|---|---|---|---|
| **Conceptual complexity** | Very high (50+ resource types) | Very high (same API) | Medium (~5 job types) | Low | **Low (7 types)** |
| **Node types** | Control plane + workers | Server + agent | Server + client | Single host | **Homogeneous** |
| **Binary count** | Many (apiserver, scheduler, controller-manager, etcd, kubelet, kube-proxy) | 1 | 1 | 1 | **1** |
| **Networking** | Overlay (CNI required) | Overlay (CNI required) | Host or overlay | Host (bridge) | **Per-container namespaces + port mapping (no overlay)** |
| **Ingress** | Separate install | Bundled (Traefik) | Separate | N/A | **Built-in (Wrapper)** |
| **TLS certificates** | Separate (cert-manager) | Separate | Separate | N/A | **Built-in (ACME or Ingress CA)** |
| **Metrics** | Separate (Prometheus) | Separate | Separate | N/A | **Built-in (Mayo + app scraping)** |
| **Alerting** | Separate (Alertmanager) | Separate | Separate | N/A | **Built-in (5 default alerts)** |
| **Logs** | Separate (Loki/EFK) | Separate | Built-in (alloc logs) | `docker logs` | **Built-in (Ketchup)** |
| **Dashboards** | Separate (Grafana) | Separate | Built-in (basic) | N/A | **Built-in (Brioche)** |
| **GitOps** | Separate (ArgoCD/Flux) | Separate | Separate | N/A | **Built-in (Lettuce)** |
| **Image registry** | Separate (Harbor etc.) | Separate | Separate | Docker Hub | **Built-in (Pickle)** |
| **Service discovery** | CoreDNS + kube-proxy | Same as K8s | Built-in (since 1.3) or Consul | Docker DNS | **Built-in (Onion eBPF, near-zero overhead)** |
| **Network security** | NetworkPolicy (CNI-dependent) | Same | Consul Connect | None | **Built-in eBPF + nftables (namespace isolation + per-app rules + deny-all egress default)** |
| **Image builds** | Separate (Tekton, Jenkins, external CI) | Same | Separate | `docker build` | **Built-in (build jobs → Pickle)** |
| **Terminal UI** | None (k9s is third-party) | Same | None | None | **Built-in (relish TUI)** |
| **Change planning** | `kubectl diff` (limited) | Same | Built-in (`nomad job plan`) | None | **Built-in (relish plan)** |
| **Connectivity debugging** | Manual (iptables, DNS, endpoints) | Same | Manual | N/A | **Built-in (relish trace)** |
| **Health diagnosis** | Manual (requires runbooks) | Same | Manual | N/A | **Built-in (relish wtf)** |
| **Fault injection** | Separate (Chaos Mesh / Litmus) | Same | Separate (Gremlin) | N/A | **Built-in (Smoker, eBPF-native)** |
| **Built-in test suite** | None | None | None | None | **Built-in (relish test, relish bench)** |
| **Config format** | YAML (verbose) | YAML (same) | HCL | YAML | **TOML (concise)** |
| **Max cluster size** | ~5,000 nodes | ~5,000 nodes | ~10,000 nodes | 1 host | **10,000 nodes** |
| **Volume model** | Local + CSI (network) | Same as K8s | Local + CSI | Docker volumes | **Local only (by design)** |
| **Multi-node** | Yes | Yes | Yes | No | **Yes** |
| **Rolling deploys** | Yes | Yes | Yes | No | **Yes (auto-rollback)** |
| **Autoscaling** | HPA (separate config) | Same as K8s | External | No | **Built-in** |
| **Time to first deploy** | Hours to days | Minutes to 30 min | 30 min to hours | Minutes | **< 5 min** |
| **Written in** | Go | Go | Go | Go | **Rust** |
| **GPU scheduling** | Via device plugin (separate install) | Via device plugin | Yes (device plugins) | No | **Built-in (NVML auto-detect)** |
| **Secret management** | Built-in (basic) or External (Vault, Sealed Secrets) | Same as K8s | Vault integration | Docker secrets | **Encrypted-in-git (built-in)** |
| **Workload identity** | Separate (SPIRE, cert-manager) | Same | Consul Connect | None | **Built-in (SPIFFE-compatible, auto-rotated)** |
| **Non-container jobs** | No | No | Yes (exec driver) | No | **Yes (cgroup + namespace isolated)** |
| **Non-container apps** | No | No | Yes (exec/raw_exec drivers) | No | **Yes (cgroup + namespace isolated)** |
| **Scheduling constraints** | nodeSelector, affinity/anti-affinity, taints/tolerations | Same | constraints, affinities | N/A | **Node labels with required/preferred (AND logic)** |
| **Daemon mode** | DaemonSet (separate resource type) | Same | system scheduler | N/A | **replicas = "*" (same App resource)** |
| **Job throughput** | Low (per-job API calls) | Low (same) | Medium | N/A | **100M+ per day** |
| **Leader recovery** | Automatic within quorum; backup for quorum loss | Same | Automatic within quorum; backup for quorum loss | N/A | **Auto-reconstructs from nodes (survives total council loss)** |
| **Multi-cluster** | Separate (Karmada / Cilium ClusterMesh / many CRDs) | Same | WAN gossip + API forwarding (Consul for service discovery) | N/A | **Built-in (Franchise — WAN gossip + Wrapper ingress, one command to peer)** |
| **K8s migration** | N/A | N/A | N/A | N/A | **Built-in (relish import/export with migration reports)** |
| **License** | Apache 2.0 | Apache 2.0 | MPL 2.0 (reverted from BSL 1.1) | Apache 2.0 | **Apache 2.0** |

> **Note on Docker Swarm:** Docker Swarm mode (via `docker stack deploy`) adds multi-node orchestration, rolling deploys, overlay networking, service discovery, and secret management to the Docker engine. It occupies a similar "simple orchestrator" space. However, Swarm has been in maintenance mode since 2019, receives only security patches, and lacks built-in metrics, dashboards, GitOps, a registry, or fault injection. The Docker Compose column above reflects standalone Compose without Swarm mode.

---

## 24. Licensing

Reliaburger is licensed under **Apache 2.0**, the same license as Kubernetes, k3s, Docker Engine, and Podman.

**Why Apache 2.0:**

- **Maximum adoption.** No enterprise legal review friction, no "is this really open source?" debates. Every organisation (startup, enterprise, government, cloud provider) can use, modify, and distribute Reliaburger without restriction.
- **Explicit patent grant.** Unlike MIT, Apache 2.0 includes a patent license from contributors. For infrastructure software touching networking, scheduling, and distributed consensus, this protection matters.
- **Foundation-ready.** Apache 2.0 is the standard license for CNCF projects. If Reliaburger grows to the point where foundation governance makes sense, no relicensing is needed.

**Why this matters now:**

The infrastructure tooling landscape experienced a wave of relicensing between 2018 and 2024. MongoDB switched from AGPL to SSPL (2018). Elasticsearch moved from Apache 2.0 to SSPL + Elastic License (2021). HashiCorp switched all products, including Nomad (Reliaburger's closest competitor), from MPL 2.0 to BSL 1.1 (2023). Redis adopted a dual SSPL + RSALv2 license (2024). Each change triggered community backlash, forks (OpenTofu, OpenSearch, Valkey), and lasting trust damage. Developers who invested in these tools saw their contributions locked into value creation for a single company. IBM's acquisition of HashiCorp and subsequent reversion to MPL 2.0 (2025) didn't undo the damage. OpenTofu continues as an independent project, and the precedent of relicensing remains a risk factor for any single-vendor project.

Reliaburger's manifesto (point #10) states: *"Open source means open source. No bait-and-switch. No relicensing."* This isn't a policy that can be quietly changed. It's a published design principle on the same level as "one binary" or "batteries included." The Apache 2.0 license is a competitive advantage: teams evaluating orchestration tools can adopt Reliaburger knowing it will never become source-available, never require a commercial license for production use, and never restrict how they deploy it.

---

## 25. Questions & Answers

This section addresses the hard questions about Reliaburger's architecture: failure modes, scaling limits, and design trade-offs that a critical reader would raise.

### Q1: Port mapping with 500 apps per node. Won't you run out of ports?

The default ephemeral port range is 10000-60000, providing 50,000 available ports per node. At 500 apps per node, even with rolling deploys (where old and new versions briefly coexist), port exhaustion isn't a practical concern. Unlike proxy-based service discovery approaches, the Onion eBPF layer consumes zero OS ports. It operates entirely at the socket level in the kernel. The only ports consumed are those allocated to actual running containers.

### Q2: How do cluster-wide metrics queries work at 10,000 nodes?

They don't fan out to every node. Metrics follow a hierarchical aggregation model: each council member acts as an aggregator for a subset of cluster nodes, collecting pre-aggregated rollups. Cluster-wide dashboard queries hit the council aggregators (3-7 nodes), not every node. Single-app queries fan out only to the nodes running that specific app (typically 3-10 nodes). If you need full-resolution cluster-wide access, you can federate via the Prometheus remote-read API into your own Thanos or Cortex setup.

### Q3: Isn't gossip carrying too much data at scale?

No, because Mustard gossip carries only lightweight, fixed-size state: membership, failure detection, leader identity, and per-node resource summaries (a few hundred bytes per node). Variable-size runtime state (what's running where, image availability, job completions) flows through a separate hierarchical reporting tree. Nodes report to their assigned council member, and council members aggregate for the leader. This separation preserves SWIM's proven O(1) per-node overhead at 10,000 nodes.

### Q4: With encrypted-in-git secrets, how do developers encrypt without cluster access?

Reliaburger uses asymmetric encryption (age). The cluster's public key is committed to the git repository and can be distributed freely. Anyone with the public key can encrypt secrets offline, no cluster access needed. Only the cluster holds the private key for decryption. New clusters can import an existing cluster's private key via `relish init --import-key` to decrypt secrets from an existing git repo. Key rotation supports a graceful transition period where both old and new ciphertexts are accepted.

### Q5: When a new leader is elected, can it make wrong scheduling decisions from incomplete state?

No. The new leader enters a learning period where it accepts StateReports from nodes but doesn't make scheduling decisions or accept new deploys. The learning period ends when 95% of known nodes have reported or a timeout expires (default 15 seconds). During this period, the data plane is completely unaffected. Apps continue running and serving traffic. Only new deploys are briefly queued.

### Q6: Aren't exec (non-container) jobs a massive security risk?

They would be, without constraints. Process workloads are locked down by default: they require an explicit `admin` or `host-exec` Permission grant, run as a dedicated unprivileged user (`burger`), have a restrictive seccomp profile, a restricted mount namespace, and cgroup resource limits. Crucially, you must configure an explicit binary allowlist in `node.toml`. Process workloads are disabled on any node without an allowlist. This deny-by-default posture ensures that no host binary can execute without explicit operator approval. The security posture is closer to a cron job running under a locked-down service account than to unrestricted shell access.

### Q7: How does Pickle ensure image durability?

The push is synchronous by default. The client receives a success response only after the image has been replicated to N peer nodes (default N=2). A successful push guarantees that the image survives the failure of any single node. For typical application images (50-200MB), replication adds 2-5 seconds to the push. If you prefer faster pushes at the cost of durability, you can configure `[images] push_sync = false`, but the default prioritizes safety over speed.

### Q8: Can a single leader actually schedule 100M+ jobs per day while doing everything else?

The leader doesn't schedule individual jobs. For batch workloads, Patty allocates job batches to nodes: "Node 7, here are your next 200 jobs." Nodes execute their assigned jobs and report completions asynchronously via the hierarchical reporting tree. The Raft log records only batch-level decisions, not individual job lifecycle events. The leader's hot path focuses on Apps (which change infrequently) and batch-level allocation decisions. The Patty scheduler runs on a dedicated async task with its own CPU budget, isolated from API serving, Brioche UI, and metrics queries.

### Q9: Local-only volumes with no distributed storage. How do teams not lose data?

Three layers of mitigation. First, Reliaburger recommends managed databases for critical data (the pattern most production teams already follow). Second, apps that manage their own replication (CockroachDB, Cassandra) work naturally with local volumes. Third, Reliaburger includes built-in volume snapshots: `relish volume snapshot` creates an instant copy-on-write snapshot on the local filesystem, and a configurable upload job (a regular container with its own credentials) ships the snapshot to S3/GCS on a schedule (see Section 5.5 for details). Local snapshots provide fast rollback from application-level corruption; the upload job provides off-node durability. This covers the common case of "don't lose my Redis cache" without the complexity of a distributed storage layer.

### Q10: Will TOML configuration get unwieldy at 50+ apps?

Reliaburger supports directory-mode configuration where each app lives in its own file, merged at deploy time. A `_defaults.toml` file provides shared values (common env vars, memory limits, deploy strategy) inherited by all apps unless overridden. `relish fmt` formats files; `relish lint` validates configuration and catches common errors. The GitOps engine (Lettuce) works with directory trees natively.

### Q11: Doesn't the eBPF approach require a modern kernel? What about older systems?

Yes, Onion requires Linux kernel 5.7 or later. This covers every actively-maintained Linux distribution as of 2026: Ubuntu 22.04+ (kernel 5.15), Debian 12+ (kernel 6.1), RHEL 9+ (kernel 5.14), Amazon Linux 2023 (kernel 6.1). Older distributions like RHEL 8 (kernel 4.18) remain under extended life support until 2029 but ship a kernel that predates the eBPF features Onion requires. The kernel requirement is a hard line. Bun refuses to start on older kernels with a clear error. This is a deliberate trade-off: eBPF socket interception eliminates the need for a DNS server and proxy process entirely, which is worth more than supporting legacy kernel versions.

### Q12: The design goals mention GPU scheduling. Does it support fractional GPUs?

Not in v1. Reliaburger v1 supports whole-device GPU allocation only (`gpu = 1`, `gpu = 2`). Fractional GPU sharing (MIG partitions on NVIDIA A100/H100, or time-slicing) requires specific hardware support and driver configuration that varies significantly across GPU generations. Rather than ship a half-baked abstraction, fractional GPU support is deferred to v2, where MIG partitions can be exposed as distinct schedulable devices.

### Q13: How does multi-tenancy work? Can one team starve the cluster?

Namespaces provide resource quotas (CPU, memory, GPU, app count, and replica count budgets) that the Patty scheduler enforces at deploy time. When a deploy would exceed a namespace's quota, Patty rejects it with a clear error. The default namespace has no quotas unless you configure them, which is appropriate for single-team clusters. Multi-team clusters should configure per-team namespaces with quotas from day one.

### Q14: What's the minimum cluster size?

Reliaburger runs on any number of nodes. A single-node cluster works for development and small production workloads: the node is simultaneously the leader, the only council member, and the only worker. A 2-node cluster provides scheduling redundancy but no quorum tolerance. If either node fails, the surviving node continues operating in single-leader mode but can't elect a new council. A 3-node cluster is the minimum for high availability, where the council can tolerate the loss of any one node while maintaining quorum. The stated "2-200 nodes" target audience reflects that Reliaburger is designed for this range, not that all configurations within it are equally resilient.

### Q15: What is the base resource overhead per node?

An idle Bun node (zero application workloads) consumes approximately 80-120 MB of resident memory and negligible CPU. This covers the Bun agent, Mustard gossip, eBPF maps (Onion), Pickle registry, Mayo metrics store, Ketchup log collector, and the Wrapper ingress proxy. On council members, Raft consensus adds approximately 20-40 MB depending on the size of the desired-state log. The Brioche web UI is compiled into every node and adds approximately 10 MB when actively serving. All components share a single process (the Bun binary), avoiding the per-process overhead of separate daemons. A 2-CPU, 4 GB node has ample headroom for application workloads after the base overhead.

### Q16: How does Franchise compare to Kubernetes multi-cluster tools?

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
