# Under the hood

Everything runs inside a single `bun` binary. No sidecars, no separate
databases, no external dependencies. Each subsystem has a burger name and a
design doc under `docs/design/`.

| Component | What it does |
|-----------|-------------|
| **Grill** | Container runtime (runc, Apple Container, process fallback) |
| **Mustard** | SWIM gossip protocol for cluster membership |
| **Council** | Raft consensus for leader election and state |
| **Meat** | Bin-packing scheduler with labels, quotas, daemon mode |
| **Onion** | Userspace `.internal` DNS + eBPF connection steering |
| **Wrapper** | Ingress proxy (host/path routing, rate limiting, TLS) |
| **Sesame** | PKI, mTLS, API auth, secret encryption, Raft encryption |
| **Pickle** | Built-in OCI image registry (push/pull, replication, GC) |
| **Mayo** | Time-series metrics (Arrow + DataFusion + Parquet) |
| **Ketchup** | Log collection (append-only, indexed, JSON-aware) |
| **Smoker** | Built-in fault injection (safety rails, eBPF network faults) |
| **Brioche** | Web dashboard (HTMX auto-refresh, uPlot charts) |
| **Lettuce** | GitOps engine (declarative repo sync) |

## Repo layout

```
src/
  bin/bun.rs           # Node agent (daemon)
  bin/relish.rs        # CLI entry point
  bin/testapp.rs       # Configurable test HTTP server
  config/              # TOML configuration parsing
  grill/               # Container runtime
  bun/                 # Node agent (event loop, API, health, supervisor)
  relish/              # CLI, TUI, manual, source reader
  smoker/              # Fault injection
  mustard/             # SWIM gossip
  council/             # Raft consensus
  meat/                # Scheduler
  reconstruction/      # State reconstruction after leader election
  reporting/           # Hierarchical reporting tree
  onion/               # Service discovery, DNS, eBPF
  wrapper/             # Ingress proxy
  firewall/            # nftables perimeter firewall
  sesame/              # PKI, mTLS, secrets, auth
  pickle/              # OCI image registry
  mayo/                # Metrics
  ketchup/             # Logs
  lettuce/             # GitOps
  brioche/             # Web dashboard
  upgrade/             # Self-upgrade
docs/
  README.md            # User documentation
  whitepaper.md        # Architectural vision
  progress.md          # What's done, what's next
  design/              # Per-component design docs
  book/                # "Building Reliaburger" chapters
  manual/              # These chapters
examples/              # Ready-to-apply workload configs
```

You're reading an embedded copy of `docs/manual/`; the full source tree is
in the binary too — try `relish source meat` to read the scheduler.

## Test suites

```sh
make test              # portable nextest suite
make test-cluster      # real multi-node failover/recovery acceptance
sudo make test-linux   # runc, netns, eBPF, Btrfs suites
make test-upgrade      # real binary-replacement acceptance
```

The harness only reports tests that truthfully ran on your platform; the
gated suites each enable their own prerequisites.
