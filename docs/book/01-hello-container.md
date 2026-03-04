# Hello, Container

You're about to build a container orchestrator from scratch. By the end of this chapter, you'll have a Rust project that compiles, a CLI that responds to commands, and the skeleton of a node agent that will eventually run your containers. No containers yet — we'll get there. First, we need a foundation that isn't going to fall over the moment we start stacking things on top of it.

## Setting up Rust

Reliaburger is written in Rust, so you'll need a working Rust toolchain. If you've already got one, skip ahead. If not, this won't take long.

### Installing rustup

The standard way to install Rust is through [rustup](https://rustup.rs/), which manages your Rust toolchain versions and keeps everything up to date. Run this:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Follow the prompts. The defaults are fine. When it's done, restart your shell or run:

```sh
source "$HOME/.cargo/env"
```

Verify it worked:

```sh
$ rustc --version
rustc 1.85.0 (4d215e2de 2025-02-17)

$ cargo --version
cargo 1.85.0 (d73d2caf9 2024-12-20)
```

Your version numbers will probably be higher. That's fine — we just need 1.85 or later, because we're using the 2024 edition.

### Platform prerequisites

**Linux** needs a C linker and basic build tools. On Debian/Ubuntu:

```sh
sudo apt install build-essential
```

On Fedora:

```sh
sudo dnf groupinstall "Development Tools"
```

**macOS** needs the Xcode command line tools:

```sh
xcode-select --install
```

That gives you a C compiler and linker. You don't need the full Xcode IDE.

### Editor setup

Any editor works. If you want autocompletion, inline errors, and go-to-definition, install [rust-analyzer](https://rust-analyzer.github.io/). It's available as a VS Code extension, a Neovim plugin via LSP, and for most other editors too. We won't go deeper into editor setup here — the rust-analyzer docs cover it well.

### What's the 2024 edition?

Rust uses *editions* to introduce breaking language changes without breaking existing code. Each crate declares which edition it uses, and the compiler handles the rest. The 2024 edition (stabilised in Rust 1.85) is the latest, and it's what we'll use throughout this book. If you've used an earlier edition, the differences are mostly small refinements — the `unsafe` rules are a bit tighter, `impl Trait` is more flexible in return position, and a few other niceties. Nothing that will trip you up.

## The project skeleton

Now let's create the Reliaburger project. We're going to make some structural decisions here that will carry through the entire book, so it's worth understanding why we're doing things this way.

### One crate, two binaries

Reliaburger ships as a single binary — that's one of the core design principles. No plugins to install, no ecosystem to assemble, no dependency hell. But the single binary actually has two entry points:

- **bun** — the node agent. This is the long-running daemon that manages containers, runs health checks, participates in cluster gossip, and reports state to the leader. Every node in the cluster runs bun.
- **relish** — the CLI. This is what operators use to deploy apps, check status, stream logs, and generally interact with the cluster.

Both binaries share the same core library, `reliaburger`, which contains all the actual logic. The binaries are just thin entry points that wire things together.

We considered splitting these into separate crates in a Cargo workspace — a `crates/bun/` binary, a `crates/relish/` binary, and a `crates/reliaburger/` library. That's a perfectly valid pattern for larger Rust projects where you want strict dependency boundaries between crates. But for Reliaburger, a single crate with multiple `[[bin]]` targets is simpler: one `Cargo.toml`, one set of dependencies, and no inter-crate version management to think about. We can always split later if we need to. We won't need to.

### Cargo.toml

Here's the manifest:

```toml
[package]
name = "reliaburger"
version = "0.1.0"
edition = "2024"
license = "Apache-2.0"
description = "A batteries-included container orchestrator"

[lib]
name = "reliaburger"
path = "src/lib.rs"

[[bin]]
name = "bun"
path = "src/bin/bun.rs"

[[bin]]
name = "relish"
path = "src/bin/relish.rs"

[dependencies]
anyhow = "1"
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
thiserror = "2"
tokio = { version = "1", features = ["full"] }
toml = "0.8"

[dev-dependencies]
insta = { version = "1", features = ["json"] }
proptest = "1"
```

Let's walk through the choices.

**Edition 2024** — we already covered this. Use the latest.

**`[lib]` and `[[bin]]`** — the `[lib]` section declares the library crate, which is where all the real code lives. The `[[bin]]` sections (note the double brackets — that's TOML array syntax) declare the two binaries. Each points to its own entry-point file under `src/bin/`.

**Dependencies — why these six?**

- **tokio** — the async runtime. Reliaburger is fundamentally an async system: it manages network connections, health check timers, container events, and cluster gossip, all concurrently. Tokio is the standard choice here. The `"full"` feature enables everything — we'll need most of it eventually, and cherry-picking features buys us nothing at this stage.
- **serde** — serialisation and deserialisation. Every config file, every API payload, every state snapshot flows through serde. The `"derive"` feature lets us annotate structs with `#[derive(Serialize, Deserialize)]` instead of writing conversion code by hand.
- **toml** — Reliaburger uses TOML for all configuration. It's more readable than YAML for the kind of structured config we need, and it doesn't have YAML's footguns (the Norway problem, the boolean problem, the indentation-is-semantic problem).
- **clap** — argument parsing for the CLI. The `"derive"` feature lets us define CLI structure as Rust types, which the compiler then validates. We'll see this in action in a moment.
- **thiserror** — for defining error types in library code. It generates `Display` and `Error` implementations from an enum, with format strings that include context. No boilerplate.
- **anyhow** — for error handling in binaries. Where `thiserror` is about *defining* precise error types, `anyhow` is about *propagating* errors with context. The split is deliberate: library code uses `thiserror` (callers need to match on specific errors), binary code uses `anyhow` (errors just need to be reported clearly).

**Dev-dependencies — testing tools:**

- **insta** — snapshot testing. We'll use it to verify CLI output, serialised configs, and TUI rendering. You write a test that produces output, `insta` captures it in a file, and future runs compare against the snapshot.
- **proptest** — property-based testing. Instead of writing specific test cases, you describe the *properties* your code should satisfy, and proptest generates thousands of random inputs to find violations. We'll use it for the scheduler, port allocator, and other algorithmic code.

### The binaries

The bun entry point is minimal right now:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("bun: reliaburger node agent v{}", env!("CARGO_PKG_VERSION"));
    todo!("Phase 1")
}
```

`#[tokio::main]` sets up the async runtime. `env!("CARGO_PKG_VERSION")` pulls the version string from `Cargo.toml` at compile time — no version string duplication. And `todo!("Phase 1")` is our convention for code that exists structurally but isn't implemented yet. It compiles, but panics if you run it. That's intentional: it marks exactly where work needs to happen.

The relish CLI is a bit more interesting. Even as a skeleton, it shows the structure:

```rust
use std::path::PathBuf;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "relish", version, about = "Reliaburger CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Apply configuration from a file or directory.
    Apply { path: PathBuf },
    /// Show cluster and app status.
    Status,
    /// Stream logs from an app or job.
    Logs { name: String },
    /// Execute a command inside a running container.
    Exec {
        app: String,
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Show detailed info about an app, node, or job.
    Inspect { name: String },
}
```

This is clap's derive API. The `Cli` struct *is* the argument parser. Each variant of the `Command` enum is a subcommand. The `///` doc comments become the help text automatically. If someone passes invalid arguments, clap generates the error message. If someone runs `relish --help`, clap generates the help output. We get all of that for free — no manual parsing, no string matching.

Can you see what we're doing here? We're letting the type system do the work. The compiler guarantees that every subcommand is handled in the `match` statement. Add a new variant to `Command` and forget to handle it? Compilation error. That's the kind of guarantee we'll lean on throughout this book.

### Where do the tests go?

Rust has two kinds of tests:

**Unit tests** live in the same file as the code they test, inside a `#[cfg(test)] mod tests` block at the bottom. The `#[cfg(test)]` attribute means this module is only compiled when running `cargo test` — it doesn't bloat the production binary. Unit tests can access private functions, which is useful for testing internal logic without exposing it publicly.

**Integration tests** live in the `tests/` directory at the crate root. Each `.rs` file there becomes a separate test binary that can only use the crate's public API, just like any external consumer would. This is where we'll put our end-to-end tests — deploying apps, checking health, verifying restart behaviour.

Right now our `tests/integration.rs` has a single placeholder test. It's there to confirm the test infrastructure works. We'll replace it with real tests before writing any real code — that's the tests-first approach, and we'll stick to it religiously.

Run `cargo test` and you should see:

```
running 1 test
test placeholder ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

One test. It passes. We're off to a good start.

## The Rust toolbox

Before we write any real code, let's get familiar with the tools we'll use every day. Cargo is the Rust build system and package manager, but it's also the entry point for formatting, linting, testing, and more. You'll run these commands hundreds of times over the course of this book, so it's worth knowing what each one does.

### cargo build

Compiles your code. By default it builds in debug mode, which compiles fast but produces a slower, larger binary. Debug builds include overflow checks, debug symbols, and no optimisations — exactly what you want during development.

```sh
$ cargo build
   Compiling reliaburger v0.1.0
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.28s
```

For a production binary, add `--release`:

```sh
$ cargo build --release
```

Release builds take longer but produce a significantly faster, smaller binary. We won't use release mode until much later.

### cargo check

Does everything `cargo build` does except for the final code generation step. It type-checks, resolves dependencies, and validates your code, but doesn't produce a binary. It's faster than a full build.

```sh
$ cargo check
```

When you're iterating on a type error and don't need to actually run anything, `cargo check` is your friend. Especially as the codebase grows.

### cargo test

Runs all tests: unit tests embedded in source files, integration tests in `tests/`, and doc-tests (code examples in documentation comments).

```sh
$ cargo test
```

You can filter by test name:

```sh
$ cargo test health_check     # runs any test with "health_check" in the name
```

Or run tests from a specific file:

```sh
$ cargo test --test integration  # runs only tests/integration.rs
```

We'll use `cargo test` constantly. The tests-first approach means it's usually the first thing we run, not the last.

### cargo fmt

Formats your code according to rustfmt's rules. There's no configuration to argue about — the defaults are the community standard, and we use them as-is.

```sh
$ cargo fmt
```

To check if everything is formatted without modifying files (useful in CI):

```sh
$ cargo fmt -- --check
```

If it finds unformatted code, it exits with a non-zero status and shows the diff. This is what our CI pipeline runs.

### cargo clippy

Clippy is Rust's official linter. It catches common mistakes, suggests more idiomatic patterns, and warns about code that compiles but is probably wrong. Think of it as a very pedantic code reviewer who happens to be right most of the time.

```sh
$ cargo clippy
```

In CI, we treat warnings as errors:

```sh
$ cargo clippy -- -D warnings
```

That `-D warnings` flag turns every warning into a hard error. If clippy isn't happy, the build fails. Sounds harsh? It catches real bugs. A clippy warning that you ignore today is a production incident next month.

### cargo run

Compiles and runs a binary in one step. Since we have two binaries, we need to say which one:

```sh
$ cargo run --bin bun         # run the node agent
$ cargo run --bin relish      # run the CLI
```

For relish, you can pass arguments after `--`:

```sh
$ cargo run --bin relish -- status
$ cargo run --bin relish -- --help
```

### The Makefile

Rather than remembering all these incantations, we wrap them in a Makefile:

```makefile
build:     ## Compile all crates (debug)
release:   ## Compile all crates (optimised release)
test:      ## Run all tests
check:     ## Type-check without producing binaries (fast)
fmt:       ## Format all Rust source with rustfmt
fmt-check: ## Check formatting without modifying files
lint:      ## Run clippy with warnings as errors
ci:        ## Run everything CI would run
clean:     ## Remove build artefacts and generated files
```

The `ci` target runs `fmt-check`, then `lint`, then `test`, in that order. If formatting is wrong, it fails before even trying to lint. If linting fails, it doesn't bother running tests. Fast feedback.

Run `make help` to see all available targets. Run `make ci` before pushing. If it passes, CI will pass too.
