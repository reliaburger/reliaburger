# Reliaburger — Project Guide

## What This Is

Reliaburger is a batteries-included container orchestrator written in Rust. It is a single binary that replaces Kubernetes + its ecosystem of add-ons with something dramatically simpler.

This project produces two things simultaneously:

1. **A working implementation** — complete, testable, simple, bug-free.
2. **A book** — "Building Reliaburger" — that walks through how we built all of it, teaching Rust and distributed systems design at the same time.

IMPORTANT: Remember that we're also writing the book in docs/book/*.md, where we're going to explain everything that we're doing, why we're doing it like that, and what else we decided not to do. Incorporate that when planning any change. The target audience are programmers familiar wth other languages (e.g. C, Python, Go), but not Rust. Focus on how Rust differs from other languages.

Each section must explain any Rust syntax appearing for the first time. The target audience knows C/Python/Go but not Rust.

## Project Structure

```
docs/
  README.md            # User documentation (install, build, run)
  whitepaper.md        # Full architectural vision (the "what and why")
  roadmap.md           # Implementation phases with tests-first ordering (the "when")
  progress.md          # Implementation checklist (what's done, what's next)
  design/              # Detailed design docs per component (the "how")
    agent-bun.md       # Node agent, container lifecycle, health checks
    cli-relish.md      # CLI and TUI
    gossip-mustard.md  # SWIM gossip protocol
    scheduler-meat.md # Scheduler and placement
    discovery-onion.md # eBPF service discovery
    ingress-wrapper.md # Ingress proxy
    security-sesame.md # PKI, mTLS, secrets, firewall
    registry-pickle.md # OCI image registry
    metrics-mayo.md    # Time-series metrics
    logs-ketchup.md    # Log collection
    ui-brioche.md      # Web dashboard
    gitops-lettuce.md  # GitOps engine
    deployments.md     # Deploy orchestration
    chaos-smoker.md    # Fault injection
  book/                # "Building Reliaburger" book chapters
    00-preface.md      # Preface
    01-hello-container.md        # Phase 1: Foundation
    02-finding-friends.md        # Phase 2: Cluster Formation
    03-talking-to-each-other.md  # Phase 3: Networking
    04-trust-no-one.md           # Phase 4: Security
    05-where-the-images-live.md  # Phase 5: Storage & Registry
    06-watching-everything.md    # Phase 6: Observability
    07-ship-it.md                # Phase 7: GitOps & Deployments
    08-breaking-things-on-purpose.md  # Phase 8: Advanced
    09-the-full-package.md       # Phase 9: User Experience
    10-locking-it-down.md        # Phase 10: Advanced Security
    11-eyes-everywhere.md        # Phase 11: Advanced Observability
    12-squeezing-every-drop.md   # Phase 12: Optimisations
    13-a-room-with-a-view.md     # Phase 13: Relish TUI
    14-changing-the-tyres.md     # Phase 14: Self-Upgrade
    15-ready-for-production.md   # Phase 15: Testing & Diagnostics
  _quarto/             # PDF build configuration (Quarto profiles)
src/                   # Rust source (created as we go)
```

## How We Work

### 1. Follow the Roadmap

We implement Reliaburger sequentially, one ROADMAP phase at a time. Each phase builds on the previous — no skipping ahead. The phases are:

1. Foundation (single-node container lifecycle)
2. Cluster Formation (gossip, Raft, scheduling)
3. Networking (eBPF service discovery, ingress)
4. Security (PKI, mTLS, secrets)
5. Storage & Registry (Pickle, volumes)
6. Observability (metrics, logs, dashboards)
7. GitOps & Deployments (rolling deploys, dependency ordering)
8. Advanced (chaos testing, process workloads, batch)
9. User Experience (blue-green, autoscaling, GitOps, K8s migration)
10. Advanced Security (workload identity, image signing, token management)
11. Advanced Observability (PromQL, hierarchical aggregation, full Brioche UI)
12. Optimisations (nftables maps, P2P downloads, compression, caching)
13. Relish TUI (interactive terminal interface)
14. Self-Upgrade (rolling binary replacement)
15. Testing & Diagnostics (test runner, benchmarks, wtf, trace)

### 2. Tests First

Every phase starts by writing failing tests. Then we implement until the tests pass. This is non-negotiable.

- **Unit tests**: Written first for each module, testing isolated logic with mocked dependencies.
- **Integration tests**: Written first for each subsystem, testing real behavior against a running node/cluster.
- **Chaos tests**: Written when the relevant infrastructure exists (Phases 2 and 8).

The ROADMAP lists the specific tests for each phase under "Tests (write first)."

### 3. Learn Rust Along the Way

This project is a vehicle for learning Rust. As we implement each phase:

- Explain Rust concepts when they first appear (ownership, borrowing, lifetimes, traits, async, unsafe, FFI).
- Prefer idiomatic Rust over clever tricks. Simple and clear beats short and obscure.
- Use the standard library and well-established crates (tokio, serde, clap, axum, ratatui). Don't reinvent what exists.
- When a design decision is driven by Rust's type system or ownership model, explain why.

### 4. Write the Book as We Go

Each ROADMAP phase produces one chapter of the book. A chapter combines:

- **Design narrative**: Why this component exists, what problem it solves, how it fits into the whole. Draw from the whitepaper and design docs.
- **Rust walkthrough**: The actual implementation, explained step by step. Code listings with commentary.
- **Test explanations**: Why each test exists, what it validates, how to read test output.
- **Lessons learned**: What was tricky, what we'd do differently, what Rust concept clicked.

The book chapters live in `docs/book/` and are written in Markdown. Chapter mapping:

| Phase | Chapter | Title |
|-------|---------|-------|
| 1 | Chapter 1 | "Hello, Container" |
| 2 | Chapter 2 | "Finding Friends" |
| 3 | Chapter 3 | "Talking to Each Other" |
| 4 | Chapter 4 | "Trust No One (Until They Prove It)" |
| 5 | Chapter 5 | "Where the Images Live" |
| 6 | Chapter 6 | "Watching Everything" |
| 7 | Chapter 7 | "Ship It" |
| 8 | Chapter 8 | "Breaking Things on Purpose" |
| 9 | Chapter 9 | "The Full Package" |
| 10 | Chapter 10 | "Locking It Down" |
| 11 | Chapter 11 | "Eyes Everywhere" |
| 12 | Chapter 12 | "Squeezing Every Drop" |
| 13 | Chapter 13 | "A Room with a View" |
| 14 | Chapter 14 | "Changing the Tyres at Full Speed" |
| 15 | Chapter 15 | "Ready for Production" |

## Quality Standards

- **Working**: Every feature must actually work, not just compile. If it's in the code, it's tested.
- **Testable**: Every behavior has a test. If you can't test it, redesign it until you can.
- **Simple**: The simplest implementation that passes the tests. No premature abstraction, no "just in case" code.
- **Bug-free**: Fix bugs before adding features. A smaller correct system beats a larger broken one.

## Conventions

- **Rust edition**: 2024
- **Async runtime**: tokio
- **Error handling**: thiserror for library errors, anyhow for binary/CLI
- **Serialization**: serde + toml for config, serde + serde_json for APIs
- **CLI parsing**: clap (derive API)
- **Web framework**: axum
- **TUI framework**: ratatui + crossterm
- **Testing**: cargo test for unit/integration, proptest for property-based, insta for snapshots
- **Formatting**: rustfmt defaults, enforced in CI. Always run `cargo fmt` before committing.
- **Linting**: clippy with default lints, warnings are errors in CI

## Example Naming Convention

Example configs in `examples/phase-N/` use a runtime prefix so it's immediately clear what runtime they target:

- **`proc-*`** — ProcessGrill (runs local processes, no container runtime needed). Uses `proc-grill:image-ignored` as the image name and `target/debug/testapp` or shell commands as the command.
- **`container-*`** — Real OCI images pulled from Docker Hub. Works with runc (`--runtime runc`) or Apple Container (`--runtime apple`).
- **`apple-*`** — Tests Apple Container-specific features (macOS only).
- **`runc-*`** — Tests runc-specific features (Linux only).

When adding a new example, pick the prefix that matches its runtime requirement.

## Rust Best Practices

The Conventions section says *what* crates to use. This section says *how* to write the code.

### Naming

- **Types**: `PascalCase`. Spell out the full word. `HealthChecker`, not `HC`. `ResourceSummary`, not `ResSumm`.
- **Functions**: `snake_case`, verb-first. `check_namespace_quota`, `select_best_peer`, `fetch_layer`.
- **Constants**: `SCREAMING_SNAKE_CASE`. `MAX_PIGGYBACK_UPDATES`, `DEFAULT_TIMEOUT`.
- **Modules**: one word where possible (`gossip`, `scheduler`). Two words with underscore if needed (`service_map`).
- **Abbreviations**: avoid in public APIs. `instance_id`, not `inst_id`. `address`, not `addr` (exception: `SocketAddr` is std). Local variables in small scopes can abbreviate (`tx`, `rx`, `cfg`).
- **Spelling**: British English in doc comments and prose. American English in serde derives (`Serialize`, `Deserialize`) because that's what the crate exports.

### Types and Data Modelling

**Newtypes for identity.** Wrap bare `String` or `u64` identifiers so the compiler prevents mix-ups:

```rust
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct NodeId(pub String);
```

**Standard derive set.** Start with `Debug, Clone`. Add others only when needed:

- `Hash, Eq, PartialEq` — for map keys and set members.
- `Serialize, Deserialize` — for anything that crosses a wire or gets stored.
- `Copy` — for small value types: fieldless enums, numeric wrappers.
- `Default` — only when the struct has genuinely sensible defaults.

Don't derive speculatively. If nothing hashes it, don't derive `Hash`.

**State machines as enums.** Model lifecycle states as exhaustive enums. Use `match` to force every state to be handled. No sentinel values, no stringly-typed states.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    Pending,
    Preparing,
    Running,
    Unhealthy,
    Stopped,
    Failed,
}
```

**`Option<T>`, not sentinels.** Every optional field is `Option<T>`. No `-1` meaning "not set", no empty strings meaning "absent".

**Collections:**

- `HashMap` — unordered lookups (the common case).
- `BTreeMap` — ordered data, deterministic serialisation (labels, tags).
- `Vec` — ordered sequences.

**`PathBuf` for filesystem paths.** Never `String`. `&Path` is the borrow.

**`#[repr(C)]` for FFI.** Structs shared with the kernel (eBPF maps, C libraries) must use `#[repr(C)]` with explicit `_pad` fields for alignment.

### Error Handling

**`thiserror` for library errors.** Each subsystem defines its own error enum. Variants carry enough context to diagnose the problem:

```rust
#[derive(Debug, thiserror::Error)]
pub enum QuotaError {
    #[error("namespace {namespace:?} exceeds max apps: {current}/{limit}")]
    MaxAppsExceeded { namespace: String, current: u32, limit: u32 },
}
```

**`anyhow` for binaries.** `main()` and CLI handlers use `anyhow::Result`. Add context with `.context()`:

```rust
let config = NodeConfig::from_file(&path)
    .with_context(|| format!("failed to load config from {}", path.display()))?;
```

**Use `?` everywhere.** Don't manually match on `Result` unless you need a specific variant.

**No `.unwrap()` in library code.** In tests, `.unwrap()` is fine. In production code, use `?`. The only exception is provably infallible cases (e.g., a compile-time regex), with a comment explaining why.

**Error messages are lowercase, no trailing full stop.** The caller adds context about *where*; the error describes *what*.

### Async Patterns

**Single tokio runtime.** One `#[tokio::main]`. Never create additional runtimes.

**Subsystems as spawned tasks.** Each subsystem is a `tokio::spawn`ed long-lived task. They communicate via channels, not shared mutexes.

**Channel selection:**

- `tokio::sync::mpsc` — multiple producers, single consumer. The default for command queues.
- `tokio::sync::watch` — single producer, multiple consumers, latest-value only. For config or routing table updates.
- `tokio::sync::oneshot` — single request-response.
- `tokio::sync::broadcast` — rarely needed. Prefer `watch` unless every subscriber must see every event.

**Never block the runtime.** No `std::thread::sleep`, no blocking I/O, no heavy CPU work on async tasks. Use `tokio::task::spawn_blocking` when you must:

```rust
let hash = tokio::task::spawn_blocking(move || {
    compute_sha256(&data)
}).await?;
```

**Explicit timeouts.** Wrap fallible async operations in `tokio::time::timeout`. Don't rely on TCP timeouts.

**Graceful shutdown.** Use `tokio_util::sync::CancellationToken` (or a shutdown channel). Every long-lived task must check for cancellation and clean up.

### Ownership and Borrowing

**Borrow by default.** Function parameters take `&str` not `String`, `&Path` not `PathBuf`, `&[T]` not `Vec<T>` — unless the function needs to own the data.

**Clone across channel boundaries.** Channels take ownership. Clone before sending. This is expected, not a code smell.

**`Arc` for shared read access.** When multiple tasks need the same data, wrap it in `Arc<T>`.

**Tokio sync for shared mutable state.** Use `Arc<tokio::sync::RwLock<T>>` for read-heavy shared data. Use `Arc<tokio::sync::Mutex<T>>` for infrequent mutations. Stick to the tokio sync stack.

**Never use `std::sync::Mutex` in async code.** It blocks the runtime. Use `tokio::sync::Mutex` instead.

### Testing

**Structure.** Unit tests go in `#[cfg(test)] mod tests` at the bottom of each source file. Integration tests go in `tests/` at the crate root.

**Name tests as behaviour sentences:**

```rust
#[test]
fn unhealthy_after_three_consecutive_failures() { ... }

#[test]
fn quota_rejects_when_cpu_limit_exceeded() { ... }
```

**What to test:**

- State machine transitions: every valid transition, every invalid one.
- Parsing: valid input, each category of invalid input, edge cases (empty, maximal).
- Business logic: happy path, each failure mode, boundary conditions.
- Don't test private helpers directly. Test them through the public API.

**Snapshot tests** (`insta`) for structured output — CLI rendering, serialised config, TUI frames.

**Property-based tests** (`proptest`) for algorithms with large input spaces — schedulers, allocators, port assignment.

**Async tests** use `#[tokio::test]`, not `#[test]` with a manual runtime.

### Comments and Documentation

**`///` on every public item.** Explain *what* it represents, not *how* it works. For functions, say what the caller should expect.

**`//` for *why*, not *what*.** If the code needs a comment explaining what it does, rewrite the code to be clearer.

```rust
// Skip .0 network and .255 broadcast addresses in the VIP range
let vip = (hash % 254) + 1;
```

**No obvious comments.** `// increment the counter` above `counter += 1` is noise.

**`// SAFETY:`** on every `unsafe` block, explaining why the invariants hold.

**`// TODO(Phase N):`** for deferred work, referencing the roadmap phase that will address it.

### What NOT to Do

- **No `unsafe` without a `// SAFETY:` comment.** If you can't explain why it's safe, don't use `unsafe`.
- **No premature abstraction.** Don't write a trait until you have two implementations. Write the concrete version first.
- **No `Box<dyn Error>`.** Use `thiserror` enums or `anyhow::Error`.
- **No stringly-typed APIs.** Don't pass state names, action types, or config keys as `&str`. Use enums or newtypes.
- **No deep nesting.** More than three levels of indentation? Use early returns, `?`, guard clauses, or extract a function.
- **No god structs.** If a struct has more than ~10 fields, check whether it mixes separate concerns.
- **No `std::sync::Mutex` in async code.** Use `tokio::sync::Mutex`.
- **No panicking in production code.** No `unwrap()`, `expect()`, `panic!()`, or `todo!()` outside of tests and explicitly unfinished phase code.

### Tracking Progress

Implementation progress lives in `docs/progress.md` — a checklist per roadmap phase.

- Check off an item only when it compiles, passes tests, and is committed.
- In code, use `todo!("Phase N")` for stubs that exist structurally but aren't implemented yet.
- Use `// TODO(Phase N):` for deferred work belonging to a later phase.
- Before starting work, check `docs/progress.md` to see what's done and what's next.
- When a phase is completed or significant progress is made, update both `docs/README.md` and the top-level `README.md` to reflect the current state (test counts, completed features, new CLI commands, new runtimes, etc.).

# Writing Style Guide

Analysed from *Chaos Engineering* (Manning, 2021) by Miko Pawlikowski. Key references used:
- [Chaos Engineering for People (Ch 13 excerpt)](https://www.linkedin.com/pulse/chaos-engineering-people-mikolaj-pawlikowski)
- [Breaking the top five myths around chaos engineering](https://www.cloudcomputing-news.net/news/breaking-the-top-five-myths-around-chaos-engineering/)
- [The First Cup of Chaos (Manning free article)](https://freecontent.manning.com/the-first-cup-of-chaos/)
- [Testing Apps in Real-World Conditions (Manning free article)](https://freecontent.manning.com/testing-apps-in-real-world-conditions/)
- [Chaos Engineering 2021 — Conf42 talk transcript](https://www.conf42.com/Chaos_Engineering_2021_Mikolaj_Pawlikowski_chaos_engineering_2021)
- [Break Things on Purpose podcast — Gremlin](https://www.gremlin.com/blog/podcast-break-things-on-purpose-mikolaj-pawlikowski-engineering-lead-at-bloomberg)

## Voice Summary

Miko writes like a knowledgeable colleague explaining something over coffee. Informal-authoritative. Direct, honest, confident without being arrogant. British English. Engineer's pragmatism: values clarity over elegance.

## DO — Miko's Natural Patterns

### 1. Vary sentence length dramatically
Long explanatory sentence, then short punchy one for emphasis.
- "Hundreds of different skills (hard and soft), a constantly shifting technological landscape, evolving requirements, personnel turnaround and a sheer scale of some organizations are all factors in how hard it can be to ensure no single points of failure. **She is a single point of failure.**"
- "I went looking for information. I wanted to understand *why*. [...] **So I wrote one.**"

### 2. Use contractions naturally
"it's", "don't", "they're", "won't", "didn't", "can't", "doesn't", "aren't", "you'll"
- "One problem with real life is that it's messy."
- "you don't need to remember the syntax of tc to implement it!"

### 3. Address the reader directly
Second person ("you", "your") and inclusive first person ("we", "let's").
- "Can you see where I'm going with this?"
- "You know your team, so do what you think works best."
- "Let's start with identifying single points of failure."

### 4. Ask rhetorical questions
Guide the reader's thinking by posing questions.
- "How do you think we could test them out?"
- "Would you do an inside job in production?"
- "Do you see any weak links?"

### 5. Prefer active voice
Name the agent. Say who did what.
- "Gruber discovered this in 1996" not "This was discovered by Gruber in 1996"
- "Studies show X" not "X has been shown"

### 6. Use "Now," and "After all," as transitions
- "Now, the team is made of individuals..."
- "After all, testing in production is an internet meme."
- "After all, to err is human."

### 7. Use dry, understated humour
Quick asides, not extended jokes. Self-deprecating. Move on immediately.
- "If only there was a methodology to uncover systemic problems in a distributed system... Oh wait!"
- "(spoiler: it's bad!)"

### 8. Define terms inline with parentheses on first use only
This is a genuine Miko pattern, but he does it once, not repeatedly.
- "(i.e. emergent properties of the system)"
- "(hard and soft)"

### 9. Lead with concrete examples, then extract principles
Describe the specific situation first, then define the concept. Never abstract-first.

### 10. Hedge honestly but briefly
Acknowledge uncertainty without throat-clearing.
- "it might be better to leave well alone"
- "The answer depends on many factors"
- NOT: "It is worth noting that" or "It should be emphasised that"

### 11. Use British English
"endeavour", "practise" (noun), "programme", "organisation", "colour", "recognised", "behaviour"

---

## DON'T — AI Patterns to Avoid

### 1. Em dash overuse
AI writes 15-40 em dashes (—) per chapter. Miko uses them sparingly. Replace most with commas, full stops, or parentheses. Keep only where they add genuine punch.

### 2. Formulaic sentence starters
NEVER start a sentence with: "Intriguingly,", "Reassuringly,", "Notably,", "Crucially,", "Importantly,", "Remarkably,", "Interestingly,"
These are the strongest AI tells. Let the content be interesting on its own.

### 3. Passive voice dominance
AI defaults to passive. Flip to active.
- BAD: "Psoriasis has been shown to increase cardiovascular risk"
- GOOD: "Psoriasis increases cardiovascular risk"
- BAD: "It is now recognised as a systemic disease"
- GOOD: "We now recognise it as a systemic disease"

### 4. Excessive parenthetical definitions
AI defines every term inline, every time. Define once on first use per chapter, then trust the reader. Don't re-define "cytokine", "biologic", or "keratinocyte" in every chapter.

### 5. Hedging throat-clearing
Delete entirely: "it is worth noting that", "it should be noted", "it is important to emphasise", "it is necessary to understand", "as previously mentioned"
Just say the thing.

### 6. Superlative stacking
AI piles on emphatic adjectives: "highly effective", "remarkably", "extraordinarily", "dramatic", "revolutionary", "transformative"
Let data speak for itself. "PASI 90 in 70% of patients" beats "remarkably effective therapy."

### 7. "Beyond X, Y" and "Whilst effective, Z"
These are formulaic AI transition patterns. Use natural transitions or just start the new idea.

### 8. Identical structural templates
AI makes every section follow the same pattern. Vary the structure: some sections can open with a question, some with a statistic, some with a historical anecdote, some with a direct statement.
