<p align="center">
  <img src="assets/images/reliaburger_logo_v1.jpg" alt="Reliaburger" width="400">
</p>

# Reliaburger

Container orchestration in one binary.

Reliaburger is a container orchestrator written in Rust. Each node runs one
agent, `bun`; you drive the cluster with one CLI, `relish`. Scheduling,
clustering, service discovery, ingress, PKI, secrets, an image registry,
metrics, logs, GitOps and fault injection are compiled into that agent. There's
no control plane to assemble and no add-ons to install.

It runs your existing Kubernetes manifests. The tour below takes a laptop to
a three-node cluster, runs a real Kubernetes app on it, breaks it and watches
it heal. Five minutes is the target.

## Five minutes, zero to cluster

The one-line installer arrives with 0.1.0. Until the signed release is
published, it stops with a release-not-published message. You'll need macOS, or
Linux with QEMU and KVM, plus about 8 GiB of free memory and 15 GiB of disk.

```sh
# Install relish and build a three-node cluster in Linux VMs
curl -fsSL https://reliaburger.com/install.sh | sh

# Run a real Kubernetes app: podinfo's frontend, backend and Redis
relish apply -f https://reliaburger.com/demo/podinfo.yaml
relish status                        # three frontends, spread across the nodes
open http://podinfo.localhost:18080  # through the built-in ingress

# See every hop from frontend to Redis: DNS, VIP, eBPF map, firewall, TCP
relish path frontend --to redis

# podinfo's own Prometheus metrics, scraped from its pod annotations
relish metrics frontend

# Make Redis slow, but only for the frontends, for two minutes
relish fault delay redis 300ms --from frontend --duration 2m --acknowledge
relish path frontend --to redis --count 3
relish dashboard                     # live charts in the browser

# Break things and watch the cluster recover
relish fault kill frontend --count 1 --acknowledge
relish local stop node-3             # lose a whole machine
relish status                        # three frontends again, on two nodes
relish wtf                           # what's wrong and what to do about it

# Clean up
relish local destroy --yes
relish uninstall
```

The [five-minute tour](docs/manual/08_five-minute-tour.md) explains each step,
and the [laptop quickstart](docs/quickstart.md) covers prerequisites, retries
and single-node setups.

<!-- relish-commands:start -->
<details>
<summary><strong>Everything relish can do</strong></summary>

Run `relish` with no command for the terminal UI. `relish help COMMAND` (or `--help` anywhere) lists every flag, and the [reference](docs/manual/13_reference.md) covers global flags, environment variables and exit codes. This list is generated from relish's own command definitions; run `make readme-commands` after changing them.

**Set up and run a cluster** ([getting started](docs/manual/00_getting-started.md), [cluster basics](docs/manual/02_cluster-basics.md))

- `relish setup`: Guided setup: detect or install bun, then write a starter config
- `relish local <status|start|stop|destroy> [NODE]`: Manage a laptop cluster created by setup --quickstart
- `relish init [DIR]`: Initialise a new cluster (generates CAs, age keypair, node identity)
- `relish join --node-id <NODE_ID> <ADDR>`: Join an existing cluster
- `relish join-token`: Manage short-lived node-enrolment tokens
  - `relish join-token create --node-id <NODE_ID>`: Create a single-use token for enrolling one node
- `relish nodes`: List cluster nodes and their gossip state
- `relish council`: Show council (Raft) composition and status, or recover from full loss
  - `relish council recover --data-dir <DATA_DIR>`: Recover a cluster whose entire council was lost
- `relish decommission-node --workloads-stopped --reason <REASON> <NODE_ID>`: Permanently retire a stopped or fenced node; return requires fresh enrolment
- `relish uninstall`: Remove the CLI, its PATH link, the managed Lima tools and the image cache

**Deploy and manage apps** ([deploy an app](docs/manual/01_deploy-an-app.md))

- `relish apply [PATH_OR_URL]`: Apply a Reliaburger TOML or Kubernetes YAML manifest
- `relish status`: Show cluster and app status
- `relish inspect <NAME>`: Show detailed info about an app, node, or job
- `relish exec <APP> [COMMAND]...`: Execute a command inside a running container
- `relish deploy <PATH>`: Trigger a rolling deploy for an app
- `relish cancel-deploy <OPERATION_ID>`: Cancel a node-local deploy and wait for its current work to finish
- `relish history <APP>`: Show deploy history for an app
- `relish rollback <APP>`: Rollback an app to the previous version
- `relish stop <APP>`: Scale an app to zero, keeping its configuration; `relish apply` starts it again
- `relish delete <APP>`: Remove an app from the cluster and stop all its instances
- `relish batch <PATH>`: Submit a batch of jobs for high-throughput scheduling
- `relish batch-status <ID>`: Show the progress of a submitted batch

**Work with config files** ([deploy an app](docs/manual/01_deploy-an-app.md), [coming from Kubernetes](docs/manual/09_kubernetes.md))

- `relish lint <PATH>`: Validate a config file without deploying
- `relish fmt <PATH>`: Format a TOML config file with canonical ordering
- `relish compile <PATH>`: Compile configs into a single resolved output
- `relish diff <PATH_A> [PATH_B]`: Show structural diff between two configs
- `relish import --file <FILES>...`: Convert Kubernetes YAML manifests to Reliaburger TOML
- `relish export --file <FILE>`: Export Reliaburger TOML to Kubernetes YAML manifests

**Networking** ([networking and ingress](docs/manual/03_networking.md))

- `relish resolve <NAME>`: Resolve a service name to its VIP and backends
- `relish routes`: Show ingress routing table

**Watch what's running** ([observability](docs/manual/04_observability.md))

- `relish tui`: Launch the interactive terminal UI
- `relish dashboard`: Open a read-only web dashboard through the current authenticated context
- `relish top`: Show every workload on every node, with its latest CPU and memory
- `relish metrics <APP>`: Show an app's own Prometheus metrics, scraped by the nodes running it
- `relish logs <NAME>`: Stream logs from an app or job
- `relish logs-export --dest <DEST>`: Export Parquet log files to a destination directory
- `relish logs-search <SOURCE> <SQL>`: Search exported Parquet log archives with SQL

**Diagnose and test** ([diagnostics](docs/manual/07_diagnostics.md))

- `relish wtf`: Diagnose cluster health and correlate likely causes
- `relish path --to <TO> <SOURCE>`: Walk the network path from a workload to a destination, hop by hop
- `relish test`: Run the built-in integration test suite against the cluster
- `relish bench`: Run reproducible performance benchmarks against the real data plane

**Break things on purpose** ([chaos](docs/manual/05_chaos.md))

- `relish fault`: Inject faults for chaos testing (Smoker)
  - `relish fault delay <TARGET> <DELAY>`: Add latency to connections to a service
  - `relish fault drop <TARGET> <PERCENTAGE>`: Fail a percentage of connections
  - `relish fault dns <TARGET> <FAULT_TYPE>`: Return NXDOMAIN for DNS resolution
  - `relish fault partition <TARGET>`: Block traffic between services
  - `relish fault bandwidth <TARGET> <LIMIT>`: Throttle bandwidth to a service
  - `relish fault cpu <TARGET> <PERCENTAGE>`: Consume CPU in a service's cgroup
  - `relish fault memory <TARGET> <VALUE>`: Push memory usage toward a service's limit
  - `relish fault disk-io <TARGET> <LIMIT>`: Throttle disk I/O for a service
  - `relish fault kill <TARGET>`: Kill instances of a service (SIGKILL)
  - `relish fault pause <TARGET>`: Freeze instances of a service (SIGSTOP)
  - `relish fault resume <TARGET>`: Resume (unfreeze) previously paused instances of a service
  - `relish fault node-drain <TARGET>`: Simulate graceful node departure
  - `relish fault node-kill <TARGET>`: Simulate abrupt node failure
  - `relish fault node-pressure <TARGET>`: Consume bounded CPU and memory capacity on one node
  - `relish fault list`: List all active faults
  - `relish fault clear [TARGET]`: Clear faults — all, by numeric id, or by service name
  - `relish fault scenario <PATH>`: Run a scripted chaos scenario from a TOML file

**Security and access** ([security and access](docs/manual/10_security.md))

- `relish token`: Manage API tokens
  - `relish token create --name <NAME>`: Create a new API token
  - `relish token list`: List all API tokens
  - `relish token revoke <NAME>`: Revoke an API token by name
- `relish secret`: Manage secrets (encrypt values for use in app configs)
  - `relish secret pubkey [DIR]`: Print the cluster's age public key (for `relish secret encrypt`)
  - `relish secret encrypt --pubkey <PUBKEY> <VALUE>`: Encrypt a plaintext value for use in app config ENC[AGE:...] fields
  - `relish secret rotate`: Rotate the secret encryption key (start or finalise)
- `relish sign --key <KEY> <IMAGE>`: Sign a Pickle-hosted image with your own key so `require_signatures` admits it
  - `relish sign keygen --out <OUT>`: Generate an image signing key and print the public key line for `[images.trust_policy] keys`

**Images and volumes** ([images and volumes](docs/manual/11_images-and-volumes.md))

- `relish images`: List images in the local Pickle registry
- `relish build <PATH>`: Build an OCI image and push to Pickle
- `relish snapshot`: Manage volume snapshots (Btrfs-backed volumes only)
  - `relish snapshot create <APP>`: Snapshot an app's managed volumes
  - `relish snapshot list <APP>`: List an app's snapshots, newest first
  - `relish snapshot restore <APP> <NAME>`: Restore a snapshot over its live volume (stop the app first)
  - `relish snapshot delete <APP> <NAME>`: Delete a snapshot

**Upgrades** ([operations](docs/manual/12_operations.md))

- `relish upgrade`: Roll a new bun binary across the cluster, or back
  - `relish upgrade check`: Check for available updates
  - `relish upgrade start [VERSION]`: Start a rolling upgrade (network: pass a version; air-gapped: pass --binary)
  - `relish upgrade plan <VERSION>`: Preview the rolling order and estimated duration
  - `relish upgrade status`: Show upgrade progress
  - `relish upgrade rollback [VERSION]`: Roll back to a previous version (cluster: version required)
  - `relish upgrade resume`: Resume a paused upgrade
  - `relish upgrade abort`: End a paused upgrade in which no node has moved. When some nodes already swapped, use `rollback <version>` instead

**Learn** ([under the hood](docs/manual/06_under-the-hood.md))

- `relish manual [CHAPTER]`: Read the built-in manual (searchable TUI; --web for the browser)
  - `relish manual examples`: Write the embedded example configs into a directory
- `relish source [QUERY]`: Browse and fuzzy-search the source this binary was built from

**Contributor tools** ([dev cluster](docs/README.md#dev-cluster))

- `relish dev`: Manage a local dev cluster (Lima VMs)
  - `relish dev create [NAME]`: Create a new dev cluster
  - `relish dev status [NAME]`: Show dev cluster status
  - `relish dev shell <NODE>`: Open a shell on a node
  - `relish dev stop [NAME]`: Stop a dev cluster (VMs stay on disk)
  - `relish dev start [NAME]`: Start a stopped dev cluster
  - `relish dev destroy [NAME]`: Destroy a dev cluster (delete all VMs)
  - `relish dev test [FILTER]`: Run tests in a Linux VM (all Linux-gated tests enabled)
  - `relish dev disk`: Show disk usage in the test VM
  - `relish dev clean`: Clean cargo build artefacts in the test VM
  - `relish dev keygen --out <OUT>`: Generate an Ed25519 release signing keypair
  - `relish dev sign-binary --key <KEY> <BINARY>`: Sign a binary, producing a detached .sig envelope
  - `relish dev countersign-binary --external-key <EXTERNAL_KEY> <BINARY>`: Add your external signature to a release binary's .sig envelope, keeping the release signature as it is (no release key needed)

</details>
<!-- relish-commands:end -->

## What's in the binary

**Runs Kubernetes YAML.** `relish apply -f` takes Deployments, StatefulSets,
DaemonSets, Services, Ingresses, ConfigMaps, Secrets, Jobs, CronJobs, HPAs and
Namespaces. It converts them on the way in and tells you what it had to
change. `relish import` writes the equivalent TOML if you'd rather keep that.

**Clustering without etcd.** Nodes find each other and detect failure with SWIM
gossip. A small Raft council, embedded in the agent, holds cluster state. Lose
a node and the scheduler moves its workloads to the survivors.

**Service discovery in the kernel.** Every app gets a stable virtual IP and,
with DNS enabled, a `.internal` name. An eBPF connect hook sends each
connection straight to a healthy backend, and per-app firewall rules decide
who may connect at all.

**Ingress with TLS.** Host-based HTTP and HTTPS routing, with certificates signed
by the cluster's own ingress CA or files you provide. WebSockets and streaming
pass through, and connections drain before an instance stops.

**Security on by default.** A generated cluster requires mTLS between nodes.
Nodes join with single-use tokens and certificate signing requests.
Certificates renew themselves. Secrets live in your config encrypted to the
cluster's public key. Workloads get SPIFFE certificates, API tokens carry
roles and namespace scopes, and the registry can refuse unsigned images.

**A registry on every node.** Pickle is an OCI registry built into the cluster.
Push once and nodes pull layers from each other. It also caches upstream
registries, and `relish build` builds images from your config straight into it.

**Metrics, logs and alerts, nothing to install.** Each node scrapes its own
workloads, including anything with `prometheus.io` annotations. Alert rules
evaluate in the agent and call webhooks, optionally HMAC-signed. Logs are
captured on each node, streamed cluster-wide with `relish logs -f`, and
exported as Parquet to disk or object storage, where SQL can query them.

**Chaos as a first-class command.** `relish fault` injects latency, dropped
connections, DNS failures, partitions, CPU, memory and disk-I/O pressure,
killed or frozen instances, and drained or failed nodes. Every fault has a
duration and cleans up after itself. Injection needs `--acknowledge`, a
Deployer credential and an operator opt-in on each node, and it's refused when
it would take out every replica or put the council's quorum at risk.
`relish test --chaos` runs scripted recovery scenarios.

**Deploys and GitOps.** Health-gated rolling deploys with automatic rollback,
blue-green switches, autoscaling on metrics, jobs, cron and batch. Point the
cluster at a Git repository and the leader keeps it in sync, verifying commit
signatures if you ask it to.

**Self-upgrade.** `relish upgrade start` rolls a new `bun` across the cluster:
workers first, then council members one at a time, leader last. The new binary
adopts running workloads without restarting them, and a crash-looping upgrade
reverts itself. Network upgrades need two Ed25519 signatures: the release's
and your own.

**Diagnostics built for incidents.** `relish wtf` correlates cluster health into
one screen of problems and next steps. `relish path` walks the network path
between two apps and labels each step observed, inferred or unavailable.
`relish test` runs a live-cluster test catalogue, and `relish bench` measures
the data plane.

**Tested like it has to survive a bad day.** Beyond thousands of unit, property
and snapshot tests, there are crash-recovery suites that kill the agent at every
awkward moment, multi-node failover and council-loss tests, power-cut fixtures
that pull the plug on a VM mid-write, a live-cluster test runner you can point
at your own cluster (`relish test`), and a soak that installs the signed
release the way you would and then breaks it for hours. CI never retries a
failure. See [how Reliaburger is tested](docs/testing.md).

**A terminal UI and a web dashboard.** Run `relish` with no arguments for the
TUI; `relish dashboard` opens the web UI through your authenticated CLI session.

**The manual ships in the binary.** `relish manual` is a searchable reader with
runnable examples, and `relish source` fuzzy-searches the exact source tree the
binary was built from. Neither needs a network.

## What you don't install

| On Kubernetes you'd add | In Reliaburger |
|---|---|
| etcd | Raft council inside `bun` |
| kube-proxy, CoreDNS | eBPF service map and `.internal` DNS |
| An ingress controller | Wrapper ingress |
| Sealed Secrets | Secrets encrypted to the cluster key |
| Harbor or another in-cluster registry | Pickle |
| Prometheus, Alertmanager | Built-in scraping, alert rules and webhooks |
| A log shipper and store | Built-in capture, streaming and Parquet export |
| Argo CD or Flux | Built-in GitOps |
| Chaos Mesh or Litmus | `relish fault` |
| Grafana (for the basics) | TUI and web dashboard |

Config is TOML. The [whitepaper](docs/whitepaper.md) explains the architecture
and its trade-offs; the [design docs](docs/design/) cover each subsystem.

## Limits in 0.1.0

- **Clusters need rootful runc on Linux with eBPF.** macOS runs containers in
  managed Linux VMs; native macOS `bun` runs plain processes only.
- **Rootless runc is standalone only.** It gets host-port forwarding, but no eBPF
  policy, workload DNS or resource limits.
- **Ingress certificates don't come from ACME.** Use the cluster CA or supply
  your own files.
- **No PromQL.** Metrics are read through `relish metrics`, the dashboards and
  the API.
- **`fault bandwidth` is refused**, and `fault delay` needs runc containers.
- **Kubernetes import covers the kinds listed above.** Anything else is reported,
  not applied.
- **Clusters start fresh.** Development-build state isn't migrated, and rolling
  upgrades need matching protocol and state formats (see the
  [compatibility policy](docs/releasing.md#cluster-compatibility)).
- **Cron doesn't catch up.** It skips firings missed during a crash, and a job
  whose outcome is unknown waits for `relish apply <file> --rerun-jobs`.

The [documentation](docs/README.md#010-scope-and-limits) has the full list.
[progress.md](docs/progress.md) tracks what's done and what's left before the
release.

## Run it from source

You'll need Rust 1.97 or later. This runs a process workload with no container
runtime, on macOS or Linux:

```sh
git clone https://github.com/reliaburger/reliaburger
cd reliaburger
cargo build --locked --bins
target/debug/bun --runtime process

# In another terminal
target/debug/relish apply examples/phase-1/proc-first-run.toml
target/debug/relish status
target/debug/relish            # terminal UI
open http://127.0.0.1:9117/    # web dashboard
```

For real containers and secure multi-node clusters, read the
[documentation](docs/README.md) or run `relish manual`. Every CLI command is
listed under *Everything relish can do* above.

## The book

This repository is also a book. [*Building Reliaburger*](docs/book/00-preface.md)
walks through how we designed and built each subsystem, teaching Rust and
distributed systems to programmers who know C, Python or Go. The chapters are
in [docs/book/](docs/book/).

## Contributing

Bug reports and pull requests are welcome. Read the
[contributing guide](CONTRIBUTING.md) first: changes come with tests and a
matching update to the book.

## Licence

[Apache 2.0](LICENSE)
