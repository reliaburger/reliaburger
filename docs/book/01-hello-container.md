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

- **[tokio](https://tokio.rs/)** — the async runtime. Reliaburger is fundamentally an async system: it manages network connections, health check timers, container events, and cluster gossip, all concurrently. Tokio is the standard choice here. The `"full"` feature enables everything — we'll need most of it eventually, and cherry-picking features buys us nothing at this stage.
- **[serde](https://serde.rs/)** — serialisation and deserialisation. Every config file, every API payload, every state snapshot flows through serde. The `"derive"` feature lets us annotate structs with `#[derive(Serialize, Deserialize)]` instead of writing conversion code by hand.
- **[toml](https://docs.rs/toml)** — Reliaburger uses TOML for all configuration. It's more readable than YAML for the kind of structured config we need, and it doesn't have YAML's footguns (the Norway problem, the boolean problem, the indentation-is-semantic problem).
- **[clap](https://docs.rs/clap)** — argument parsing for the CLI. The `"derive"` feature lets us define CLI structure as Rust types, which the compiler then validates. We'll see this in action in a moment.
- **[thiserror](https://docs.rs/thiserror)** — for defining error types in library code. It generates `Display` and `Error` implementations from an enum, with format strings that include context. No boilerplate.
- **[anyhow](https://docs.rs/anyhow)** — for error handling in binaries. Where `thiserror` is about *defining* precise error types, `anyhow` is about *propagating* errors with context. The split is deliberate: library code uses `thiserror` (callers need to match on specific errors), binary code uses `anyhow` (errors just need to be reported clearly).

**Dev-dependencies — testing tools:**

- **[insta](https://insta.rs/)** — snapshot testing. We'll use it to verify CLI output, serialised configs, and TUI rendering. You write a test that produces output, `insta` captures it in a file, and future runs compare against the snapshot.
- **[proptest](https://docs.rs/proptest)** — property-based testing. Instead of writing specific test cases, you describe the *properties* your code should satisfy, and proptest generates thousands of random inputs to find violations. We'll use it for the scheduler, port allocator, and other algorithmic code.

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

## Designing the configuration format

Before we write any implementation code, we need to decide how users will describe what they want to run. In Kubernetes, you'd write YAML. Lots of YAML. Seven different resource types just to get a web app with a health check, a service, TLS, autoscaling, a secret, and a config file. We're going to do all of that with one.

### Why TOML?

TOML, not YAML. Here's why.

YAML has footguns that have bitten every team that's used it at scale. The "Norway problem" — where the country code `NO` gets silently parsed as boolean `false`. The version number `1.10` becoming the float `1.1`. Indentation-as-structure meaning a single misplaced space changes the meaning of your file. These aren't theoretical concerns; they're production incidents.

TOML is explicit about types. `true` is a boolean. `"NO"` is a string. `1.10` stays `1.10`. Structure comes from headers and key-value pairs, not whitespace. It supports comments (JSON doesn't). It supports multiline strings (JSON doesn't). And it's less verbose than JSON for the kind of nested configuration we need.

JSON was never a serious contender. No comments, no multiline strings, and trailing commas are a syntax error. It's fine for APIs, not for human-edited config files.

### Seven resource types

Kubernetes has Deployments, Services, Ingresses, ConfigMaps, Secrets, PersistentVolumeClaims, HorizontalPodAutoscalers, Jobs, CronJobs, and more. We have seven.

1. **App** — a long-running containerised process with replicas, health checks, ingress, autoscaling. Replaces Deployment + Service + Ingress + HPA.
2. **Job** — a run-to-completion task, optionally on a cron schedule. Replaces Job + CronJob.
3. **Secret** — not a separate resource type. Encrypted values in the `[env]` section, prefixed with `ENC[AGE:`. Lives inline, right next to the plain-text env vars.
4. **ConfigFile** — a file injected into the container at a specific path. Inline content or a reference to a file in git.
5. **Volume** — local persistent storage. One line: path and size.
6. **Permission** — who can do what. Actions, apps, optionally scoped to namespaces.
7. **Namespace** — resource quotas. CPU, memory, GPU limits, max app count.

That's it. Seven types cover what Kubernetes needs dozens for.

### The table-of-tables pattern

Here's what a Reliaburger config file looks like:

```toml
[app.web]
image = "myapp:v1.4.2"
replicas = 3
port = 8080
memory = "128Mi-512Mi"
cpu = "100m-500m"

[app.web.health]
path = "/healthz"

[app.web.ingress]
host = "myapp.com"

[app.redis]
image = "redis:7-alpine"
port = 6379
volume = { path = "/data", size = "10Gi" }

[job.db-migrate]
image = "myapp:v1.4.2"
command = ["npm", "run", "migrate"]
run_before = ["app.api"]
```

`[app.web]` is TOML's table syntax. The dot creates a nested structure: a table called `app` containing a table called `web`. In Rust, that maps to `BTreeMap<String, AppSpec>` — a sorted map from app name to app definition. Multiple apps? Multiple tables. No arrays-of-objects, no anchor-reference tricks, no indentation puzzles.

We use `BTreeMap` rather than `HashMap` for a reason that matters in practice: deterministic ordering. When you serialise a config back to TOML, or display it in the CLI, or diff it in git, the keys always come out in the same order. With `HashMap`, they'd be random. Small detail, big quality-of-life improvement.

## Parsing configuration with serde

Now let's turn that TOML into Rust types. This is where serde earns its keep.

### The standard derive

Here's the top-level config type:

```rust
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(default)]
    pub app: BTreeMap<String, AppSpec>,
    #[serde(default)]
    pub job: BTreeMap<String, JobSpec>,
    #[serde(default)]
    pub namespace: BTreeMap<String, NamespaceSpec>,
    #[serde(default)]
    pub permission: BTreeMap<String, PermissionSpec>,
}
```

A few things to notice. If you're new to Rust, those `#[...]` annotations above the struct and its fields might look strange. They're called *attributes*, and they're Rust's equivalent of Java's `@Annotations` or Python's `@decorators`, though they work at compile time rather than at runtime. `#[derive(Serialize, Deserialize)]` tells the compiler to generate serialisation code. `#[serde(default)]` is an attribute specific to the serde crate, configuring how that generated code behaves. You'll see attributes everywhere in Rust: `#[test]` marks a test function, `#[cfg(test)]` makes code conditional, `#[tokio::main]` sets up the async runtime. The pattern is always the same: metadata that tells the compiler or a library how to handle the item that follows.

`#[serde(default)]` on the struct means every section is optional — a config file with only `[app.web]` parses fine, with empty maps for jobs, namespaces, and permissions. `#[serde(default)]` on each field does the same thing at the field level. The redundancy is intentional: struct-level default handles missing sections in TOML, field-level default handles the Rust `Default` trait.

`Option<T>` handles optional fields within each spec. If TOML doesn't have a key, serde fills in `None`. No sentinel values, no empty strings meaning "absent", no `-1` meaning "not set". The type system tells you whether a value is present.

### Custom deserialisers: the Replicas type

Most fields are straightforward derives. But some need custom logic. The `replicas` field is our first example.

In TOML, `replicas = 3` is an integer. `replicas = "*"` is a string. Same field, two types. TOML doesn't care — it's dynamically typed at the value level. But Rust does care. We need a type that can be either.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replicas {
    Fixed(u32),
    DaemonSet,
}

impl Default for Replicas {
    fn default() -> Self {
        Replicas::Fixed(1)
    }
}
```

The default is 1 replica — a reasonable starting point. Now the serde part. We implement `Deserialize` by hand, using a *visitor*. The visitor pattern lets serde call different methods depending on what it finds in the input:

```rust
impl<'de> Deserialize<'de> for Replicas {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ReplicasVisitor;

        impl<'de> de::Visitor<'de> for ReplicasVisitor {
            type Value = Replicas;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a positive integer or \"*\"")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                if v == 0 {
                    return Err(E::custom("replicas must be a positive integer"));
                }
                u32::try_from(v)
                    .map(Replicas::Fixed)
                    .map_err(|_| E::custom("replicas value too large"))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if v == "*" {
                    Ok(Replicas::DaemonSet)
                } else {
                    Err(E::custom("expected a positive integer or \"*\""))
                }
            }
        }

        deserializer.deserialize_any(ReplicasVisitor)
    }
}
```

This is the densest piece of Rust we've seen so far, so let's slow down and unpack the syntax.

**`impl<'de> Deserialize<'de> for Replicas`** — this says "I'm implementing the `Deserialize` trait for the `Replicas` type." In Rust, a *trait* is like an interface in Java or Go: it defines a set of methods that a type must provide. `Deserialize` is serde's trait for types that can be constructed from serialised data. The `<'de>` part is a *lifetime parameter*. Lifetimes are Rust's way of tracking how long references are valid, so the compiler can guarantee you never use data after it's been freed. Here, `'de` represents the lifetime of the input data being deserialised. You don't need to fully understand lifetimes right now. Just read `<'de>` as "this code works with borrowed data that lives for some duration `'de`." We'll encounter lifetimes again in later chapters, and they'll make more sense once you've seen a few more examples.

**`fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error>`** — three things packed into one function signature. `<D: serde::Deserializer<'de>>` introduces a *generic type parameter* `D` with a *trait bound*: `D` can be any type that implements the `Deserializer` trait. If you're coming from C++, think of it as a template with a concept constraint. From Go, it's like a function that takes an interface parameter, except the Rust compiler generates specialised code for each concrete type rather than using dynamic dispatch. `Self` refers to the type we're implementing for (`Replicas`). And `D::Error` is an *associated type* on the `Deserializer` trait — each deserialiser defines its own error type, and we use whatever it provides. `Result<Self, D::Error>` means this function either succeeds with a `Replicas` value or fails with whatever error type the deserialiser uses.

**The visitor pattern.** We define a zero-sized `ReplicasVisitor` struct and implement `de::Visitor` for it. The visitor has methods for each data type serde might encounter: `visit_u64` for integers, `visit_str` for strings. We implement the ones we care about and let the default implementations of the others return errors using the `expecting` message. When we call `deserializer.deserialize_any(ReplicasVisitor)`, we're saying "parse whatever's in the input and call the appropriate visitor method." If the TOML contains an integer, serde calls `visit_u64`. If it contains a string, serde calls `visit_str`. If it contains something else — a boolean, an array — serde calls the default method, which returns an error saying "expected a positive integer or `*`."

This pattern — enum type, custom visitor, `deserialize_any` — is the standard way to handle TOML values that can be more than one type. You'll see it again. The syntax is admittedly heavy for what it does, but most of it is mechanical: define a visitor, implement the methods you need, call `deserialize_any`. Once you've written one, the rest are copy-paste-and-modify.

### Custom deserialisers: ResourceRange

Resources like CPU and memory use a range format: `"128Mi-512Mi"` means "request 128 mebibytes, limit 512 mebibytes". A single value like `"256Mi"` means request and limit are equal.

The `parse_resource_value` function handles the suffixes:

```rust
pub fn parse_resource_value(s: &str) -> Result<u64, ConfigError> {
    if let Some(num) = s.strip_suffix("Gi") {
        return parse_num(num, 1024 * 1024 * 1024, s);
    }
    if let Some(num) = s.strip_suffix("Mi") {
        return parse_num(num, 1024 * 1024, s);
    }
    if let Some(num) = s.strip_suffix("Ki") {
        return parse_num(num, 1024, s);
    }
    if let Some(num) = s.strip_suffix('m') {
        return parse_num(num, 1, s); // millicores
    }
    s.parse::<u64>().map_err(/* ... */)
}
```

`Ki`, `Mi`, `Gi`, `Ti` are binary prefixes (powers of 1024), used for memory. The `m` suffix is millicores for CPU — 500m means half a CPU core. Bare numbers are passed through as-is.

Two pieces of Rust syntax here deserve a closer look, because they'll appear constantly from now on.

**`if let Some(num) = s.strip_suffix("Gi")`** — this combines two concepts. First, `strip_suffix` doesn't return a plain string. It returns `Option<&str>`, which is Rust's way of saying "this might or might not have a value." `Option` is an enum with two variants: `Some(value)` when there's a result, and `None` when there isn't. No nulls, no null pointer exceptions, no checking for `-1` or empty strings. The compiler forces you to handle both cases.

`if let` is pattern matching in a conditional. It says "if this value matches the pattern `Some(num)`, bind the inner value to `num` and run the block. Otherwise, skip it." In Python you'd write `if (num := s.removesuffix("Gi")) != s:` or check the return value for `None`. In Go you'd check `if strings.HasSuffix(s, "Gi")` and then strip it in a separate step. Rust fuses the check and the extraction into one expression, and the compiler guarantees you can't accidentally use `num` when the suffix wasn't there.

**`s.parse::<u64>().map_err(/* ... */)`** — two things here. `s.parse::<u64>()` calls the `parse` method on a string, and the `::<u64>` part is a *turbofish* (yes, that's what the Rust community actually calls it). It tells the compiler which type to parse into. Without it, the compiler would need to infer the type from context, which isn't always possible. You could also write `let n: u64 = s.parse()` and let the type annotation do the work, but turbofish keeps it inline.

`parse` returns `Result<u64, ParseIntError>`, which is Rust's other "might fail" type. Where `Option` is "value or nothing," `Result` is "value or error." `.map_err(...)` transforms the error variant: it takes the `ParseIntError` from the standard library and converts it into our `ConfigError` type, keeping the success value untouched. This kind of chaining — calling a method, then mapping the error — is idiomatic Rust. You'll see `.map()`, `.map_err()`, `.and_then()`, and `?` used together to build pipelines that handle errors without nested `if` statements.

`ResourceRange` uses a custom `Deserialize` that reads the string, splits on `-`, and parses both halves. If there's no `-`, request and limit are the same value.

### EnvValue and encrypted secrets

Environment variables can be plain text or encrypted:

```toml
[app.api.env]
NODE_ENV = "production"
DATABASE_URL = "ENC[AGE:YWdlLWVuY3J5cHRpb24...]"
```

Rather than treating everything as strings and checking later, we detect the `ENC[AGE:` prefix at parse time:

```rust
pub enum EnvValue {
    Plain(String),
    Encrypted(String),
}
```

If you're coming from C or Go, you might be thinking "I'd just use a string and check the prefix when I need to." You could. But then every function that handles env vars would need to remember to check. Forget once, and you've got an encrypted blob being passed to a process as a literal `DATABASE_URL=ENC[AGE:YWdl...]`. That's the kind of bug you find at 3am in production.

Rust enums prevent this entirely, because they're not like C enums. A C enum is just a named integer. A Rust enum is a *tagged union*: each variant can carry different data. `EnvValue::Plain(String)` and `EnvValue::Encrypted(String)` both hold a string, but the tag tells you which kind. When you want to use the value, you `match` on it:

```rust
match env_value {
    EnvValue::Plain(s) => set_env_var(key, s),
    EnvValue::Encrypted(s) => set_env_var(key, decrypt(s)?),
}
```

The compiler won't let you forget a variant. If you handle `Plain` but not `Encrypted`, the code doesn't compile. That's the guarantee: you can't accidentally skip decryption, because the type system forces you to decide what to do with each case. We already saw this pattern with `Replicas` (`Fixed(u32)` vs `DaemonSet`) and `Option` (`Some(value)` vs `None`). It's the same idea each time: encode the possibilities in the type, let the compiler enforce exhaustive handling.

## Validation as a separate pass

You might wonder why we don't validate everything during parsing. An app without an image should be an error, right? A config file with both `content` and `source` set should be rejected.

We *could* do this in the serde layer, but we'd lose something valuable. TOML parse errors come with line numbers: "expected a string at line 42, column 8". Domain validation errors need different context: "app 'web' requires an image". If we mix them, we get confusing error messages — a line number for a semantic problem, or a semantic message for a syntax problem.

So we separate the two. First, parse the TOML. If that fails, the user sees a TOML error with a line number. If it succeeds, run validation. If *that* fails, the user sees a domain error with the resource name and a clear description.

```rust
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to parse TOML: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("app {name:?} requires an image")]
    MissingImage { name: String },

    #[error("config file in app {name:?} must have exactly one of 'content' or 'source'")]
    InvalidConfigFile { name: String },
    // ...
}
```

The `thiserror` crate generates the `Display` implementation from the `#[error(...)]` attributes. The `#[from]` attribute on `TomlParse` generates a `From<toml::de::Error>` impl, so we can use `?` to convert TOML errors automatically.

The validation function checks each app, each job, each config file:

```rust
impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (name, app) in &self.app {
            if app.image.is_none() {
                return Err(ConfigError::MissingImage { name: name.clone() });
            }
            for cf in &app.config_file {
                match (&cf.content, &cf.source) {
                    (Some(_), Some(_)) | (None, None) => {
                        return Err(ConfigError::InvalidConfigFile { name: name.clone() });
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
}
```

Parse, then validate. Two steps, clean errors, no confusion.

## Tests first

We haven't written a single line of implementation code without a test to drive it. Here's what the testing process looks like.

The naming convention is important: test names are behaviour sentences. Not `test_config_1` or `test_parse`. Instead:

```rust
#[test]
fn replicas_deserialise_integer() {
    #[derive(Deserialize)]
    struct W { replicas: Replicas }
    let w: W = toml::from_str("replicas = 3").unwrap();
    assert_eq!(w.replicas, Replicas::Fixed(3));
}
```

Let's walk through this line by line. `#[test]` marks the function as a test case — `cargo test` will find it and run it. The function takes no arguments and returns nothing; if it panics, the test fails.

`struct W { replicas: Replicas }` defines a throwaway wrapper struct *inside* the test function. Yes, you can define types inside functions in Rust. We need this because TOML has key-value pairs (`replicas = 3`), not bare values. Serde needs a struct with a field named `replicas` to know which key to look for. The `#[derive(Deserialize)]` on the line above generates the deserialisation code for this struct, same as it would at the top level.

`let w: W = toml::from_str("replicas = 3").unwrap();` parses the TOML string into our wrapper struct. `let` declares a variable (`w`), and `: W` is a type annotation telling the compiler what type we expect. `toml::from_str` returns a `Result<W, Error>` — remember, parsing can fail. `.unwrap()` says "if this is `Ok`, give me the value; if it's `Err`, panic." We said earlier that `.unwrap()` is banned in production code because panicking crashes the programme. But in tests, panicking *is* how you signal failure. If this parse fails, the test fails with a clear error message showing what went wrong.

`assert_eq!(w.replicas, Replicas::Fixed(3))` checks that the parsed value matches what we expect. `assert_eq!` is a macro (the `!` suffix is how Rust marks macros) that compares two values and panics with a nice diff if they're not equal. This works because we derived `PartialEq` on `Replicas` earlier — without that derive, the compiler wouldn't know how to compare two `Replicas` values.

Now the other two tests:

```rust
#[test]
fn replicas_deserialise_zero_rejected() {
    #[derive(Deserialize)]
    struct W { replicas: Replicas }
    assert!(toml::from_str::<W>("replicas = 0").is_err());
}

#[test]
fn parse_resource_range_request_exceeds_limit_rejected() {
    assert!(ResourceRange::parse("500m-100m").is_err());
}
```

Read the test name and you know exactly what behaviour it's verifying. `replicas_deserialise_zero_rejected` — replicas of zero should fail. `parse_resource_range_request_exceeds_limit_rejected` — asking for more than your limit is an error.

The full test suite for config parsing has 66 tests. Each one was written before the code it tests. Some of them describe happy paths (minimal app parses, all fields parse, round-trip works). Others describe failure modes (missing image rejected, invalid port rejected, non-absolute path rejected). Every one passed before we moved on.

`cargo test` runs them all in under a second. That's the feedback loop we want: make a change, run the tests, know immediately if something broke.

```
running 66 tests
test config::types::tests::parse_resource_range_cpu_with_range ... ok
test config::types::tests::replicas_deserialise_star ... ok
test config::app::tests::parse_app_with_health_check ... ok
test config::validate::tests::validate_app_missing_image_rejected ... ok
...
test result: ok. 66 passed; 0 failed; 0 ignored
```

66 passing tests. No containers yet, no networking, no gossip protocol. Just a solid configuration layer that we can trust completely. Everything we build from here — the container runtime, the health checker, the scheduler — will parse its config through these types. And we know they work.
