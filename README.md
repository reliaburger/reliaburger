<p align="center">
  <img src="assets/images/reliaburger_logo_v1.jpg" alt="Reliaburger" width="400">
</p>

# Reliaburger

A batteries-included container orchestrator written in Rust. One binary that replaces Kubernetes and its ecosystem of add-ons with something dramatically simpler. Targeted at teams running 2-5000 nodes who want containers in production without the PhD.

This repo produces two things simultaneously:

1. **A working implementation** — complete, testable, simple.
2. **A book** — *Building Reliaburger* — that walks through how we built all of it, teaching Rust and distributed systems along the way.

The full architectural vision lives in the [whitepaper](docs/whitepaper.md). For installation and usage instructions, see the [documentation](docs/README.md).

## What's included

Everything runs inside a single `bun` binary. No sidecars, no separate databases, no external dependencies.

| Component | What it does |
|-----------|-------------|
| **Grill** | Container runtime (runc, Apple Container, process fallback) |
| **Mustard** | SWIM gossip protocol for cluster membership |
| **Council** | Raft consensus for leader election and state |
| **Meat** | Bin-packing scheduler with labels, quotas, daemon mode |
| **Onion** | eBPF service discovery (DNS + connect rewrite) |
| **Wrapper** | Ingress proxy (host/path routing, rate limiting, TLS) |
| **Sesame** | PKI, mTLS, API auth, secret encryption, Raft encryption |
| **Pickle** | Built-in OCI image registry (push/pull, replication, GC) |
| **Mayo** | Time-series metrics (Arrow + DataFusion + Parquet) |
| **Ketchup** | Log collection (append-only, indexed, JSON-aware) |
| **Smoker** | Built-in fault injection (safety rails, eBPF network faults, scenarios) |
| **Brioche** | Web dashboard (server-rendered, auto-refresh) |

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

# View the dashboard
open http://localhost:9117/

# Show live resource usage
cargo run --bin relish -- top
```

See [docs/README.md](docs/README.md) for prerequisites, container runtime setup, and full CLI reference.

## Try it

```sh
make test                    # run all tests (1255 and counting)
make observability-demo      # start bun, collect metrics, query APIs, show dashboard
make pickle-test-macos       # push/pull a Docker image through the Pickle registry
```

## Repo layout

```
src/
  lib.rs               # Core library
  bin/bun.rs           # Node agent (daemon)
  bin/relish.rs        # CLI entry point
  bin/testapp.rs       # Configurable test HTTP server
  config/              # TOML configuration parsing
  grill/               # Container runtime (runc, Apple Container, process)
  bun/                 # Node agent (event loop, API, health, supervisor)
  relish/              # CLI (commands, client, output, plan, chaos, fault, dev)
  smoker/              # Built-in fault injection (safety, registry, eBPF, scenarios)
  mustard/             # SWIM gossip protocol
  council/             # Raft consensus
  meat/                # Scheduler (filter, score, select, commit)
  reconstruction/      # State reconstruction after leader election
  reporting/           # Hierarchical reporting tree
  onion/               # eBPF service discovery
  wrapper/             # Ingress proxy
  firewall/            # nftables perimeter firewall
  sesame/              # PKI, mTLS, secrets, API auth, Raft encryption
  pickle/              # OCI image registry (blob store, API, replication, GC)
  mayo/                # Time-series metrics (Arrow, DataFusion, Parquet)
  ketchup/             # Log collection (append-only, indexed, queries)
  brioche/             # Web dashboard
docs/
  README.md            # User documentation (install, build, run)
  whitepaper.md        # Full architectural vision
  roadmap.md           # 9 implementation phases
  progress.md          # What's done, what's next
  design/              # Detailed design docs per component (14 files)
  book/                # "Building Reliaburger" chapter drafts (8 chapters + preface)
  _quarto/             # PDF build configuration
examples/              # Example app and job configs
scripts/               # Test and demo scripts
assets/                # Logo and project media
Makefile               # Build, test, lint, format, demo targets
CLAUDE.md              # Project guide, conventions, writing style
```

## Current status

**1,263 tests across 8 completed phases.** See [progress.md](docs/progress.md) for the full checklist.

| Phase | Status | Tests |
|-------|--------|-------|
| 1. Foundation | Done | 321 |
| 2. Cluster Formation | Done | 588 |
| 3. Networking | Done | 702 |
| 4. Security | Done | 795 |
| 5. Storage & Registry | Done | 867 |
| 6. Observability | Done | 991 |
| 7. Deployments | Done | 1,050 |
| 8. Advanced | Done | 1,263 |

## The book

Each phase produces a chapter of *Building Reliaburger*, a book that teaches Rust and distributed systems through the implementation:

0. [Preface](docs/book/00-preface.md)
1. [Hello, Container](docs/book/01-hello-container.md)
2. [Finding Friends](docs/book/02-finding-friends.md)
3. [Talking to Each Other](docs/book/03-talking-to-each-other.md)
4. [Trust No One](docs/book/04-trust-no-one.md)
5. [Where the Images Live](docs/book/05-where-the-images-live.md)
6. [Watching Everything](docs/book/06-watching-everything.md)
7. [Ship It](docs/book/07-ship-it.md)
8. [Breaking Things on Purpose](docs/book/08-breaking-things-on-purpose.md)

## Licence

[Apache 2.0](LICENSE)
