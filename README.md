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
docs/
  whitepaper.md        # Full architectural vision (the "what and why")
  roadmap.md           # 9 implementation phases, tests-first ordering
  progress.md          # What's done, what's next
  design/              # Detailed design docs per component (14 files)
  book/                # "Building Reliaburger" chapter drafts
  _quarto/             # PDF build configuration
assets/
  images/              # Logo and project media
Makefile               # PDF build targets (via Quarto)
CLAUDE.md              # Project guide, conventions, writing style
```

No `src/` directory yet. We haven't written a single line of Rust. That's next.

## Current status

Design-complete, implementation not started. Ready to begin Phase 1 (single-node container lifecycle). See [progress.md](docs/progress.md) for the full checklist.

## Licence

[Apache 2.0](LICENSE)
