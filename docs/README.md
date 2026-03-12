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
make ci          # fmt-check + lint + test (what CI runs)
make clean       # remove build artefacts
```

Or use cargo directly:

```sh
cargo build
cargo test
```

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

The CLI uses this API internally. You can also call it directly:

```sh
curl http://127.0.0.1:9117/v1/health
curl http://127.0.0.1:9117/v1/status
```
