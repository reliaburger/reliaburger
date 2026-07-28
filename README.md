<p align="center">
  <img src="assets/images/reliaburger_logo_v1.jpg" alt="Reliaburger" width="400">
</p>

# Reliaburger

One binary. A whole container platform.

Reliaburger is a batteries-included container orchestrator written in Rust,
for teams running 2-5,000 nodes who want containers in production without
the PhD. The things you normally assemble from a dozen projects — scheduling,
gossip clustering, Raft consensus, service discovery, ingress, mTLS PKI, an
OCI image registry, metrics, logs, dashboards, GitOps, chaos testing, even
rolling self-upgrade of the orchestrator itself — ship compiled into one
`bun` agent and one `relish` CLI.

No sidecars. No add-on shopping list. No YAML archaeology. You get:

- **A five-minute start.** `relish setup` takes a fresh machine to a
  configured node: it detects or installs `bun` (signature-verified through
  the same dual-signed pipeline the cluster uses to upgrade itself) and asks
  a handful of questions.
- **A cluster that heals itself.** SWIM gossip membership, a self-healing
  Raft council, automatic rescheduling, council disaster recovery, and
  rolling binary upgrades where workloads survive the swap.
- **Security that's on by default.** Generated clusters require mTLS;
  joins are single-use-token, CSR-based; images can be signature-gated;
  secrets are encrypted at rest.
- **Batteries you'd otherwise deploy separately.** Built-in registry with
  P2P image distribution, time-series metrics with SQL, indexed logs,
  ingress with TLS and draining, web + terminal dashboards, and a fault
  injector for breaking things on purpose.

The full architectural vision lives in the [whitepaper](docs/whitepaper.md).
Install and usage details are in the [documentation](docs/README.md), and
implementation status in [progress.md](docs/progress.md).

## Quick start

```sh
cargo build --bins

# Run the node agent — no container runtime needed for the first taste
target/debug/bun --runtime process

# In another terminal: deploy, inspect, explore
target/debug/relish apply examples/phase-1/proc-first-run.toml
target/debug/relish status
target/debug/relish            # interactive terminal dashboard
open http://localhost:9117/    # web dashboard
```

With runc (Linux) or Apple Container (macOS) installed, the same flow runs
real OCI images — and `relish init cluster` generates the PKI and mTLS
config for a secure multi-node cluster. The [documentation](docs/README.md)
has the full secure-cluster walkthrough.

## The manual is in the binary

Reliaburger documents itself. `relish manual` opens the reference as a
searchable terminal reader — chapters, runnable examples, fuzzy search —
with no repo checkout and no internet:

```sh
relish manual              # read it in the terminal (/ to search)
relish manual --web        # the same manual as one page in your browser
relish manual examples     # drop the runnable example configs right here
```

<!-- asciinema: `relish manual` demo cast goes here -->

The source ships too. `relish source ebpf` opens a fuzzy search over the
exact `src/` tree the binary was compiled from. The platform carries its own
reference, examples and implementation wherever the binary goes.

## The book

This repository is also a book. *Building Reliaburger* walks through how
every subsystem was designed and built — teaching Rust and distributed
systems along the way, aimed at programmers coming from C, Python or Go:

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
13. [A Room with a View](docs/book/13-a-room-with-a-view.md)
14. [Changing the Tyres at Full Speed](docs/book/14-changing-the-tyres.md)
15. [Ready for Production](docs/book/15-ready-for-production.md) *(in progress)*
- [Appendix: Rust for C, Python, and Go Programmers](docs/book/16-appendix-rust.md)

## What's inside

Twelve burger-named subsystems in one binary — Grill (runtimes), Mustard
(gossip), Council (Raft), Meat (scheduler), Onion (discovery/DNS/eBPF),
Wrapper (ingress), Sesame (security), Pickle (registry), Mayo (metrics),
Ketchup (logs), Smoker (chaos), Brioche (dashboard). The component tour and
repo layout live in the manual (`relish manual`, "Under the hood") and the
[design docs](docs/design/).

## Try it

```sh
make test                    # run the portable nextest suite
make audit                   # reject new RustSec dependency findings
relish test --profile development # run the 39-case live-cluster catalogue
make examples                # validate and dry-run every example config
make observability-demo      # start bun, collect metrics, query APIs, show dashboard
make pickle-test-macos       # push/pull a Docker image through the Pickle registry
```

The Phase 15 runner and 39-case catalogue are live. Bun also exposes durable,
server-owned app/namespace leases, and the runner now uses one for every case.
Container cases use one digest-pinned BusyBox 1.37.0 OCI index, accepted through
both runc and Apple Container. Real node and pressure primitives are the next
Phase 15 prerequisite.

## Licence

[Apache 2.0](LICENSE)
