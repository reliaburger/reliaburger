<p align="center">
  <img src="assets/images/reliaburger_logo_v1.jpg" alt="Reliaburger" width="400">
</p>

# Reliaburger

A batteries-included container orchestrator written in Rust. One binary that replaces Kubernetes and its ecosystem of add-ons with something dramatically simpler. Targeted at teams running 2-5000 nodes who want containers in production without the PhD.

This repo produces two things simultaneously:

1. **A working implementation** — complete, testable, simple, bug-free.
2. **A book** — *Building Reliaburger* — that walks through how we built all of it, teaching Rust and distributed systems along the way.

The full architectural vision lives in the [whitepaper](docs/whitepaper.md). For installation and usage instructions, see the [documentation](docs/README.md).

## Quick start

```sh
# Build
cargo build

# Run the node agent
cargo run --bin bun

# In another terminal — deploy the example app
cargo run --bin relish -- apply examples/phase-1/proc-minimal-app.toml

# Check what's running
cargo run --bin relish -- status
```

See [docs/README.md](docs/README.md) for prerequisites, container runtime setup, and full CLI reference.

## Repo layout

```
src/
  lib.rs               # Core library
  bin/bun.rs           # Node agent (daemon)
  bin/relish.rs        # CLI entry point
  bin/testapp.rs       # Configurable test HTTP server
  config/              # TOML configuration parsing (7 resource types)
  grill/               # Container runtime interface (state machine, ports, cgroups, OCI)
    process.rs         # Cross-platform process-based runtime
    runc.rs            # Linux runc runtime (image pulling, rootless support)
    apple.rs           # macOS Apple Container runtime
    image.rs           # OCI image pulling and layer unpacking
    rootless.rs        # Rootless runc spec modifications (Linux only)
  bun/                 # Node agent internals
    agent.rs           # Event loop (tokio::select, command channels)
    api.rs             # Local HTTP API (axum, port 9117)
    probe.rs           # HTTP health probing
    supervisor.rs      # Workload lifecycle management
    health.rs          # Health check state machine
  relish/              # CLI internals
    client.rs          # HTTP client for bun agent
    commands.rs        # Subcommand implementations
docs/
  README.md            # User documentation (install, build, run)
  whitepaper.md        # Full architectural vision (the "what and why")
  roadmap.md           # 9 implementation phases, tests-first ordering
  progress.md          # What's done, what's next
  design/              # Detailed design docs per component (14 files)
  book/                # "Building Reliaburger" chapter drafts
  _quarto/             # PDF build configuration
examples/
  phase-1/
    proc-minimal-app.toml         # App with health check + worker
    proc-restarts.toml             # App that goes unhealthy and restarts
    proc-job-success.toml          # Job that runs to completion
    proc-job-failure.toml          # Job that fails and gets retried
    proc-init-container.toml       # App with init container
    proc-full-featured.toml        # All Phase 1 features
    proc-multi-app.toml            # Multiple apps in one config
    proc-volumes.toml              # Managed and HostPath volumes
    container-hello.toml           # Alpine hello world (real OCI image)
    container-nginx.toml           # nginx with health check (real OCI image)
    container-job-failure.toml     # Job that fails (real OCI image)
    container-init-container.toml  # App with init container (real OCI image)
    container-full-featured.toml   # All Phase 1 features (real OCI image)
    container-multi-app.toml       # Multiple apps in one config (real OCI image)
    container-volumes.toml         # Managed and HostPath volumes (real OCI image)
assets/
  images/              # Logo and project media
Makefile               # Build, test, lint, format targets
CLAUDE.md              # Project guide, conventions, writing style
```

## Current status

**Phase 1 complete** (single-node container lifecycle). 339 passing tests.

- TOML config parsing for all 7 resource types with custom serde deserialisers
- Container runtime interface: 10-state lifecycle state machine, concurrent port allocator, cgroup v2 parameter computation, OCI runtime spec generation
- Three container runtimes: ProcessGrill (cross-platform), RuncGrill (Linux), AppleContainerGrill (macOS) with auto-detection
- OCI image pulling: pull real images from Docker Hub, content-addressed blob caching, layer unpacking with whiteout support
- Rootless runc: user namespace UID/GID mapping, rootless cgroups v2, no-sudo container execution
- Bun node agent: event loop with health check timer, command channels, graceful shutdown
- Job execution: run-to-completion tasks with exit code tracking, retry with exponential backoff
- Init containers: sequential pre-start execution, failure prevents main app start
- Restart re-drive: instances automatically restart through full lifecycle after health check or job failure
- Local HTTP API (axum on port 9117): deploy, status, stop, logs, health endpoints with SSE streaming
- Relish CLI: `init`, `apply` (with dry-run fallback and streaming progress), `status`, `logs` (with `--tail` and `--follow`), `exec`, `inspect`, three output formats
- HostPath-style volumes: dual-mode with explicit source (hostPath) or managed storage
- HTTP health probing with configurable intervals, timeouts, and thresholds
- TestApp standalone binary for demos and integration tests
- 21 integration tests exercising the full stack end to end

See [progress.md](docs/progress.md) for the full checklist.

## Licence

[Apache 2.0](LICENSE)
