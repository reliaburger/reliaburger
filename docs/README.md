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
- Requires root privileges for container operations
- Bun stores OCI bundles in `/var/lib/reliaburger/bundles/`
- To run runc-specific tests: `RELIABURGER_RUNC_TESTS=1 cargo test`

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
| `inspect <name>` | Detailed info about an app |
| `exec <app> <cmd...>` | Execute a command in a running container (Phase 8) |

Global flags:

| Flag | Default | Description |
|------|---------|-------------|
| `--output <format>` | `human` | Output format: `human`, `json`, `yaml` |

Examples:

```sh
# Deploy the example app (agent must be running)
cargo run --bin relish -- apply examples/phase-1-minimal-app.toml

# Deploy without agent (shows dry-run plan)
cargo run --bin relish -- apply examples/phase-1-minimal-app.toml

# List running workloads
cargo run --bin relish -- status

# JSON output
cargo run --bin relish -- --output json status

# Show logs
cargo run --bin relish -- logs web
```

If no agent is running, `apply` falls back to a dry-run plan showing what *would* happen:

```
app "web" (myapp:v1)
  1 replica, port 8080
  health: GET /healthz every 10s

(dry run — bun agent not reachable, showing plan only)
```

## Configuration

Workloads are defined in TOML. See [`examples/phase-1-minimal-app.toml`](../examples/phase-1-minimal-app.toml) for a ready-to-apply config, or write your own:

```toml
[app.web]
image = "myapp:v1"
port = 8080

[app.web.health]
path = "/healthz"
interval = 10
timeout = 5
```

For the full configuration reference (resource limits, replicas, environment variables, volumes, jobs, secrets, namespaces), see the book chapter [Hello, Container](book/01-hello-container.md).

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
| `GET` | `/v1/logs/{app}/{namespace}` | Captured stdout/stderr |

The CLI uses this API internally. You can also call it directly:

```sh
curl http://127.0.0.1:9117/v1/health
curl http://127.0.0.1:9117/v1/status
```
