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
- **Rootless mode** is supported — runs containers without sudo using user namespaces
- Rootless stores bundles/images in `~/.local/share/reliaburger/`; root mode uses `/var/lib/reliaburger/`
- OCI images are pulled from Docker Hub automatically when the spec's `image` field is set (e.g. `alpine:latest`)
- To run runc-specific tests: `RELIABURGER_RUNC_TESTS=1 cargo test`
- To run image pull tests (requires network): `RELIABURGER_IMAGE_PULL_TESTS=1 cargo test`

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
- To run Apple Container-specific tests: `RELIABURGER_APPLE_CONTAINER_TESTS=1 cargo test`

### ProcessGrill (built-in fallback)

Works on any platform. Spawns child processes instead of real containers — no namespaces, no cgroups, no rootfs isolation. Useful for development, testing, and platforms without a container runtime installed.

No installation needed. This is what you get by default.

## Building

The [Makefile](../Makefile) provides all build targets:

```sh
make build       # compile (debug)
make release     # compile (optimised)
make test        # run all tests
make lint        # clippy with warnings as errors
make fmt         # format with rustfmt
make ci          # fmt-check + lint + test + bench (what CI runs)
make clean       # remove build artefacts
```

Or use cargo directly:

```sh
cargo build
cargo test
```

## Testing and benchmarking

### Tests

```sh
make test          # run all tests (unit + integration)
make ci            # fmt-check + lint + test + bench (what CI runs)
```

Some tests require specific runtimes or network access and are gated behind environment variables:

| Variable | What it enables |
|----------|----------------|
| `RELIABURGER_RUNC_TESTS=1` | runc container runtime tests (Linux only) |
| `RELIABURGER_APPLE_CONTAINER_TESTS=1` | Apple Container tests (macOS only) |
| `RELIABURGER_IMAGE_PULL_TESTS=1` | OCI image pull tests (requires network) |
| `RELIABURGER_UPGRADE_TESTS=1` | real-binary self-upgrade tests (slow; also via `make test-upgrade`) |
| `RELIABURGER_BTRFS_TESTS=1` | Btrfs quota + snapshot tests (Linux root; each test provisions its own loopback btrfs) |
| `RELIABURGER_BUILDAH_TESTS=1` | real `buildah` image-build tests (Linux with buildah installed) |
| `RELIABURGER_EBPF_TESTS=1` | eBPF connect-hook, egress (IPv4/IPv6/CIDR) and kernel-sweep tests (Linux root, kernel 5.7+, `--features ebpf`; run via `relish dev test ebpf`) |

### Benchmarks

Gossip protocol benchmarks use [criterion](https://docs.rs/criterion) for statistical analysis with regression detection.

```sh
make bench         # fast benchmarks: transport, single round, convergence 5-250 nodes (~2 min)
make bench-large   # large cluster benchmarks: 500 and 1000 nodes (~10 min)
make bench-10k     # 10,000 node convergence validation (~1 hour)
```

The fast benchmarks (`cargo bench --bench gossip`) are the ones to run regularly — they catch performance regressions in the gossip protocol. Results are stored in `target/criterion/` and criterion reports whether performance changed between runs.

The large benchmarks (`cargo bench --bench gossip_large`) test the same convergence logic at 500 and 1000 nodes. These take longer because the simulation is O(N² log N) — all N nodes are driven sequentially each round.

The 10k test (`make bench-10k`) is not a criterion benchmark but an ignored test that runs a single 10,000-node convergence simulation. It validates the whitepaper's scalability target and prints progress as membership knowledge spreads through the cluster. Run it when you want to verify the protocol scales, not on every commit.

## Running

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

# Use a custom listen address
cargo run --bin bun -- --listen 0.0.0.0:9117

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

### CLI (relish)

Relish is the command-line interface for interacting with a running bun agent.

```sh
cargo run --bin relish -- <command>
```

Commands:

| Command | Description |
|---------|-------------|
| `apply <path>` | Deploy workloads from a TOML config file |
| `status` | List all running workloads |
| `logs <name>` | Show captured stdout/stderr for an app |
| `logs <name> --tail N` | Show only the last N lines |
| `logs <name> --follow` / `-f` | Stream new log lines as they appear |
| `inspect <name>` | Detailed info about an app |
| `exec <app> <cmd...>` | Execute a command inside a running instance |
| `stop <app>` | Stop all instances of an app |
| `init [dir]` | Scaffold starter config files in a directory |
| `nodes` | List cluster nodes and their gossip state |
| `council` | Show council (Raft) composition and status |
| `join --token <token> <addr>` | Join an existing cluster |
| `chaos <action>` | Run chaos testing scenarios (council-partition, worker-isolation, status, heal) |
| `resolve <name>` | Resolve a service name to its VIP and backends |
| `routes` | Show ingress routing table |
| `top` | Show live resource usage (CPU, memory) for all apps |
| `deploy <path>` | Trigger a rolling deploy for an app |
| `history <app>` | Show deploy history for an app |
| `rollback <app>` | Rollback an app to the previous version |
| `lint <path>` | Validate a config file without deploying |
| `images` | List images in the local Pickle registry |
| `build <path>` | Build OCI images from `[build.*]` sections and push to Pickle (async: submits, then polls) |
| `batch <path>` | Submit `[job.*]` sections as a high-throughput batch across the cluster |
| `batch-status <id>` | Show a submitted batch's progress (total/pending/completed/failed) |
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

Examples:

```sh
# Deploy the example app (agent must be running)
cargo run --bin relish -- apply examples/phase-1/proc-minimal-app.toml

# Deploy without agent (shows dry-run plan)
cargo run --bin relish -- apply examples/phase-1/proc-minimal-app.toml

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

If no agent is running, `apply` falls back to a dry-run plan showing what *would* happen:

```
app "web" (proc-grill:image-ignored)
  1 replica, port 8080
  health: GET /healthz every 10s

(dry run — bun agent not reachable, showing plan only)
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

## Configuration

Workloads are defined in TOML. See [`examples/`](../examples/) for ready-to-apply configs:

| Example | Demonstrates |
|---------|-------------|
| **ProcessGrill** (`proc-*`) | **Runs local processes — no container runtime needed** |
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
