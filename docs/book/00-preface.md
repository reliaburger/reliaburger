# Preface {.unnumbered}

Kubernetes works. Millions of containers run on it every day, and for good reason. But it's also 2.5 million lines of Go, backed by an ecosystem of add-ons that could fill a phone book, if phone books were still a thing. For large organisations with platform teams, that's fine. For a small team running a handful of machines, it's a lot of overhead for what should be a simple job: run these containers, keep them healthy, route traffic to them.

I wanted to understand what a simpler alternative would look like. Not a toy, but a real orchestrator that combined containers, networking, security, observability, and deployments into a single binary. Something you could download and run without a week of setup.

So I built one.

## What this book is

This is the story of building Reliaburger, a batteries-included container orchestrator written in Rust. You'll build it with me, one chapter at a time, from an empty `cargo new` to a system that can:

- Run containers on multiple machines
- Discover services automatically via eBPF
- Encrypt all traffic with mutual TLS
- Push and replicate images in a built-in registry
- Collect metrics and logs without external agents
- Roll out new versions with automatic rollback

Each chapter corresponds to one implementation phase. We write failing tests first, then implement until they pass. The code is real, the tests are real, and by the end you'll have a working orchestrator.

## Who this book is for

You're a programmer who knows at least one other language — C, Python, Go, Java, whatever. You can read code, understand pointers (or at least have a working theory about them), and you've deployed something to a server at least once.

You don't need to know Rust. We'll learn it together as we go. Every time a new Rust concept shows up — ownership, borrowing, lifetimes, traits, async — I'll explain it in terms of what you already know from other languages. If you've ever wondered why the borrow checker is yelling at you, you're in the right place.

You also don't need to be a distributed systems expert. We'll build up the concepts from first principles: gossip protocols, consensus algorithms, service discovery, PKI. But you should know what a TCP connection is, and "distributed" shouldn't be an entirely foreign word.

## What you'll learn

Two things, simultaneously:

**Rust.** Not academic Rust — practical Rust. The kind you write when you need something that compiles to a single binary, runs fast, and doesn't segfault at 3am. We'll use the standard library, tokio for async, axum for HTTP, serde for serialisation, and a handful of other well-established crates. No unsafe code except where the kernel demands it (eBPF), and every unsafe block gets a `// SAFETY:` comment.

**Distributed systems.** Not the textbook kind — the practical kind. How gossip protocols actually converge. Why Raft is simpler than Paxos but still subtle. What happens when the network partitions and half your cluster is gone. How to build something that keeps working when individual pieces fail. We'll break things deliberately and watch the system recover.

## How to read this book

Start at Chapter 1 and go forwards. Each chapter builds on the previous one. The code compiles and passes tests at the end of every chapter — you can stop anywhere and have a working (if incomplete) system.

The chapters follow a pattern:

1. **What we're building** — the problem and why it matters
2. **Design** — how we'll solve it, and what alternatives we rejected
3. **Implementation** — the actual code, explained step by step
4. **Tests** — what we're testing, why, and how to read the output
5. **Lessons learned** — what was tricky, what clicked, what we'd do differently

If you want to type along, the full source is available on GitHub. If you'd rather read and understand, that works too. The code listings are complete enough to follow without a terminal open.

## A note on naming

Every component in Reliaburger has a food-related name. The agent is Bun. The gossip protocol is Mustard. The scheduler is Meat. The ingress proxy is Wrapper. The security layer is Sesame. The registry is Pickle. The metrics store is Mayo. The log collector is Ketchup. The dashboard is Brioche.

This started as a joke and stuck. It turns out that `bun start` is more memorable than `reliaburger-agent-daemon start`, and "the Mustard protocol" is easier to discuss than "the SWIM-based gossip subsystem". The names make the system easier to talk about, and they make the code more fun to read. That's reason enough.

Let's build something.
