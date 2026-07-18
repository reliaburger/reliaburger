# Reliaburger Documentation

User guide for building and running Reliaburger. For the full architectural vision, see the [whitepaper](whitepaper.md). For current implementation status, see [progress.md](progress.md).

## Prerequisites

### Rust toolchain

Reliaburger requires Rust 1.85+ (2024 edition). Install via [rustup](https://rustup.rs/):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Follow the prompts (defaults are fine), then restart your shell or run:

```sh
source "$HOME/.cargo/env"
```

Verify:

```sh
rustc --version   # needs 1.85+
cargo --version
```

If you already have Rust installed, make sure it's up to date:

```sh
rustup update
```

### Platform build tools

**Linux** (Debian/Ubuntu):

```sh
sudo apt install build-essential pkg-config
```

**Linux** (Fedora):

```sh
sudo dnf groupinstall "Development Tools"
```

**macOS**:

```sh
xcode-select --install
```

## Container runtimes (optional)

Reliaburger supports three container runtimes. The agent auto-detects which one to use at startup. **ProcessGrill** (plain OS processes) is the built-in fallback that works everywhere without extra software — you don't need to install anything else to get started.

### runc (Linux)

[runc](https://github.com/opencontainers/runc) is the reference OCI container runtime. Docker and containerd use it under the hood.

**Install on Ubuntu/Debian:**

```sh
sudo apt install runc
```

**Install from GitHub releases:**

Download the latest binary from [github.com/opencontainers/runc/releases](https://github.com/opencontainers/runc/releases) and place it in your `PATH`.

Notes:
- Rootless runc's namespace/spec path supports read-only OCI roots and path-based test bundles. Declarative app specs currently request writable roots, so normal image workloads must use root mode until Reliaburger owns a safe unprivileged snapshotter; they never fall back to a shared writable image tree.
- Rootless stores bundles/images in `~/.local/share/reliaburger/`; root mode uses `/var/lib/reliaburger/`
- OCI images are pulled from Docker Hub automatically when the spec's `image` field is set (e.g. `alpine:latest`)
- Root-mode writable images use one private OverlayFS upper per workload over the shared content-addressed image generation. A restart or Bun adoption reuses that workload's upper; exit and kill unmount it.
- To run provisioned Linux runtime tests: `sudo make test-linux`
- OCI protocol tests use a local digest-pinned registry fixture; they need no public registry

### Apple Container (macOS)

[Apple Container](https://github.com/apple/container) runs Linux containers in lightweight VMs on Apple Silicon. It's OCI-compatible and pulls standard images from Docker Hub.

**Requirements:**
- macOS 15 (Sequoia) or later
- Apple Silicon (M1/M2/M3/M4)

**Install via Homebrew:**

```sh
brew install container
```

Or build from source — see the [project README](https://github.com/apple/container).

**First-time setup:**

```sh
container system start
```

Notes:
- To run Apple Container-specific tests: `make test-apple`

### ProcessGrill (built-in fallback)

Works on any platform. Spawns child processes instead of real containers — no namespaces, no cgroups, no rootfs isolation. Useful for development, testing, and platforms without a container runtime installed.

No installation needed. This is what you get by default.

## Building

The [Makefile](../Makefile) provides all build targets:

```sh
make build       # compile (debug)
make release     # compile (optimised)
make test        # portable nextest suite
make test-doc    # Rust documentation examples
make test-linux  # provisioned Linux runtime/kernel suite
make lint        # clippy with warnings as errors
make audit       # RustSec advisory and dependency-maintenance gate
make fmt         # format with rustfmt
make ci          # portable format, lint and test checks
make clean       # remove build artefacts
```

Or use cargo directly:

```sh
cargo build
cargo test
```

## Testing and benchmarking

### Test suites

```sh
make test                  # portable nextest suite
make test-no-default       # portable suite without default features
make test-doc              # doctests (nextest does not run them)
make test-slow             # genuine wall-clock acceptance tests
sudo make test-linux       # runc, netns, eBPF, Btrfs, Buildah and root-only tests
make test-cluster          # failover, healing, recovery, placement and chaos
make test-upgrade-node     # real single-node binary replacement
make test-upgrade-cluster  # real rolling cluster replacement
make coverage              # combined HTML and LCOV coverage
make audit                 # fail on new RustSec dependency findings
```

`make test` runs only tests that can execute truthfully on an ordinary developer machine.
Provisioned tests use `#[ignore = "requires …"]`; their named target enables the prerequisite,
selects ignored tests only and fails if its filter finds no tests. Target-specific code uses
`#[cfg(...)]`, so Linux-only tests are reported separately rather than pretending to pass on
macOS. Retries are disabled.

Apple Container remains a manual Apple-silicon check because hosted macOS runners cannot
provide the nested virtualisation it needs. See the [test harness design](design/test-harness.md)
for the audit, exact suite contracts and CI mapping.

### Benchmarks

Gossip protocol benchmarks use [criterion](https://docs.rs/criterion) for statistical analysis with regression detection.

```sh
make bench         # reproducible transport and 5-250 node measurements
make bench-large   # reproducible 500 and 1,000 node measurements
make bench-10k     # deterministic 10,000-member per-node scale acceptance
```

The fast benchmarks (`cargo bench --bench gossip`) are the ones to run regularly — they catch performance regressions in the gossip protocol. Results are stored in `target/criterion/` and criterion reports whether performance changed between runs.

The large benchmarks (`cargo bench --bench gossip_large`) test the same convergence logic
at 500 and 1000 nodes. They maintain a seeded, incrementally sorted peer index so the
benchmark measures protocol work rather than repeatedly allocating and sorting membership
snapshots. Setup and convergence are reported separately, and non-convergence fails rather
than returning a sentinel duration.

The 10k target checks the per-node production invariant: one real Mustard node ingests a
10,000-member table through bounded gossip messages, can select a probe target and exposes
every learned update through fixed-size dissemination batches. Full 10,000-node all-to-all
simulation would allocate 100 million membership records on one runner. That measures one
machine pretending to be a datacentre, and did not finish inside its 90-minute budget.
CI uploads Criterion data; it does not enforce regression percentages until measurements
are stable on consistent hardware.

## Running

### Portable first run

This path needs no container runtime. Build all binaries first because the
workload itself is the `testapp` binary:

```sh
cargo build --bins
target/debug/bun --runtime process
```

Leave Bun running. In a second terminal:

```sh
target/debug/relish apply examples/phase-1/proc-first-run.toml
target/debug/relish status
target/debug/relish top
```

You should see the `hello` workload in `Running` state. Open
<http://127.0.0.1:9117/> for the dashboard. ProcessGrill supervises a real OS
process, but it doesn't isolate it; use runc or Apple Container for container
workloads.

### Node agent (bun)

The bun agent manages container lifecycle, health checks, and the local HTTP API.

```sh
cargo run --bin bun
```

Options:

| Flag | Default | Description |
|------|---------|-------------|
| `--config <path>` | (none) | Path to node config TOML file |
| `--listen <addr>` | `127.0.0.1:9117` | API listen address |
| `--runtime <name>` | `auto` | Runtime: `auto`, `process`, `runc`, `apple` |

Examples:

```sh
# Start with auto-detected runtime (default)
cargo run --bin bun

# Force process runtime (no container tools needed)
cargo run --bin bun -- --runtime process

# Use a custom loopback address
cargo run --bin bun -- --listen 127.0.0.1:9217

# Load node configuration from file
cargo run --bin bun -- --config node.toml
```

The agent prints which runtime it selected on startup:

```
bun: reliaburger node agent v0.1.0
bun: auto-detected runtime: process
bun: API server listening on 127.0.0.1:9117
```

Stop with `Ctrl-C` — the agent shuts down gracefully.

An empty token store is a local bootstrap window: administrative routes are
open so the first cluster token can be created. Bun therefore accepts only an
IP-literal loopback `--listen` address in that state. Wildcard, routable and
hostname listeners are rejected before subsystem startup. Initialise an
authenticated cluster and create the first admin token before exposing the API
on a non-loopback address.

### Secure cluster initialisation

`relish init` generates the cluster PKI, the first node's identity and a
`reliaburger.toml` that requires mTLS. Start the cluster with that generated
configuration; no security switch needs hand-editing:

```sh
target/debug/relish init cluster --cluster-name prod --node-id node-01
sudo target/debug/bun --cluster --runtime runc --config cluster/reliaburger.toml
```

The output directory is created if it doesn't exist. Leave Bun running and,
from another terminal, mint the first administrator token over the generated
cluster CA before doing anything else:

```sh
export RELIABURGER_TOKEN="$(target/debug/relish \
  --ca-cert cluster/identity/root-ca.crt \
  token create --name first-admin --role admin)"

target/debug/relish --ca-cert cluster/identity/root-ca.crt apply cluster/app.toml
target/debug/relish --ca-cert cluster/identity/root-ca.crt status
```

This is a one-node Raft cluster: clustered code paths are live, but it cannot
survive a node failure. The API listens at `https://127.0.0.1:9117`. On Apple
Silicon use `--runtime apple` and run Bun as your ordinary user; don't put
Apple Container behind `sudo`.

To grow this into a resilient three-voter council, mint one token per new
node. Join tokens are deliberately separate from API bearer tokens:

```sh
JOIN_NODE_02="$(target/debug/relish \
  --ca-cert cluster/identity/root-ca.crt \
  join-token create --ttl 15m)"
JOIN_NODE_03="$(target/debug/relish \
  --ca-cert cluster/identity/root-ca.crt \
  join-token create --ttl 15m)"

# Run this on node-02 after provisioning the binary, cluster master key and
# a node-specific config. Repeat with JOIN_NODE_03 and node-03.
target/debug/relish join --token "$JOIN_NODE_02" --node-id node-02 \
  --identity-dir identity \
  --ca-fingerprint sha256:<ROOT_CA_FINGERPRINT> \
  https://<CURRENT_LEADER>:9117
```

Set each joiner's `[cluster].join` to an existing member's gossip address
(port 9443 by default), give it unique storage paths and ports, and then start
`bun --cluster` with that config. `relish join` enrols identity only; it
doesn't provision or start the node. Token creation accepts `--ttl` from `1s`
to `1h` (default `15m`), requires an Admin bearer after bootstrap, commits only
the token hash and expiry to Raft, and prints the plaintext once. During an
election, retry against the current leader: a follower returns an error and
does not commit or disclose a usable token.

The generated security section contains the material paths and the secure
mode:

```toml
[security]
master_key_path = "cluster/prod-master.key"
bootstrap_path = "cluster/prod-security-bootstrap.json"
identity_dir = "cluster/identity"
require_mtls = true
```

With this mode, Raft and reporting require mutually authenticated node
certificates. Peer API calls also present their node certificate and check the
live revocation list; Relish and browsers may omit a client certificate and
authenticate with a bearer token or session cookie over TLS.

If you specifically need plaintext transports for an isolated local test,
make that exception explicit:

```sh
cargo run --bin relish -- init cluster --development-plaintext
```

That command writes `require_mtls = false`, embeds a warning in the generated
file and prints a warning. Bun repeats the warning whenever that config starts
in cluster mode. `relish dev create` does the same for its deliberately local
Lima-VM configuration. Do not reuse either config on a shared network.

### CLI (relish)

Relish is the command-line interface for interacting with a running bun agent.
Run it without a subcommand, or use `relish tui`, to open the interactive
terminal dashboard. The TUI needs a terminal of at least 80×24 cells.

```sh
cargo run --bin relish              # interactive TUI
cargo run --bin relish -- tui       # the same, explicitly
cargo run --bin relish -- <command>
```

Commands:

| Command | Description |
|---------|-------------|
| `tui` (or no command) | Interactive dashboard for apps, nodes, jobs, events, logs, metrics and routes |
| `setup` | Guided install: detect/install bun (signature-verified) and write a starter config (`--yes` for defaults) |
| `manual` | Read the built-in manual in a searchable terminal reader |
| `manual --web` | Serve the manual as one HTML page and open the browser |
| `manual examples` | Write the embedded example configs into the current directory |
| `apply <path>` | Deploy workloads from a TOML config file |
| `status` | List all running workloads |
| `logs <name>` | Show captured stdout/stderr for an app |
| `logs <name> --tail N` | Show only the last N lines |
| `logs <name> --follow` / `-f` | Stream new log lines as they appear |
| `inspect <name>` | Detailed info about an app |
| `exec <app> <cmd...>` | Execute a command inside a running instance |
| `stop <app>` | Stop all instances of an app |
| `init [dir]` | Generate PKI and an mTLS-required starter config (`--development-plaintext` is an explicit local-only exception) |
| `nodes` | List cluster nodes and their gossip state |
| `council` | Show council (Raft) composition and status |
| `join --token <token> --node-id <id> <api-addr>` | Enrol a node identity with an existing cluster member |
| `join-token create --ttl 15m` | Mint one Admin-authorised, single-use node-enrolment token |
| `chaos <action>` | Run chaos testing scenarios (council-partition, worker-isolation, status, heal) |
| `resolve <name>` | Resolve a service name to its VIP and backends |
| `routes` | Show ingress routing table |
| `top` | Show live resource usage (CPU, memory) for all apps |
| `deploy <path>` | Trigger a rolling deploy for an app |
| `history <app>` | Show deploy history for an app |
| `rollback <app>` | Rollback an app to the previous version |
| `lint <path>` | Validate a config file without deploying |
| `images` | List images in the local Pickle registry |
| `build <path>` | Build OCI images from `[build.*]` sections and push to Pickle (async: submits, then polls; `--timeout` bounds the wait) |
| `batch <path>` | Submit `[job.*]` sections as a high-throughput batch across the cluster |
| `batch-status <id>` | Show a submitted batch's progress (`--wait --timeout` polls to a terminal state) |
| `sign <image>` | Sign an image in the Pickle registry and attach the signature |
| `snapshot create <app>` | Snapshot an app's managed volumes (Btrfs-backed; `--volume` for one, `--name` to label) |
| `snapshot list <app>` | List an app's snapshots, newest first |
| `snapshot restore <app> <name>` | Restore a snapshot over the live volume (stop the app first) |
| `snapshot delete <app> <name>` | Delete a snapshot |
| `secret pubkey [dir]` | Print the cluster's age public key |
| `secret encrypt --pubkey <key> <value>` | Encrypt a value for use in app configs |
| `fault delay <target> <delay>` | Add latency to connections to a service |
| `fault drop <target> <pct>` | Fail a percentage of connections (ECONNREFUSED) |
| `fault dns <target> nxdomain` | Return NXDOMAIN for DNS resolution |
| `fault partition <target>` | Block traffic between services |
| `fault kill <target>` | Kill instances of a service (SIGKILL) |
| `fault pause <target>` | Freeze instances of a service (SIGSTOP) |
| `fault node-drain <node>` | Simulate graceful node departure |
| `fault node-kill <node>` | Simulate abrupt node failure |
| `fault list` | List all active faults |
| `fault clear [id]` | Clear all faults (or a specific one by ID) |
| `fault scenario <file>` | Run a scripted chaos scenario from a TOML file |
| `dev create` | Create a local dev cluster (Lima VMs with rootless runc) |
| `dev status` | Show dev cluster status |
| `dev shell <node>` | Open a shell on a dev cluster node |
| `dev stop` | Stop a dev cluster (VMs stay on disk) |
| `dev start` | Start a stopped dev cluster |
| `dev destroy` | Destroy a dev cluster (delete all VMs) |
| `dev keygen --out <dir>` | Generate an Ed25519 release signing keypair |
| `dev sign-binary --key <key> <binary>` | Sign a binary, producing a detached `.sig` envelope |
| `upgrade check` | Check the release metadata for available updates |
| `upgrade start <version>` | Start a rolling binary upgrade (network) |
| `upgrade start --binary <path>` | Upgrade from a local signed binary (air-gapped) |
| `upgrade plan <version>` | Preview the rolling order and estimated duration |
| `upgrade status` | Show upgrade progress (cluster or node) |
| `upgrade rollback [version]` | Roll back to a previous binary version |
| `upgrade resume` | Resume a paused upgrade under a fresh attempt id |

TUI keys:

| Key | Action |
|-----|--------|
| `a`, `n`, `j`, `e`, `l`, `r` | Open apps, nodes, jobs, events, logs or routes |
| `s`, `?`, `:` | Search, help or command palette |
| Arrow keys, Enter | Select and open a row |
| Tab, Shift-Tab | Cycle app-detail tabs |
| `/` | Filter the current list |
| `f` | Toggle log following |
| Escape, `q` | Go back; quit from the dashboard |

### Self-upgrade (Phase 14)

`bun` can replace its own binary in place: the process `exec()`s the new
version, running workloads are *adopted* by the new binary (same pids, no
restarts), and a crash-looping upgrade automatically reverts to the previous
binary. On a cluster the Raft leader rolls the fleet: workers first (with
`--parallel`), council members one at a time, then the leader upgrades
itself last (in place; a ≥3-node council keeps quorum through the bounce).

Requirements:

- **A process supervisor.** Run bun under something that restarts it whenever
  it exits (systemd `Restart=always`, or any `while true; do bun ...; done`
  loop). Startup-side recovery does the rest — including the automatic
  symlink revert after a crash-looping upgrade.
- **Signatures.** Network upgrades need two Ed25519 signatures over the
  binary: one from the release key set compiled into the running binary, and
  one from the operator key configured as `upgrades.external_signing_key` in
  node.toml (generate one with `relish dev keygen`). Air-gapped
  `upgrade start --binary` needs only the release signature (expects
  `{binary}.sig` alongside the file — see `relish dev sign-binary`).
- **A versioned binary directory** (default: the directory of the running
  executable): `bun` is a symlink to `bun-vX.Y.Z`; previous versions are
  retained for rollback (`upgrades.retain_versions`, default 3).

The release private key must live outside any repository. The project key's
public half is compiled into `src/upgrade/keys.rs`; rotating it means shipping
a release that trusts both old and new keys, then dropping the old one.

### Dev cluster

`relish dev create` spins up a real multi-node Reliaburger cluster in Lima VMs — gossip membership, a Raft council that elects a leader, and live state reporting — not isolated single nodes. The same `bun` binary as production runs in each VM (started with `--cluster`).

```sh
relish dev create mycluster --nodes 3
limactl shell reliaburger-1 relish nodes     # all three nodes
limactl shell reliaburger-1 relish council   # council members + leader
relish dev destroy mycluster
```

Notes:

- **Lima required** (`brew install lima`). VMs use Lima's `user-v2` network so they can reach each other with no `socket_vmnet`/sudo setup; each node advertises its inter-VM IP. That network isn't routable from the host, so run `relish nodes`/`council` *inside* a node (`limactl shell reliaburger-1 …`), where the CLI reaches the local agent on `127.0.0.1:9117`.
- **Binaries are built from your current tree, not downloaded.** `dev create` builds `bun`/`relish` for Linux inside the persistent build VM (the same one `relish dev test` uses), so the **first `create` is slow** (a full build); later runs are incremental.
- `--bun <path>` / `--relish <path>` install a pre-built Linux binary instead, skipping the build.

Global flags:

| Flag | Default | Description |
|------|---------|-------------|
| `--output <format>` | `human` | Output format: `human`, `json`, `yaml` |
| `--endpoint <url>` | local API | Bun API base URL; overrides `RELIABURGER_ENDPOINT` |
| `--ca-cert <path>` | unset | Cluster root CA PEM; switches the local default to HTTPS |
| `--token <token>` | environment | API bearer token; overrides `RELIABURGER_TOKEN` |

Examples:

```sh
# Deploy the example app (agent must be running)
cargo run --bin relish -- apply examples/phase-1/proc-minimal-app.toml

# Preview without contacting an agent
cargo run --bin relish -- apply examples/phase-1/proc-minimal-app.toml --dry-run

# List running workloads
cargo run --bin relish -- status

# JSON output
cargo run --bin relish -- --output json status

# Show logs
cargo run --bin relish -- logs web

# Show last 20 lines
cargo run --bin relish -- logs web --tail 20

# Stream logs in real time
cargo run --bin relish -- logs web --follow

# Execute a command inside a running instance
cargo run --bin relish -- exec web echo hello

# Stop an app
cargo run --bin relish -- stop web

# Scaffold a new project
cargo run --bin relish -- init myproject
```

`apply --dry-run` shows what would happen without contacting an agent:

```
app "web" (proc-grill:image-ignored)
  1 replica, port 8080
  health: GET /healthz every 10s

(dry run — nothing deployed)
```

### TestApp utility

A built-in test HTTP server with configurable behaviour:

```sh
cargo run --bin testapp -- --mode healthy --port 8080
cargo run --bin testapp -- --mode unhealthy-after --count 5 --port 8080
cargo run --bin testapp -- --mode hang --port 8080
cargo run --bin testapp -- --mode slow --delay 3000 --port 8080
```

Used in the example configs to demonstrate health checks, restarts, and lifecycle transitions with ProcessGrill.

## Configuration

### Running real containers

If you have a real container runtime (Apple Container on macOS, runc on Linux), you can run real Docker Hub images:

```sh
# Terminal 1 — start the agent with a real runtime
cargo run --bin bun -- --runtime apple   # or --runtime runc

# Terminal 2 — deploy nginx with health checks
cargo run --bin relish -- apply examples/phase-1/container-nginx.toml

# Check status (nginx should reach Running after health checks pass)
cargo run --bin relish -- status

# Or run a quick Alpine hello world job
cargo run --bin relish -- apply examples/phase-1/container-hello.toml
```

The first deploy will pull the image from Docker Hub, which takes a few seconds. Subsequent deploys reuse the cached image.

The `proc-*` examples use `command` to run local binaries and work without any container runtime. The `container-*` examples use `image` to pull and run real OCI containers.

### Internal DNS on rootful runc

The `.internal` responder is opt-in and currently supports rootful runc on
Linux. It also requires the eBPF service data path: DNS returns a virtual IP,
and the connect hook turns that VIP into a healthy backend. Bun refuses any
other combination before it adopts or creates a workload.

```toml
[dns]
enabled = true
listen = "0.0.0.0:53"       # derive and bind this node's runc gateway
upstream = "8.8.8.8:53"
default_namespace = "default"
restrict_sources = true

[ebpf]
enabled = true
```

`0.0.0.0:53` is a derivation setting, not the socket Bun ultimately exposes.
Bun calculates the node-side veth gateway and binds that precise address with
Linux `IP_FREEBIND` before the first workload creates the interface. This avoids
host loopback, which a container namespace can't reach, and avoids claiming
wildcard port 53 from `systemd-resolved`. `/etc/resolv.conf` has no port syntax,
so other ports are rejected.

Runc receives a per-instance, read-only resolver file. Bun doesn't modify the
shared unpacked image. Both UDP and TCP must bind before the node reports DNS
ready; a later responder-task failure stops Bun so its capability expires.
Rootless runc, ProcessGrill, Apple Container and IPv6-only listeners remain
unsupported rather than silently falling back to broken host DNS.

## Configuration

Workloads are defined in TOML. See [`examples/`](../examples/) for ready-to-apply configs:

| Example | Demonstrates |
|---------|-------------|
| **ProcessGrill** (`proc-*`) | **Runs local processes — no container runtime needed** |
| [`proc-first-run.toml`](../examples/phase-1/proc-first-run.toml) | Collision-free portable first run |
| [`proc-minimal-app.toml`](../examples/phase-1/proc-minimal-app.toml) | App with health check + worker |
| [`proc-restarts.toml`](../examples/phase-1/proc-restarts.toml) | App that goes unhealthy and gets restarted |
| [`proc-job-success.toml`](../examples/phase-1/proc-job-success.toml) | Job that runs to completion |
| [`proc-job-failure.toml`](../examples/phase-1/proc-job-failure.toml) | Job that fails and gets retried |
| [`proc-init-container.toml`](../examples/phase-1/proc-init-container.toml) | App with init container |
| [`proc-full-featured.toml`](../examples/phase-1/proc-full-featured.toml) | All Phase 1 features |
| [`proc-multi-app.toml`](../examples/phase-1/proc-multi-app.toml) | Multiple apps in one config |
| [`proc-volumes.toml`](../examples/phase-1/proc-volumes.toml) | Managed and HostPath volumes |
| **Real containers** (`container-*`) | **Pulls OCI images — requires runc or Apple Container** |
| [`container-hello.toml`](../examples/phase-1/container-hello.toml) | Alpine hello world job |
| [`container-nginx.toml`](../examples/phase-1/container-nginx.toml) | nginx with health check |
| [`container-job-failure.toml`](../examples/phase-1/container-job-failure.toml) | Job that fails and gets retried |
| [`container-init-container.toml`](../examples/phase-1/container-init-container.toml) | App with init container |
| [`container-full-featured.toml`](../examples/phase-1/container-full-featured.toml) | All Phase 1 features |
| [`container-multi-app.toml`](../examples/phase-1/container-multi-app.toml) | Multiple apps in one config |
| [`container-volumes.toml`](../examples/phase-1/container-volumes.toml) | Managed and HostPath volumes |

### Images, the pull-through cache, and cluster registries (Phase 12)

External images (`docker.io`, `ghcr.io`, …) are served through a pull-through
cache by default: the first pull fetches from upstream and commits under a
`cache/<host>/<repo>` catalog entry; later pulls anywhere in the cluster are
served peer-to-peer. Cluster-pushed images download from multiple peers in
parallel (rarest layer first). Two operational constraints to know:

- **`registry_port` must be uniform across the cluster** — peers derive each
  other's registry URLs from gossip IPs plus the local port setting.
- **`registry_bind` defaults to loopback**, which disables peer replication
  and P2P in cluster mode (bun warns at startup). Bind wider only behind the
  perimeter firewall's cluster-node allowlist: the registry has no auth/TLS yet.

```toml
[images]
registry_bind = "0.0.0.0"   # cluster mode; keep firewalled
pull_through = true          # cache external images in the cluster
cache_recheck_secs = 3600    # how long a cached mutable tag is trusted
p2p_concurrency = 4          # parallel layer fetches per image pull
build_timeout_secs = 900     # ceiling per buildah stage
max_context_bytes = 268435456 # 256 MiB cap on an extracted build context

[[images.external_registries]]
host = "ghcr.io"
username = "bot"
password_secret = "GHCR_TOKEN"   # environment variable, read at startup
```

### Volume snapshots and scheduled backups (Phase 12)

Managed volumes on a Btrfs-backed `[storage] volumes` directory are created as
subvolumes (size limits become qgroup quotas) and can be snapshotted — O(1)
copy-on-write — via `relish snapshot` or on a schedule:

```toml
[storage.snapshots]
interval_secs = 86400                 # 0 disables the loop
retain = 7                            # newest N kept per volume
upload_url = "s3://backups/burger"    # optional; file:// and gs:// work too
```

Snapshot archives upload as `.tar.gz` through `object_store`; credentials come
from each backend's standard environment variables. On non-Btrfs filesystems
snapshots return a clear error (sized volumes fall back to loop-mounted ext4).

### Apps

```toml
[app.web]
image = "proc-grill:image-ignored"
command = ["target/debug/testapp", "--mode", "healthy", "--port", "8080"]
port = 8080

[app.web.health]
path = "/healthz"
interval = 10
timeout = 5
```

The `image` field is required for real runtimes (runc, Apple Container) but **ignored by ProcessGrill**, which runs the `command` directly as an OS process. ProcessGrill examples use `proc-grill:image-ignored` to make this explicit. If no `command` is set, ProcessGrill falls back to `sleep 86400`.

### Jobs

Jobs are run-to-completion tasks. They retry up to 3 times with exponential backoff on failure.

```toml
[job.migrate]
image = "proc-grill:image-ignored"
command = ["echo", "migration complete"]
```

### Init containers

Init containers run sequentially before the main app starts. If any init container fails, the app transitions to Failed.

```toml
[app.web]
image = "proc-grill:image-ignored"
command = ["sleep", "60"]

[[app.web.init]]
command = ["echo", "initialising database"]
```

For the full configuration reference (resource limits, replicas, environment variables, volumes, secrets, namespaces), see the book chapter [Hello, Container](book/01-hello-container.md).

## Runtime auto-detection

When `--runtime auto` (the default), bun checks what's available:

1. **macOS**: looks for `container` in PATH → uses Apple Container
2. **Linux**: looks for `runc` in PATH → uses RuncGrill
3. **Fallback**: uses ProcessGrill (always available)

Override with `--runtime process`, `--runtime runc`, or `--runtime apple`. Selecting a runtime that isn't available on your platform produces an error.

## API

The bun agent exposes a local HTTP API on port 9117:

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/v1/health` | Agent liveness check |
| `POST` | `/v1/apply` | Deploy workloads (TOML body) |
| `GET` | `/v1/status` | List all instances |
| `GET` | `/v1/status/{app}/{namespace}` | Status for a specific app |
| `POST` | `/v1/stop/{app}/{namespace}` | Stop an app |
| `GET` | `/v1/logs/{app}/{namespace}` | Captured stdout/stderr (`?tail=N&follow=true`) |
| `POST` | `/v1/exec/{app}/{namespace}` | Execute a command (JSON body: `{"command":["..."]}`) |
| `GET` | `/v1/cluster/nodes` | List cluster nodes (gossip membership) |
| `GET` | `/v1/cluster/council` | Council (Raft) status |
| `POST` | `/v1/cluster/join` | Join a cluster (JSON body: `{"token":"...","addr":"..."}`) |
| `POST` | `/v1/chaos/partition` | Inject network partition (JSON: `{"peers":[...],"duration_secs":N}`) |
| `POST` | `/v1/chaos/heal` | Remove all active partitions |
| `GET` | `/v1/chaos/status` | Query active chaos state |

The CLI uses this API internally. You can also call it directly:

```sh
curl http://127.0.0.1:9117/v1/health
curl http://127.0.0.1:9117/v1/status
```
