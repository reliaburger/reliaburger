# Preface {.unnumbered}

Kubernetes works. Millions of containers run on it every day, and for good reason. But it's also 2.5 million lines of Go, backed by an ecosystem of add-ons that could fill a phone book, if phone books were still a thing. For large organisations with platform teams, that's fine. For a small team running a handful of machines, it's a lot of overhead for what should be a simple job: run these containers, keep them healthy, route traffic to them.

I wanted to understand what a simpler alternative would look like. Not a toy, but a real orchestrator that combined containers, networking, security, observability, and deployments into a single binary. Something you could download and run without a week of setup.

So I built one.

## Trust you can check

There's a second reason, and it got stronger while I was writing.

How do you decide that a piece of infrastructure is safe to run? If you're honest, you probably do what I did for years: you look at how many people use it. Thousands of companies run this, so someone would have noticed by now. Eric Raymond named it [Linus's Law](https://en.wikipedia.org/wiki/Linus%27s_law): "given enough eyeballs, all bugs are shallow".

It was always a hopeful rule. [Heartbleed](https://heartbleed.com/) lived in OpenSSL, about as widely used as code gets, for over two years. The [xz backdoor](https://www.openwall.com/lists/oss-security/2024/03/29/4) was caught by one engineer who wondered why his ssh logins had got slower, in a library whose maintainer had described it as ["an unpaid hobby project"](https://research.swtch.com/xz-timeline). Lots of users, very few eyes.

Then the eyes stopped being human. In November 2026 OpenAI reported that one run of an unreleased model built working root exploits for 948 distinct Linux kernel bugs, for about $1.2 million in API credits ([OpenAI](https://openai.com/index/a-thousand-fewer-paths-to-root/), [SecurityWeek](https://www.securityweek.com/ai-agents-got-root-on-linux-via-948-kernel-flaws-in-openai-research/)). Anthropic scanned more than 1,000 open-source projects and estimated 6,202 high- or critical-severity bugs among them ([Anthropic](https://www.anthropic.com/research/glasswing-initial-update)). The kernel, the most-watched code base on the planet, went from about 500 vulnerabilities found a release at 6.9 to nearly 2,000 at 7.2 ([Linux Pro Magazine](https://www.linuxpromagazine.com/index.php/layout/set/print/Online/News/2-000-Vulnerabilities-per-Linux-Release)). Popularity doesn't protect you any more. If anything, it makes you the first thing everyone points their models at.

So what *would* make an orchestrator worth trusting? I think it's three things, and you'll see each of them in this book:

- **You can check it.** The code is open, and every release `relish` carries the exact source it was built from, so your agent can read the code your cluster runs (chapter 19).
- **There's less of it, in a safer language.** One code base in Rust instead of Kubernetes plus a dozen add-ons, each with its own advisories. Rust doesn't make a program correct, but it removes whole classes of memory bugs, and Google found memory-safety vulnerabilities about a thousand times rarer per line in Android's Rust than in its C and C++ ([Google](https://security.googleblog.com/2025/11/rust-in-android-move-fast-fix-things.html)).
- **A fix reaches every node quickly.** It's one binary, so one patch is one upgrade, and once you ask, the cluster rolls it out node by node by itself (chapter 14).

I'll be straight about the limits too, because you'll find them anyway. The source is about 274,000 lines of Rust outside the tests and pulls in over 500 crate versions. That's not small, and we haven't yet measured Kubernetes and its add-ons the same way, so I can't yet tell you how they compare. Machines check it every day (fuzzing, mutation testing, a dependency audit), but LLM reviews still happen when someone starts one, not on a schedule. And one binary on every node is also one target on every node. That's the deal: we can't promise there are no bugs, so we make them as cheap as possible to find, and as quick as possible to fix.

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

There are two ways to read it, and both work.

The first is front to back. Start at Chapter 1 and go forwards. Each chapter builds on the previous one, and Rust concepts are introduced inline, the first time they appear — ownership when we first move a value, traits when we first need one, async when the agent first has to juggle several things at once. You learn the language in the order the code needs it. This is the path most readers take.

The second is for people who hate surprises. Start at the end. There's a back-of-book appendix, *Rust for C, Python, and Go Programmers*, that lays out the whole language in one go: the memory model, types, traits, error handling, async, the lot. Read it first and the scattered "here's a new bit of Rust" explanations in the chapters become reinforcement — a second look at something you've already met — rather than first contact. If you'd rather have the full picture before you start, or you already know you learn languages better as a system than in fragments, begin there and then come back to Chapter 1.

Either way: the code compiles and passes tests at the end of every chapter, so you can stop anywhere and have a working (if incomplete) system.

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
