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
| **Brioche** | Web dashboard (HTMX auto-refresh, uPlot charts, app/node detail pages) |

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
make test                    # run all tests (2204 and counting)
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
  mayo/                # Time-series metrics (Arrow, DataFusion, Parquet, hierarchical rollups)
  ketchup/             # Log collection (append-only, indexed, queries)
  brioche/             # Web dashboard
  upgrade/             # Self-upgrade (dual-signed binaries, exec-in-place, rolling orchestration)
docs/
  README.md            # User documentation (install, build, run)
  whitepaper.md        # Full architectural vision
  roadmap.md           # 9 implementation phases
  progress.md          # What's done, what's next
  design/              # Detailed design docs per component (14 files)
  book/                # "Building Reliaburger" chapters (preface, 1-11, 14, Rust appendix)
  _quarto/             # PDF build configuration
examples/              # Example app and job configs
scripts/               # Test and demo scripts
assets/                # Logo and project media
Makefile               # Build, test, lint, format, demo targets
CLAUDE.md              # Project guide, conventions, writing style
```

## Current status

**2,204 tests across 13 completed phases** (plus the Lima-gated eBPF/netns/btrfs/buildah integration suites run in the dev VM). Phase 12 (Optimisations) closed the loop on the whole image pipeline — O(1) nftables port maps, rarest-first P2P image downloads, a pull-through cache for external registries, Btrfs-quota'd volumes with CoW snapshots and scheduled object-store backups, and cluster-wide batch and build execution — and, in the process, wired several long-dead paths (managed volumes, port-mapping DNAT, cluster-image deploys). A follow-on security hardening pass (Phase 11b Stage 5) is underway; see below. See [progress.md](docs/progress.md) for the full checklist.

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
| 9. User Experience | Done | 1,271 |
| 10. Advanced Security | Done | 1,448 |
| 11. Advanced Observability | Done | 1,595 |
| 11b. Review & wiring | Done | 1,703 |
| 14. Self-Upgrade | Done | 1,880 |
| 12. Optimisations | Done | 1,981 |

Phase 14 (rolling binary upgrades) landed ahead of 12 and 13: `relish
upgrade start v0.2.0` rolls a live cluster onto a new dual-signed binary —
workloads survive the swap (same pids, adopted across `exec()`), a
crash-looping release reverts itself, and the leader upgrades itself last
(in place, quorum preserved through the sub-second exec bounce).

Phase 12 (optimisations) followed: port mapping via O(1) nftables named
maps, parallel rarest-first P2P image pulls between nodes, a pull-through
cache so external images are fetched from upstream once, Btrfs subvolume
quotas and instant CoW snapshots (`relish snapshot`, scheduled S3/GCS
backups), and cluster-wide batch (`relish batch`) and image-build
(`relish build`) execution with capability-based placement.

Phase 11b Stage 5 (security hardening, in progress) turns the PKI from a
tested library into a live boundary. Each node persists its own certificate
(written by `relish init`, delivered to joiners in the join response with a
trust-on-first-use fingerprint check), and — when `[security] require_mtls`
is set — the Raft, reporting and agent-API listeners all run mutual TLS with
a shared, Raft-replicated revocation list; a revoked node is refused on its
next handshake. Image-signature verification checks that revocation list too.
The Brioche dashboard now authenticates: a token is exchanged once for a
read-only, `HttpOnly` session cookie, and the UI routes sit behind it.
Network policy enforcement is now complete too: egress allowlists cover
IPv6 (a new `connect6` hook — a v4-only allowlist was bypassable over v6)
and CIDR ranges (kernel LPM tries), egress is programmed *before* the
process starts on root-mode runc (create → program → start, failing the
deploy closed if policy can't be installed), a periodic sweep reconciles
the kernel maps against live instances, and the nftables perimeter is
dual-stack with parsed-and-validated admin CIDRs and bounded `nft` calls.
Secret and workload-identity safety rounds out the 12b.1 "stop the
bleeding" themes: workload certificates get exact one-hour validity
windows and server-rebuilt SANs (a CSR can't smuggle extra names), each
instance's identity lives in its own tmpfs-backed directory that follows
the instance from creation to stop and survives agent restarts with its
rotation schedule intact, and secret-key rotation refuses to retire an
old key while any stored secret is still sealed under it.
Still open in this stage: the Pickle registry's own auth/TLS, and the
broader correctness/observability items tracked in
[progress.md](docs/progress.md).

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
9. [The Full Package](docs/book/09-the-full-package.md)
10. [Locking It Down](docs/book/10-locking-it-down.md)
11. [Eyes Everywhere](docs/book/11-eyes-everywhere.md)
12. [Squeezing Every Drop](docs/book/12-squeezing-every-drop.md) *(in progress)*
14. [Changing the Tyres at Full Speed](docs/book/14-changing-the-tyres.md)
- [Appendix: Rust for C, Python, and Go Programmers](docs/book/16-appendix-rust.md)

## Licence

[Apache 2.0](LICENSE)
