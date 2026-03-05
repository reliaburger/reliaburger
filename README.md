<p align="center">
  <img src="assets/images/reliaburger_logo_v1.jpg" alt="Reliaburger" width="400">
</p>

# Reliaburger

A batteries-included container orchestrator written in Rust. One binary that replaces Kubernetes and its ecosystem of add-ons with something dramatically simpler. Targeted at teams running 2-5000 nodes who want containers in production without the PhD.

This repo produces two things simultaneously:

1. **A working implementation** — complete, testable, simple, bug-free.
2. **A book** — *Building Reliaburger* — that walks through how we built all of it, teaching Rust and distributed systems along the way.

The full architectural vision lives in the [whitepaper](docs/whitepaper.md).

## Repo layout

```
src/
  lib.rs               # Core library
  bin/bun.rs           # Node agent entry point
  bin/relish.rs        # CLI entry point
  config/              # TOML configuration parsing (7 resource types)
  grill/               # Container runtime interface (state machine, ports, cgroups, OCI)
docs/
  whitepaper.md        # Full architectural vision (the "what and why")
  roadmap.md           # 9 implementation phases, tests-first ordering
  progress.md          # What's done, what's next
  design/              # Detailed design docs per component (14 files)
  book/                # "Building Reliaburger" chapter drafts
  _quarto/             # PDF build configuration
assets/
  images/              # Logo and project media
Makefile               # Build, test, lint, format targets
CLAUDE.md              # Project guide, conventions, writing style
```

## Current status

Phase 1 in progress (single-node container lifecycle). Completed so far:

- Cargo workspace with two binaries (`bun` agent, `relish` CLI) and core library
- TOML config parsing for all 7 resource types with custom serde deserialisers
- Container runtime interface: 10-state lifecycle state machine, concurrent port allocator, cgroup v2 parameter computation, OCI runtime spec generation
- 135 passing tests

See [progress.md](docs/progress.md) for the full checklist.

## Licence

[Apache 2.0](LICENSE)
