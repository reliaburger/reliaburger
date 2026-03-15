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
  mustard/             # SWIM gossip protocol
    state.rs           # NodeState enum and conflict resolution
    message.rs         # GossipMessage, MembershipUpdate, piggybacked payloads
    membership.rs      # MembershipTable (who's in the cluster)
    dissemination.rs   # Piggyback queue with priority ordering
    transport.rs       # MustardTransport trait + InMemoryNetwork
    protocol.rs        # SWIM probe cycle (MustardNode)
    config.rs          # GossipConfig (intervals, timeouts)
  council/             # Raft consensus (3–7 council nodes)
    types.rs           # TypeConfig, RaftRequest, CouncilResponse, DesiredState
    log_store.rs       # MemLogStore (in-memory Raft log + vote storage)
    state_machine.rs   # CouncilStateMachine (applies entries, snapshots)
    network.rs         # InMemoryRaftRouter (test network with partitions)
    node.rs            # CouncilNode (high-level wrapper over openraft)
  meat/               # Scheduler (shared types, placement TBD)
    types.rs           # NodeId, AppId, Resources, NodeCapacity
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

See [progress.md](docs/progress.md) for the full implementation checklist.

## Licence

[Apache 2.0](LICENSE)
