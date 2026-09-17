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

- **A built-in guide.** `relish manual` provides searchable documentation
  and runnable examples without a repo checkout or internet connection.
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

Source builds require Rust 1.97 or later; releases use Rust 1.98.0 and the
committed lockfile.

```sh
cargo build --locked --bins

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

Thirteen burger-named subsystems in one binary — Grill (runtimes), Mustard
(gossip), Council (Raft), Meat (scheduler), Onion (discovery/DNS/eBPF),
Wrapper (ingress), Sesame (security), Pickle (registry), Mayo (metrics),
Ketchup (logs), Lettuce (GitOps), Smoker (chaos), Brioche (dashboard). The component tour and
repo layout live in the manual (`relish manual`, "Under the hood") and the
[design docs](docs/design/).

## Try it

Use the source-based quick start above while we prepare the first release.
From a checkout, install cargo-nextest 0.9.145 or newer (see
[build prerequisites](docs/README.md#building)), then run the portable tests and
check the examples:

```sh
make test                    # run the portable nextest suite
make audit                   # check dependency advisories
make examples                # validate and dry-run every example config
```

With a running, configured cluster and `relish` on your PATH:

```sh
relish status
relish wtf                   # diagnose the live cluster
relish test --profile development
```

The managed laptop flow is now available in the source and undergoing VM
qualification. Once the signed release and website are published, the install
path will be:

```sh
curl -fsSL https://reliaburger.com/install.sh | bash
export PATH="$HOME/.reliaburger/bin:$PATH"
relish nodes
relish status
relish logs hello
relish dashboard             # Ctrl-C stops the browser connection
# Open http://localhost:18080/
relish local stop
relish local start
relish local destroy --yes   # permanently remove the cluster's VMs and data
```

See the [laptop quickstart](docs/quickstart.md) for prerequisites, single-node
setup, retries and development qualification. The public release is still pending.

See the [diagnostics guide](docs/manual/07_diagnostics.md) for test prerequisites,
profiles and interpreting results.

## Getting to 0.1.0

Node startup now verifies the agent response, version and critical subsystem
readiness before setup reports success. Managed setup also checks the council
and the sample app through ingress; end-to-end qualification is still in progress.

The core platform is implemented. We're preparing a release that takes a
laptop to three healthy Linux nodes and a working sample app, with no Rust
build or repo checkout. **Under five minutes is the target; the public
installer and that timing guarantee aren't available yet.**

The [0.1.0 release plan](docs/plans/2026-09-16-v0.1.0-release-plan.md)
sets out the remaining work, in order:

1. **Establish the release baseline.** Reconcile the older TODO lists,
   define supported platforms and run the complete test matrix.
2. **Finish correctness fixes.** Make readiness checks trustworthy, bound
   registry upload memory and close the remaining consequential bugs.
3. **Package and sign the release.** Publish Linux binaries, native macOS
   CLI binaries, eBPF assets and matching release metadata.
4. **Make laptop clusters reliable.** Use prebuilt assets and managed Linux
   VMs, with secure enrolment, resumable setup and access from the host CLI
   and browser.
5. **Add the installer and first-run flow.** Install, start the cluster and
   deploy a sample app, with clear progress, errors and cleanup commands.
6. **Prove the downloadable release works.** Test clean installations,
   restart and recovery, and measure the full three-node start with empty
   caches before publishing the five-minute claim.

Each step includes tests and updates to the documentation and book. Detailed
acceptance gates and deferred features live in the release plan; implementation
history remains in [progress.md](docs/progress.md).

The [17 September codebase audit and completion plan](docs/plans/2026-09-17-codebase-completion-plan.md)
reconciles the older TODOs, records remaining correctness gaps and separates
release acceptance from deferred capabilities. The current checklist lives in
[progress.md](docs/progress.md).

Log exports now preserve content generations, scope receipts to the destination,
and serialise durable checkpoint updates across agent and offline exports. Source
and checkpoint errors stop the export and prevent disk-pressure pruning.

## Licence

[Apache 2.0](LICENSE)
