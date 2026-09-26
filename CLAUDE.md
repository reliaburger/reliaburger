# Reliaburger — Project Guide

## What This Is

Reliaburger is a batteries-included container orchestrator written in Rust. It is a single binary that replaces Kubernetes + its ecosystem of add-ons with something dramatically simpler.

This project produces two things simultaneously:

1. **A working implementation** — complete, testable, simple, bug-free.
2. **A book** — "Building Reliaburger" — that walks through how we built all of it, teaching Rust and distributed systems design at the same time.

IMPORTANT: Remember that we're also writing the book in docs/book/*.md, where we explain everything we're doing, why we're doing it like that, and what else we decided not to do. Incorporate that when planning any change. The target audience knows C, Python or Go, but not Rust: focus on how Rust differs, and explain any Rust syntax the first time it appears.

For book, manual or website prose, follow [docs/book/STYLE.md](docs/book/STYLE.md).

## Project Structure

- `docs/` — [roadmap.md](docs/roadmap.md), [progress.md](docs/progress.md), [testing.md](docs/testing.md), [releasing.md](docs/releasing.md), [quickstart.md](docs/quickstart.md), [whitepaper.md](docs/whitepaper.md)
- `docs/design/` — one design doc per subsystem, plus [test-harness.md](docs/design/test-harness.md)
- `docs/book/` — the "Building Reliaburger" chapters
- `docs/manual/` — the user manual (compiled into `relish`)
- `docs/plans/` — dated plans and reviews; finished ones move to `docs/plans/archive/`
- `docs/qualification/` — dated release-gate and audit records
- `docs/website/` — the project homepage and `install.sh`
- `docs/talks/` — conference talk material
- `docs/_quarto/` — PDF build configuration (Quarto profiles)
- `src/` — Rust source
- `ebpf/` — eBPF C programs (Onion service discovery, Smoker faults)
- `brioche/` — built web dashboard assets
- `benches/` — Criterion gossip benchmarks
- `tests/suite/` — the single portable integration binary; `tests/*.rs` — gated, heavy or process-isolated suites
- `examples/` — `phase-1/` and `phase-8/` workload configs, `kubernetes/` YAML
- `scripts/release/` — packaging, staging and `qualify-*.sh` release qualification
- `.github/workflows/` — `ci.yml`, `security.yml`, `build.yml`, `stage.yml`, `promote.yml`, `soak.yml`, `v02-loops.yml`, `static.yml`

## How We Work

### 1. Follow the Plan

Every roadmap phase is done. Current work is closing out the 0.1.0 release: read the status box at the top of [docs/progress.md](docs/progress.md) and the plan it links before starting. New work goes through a dated plan in `docs/plans/`.

### 2. Tests First

Every change starts by writing failing tests. Then we implement until the tests pass. This is non-negotiable.

- **Unit tests**: written first for each module, testing isolated logic with mocked dependencies.
- **Integration tests**: written first for each subsystem, testing real behaviour against a running node or cluster.

### 3. Learn Rust Along the Way

This project is a vehicle for learning Rust:

- Explain Rust concepts when they first appear (ownership, borrowing, lifetimes, traits, async, unsafe, FFI).
- Prefer idiomatic Rust over clever tricks. Simple and clear beats short and obscure.
- Use the standard library and well-established crates (tokio, serde, clap, axum, ratatui). Don't reinvent what exists.
- When a design decision is driven by Rust's type system or ownership model, explain why.

### 4. Write the Book as We Go

Each roadmap phase produced one chapter. A chapter combines:

- **Design narrative**: why this component exists, what problem it solves, how it fits into the whole. Draw from the whitepaper and design docs.
- **Rust walkthrough**: the actual implementation, explained step by step. Code listings with commentary.
- **Test explanations**: why each test exists, what it validates, how to read test output.
- **Lessons learned**: what was tricky, what we'd do differently, what Rust concept clicked.

Fixes and hardening update the existing chapter for that subsystem; they don't get a new chapter. Chapter mapping (`docs/book/`):

| Phase | File | Title |
|-------|------|-------|
| — | `00-preface.md` | "Preface" |
| 1 | `01-hello-container.md` | "Hello, Container" |
| 2 | `02-finding-friends.md` | "Finding Friends" |
| 2 | `02a-code-walkthrough.md` | "Code Walkthrough: Phase 2" |
| 3 | `03-talking-to-each-other.md` | "Talking to Each Other" |
| 4 | `04-trust-no-one.md` | "Trust No One (Until They Prove It)" |
| 5 | `05-where-the-images-live.md` | "Where the Images Live" |
| 6 | `06-watching-everything.md` | "Watching Everything" |
| 7 | `07-ship-it.md` | "Ship It" |
| 8 | `08-breaking-things-on-purpose.md` | "Breaking Things on Purpose" |
| 9 | `09-the-full-package.md` | "The Full Package" |
| 10 | `10-locking-it-down.md` | "Locking It Down" |
| 11 | `11-eyes-everywhere.md` | "Eyes Everywhere" |
| 12 | `12-squeezing-every-drop.md` | "Squeezing Every Drop" |
| 13 | `13-a-room-with-a-view.md` | "A Room with a View" |
| 14 | `14-changing-the-tyres.md` | "Changing the Tyres at Full Speed" |
| 15 | `15-ready-for-production.md` | "Ready for Production" |
| — | `16-appendix-rust.md` | "Appendix: Rust for C, Python, and Go Programmers" |

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
- **Testing**: [cargo-nextest](https://nexte.st) (`make test`) plus `make test-doc` for doctests; proptest for property-based, insta for snapshots
- **MSRV**: `rust-version` in `Cargo.toml`; release builds pin the compiler (see "Compiler baseline" in [docs/releasing.md](docs/releasing.md))
- **Toolchain**: CI uses the latest stable Rust, so keep your local `stable` current and lint with `-D warnings`, or lints diverge
- **Formatting**: rustfmt defaults, enforced in CI
- **Linting**: clippy with default lints, warnings are errors
- **Logging**: diagnostics go to stderr via `eprintln!`; there is no `tracing` or `log` crate

## Testing

See [docs/testing.md](docs/testing.md) and [docs/design/test-harness.md](docs/design/test-harness.md).

- Tests run under nextest. The portable suite is the unit tests plus the single `tests/suite/` binary: add a portable integration test there as a module. A new `tests/*.rs` binary is only for gated, heavy or process-isolated tests.
- `make ci` runs `fmt-check`, `lint`, `test` and `test-doc`, portable only. Also run the gated target matching what you touched: `make test-cluster`, `test-linux`, `test-rootless-runc`, `test-upgrade` (or `test-upgrade-node` / `test-upgrade-cluster`), `test-slow`, `test-apple`, `bench` / `bench-large`.
- Retries are 0 (`.config/nextest.toml`). A failure that passes on re-run goes in the "Known flakes" register in [docs/progress.md](docs/progress.md) the same day.
- The `full-ci` PR label runs the heavy suites on a stacked PR.
- The coverage floor `COVERAGE_MIN_LINES` in the `Makefile` is never lowered.

## Releases & Compatibility

- Releases follow [docs/releasing.md](docs/releasing.md): `build.yml` → `stage.yml` → qualification with `scripts/release/qualify-*.sh` (including the V02 soak, `qualify-sustained.sh --tier fast|final`), recorded in `docs/qualification/` → `promote.yml`.
- Only the maintainer tags and promotes. Agents never do.
- Any change to a wire or durable-state format bumps `protocol` or `state` in `CURRENT` in `src/compatibility.rs`.
- No legacy code, shims or migrations before 0.1.0. Old state is refused; start a fresh cluster.

## Commit Hygiene

- Run `make ci` plus the matching gated target before committing.
- `cargo fmt` and `cargo clippy` with `-D warnings` must be clean.
- Never amend a commit (`git commit --amend`); always make a new one.
- Update the book chapter in the same change, not at the end.
- Update `README.md` and `docs/README.md` when features, commands or status change. Never quote test counts.

## Example Naming Convention

Example configs in `examples/phase-N/` use a runtime prefix so it's immediately clear what runtime they target:

- **`proc-*`** — ProcessGrill (runs local processes, no container runtime needed). Uses `proc-grill:image-ignored` as the image name and `target/debug/testapp` or shell commands as the command.
- **`container-*`** — Real OCI images pulled from Docker Hub. Works with runc (`--runtime runc`) or Apple Container (`--runtime apple`).
- **`apple-*`** — Tests Apple Container-specific features (macOS only).
- **`runc-*`** — Tests runc-specific features (Linux only).

When adding a new example, pick the prefix that matches its runtime requirement.

## Rust Best Practices

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

**Tokio sync for shared mutable state.** Use `Arc<tokio::sync::RwLock<T>>` for read-heavy shared data. Use `Arc<tokio::sync::Mutex<T>>` for infrequent mutations. Prefer the tokio sync stack.

**`std::sync::Mutex` only in synchronous callbacks.** Never hold one across `.await`. It's allowed only in a synchronous callback with a short critical section and a comment saying why (as `src/wrapper/tls.rs` does).

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

**Async tests** use `#[tokio::test]`, not `#[test]` with a manual runtime. Don't combine `tokio::spawn` with `start_paused`; drive the future manually instead.

### Comments and Documentation

**`///` on every public item.** Explain *what* it represents, not *how* it works. For functions, say what the caller should expect.

**`//` for *why*, not *what*.** If the code needs a comment explaining what it does, rewrite the code to be clearer.

```rust
// Skip .0 network and .255 broadcast addresses in the VIP range
let vip = (hash % 254) + 1;
```

**No obvious comments.** `// increment the counter` above `counter += 1` is noise.

**`// SAFETY:`** on every `unsafe` block, explaining why the invariants hold.

**`// TODO(<progress.md item or plan>):`** for deferred work, naming the `docs/progress.md` item or plan that will address it.

### What NOT to Do

- **No `unsafe` without a `// SAFETY:` comment.** If you can't explain why it's safe, don't use `unsafe`.
- **No premature abstraction.** Don't write a trait until you have two implementations. Write the concrete version first.
- **No `Box<dyn Error>`.** Use `thiserror` enums or `anyhow::Error`.
- **No stringly-typed APIs.** Don't pass state names, action types, or config keys as `&str`. Use enums or newtypes.
- **No deep nesting.** More than three levels of indentation? Use early returns, `?`, guard clauses, or extract a function.
- **No god structs.** If a struct has more than ~10 fields, check whether it mixes separate concerns.
- **No panicking in production code.** No `unwrap()`, `expect()`, `panic!()`, or `todo!()` outside of tests.

### Tracking Progress

- [docs/progress.md](docs/progress.md) is the checklist. Check an item off only when it compiles, passes tests and is committed.
- Use `// TODO(<progress.md item or plan>):` for deferred work.
