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

For rootless containers (running without sudo), you need kernel 5.11 or later. That's where unprivileged user namespaces became stable enough for general use. Ubuntu 22.04+, Fedora 31+, and Debian 11+ all ship kernels that qualify. Check yours with `uname -r`. If you're on an older kernel, runc still works — you'll just need root.

You also need `runc` installed. Your package manager probably has it (`sudo apt install runc` on Debian/Ubuntu), or grab a binary from the [GitHub releases](https://github.com/opencontainers/runc/releases). Rootless mode requires cgroups v2 with systemd delegation, which is the default on any modern systemd-based distro. You can verify with:

```sh
# Should print "cgroup2fs" — if it says "tmpfs", you're on cgroups v1
stat -f -c %T /sys/fs/cgroup
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
[[app.redis.volumes]]
path = "/data"
size = "10Gi"

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

## The container runtime interface

Now we need something that actually runs containers. That something is Grill.

The name follows the burger theme (Reliaburger, Bun, Relish, Patty, Mustard...), but the architecture is practical. When you run a container on Linux, two layers are involved:

1. **runc** does the actual work. It creates Linux namespaces (isolating the container's view of the filesystem, network, and processes from the host), sets up cgroups (limiting CPU, memory, and other resources), applies security profiles (seccomp, capabilities), and exec's the container's entrypoint. It's a single binary that creates a container and exits.

2. **Grill** manages runc. It translates our `AppSpec` configuration into the OCI runtime specification that runc expects, allocates host ports, computes cgroup parameters, and tracks the lifecycle state of each container. Grill is the only part we write.

In the Docker and Kubernetes world, there's usually a third layer in between: **containerd**, a daemon that manages runc, pulls images, persists state across reboots, and exposes a gRPC API. We skip it. containerd adds protobuf serialisation, a socket connection, and a daemon that needs to be running. We talk to runc directly, and handle image pulling ourselves. This gives us fewer moving parts, no daemon dependency, and full control over the lifecycle. If we need containerd later, we'll add another `Grill` implementation. The trait boundary makes that a local change.

For Phase 1, we're building the parts of Grill that work without real containers: the state machine, port allocation, cgroup computation, and OCI spec generation. These are pure logic, testable on any platform. The runc integration and image pulling come later in this chapter.

## State machines in Rust

Every container goes through a lifecycle. It starts as a request, gets prepared, starts running, might become unhealthy, gets stopped, and might restart. We need to track this precisely, because the wrong state transition — marking a container as "running" when it hasn't passed its health check yet — means sending traffic to a process that isn't ready.

Here's the state machine:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContainerState {
    Pending,
    Preparing,
    Initialising,
    Starting,
    HealthWait,
    Running,
    Unhealthy,
    Stopping,
    Stopped,
    Failed,
}
```

Ten states. Each one means something specific:

- **Pending** — we've received the spec but haven't started work.
- **Preparing** — pulling the container image.
- **Initialising** — running init containers (setup tasks that must complete before the main process).
- **Starting** — the main process is launching.
- **HealthWait** — it's running, but we haven't confirmed it's healthy yet. No traffic.
- **Running** — healthy and receiving traffic.
- **Unhealthy** — health checks are failing. Removed from the service map, no new traffic.
- **Stopping** — graceful shutdown in progress, draining existing connections.
- **Stopped** — process has exited, resources cleaned up.
- **Failed** — permanent failure. Exceeded restart limits, or an init container crashed.

The key insight is that not all transitions are valid. You can't go from `Pending` directly to `Running` — you have to prepare first, start, and pass health checks. You can't go from `Failed` to `Running` — you have to start over from `Pending`. The state machine enforces this:

```rust
impl ContainerState {
    pub fn can_transition_to(self, next: ContainerState) -> bool {
        matches!(
            (self, next),
            (Pending, Preparing)
                | (Preparing, Initialising)
                | (Preparing, Starting)
                | (Preparing, Failed)
                | (Initialising, Starting)
                | (Initialising, Failed)
                | (Starting, HealthWait)
                | (Starting, Failed)
                | (HealthWait, Running)
                | (HealthWait, Failed)
                | (Running, Unhealthy)
                | (Running, Stopping)
                | (Unhealthy, Running)
                | (Unhealthy, Stopping)
                | (Stopping, Stopped)
                | (Stopped, Pending)
                | (Stopped, Failed)
        )
    }
}
```

This uses `match`, which is Rust's pattern matching. If you're coming from C, think of `switch`, but the compiler guarantees you handle every case. If you add a new variant to `ContainerState` and forget to add its transitions here, the code won't compile. In C, the `switch` would silently fall through to `default`. In Go, the compiler wouldn't complain either. Rust catches it at build time.

The `matches!()` macro is shorthand for "does this value match any of these patterns?" It returns a `bool`. The `(self, next)` creates a tuple, and each `(Pending, Preparing)` is a pattern that matches when `self` is `Pending` and `next` is `Preparing`. The `|` between patterns means "or" — match any of these.

The `_` wildcard pattern (which you'll see in other `match` expressions) means "I don't care what this value is." It's the catch-all case. But notice we don't use `_` here. We list every valid transition explicitly. If we'd used a catch-all `_ => false`, adding a new state wouldn't trigger a compilation error, and we'd silently miss transitions. By listing them exhaustively, the compiler works for us.

Now look at how we implement `Display` for `ContainerState`:

```rust
impl fmt::Display for ContainerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContainerState::Pending => write!(f, "pending"),
            ContainerState::Preparing => write!(f, "preparing"),
            ContainerState::Initialising => write!(f, "initialising"),
            // ... and so on for each variant
        }
    }
}
```

This is the first time we're implementing a trait by hand rather than deriving it. When we wrote `#[derive(Debug)]`, the compiler generated the `Debug` trait implementation automatically. But `Display` can't be derived — the compiler doesn't know how you want your type displayed to users (should it say "Pending" or "pending" or "PENDING"?). So we write the `impl` block ourselves.

An `impl Trait for Type` block is like implementing an interface in Go or Java, but explicit. In Go, a type satisfies an interface just by having the right methods — you don't declare it. In Rust, you say `impl Display for ContainerState` and the compiler checks that you've provided all the required methods. The `write!()` macro inside `fmt` works like `format!()` but writes to the formatter instead of creating a new string.

## Port allocation: randomness and concurrency

Every container that exposes a port needs a host port mapped to it. When traffic arrives at the host on port 34217, Grill routes it to the container's internal port (say, 8080). We need to hand out these host ports without collisions, and release them when containers stop.

```rust
#[derive(Debug, Clone)]
pub struct PortAllocator {
    range_start: u16,
    range_end: u16,
    allocated: Arc<Mutex<HashSet<u16>>>,
}
```

Three new concepts packed into one struct.

**`Arc`** stands for "Atomically Reference Counted." In Rust, every value has exactly one owner. When the owner goes out of scope, the value is dropped (freed). This is how Rust avoids garbage collection — the compiler knows at compile time when to free memory. But sometimes multiple parts of your program need to share a value. `Arc` solves this: it wraps a value and keeps a count of how many references exist. When the count reaches zero, the value is dropped. If you're coming from Python or Java, this is roughly what the garbage collector does, but deterministic — you know exactly when it happens, and there are no GC pauses.

**`Mutex`** provides mutual exclusion. Rust normally prevents you from mutating shared data — if two tasks could modify the `HashSet` simultaneously, you'd get a data race, and Rust's ownership rules are designed to make data races a compile-time error. `Mutex` is the escape hatch: it lets you mutate shared data, but only after acquiring a lock. The key word here is `tokio::sync::Mutex`, not `std::sync::Mutex`. The standard library's mutex blocks the OS thread when waiting for the lock. In an async programme, that's catastrophic — it blocks the entire Tokio worker thread, starving every other task on that thread. Tokio's mutex suspends the async task instead, letting the worker thread run other tasks while waiting. In Go, goroutines are preemptively scheduled, so a blocked goroutine doesn't starve others. In Rust, you have to make the right choice yourself.

**`Arc<Mutex<HashSet<u16>>>`** is the idiomatic Rust way to share mutable data across async tasks. `Arc` handles the sharing, `Mutex` handles the mutation, `HashSet` is the actual data. In Go, you'd write a `struct` with a `sync.Mutex` and a `map`. The difference is that in Rust, the compiler won't let you access the `HashSet` without going through the `Mutex` — there's no way to accidentally forget the lock.

Here's how allocation works:

```rust
pub async fn allocate(&self) -> Result<u16, PortError> {
    let mut allocated = self.allocated.lock().await;
    if allocated.len() >= self.total_ports() {
        return Err(PortError::Exhausted { ... });
    }

    let mut rng = rand::thread_rng();
    loop {
        let port = rng.gen_range(self.range_start..self.range_end);
        if allocated.insert(port) {
            return Ok(port);
        }
    }
}
```

`async fn` and `.await` — this is the first async code in the project. Rust's async model is fundamentally different from Go's goroutines. In Go, every goroutine is independently scheduled by the runtime — you call a function, it might be suspended at any point, and you don't explicitly mark where. In Rust, `async fn` returns a *future* — a value that represents work that hasn't happened yet. Nothing executes until you `.await` it. Each `.await` is an explicit point where the runtime can suspend your task and run another. This is more verbose than Go (you have to write `.await` everywhere), but it makes suspension points visible in the code. You always know where your task might be paused.

`self.allocated.lock().await` acquires the mutex lock. The `.await` is crucial: it means "if the lock is held by another task, suspend me and let other tasks run." Without `.await`, this would be `std::sync::Mutex::lock()`, which blocks the thread.

Why random port selection instead of sequential? Two reasons. Sequential assignment is predictable, which is a minor security concern (an attacker who knows your allocation pattern can guess which ports are in use). And sequential assignment creates hot-spots when containers are frequently created and destroyed — you'd keep reusing the same low ports while the rest of the range sits idle.

`rand::thread_rng()` creates a thread-local random number generator. `rng.gen_range(start..end)` picks a random number in the half-open range `[start, end)`. These come from the `rand` crate, which we added to our dependencies. `allocated.insert(port)` tries to add the port to the set. If it's already there (collision), `insert` returns `false` and we try again. If it succeeds, `insert` returns `true` and we return the port.

## Speaking cgroup v2

Cgroups (control groups) are a Linux kernel feature that limits how much CPU, memory, and other resources a process can use. Every container orchestrator uses them. Kubernetes uses them. Docker uses them. We use them.

cgroup v2 organises limits as a filesystem hierarchy under `/sys/fs/cgroup`. To limit a container's CPU, you write a value to a file. No system calls, no APIs — just write a string to a file. The kernel reads it and enforces the limit.

Reliaburger creates its cgroup hierarchy under `/sys/fs/cgroup/reliaburger/{namespace}/{app_name}/{instance}`:

```rust
pub fn cgroup_path(namespace: &str, app_name: &str, instance: u32) -> PathBuf {
    PathBuf::from(format!("{CGROUP_ROOT}/{namespace}/{app_name}/{instance}"))
}
```

`format!()` is Rust's string interpolation macro, similar to Python's f-strings or Go's `fmt.Sprintf`. But unlike `Sprintf`, the compiler checks the format string at compile time. If you reference a variable that doesn't exist, or use the wrong format specifier, you get a compilation error, not a runtime panic.

CPU limits use two parameters. `cpu.max` is a hard limit: "this container can use at most X microseconds of CPU time per Y-microsecond period." `cpu.weight` is proportional sharing: "when the CPU is contended, give this container this share relative to others."

The conversion from millicores (what users write in config) to microseconds (what the kernel expects) is straightforward:

```rust
pub fn cpu_max_from_millicores(millicores: u64) -> String {
    let quota_us = millicores * CGROUP_PERIOD_US / 1000;
    format!("{quota_us} {CGROUP_PERIOD_US}")
}
```

500 millicores means "half a CPU." With a 100ms (100,000 microsecond) period, that's 50,000 microseconds of quota: `"50000 100000"`. 1000 millicores is a full CPU: `"100000 100000"`. 2000 millicores is two CPUs: `"200000 100000"` — the kernel allows quota larger than the period.

For `cpu.weight`, we convert millicores to the kernel's 1-10000 range using `.clamp()`:

```rust
pub fn cpu_weight_from_millicores(millicores: u64) -> u32 {
    let weight = millicores / 10;
    (weight as u32).clamp(1, 10000)
}
```

`.clamp(min, max)` is a standard library method on numeric types that bounds a value: if it's below `min`, return `min`; if it's above `max`, return `max`; otherwise return it unchanged.

Memory limits are simpler. `memory.max` is the hard limit in bytes — exceed it and the kernel OOM-kills your process. `memory.high` is a soft limit — exceed it and the kernel starts reclaiming memory aggressively, slowing your process down but not killing it. We set `memory.max` to the configured limit and `memory.high` to the request. This gives containers breathing room between "the kernel starts pushing back" and "the kernel kills you."

Notice that none of these functions touch the filesystem. They compute values and return them. The actual writing to `/sys/fs/cgroup` happens elsewhere, when we're running on Linux with the right permissions. This separation is deliberate. These functions are pure: same input, same output, no side effects. That makes them testable on any platform — your CI server, your macOS laptop, anywhere. We test the computation now and test the filesystem operations in integration tests on Linux later.

## Generating OCI specs

The Open Container Initiative (OCI) runtime specification is a JSON document that tells runc exactly how to create a container: what filesystem to use, what processes to run, what namespaces to create, what resource limits to apply. Containerd passes this spec to runc, and runc does the rest.

We define our own Rust types for the spec rather than importing an external crate:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciSpec {
    pub root: OciRoot,
    pub process: OciProcess,
    pub mounts: Vec<OciMount>,
    pub linux: OciLinux,
}
```

Why not use an existing OCI spec crate? We only need a subset of the full spec. Defining our own types means we control exactly which derives they have, what methods are available, and how they serialise. An external crate might not derive `PartialEq` (needed for tests), or might pull in dependencies we don't want.

The `OciMount` type shows a serde trick we haven't seen:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciMount {
    pub destination: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<PathBuf>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub mount_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}
```

`#[serde(rename = "type")]` maps the Rust field name `mount_type` to `"type"` in JSON. We can't name the field `type` because it's a reserved keyword in Rust — you'd use it to define a type alias or trait object. So we pick a Rust-friendly name and tell serde to use the JSON-friendly name on the wire. Every language with JSON serialisation has the same problem. In Go, you'd use a struct tag:

```go
type OciMount struct {
    Destination string  `json:"destination"`
    Source      *string `json:"source,omitempty"`
    MountType   *string `json:"type,omitempty"`
    Options     []string `json:"options,omitempty"`
}
```

In Python with Pydantic or dataclasses, you'd use `Field(alias="type")`:

```python
from pydantic import BaseModel, Field

class OciMount(BaseModel):
    destination: str
    source: str | None = None
    mount_type: str | None = Field(None, alias="type")
    options: list[str] = []
```

The Rust version is more verbose than Go's struct tags, but more flexible. `skip_serializing_if` lets you control exactly when a field is omitted, rather than relying on Go's all-or-nothing `omitempty` (which, by the way, treats `0` and `false` as empty too — a footgun if you actually want to serialise a zero value).

`#[serde(skip_serializing_if = "Option::is_none")]` omits the field from JSON output when it's `None`. Without it, you'd get `"source": null` in the JSON. The OCI spec doesn't expect null fields — it expects them to be absent entirely. `skip_serializing_if` takes a function name (as a string) that returns `bool`. For `Vec`, we use `"Vec::is_empty"` to skip empty arrays. You can pass any function here — including your own custom ones — which is something neither Go's `omitempty` nor Python's `exclude_none` can do without extra work.

The `generate_oci_spec` function builds the spec from our `AppSpec`:

```rust
pub fn generate_oci_spec(
    app_name: &str,
    namespace: &str,
    spec: &AppSpec,
    host_port: Option<u16>,
    cgroup_path: &str,
) -> OciSpec { ... }
```

It iterates the environment variables, building `"KEY=VALUE"` strings:

```rust
for (key, value) in &spec.env {
    match value {
        EnvValue::Plain(v) => env.push(format!("{key}={v}")),
        EnvValue::Encrypted(v) => env.push(format!("{key}={v}")),
    }
}
```

`for (key, value) in &spec.env` iterates over the `BTreeMap` by reference. The `&` is important: without it, Rust would try to *move* the map out of `spec`, taking ownership. Since we're borrowing `spec` (the function takes `&AppSpec`, not `AppSpec`), we can only borrow its contents. In Go, `range` over a map copies the key and value — there's no distinction. In Rust, you choose between borrowing (`&`), mutable borrowing (`&mut`), and consuming (no `&`). Most of the time you want borrowing.

You might notice that encrypted values are passed through as the literal `ENC[AGE:...]` string. That's intentional. The decryption infrastructure (Sesame PKI) doesn't exist yet — that's Phase 4. Rather than stubbing it out or adding a no-op decrypt function, we pass the encrypted blob through and leave a `// TODO(Phase 4)` comment. The container will see the encrypted string as its environment variable. That's not useful, but it's honest — and it compiles, tests, and ships.

## Designing for what doesn't exist yet

Grill depends on systems we haven't built. The image registry (Pickle, Phase 5) stores and distributes container images. The eBPF service discovery (Onion, Phase 3) routes traffic to healthy containers. The PKI system (Sesame, Phase 4) decrypts secrets and issues workload identity certificates. None of these exist.

We handle this with traits. A trait in Rust is like an interface in Go, but with one important difference: you implement it explicitly.

```rust
pub trait Grill: Send + Sync {
    fn create(
        &self,
        instance: &InstanceId,
        spec: &OciSpec,
    ) -> impl std::future::Future<Output = Result<(), GrillError>> + Send;

    fn start(
        &self,
        instance: &InstanceId,
    ) -> impl std::future::Future<Output = Result<(), GrillError>> + Send;

    // ... stop, kill, state
}
```

In Go, any type with the right methods automatically satisfies an interface. In Rust, you write `impl Grill for ContainerdGrill { ... }` and the compiler verifies that you've implemented every method. This is more verbose, but it means you can't accidentally satisfy a trait — it's always intentional.

The trait methods return `impl Future<Output = Result<(), GrillError>> + Send`. Let's break that down. `impl Future<Output = ...>` is Rust 2024's way of writing async methods in traits — earlier editions needed a separate `async-trait` crate to do this. `Result<(), GrillError>` means the method either succeeds (returning nothing, `()`) or fails with a `GrillError`. And `+ Send` means the future can be moved between threads. Tokio is a multi-threaded runtime — it might start running your future on one thread and resume it on another. `Send` is a *marker trait* that tells the compiler "this is safe to send to another thread." If your future holds a reference to something that isn't thread-safe (like an `Rc`, which is a non-atomic reference count), the compiler would reject the `+ Send` bound at compile time.

`InstanceId` is a newtype:

```rust
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct InstanceId(pub String);
```

This is a tuple struct with one field — it's just a `String` at runtime, with zero overhead. But the compiler treats `InstanceId` and `String` as different types. If you write a function that takes an `InstanceId` and accidentally pass it a `String`, the compiler rejects it. This is Rust's answer to "stringly-typed" APIs. In Go or Python, you'd pass `string` everywhere and rely on variable names and documentation to keep track of what's what. In Rust, the type system does it for you.

The `GrillError` enum shows another thiserror pattern:

```rust
#[derive(Debug, thiserror::Error)]
pub enum GrillError {
    #[error("invalid state transition: {0}")]
    InvalidTransition(#[from] state::InvalidTransition),

    #[error("port allocation failed: {0}")]
    Port(#[from] PortError),

    // ...
}
```

`#[from]` on a variant generates a `From<InvalidTransition> for GrillError` implementation. This means anywhere you have a `Result<_, InvalidTransition>`, you can use `?` to automatically convert it to `Result<_, GrillError>`. No manual error wrapping, no `map_err` calls. The `?` operator does three things: if the result is `Ok`, unwrap the value and continue; if it's `Err`, convert the error using `From` and return early. It's the Rust equivalent of Go's `if err != nil { return err }`, but in three characters instead of three lines.

With the trait defined, Phase 1's implementation focuses on the parts that don't need a real container runtime: the state machine, port allocator, cgroup computation, and OCI spec generation. Together, these four modules add 69 tests to the project — bringing the total to 135. Every transition in the state machine is validated. Every edge case in port allocation is covered. Every cgroup parameter is verified against known-correct values. And the OCI spec serialises to valid JSON with the right structure.

When we implement the runc integration (the part that creates real containers), we'll write a struct that implements the `Grill` trait. For testing, we can write a mock that also implements the trait. The business logic that uses Grill — the supervisor, the health checker, the scheduler — won't know or care which implementation it's talking to. That's the point of the trait boundary.

## The process supervisor

The Grill trait tells us *how* to create and stop containers. But who decides *when* to create them, how many to create, and what to do when one dies? That's the job of Bun, the per-node agent. Its central piece is the `WorkloadSupervisor`, which manages the lifecycle of every workload instance running on a single node.

### WorkloadInstance: what the supervisor tracks

Each container (or, eventually, process) running on the node is represented by a `WorkloadInstance`:

```rust
pub struct WorkloadInstance {
    pub id: InstanceId,
    pub app_name: String,
    pub namespace: String,
    pub state: ContainerState,
    pub health_counters: HealthCounters,
    pub restart_count: u32,
    pub last_restart: Option<Instant>,
    pub host_port: Option<u16>,
    pub created_at: Instant,
    pub restart_policy: RestartPolicy,
    pub health_config: Option<HealthCheckConfig>,
    pub is_job: bool,
    pub oci_spec: Option<OciSpec>,
}
```

Two fields at the bottom deserve a note. `is_job` distinguishes run-to-completion tasks from long-running apps. We could have modelled this as an enum (`WorkloadKind::App` vs `WorkloadKind::Job`) instead of a boolean, and for a system with more workload types that would be the right call. For now, a boolean is honest about the two cases we actually have.

`oci_spec` stores the OCI spec that was used to create this instance. Why keep it around? Because when an instance fails and gets restarted, the agent needs to call `grill.create()` again with the same spec. Without storing it, we'd need to regenerate it from the original `AppSpec` or `JobSpec`, which means the agent would need to keep the config around and know which spec belongs to which instance. Storing the OCI spec directly is simpler and self-contained.

Every field and the struct itself are marked `pub`, which means they're visible outside the module. In Rust, everything is private by default. If you just wrote `struct WorkloadInstance { state: ContainerState, ... }`, the struct and all its fields would only be accessible within `supervisor.rs`. Other modules in the same crate couldn't even name the type, let alone read its fields. `pub` opens things up one level at a time: `pub` on the struct makes the type visible, but each field is still independently private unless it also has `pub`. You can have a public struct with a mix of public and private fields, which is how you expose some data while keeping internal bookkeeping hidden. In C, everything in a header file is public. In Go, capitalisation controls visibility (exported vs unexported). Rust's approach is more granular: you decide per-item, and the compiler enforces it.

For `WorkloadInstance`, we make everything `pub` because the supervisor's callers (the Bun agent's main loop, tests, and eventually the API layer) need to read and sometimes modify instance state directly. If we wanted to protect certain fields, we'd drop the `pub` and add getter methods instead.

The interesting design choice here is storing `health_config` directly on the instance rather than looking it up from the `HealthChecker`. We'll come back to why in a moment.

### Generic structs with trait bounds

The supervisor needs a container runtime to do its work, but we don't want to hardcode which runtime. Here's how Rust handles that:

```rust
pub struct WorkloadSupervisor<G: Grill> {
    grill: G,
    port_allocator: PortAllocator,
    instances: HashMap<InstanceId, WorkloadInstance>,
    health_checker: HealthChecker,
    app_instances: HashMap<(String, String), Vec<InstanceId>>,
}
```

The `<G: Grill>` syntax means "this struct is generic over any type `G` that implements the `Grill` trait." If you've used Go, you've seen this with interfaces: a struct holds an `interface` field, and at runtime Go uses a fat pointer (interface value = data pointer + vtable pointer) to dispatch method calls. C++ templates work similarly to Rust generics, but without the trait bound: the compiler checks whether the type has the right methods only when you try to use them, leading to notoriously terrible error messages.

Rust takes a different approach. When you write `WorkloadSupervisor<MockGrill>`, the compiler generates a completely separate copy of `WorkloadSupervisor` with `MockGrill` in place of `G`. Every method call on `self.grill` is a direct, statically-dispatched call. No vtable, no pointer indirection, no runtime cost. This process is called *monomorphisation*, and it's why Rust generics have zero overhead compared to writing the code by hand for each concrete type.

The trade-off is compile time: more concrete types means more code to generate. For our use case (one real implementation plus one mock), that's negligible.

### The secondary index pattern

Look at the `app_instances` field: `HashMap<(String, String), Vec<InstanceId>>`. This maps `(app_name, namespace)` tuples to the list of instance IDs belonging to that app. Why maintain this separately from `instances`?

Because `instances` is keyed by `InstanceId`. If you want to stop all instances of an app called "web" in namespace "prod", you'd have to iterate every instance in the map and check whether its `app_name` and `namespace` match. That's O(n) in the total number of instances on the node.

With the secondary index, it's O(1) to find the right instance IDs, then O(k) to look up each one, where k is the number of replicas for that specific app. At 500 apps with 3 replicas each, that's the difference between scanning 1,500 entries and looking up 3.

In Go you'd probably reach for the same pattern, but the compiler wouldn't force you to think about it. Rust's ownership model makes you explicitly decide how to structure your data, because you can't just iterate a map while mutating entries you find along the way. The borrow checker catches that: you can't hold an immutable iterator over `instances` and simultaneously get a mutable reference to modify an instance. The secondary index is the clean solution.

### Deploying an app

Here's the core of `deploy_app`, showing how the supervisor creates instances:

```rust
pub async fn deploy_app(
    &mut self,
    app_name: &str,
    namespace: &str,
    spec: &AppSpec,
    now: Instant,
) -> Result<Vec<InstanceId>, BunError> {
    let replica_count = match spec.replicas {
        Replicas::Fixed(n) => n,
        Replicas::DaemonSet => 1,
    };

    let mut instance_ids = Vec::with_capacity(replica_count as usize);

    for i in 0..replica_count {
        let instance_id = InstanceId(format!("{app_name}-{i}"));

        let host_port = if spec.port.is_some() {
            Some(self.port_allocator.allocate().await?)
        } else {
            None
        };

        // ... create WorkloadInstance, register health checks ...

        self.instances.insert(instance_id.clone(), instance);
        instance_ids.push(instance_id);
    }

    self.app_instances.insert(
        (app_name.to_string(), namespace.to_string()),
        instance_ids.clone(),
    );

    Ok(instance_ids)
}
```

Look at the first parameter: `&mut self`. In Rust, methods receive `self` in one of three ways, and the choice tells the compiler (and anyone reading the code) what the method is allowed to do:

- `&self` borrows the struct immutably. You can read fields but not change them. Multiple `&self` borrows can exist at the same time, because read-only access is always safe to share. This is the default for "getter" methods.
- `&mut self` borrows the struct mutably. You can read *and* write fields, but the compiler guarantees you're the only one with access. No other reference to the struct can exist while you hold a `&mut`. This is what `deploy_app` needs, because it modifies `self.instances` and `self.app_instances`.
- `self` (no `&`) takes ownership of the struct entirely. The caller gives up the value and can't use it afterwards. This is rare for methods on long-lived types. You'd use it for a `into_parts()` method that dismantles a struct and returns its pieces.

In C, there's no distinction: you get a pointer, and it's up to you not to mess things up. In Go, methods on pointer receivers (`*T`) can mutate freely, and the compiler won't stop two goroutines from doing it simultaneously. Rust's borrow checker enforces the rule at compile time: if `deploy_app` holds `&mut self`, nothing else can read or write the supervisor until `deploy_app` returns. Data races are structurally impossible.

The other parameters show the read-only side: `app_name: &str` and `spec: &AppSpec` are immutable borrows. The function reads them but doesn't need to own or modify them.

A few more things to notice. The `?` operator after `self.port_allocator.allocate().await?` does double duty: it awaits the future (`.await`), then propagates the error if the allocation failed (`?`). If the port range is exhausted, `allocate()` returns a `PortError`, and the `?` converts it to a `BunError::Port` automatically (via the `#[from]` attribute on the error enum) and returns early.

`Vec::with_capacity(replica_count as usize)` pre-allocates the right amount of memory. Without it, the `Vec` would start empty and double its allocation each time it runs out of space. For small replica counts this doesn't matter. It's a habit worth building: if you know the final size, tell the allocator.

And notice the last line of the function: `Ok(instance_ids)` with no `return` keyword and no semicolon. In Rust, every block (function body, `if` branch, `match` arm) is an expression that evaluates to its last line, as long as that line doesn't end with a semicolon. Adding a semicolon turns an expression into a statement, which evaluates to `()` (Rust's unit type, roughly equivalent to `void`). So `Ok(instance_ids)` is the function's return value, and `Ok(instance_ids);` would be a type error because the function promises to return `Result<Vec<InstanceId>, BunError>`, not `()`.

You can use `return Ok(instance_ids);` explicitly, and you'll see that in early-return situations (the `?` operator is actually shorthand for an early `return Err(...)`). But idiomatic Rust reserves `return` for early exits and uses the implicit expression form for the "happy path" at the end of a function. It takes a day to get used to. After that, explicit `return` at the end of a function starts looking like a code smell.

The same rule applies to `if`/`else`. Look at the `host_port` assignment:

```rust
let host_port = if spec.port.is_some() {
    Some(self.port_allocator.allocate().await?)
} else {
    None
};
```

This isn't special syntax. `if`/`else` is an expression, and each branch evaluates to its last line. The whole thing works like a ternary operator (`condition ? a : b` in C), but it scales to multiple lines without getting unreadable. The semicolon after the closing `};` is there because we're using the `if` expression as a statement (assigning its result to `host_port`).

### Testing with MockGrill

Because `WorkloadSupervisor` is generic over `G: Grill`, testing doesn't require a mocking framework. We define a `MockGrill` in the test module:

```rust
#[derive(Debug, Clone, Default)]
struct MockGrill {
    calls: Arc<Mutex<Vec<(String, InstanceId)>>>,
}

impl Grill for MockGrill {
    async fn create(&self, instance: &InstanceId, _spec: &OciSpec)
        -> Result<(), GrillError>
    {
        self.calls.lock().unwrap()
            .push(("create".to_string(), instance.clone()));
        Ok(())
    }
    // ... same for start, stop, kill, state
}
```

This records every call so tests can assert that the right methods were called with the right arguments. The `Arc<Mutex<Vec<...>>>` is a common test pattern in Rust: `Arc` (atomic reference count) lets multiple references exist, `Mutex` makes it safe to mutate from any of them. We use `std::sync::Mutex` here, not `tokio::sync::Mutex`, because in tests we're calling `.unwrap()` anyway and the lock is never held across an `.await` point.

Then in tests:

```rust
fn test_supervisor() -> WorkloadSupervisor<MockGrill> {
    let grill = MockGrill::new();
    let port_allocator = PortAllocator::new(30000, 31000);
    WorkloadSupervisor::new(grill, port_allocator)
}
```

The compiler generates a `WorkloadSupervisor<MockGrill>` that calls `MockGrill` methods directly. In production, we'll create a `WorkloadSupervisor<ContainerdGrill>` that calls the real runtime. Same code, different concrete type, zero runtime overhead.

### Deploying a job

Apps run forever (or until someone stops them). Jobs run to completion. A database migration, a batch data export, a cleanup script. You start it, it does its thing, it exits. If it fails, you retry a few times. If it keeps failing, you give up.

The supervisor handles both, but the differences show up in the deploy method:

```rust
pub async fn deploy_job(
    &mut self,
    job_name: &str,
    namespace: &str,
    _spec: &JobSpec,
    now: Instant,
) -> Result<Vec<InstanceId>, BunError> {
    let instance_id = InstanceId(format!("{job_name}-0"));

    let instance = WorkloadInstance {
        id: instance_id.clone(),
        app_name: job_name.to_string(),
        namespace: namespace.to_string(),
        state: ContainerState::Pending,
        health_counters: HealthCounters::new(),
        restart_count: 0,
        last_restart: None,
        host_port: None,
        created_at: now,
        restart_policy: RestartPolicy::for_job(3),
        health_config: None,
        is_job: true,
        oci_spec: None,
    };

    self.instances.insert(instance_id.clone(), instance);
    // ...
    Ok(vec![instance_id])
}
```

Compare this to `deploy_app`. Three things are different. First, no port allocation. Jobs don't listen for connections, so `host_port` is always `None`. Second, no health check config. There's nothing to probe because the process will exit on its own. Third, the restart policy uses `RestartPolicy::for_job(3)` instead of the app's infinite-restart default. A job gets 3 retry attempts. After that, it's marked Failed and stays there. Nobody wants a broken migration retrying forever.

The `_spec` parameter has a leading underscore. That's Rust telling you the parameter is intentionally unused. Without the underscore, the compiler warns about unused variables. The underscore prefix suppresses the warning while keeping the parameter in the signature for forward compatibility. We'll use the spec in later phases when jobs need environment variables, resource limits, and secrets.

### The `command` field

Both apps and jobs have an optional `command` field in their TOML config:

```toml
[app.web]
image = "myapp:v1"
command = ["target/debug/testapp", "--mode", "healthy", "--port", "8080"]
port = 8080
```

The `image` field is required by real container runtimes (runc, Apple Container) to know which rootfs to use. But ProcessGrill doesn't pull images. It spawns OS processes. So for ProcessGrill, `command` is what actually matters. If neither `command` nor `image` produces something to run, ProcessGrill falls back to `sleep 86400` as a placeholder.

This split is honest about the difference between development and production. During development, you'll run with ProcessGrill and use `command` to point at local binaries. In production, the OCI image provides the command, and `command` becomes an override (like Kubernetes's `command` field overriding the image's `ENTRYPOINT`).

### The TestApp binary

To make examples and demos meaningful, we ship a configurable test HTTP server:

```sh
cargo run --bin testapp -- --mode healthy --port 8080
cargo run --bin testapp -- --mode unhealthy-after --count 5 --port 8080
cargo run --bin testapp -- --mode slow --delay 3000 --port 8080
```

It serves `/healthz` with configurable behaviour: always healthy, healthy for N requests then unhealthy, slow responses, or total hang. The example configs reference it via `command`, so when you `relish apply examples/phase-1/minimal-app.toml` with a running agent, you get real health checks against a real HTTP server. No containers needed.

## Health checking: priority queues and state transitions

A health check answers a simple question: is this container still working? The answer drives the state machine: if a container in `HealthWait` passes enough consecutive checks, it transitions to `Running`. If a `Running` container fails enough consecutive checks, it goes to `Unhealthy`. And if an `Unhealthy` container starts passing again, it recovers back to `Running`.

The tricky part isn't the logic. It's the scheduling.

### Separating scheduling from probing

You could run a health check loop for each container: `loop { sleep(interval); probe(); }`. With 500 containers, that's 500 concurrent loops. It works, but it's wasteful. Most of the time they're just sleeping.

Instead, we use a priority queue. Every health check is an entry with a deadline. The supervisor asks the queue: "what's the next check I need to run, and when?" Then it sleeps until that deadline, runs the check, and schedules the next one. One loop, one sleep, handles all containers.

This separation also makes testing much cleaner. The scheduling logic (when to check) is pure data structure manipulation. The probing logic (HTTP requests) involves network I/O. We can test the first exhaustively without touching a network socket.

### The priority queue

Rust's standard library provides `BinaryHeap`, a max-heap. We want a min-heap (earliest deadline first), so we wrap entries in `Reverse`:

```rust
pub struct HealthChecker {
    heap: BinaryHeap<Reverse<HealthCheckEntry>>,
    configs: HashMap<InstanceId, HealthCheckConfig>,
}
```

The `HealthCheckEntry` carries a deadline and an instance ID:

```rust
#[derive(Debug, Clone, Eq, PartialEq)]
struct HealthCheckEntry {
    deadline: Instant,
    instance_id: InstanceId,
}
```

To put something in a `BinaryHeap`, it needs to implement `Ord`. And `Ord` requires `PartialOrd`. This is where Rust differs from most languages.

In Python, you can compare anything to anything. In Go, comparison operators work on comparable types, and if they don't, you pass a `less` function. In Rust, comparison is split into two traits:

- `PartialOrd` means "these values can *sometimes* be compared." Not every pair of values has a defined ordering. The classic example is `f64`: `NaN` is not less than, equal to, or greater than any other value, including itself.
- `Ord` means "these values have a *total* ordering." Every pair can be compared, and the result is always `Less`, `Equal`, or `Greater`.

`BinaryHeap` requires `Ord` because it needs total ordering to maintain the heap invariant. If two entries couldn't be compared, the heap would break. We implement `Ord` on `HealthCheckEntry` by comparing deadlines:

```rust
impl Ord for HealthCheckEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline.cmp(&other.deadline)
    }
}

impl PartialOrd for HealthCheckEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
```

The `PartialOrd` implementation just delegates to `Ord` and wraps the result in `Some`. This is the standard boilerplate when you have a total ordering. It looks redundant, but it exists because Rust won't let you accidentally claim total ordering for a type that doesn't have it (like one containing `f64` fields).

Then `Reverse<HealthCheckEntry>` flips the ordering, turning the max-heap into a min-heap. `Reverse` is just a wrapper in `std::cmp` that reverses the `Ord` implementation. No custom comparator functions, no separate `MinHeap` type.

### Lazy deletion

When an instance is unregistered (say, because it was stopped), we don't scan the heap to remove its entry. That would be O(n). Instead, `unregister` only removes the instance from the `configs` HashMap:

```rust
pub fn unregister(&mut self, instance_id: &InstanceId) {
    self.configs.remove(instance_id);
}
```

When `pop_due` encounters a stale entry (one whose `instance_id` isn't in `configs`), it skips it:

```rust
pub fn pop_due(&mut self, now: Instant) -> Option<(InstanceId, HealthCheckConfig)> {
    while let Some(Reverse(entry)) = self.heap.peek() {
        if entry.deadline > now {
            return None;
        }
        let Reverse(entry) = self.heap.pop().unwrap();
        if let Some(config) = self.configs.get(&entry.instance_id) {
            return Some((entry.instance_id, config.clone()));
        }
    }
    None
}
```

This is called lazy deletion. The stale entries sit in the heap until they naturally rise to the top, at which point they're discarded. It's a well-known trick in priority queue implementations. The trade-off is memory: stale entries take up space until they're popped. For our scale (hundreds of containers, not millions), this is fine.

### Explicit time injection

Every method on `HealthChecker` that deals with time takes a `now: Instant` parameter instead of calling `Instant::now()` internally:

```rust
pub fn register(&mut self, instance_id: InstanceId, config: HealthCheckConfig, now: Instant) {
    let deadline = now + config.initial_delay;
    // ...
}
```

This makes tests completely deterministic. You create an `Instant`, advance it by known amounts, and assert on the results. No flaky tests from timing races. No need for time-mocking libraries. Just pass the time you want.

This is a general pattern worth adopting for any code that depends on the clock: make time a parameter, not a hidden dependency.

### evaluate_result: a pure function

The function that decides state transitions is deliberately stateless:

```rust
pub fn evaluate_result(
    status: HealthStatus,
    counters: &HealthCounters,
    current_state: ContainerState,
    config: &HealthCheckConfig,
) -> Option<ContainerState> {
    match (current_state, status.is_healthy()) {
        (ContainerState::HealthWait, true)
            if counters.consecutive_healthy >= config.threshold_healthy =>
        {
            Some(ContainerState::Running)
        }
        (ContainerState::Running, false)
            if counters.consecutive_unhealthy >= config.threshold_unhealthy =>
        {
            Some(ContainerState::Unhealthy)
        }
        (ContainerState::Unhealthy, true)
            if counters.consecutive_healthy >= config.threshold_healthy =>
        {
            Some(ContainerState::Running)
        }
        _ => None,
    }
}
```

It takes all its inputs as parameters, does a `match`, and returns `Option<ContainerState>`. `Some(new_state)` means "transition to this state." `None` means "no change." The caller decides what to do with the result.

The `match` uses Rust's pattern matching with guards (the `if` clauses). The tuple pattern `(current_state, status.is_healthy())` lets us branch on both values simultaneously. The `_` arm catches everything else: states where health check results don't drive transitions (`Pending`, `Preparing`, `Stopping`, etc.).

Making this a free function rather than a method on `HealthChecker` or `WorkloadInstance` is a deliberate choice. It has no hidden state, no side effects, and it's trivially testable. You can write a dozen test cases for it without setting up a supervisor, a mock runtime, or an instance. Just call the function with different inputs and check the output.

## Restart backoff

When a container fails, you want to restart it. But you don't want to restart it immediately, because if it's crashing due to a configuration error or a missing dependency, it'll just crash again. And again. And again. Each failed start consumes CPU, writes logs, and might even make things worse (imagine a container that corrupts a database on startup before crashing).

Exponential backoff solves this: wait 1 second before the first restart, 2 seconds before the second, 4 before the third, 8 before the fourth, and so on. If the failure is transient (a network hiccup, a briefly unavailable dependency), the container recovers quickly. If it's permanent, the restarts space out so you're not burning resources.

### The RestartPolicy struct

```rust
#[derive(Debug, Clone)]
pub struct RestartPolicy {
    pub max_restarts: Option<u32>,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub backoff_multiplier: f64,
}
```

`max_restarts` is `Option<u32>`, not `u32`. This is Rust's answer to the sentinel value problem. In Go, you might use `0` to mean "unlimited restarts" and then write a comment explaining it. But what if someone actually wants zero restarts? They can't, because `0` is already taken. The documentation becomes a critical part of the API, and the compiler can't help you if you forget to check it.

With `Option<u32>`, the meaning is unambiguous: `None` means no limit, `Some(5)` means at most 5 restarts. And `match` forces you to handle both cases:

```rust
pub fn should_restart(&self, restart_count: u32) -> bool {
    match self.max_restarts {
        None => true,
        Some(max) => restart_count < max,
    }
}
```

If you forget to handle one of the variants, the code won't compile. The compiler literally will not let you ignore the `None` case.

### Computing backoff without overflow

The backoff formula is `initial_backoff * multiplier^restart_count`. With a 2x multiplier, after 31 restarts that's 2^31 seconds, which overflows a 32-bit integer. After 63 restarts, it overflows a 64-bit integer. Rust catches integer overflow in debug mode (it panics) but wraps silently in release mode. Neither is what we want.

The solution is to do the arithmetic in floating point:

```rust
pub fn compute_backoff(&self, restart_count: u32) -> Duration {
    let base = self.initial_backoff.as_secs_f64();
    let multiplier = self.backoff_multiplier.powi(restart_count as i32);
    let uncapped = base * multiplier;
    let capped = uncapped.min(self.max_backoff.as_secs_f64());
    Duration::from_secs_f64(capped)
}
```

`f64` handles large exponents gracefully. At extreme values, `powi` returns infinity, and `infinity.min(300.0)` is `300.0` (the max backoff). No overflow, no panic, no wrapping. The `.min()` method on floats is exactly the cap we need.

`powi` is "power, integer exponent." It's faster than `powf` (which takes a float exponent) because integer exponentiation can use repeated squaring. The `i` suffix is Rust's naming convention for integer-argument variants.

### The struct update syntax

The `for_job` constructor uses `..Self::default()` to fill in fields it doesn't explicitly set:

```rust
pub fn for_job(max_restarts: u32) -> Self {
    Self {
        max_restarts: Some(max_restarts),
        ..Self::default()
    }
}
```

`..Self::default()` means "for any field I didn't mention above, take the value from `Self::default()`." It's similar to JavaScript's `{ ...defaults, ...overrides }` spread syntax, but checked at compile time. If `RestartPolicy` gains a new field later, `for_job` will automatically pick up its default. If the new field doesn't have a default (because `Default` isn't derived or implemented for the new type), the compiler will flag it.

## GPU detection: traits for optional dependencies

Some nodes have GPUs. Most don't. We need to know which GPUs are available so the scheduler can place GPU workloads on the right nodes. But GPU detection requires vendor-specific libraries (NVIDIA's NVML, for instance), and we don't want to pull those in as hard dependencies.

### The trait approach

We define a trait:

```rust
pub trait GpuDetector {
    fn detect(&self) -> Vec<GpuInfo>;
}
```

And a stub that always reports no GPUs:

```rust
pub struct StubGpuDetector;

impl GpuDetector for StubGpuDetector {
    fn detect(&self) -> Vec<GpuInfo> {
        Vec::new()
    }
}
```

The alternative would be conditional compilation:

```rust
#[cfg(feature = "nvml")]
fn detect_gpus() -> Vec<GpuInfo> { /* real NVML calls */ }

#[cfg(not(feature = "nvml"))]
fn detect_gpus() -> Vec<GpuInfo> { Vec::new() }
```

Both work, but they have different testing properties. With `#[cfg(feature)]`, your CI either compiles with the `nvml` feature (and tests the real GPU path, which requires actual GPU hardware) or without it (and tests nothing interesting). You can't test GPU-aware scheduling on a macOS laptop because the code literally doesn't exist in the binary.

With a trait, you can write a `FakeGpuDetector` that reports synthetic GPUs:

```rust
struct FakeGpuDetector {
    gpus: Vec<GpuInfo>,
}

impl GpuDetector for FakeGpuDetector {
    fn detect(&self) -> Vec<GpuInfo> {
        self.gpus.clone()
    }
}
```

Now you can test that the scheduler correctly places a workload requiring 2 GPUs on a node that reports 4, or rejects it when the node only has 1. All without GPU hardware, all on any platform, all in unit tests that run in milliseconds.

The trait boundary is a seam for testing, not just for polymorphism. Define the trait now, use the stub, swap in the real implementation later without changing any callers. This is dependency inversion in Rust: the same principle as in Go or Java, but with zero runtime cost because the compiler monomorphises generic code.

With the Bun agent core in place, we've added 62 tests (bringing the total to 197) covering the supervisor, health checking, restart backoff, and GPU detection. Every state transition is tested. Every edge case in backoff computation is covered. The priority queue correctly orders, lazily deletes, and reschedules. And the mock-based testing pattern we've established here will scale to every subsystem we build next.

## The Relish CLI

We've got a config parser, a container runtime, and a node agent. But so far, the only way to interact with Reliaburger is by writing Rust code. That's fine for tests, not so fine for humans. Time to build the CLI.

Relish is the command-line tool that operators use to interact with a Reliaburger cluster. Think `kubectl`, but with a lot less YAML and a lot more opinion. In Phase 1 we don't have a cluster yet, so Relish can't actually deploy anything. But it can do something genuinely useful: parse a config file, validate it, and show you exactly what *would* happen. A dry-run planner.

### clap and derive macros

Rust has several CLI parsing libraries. We're using [clap](https://docs.rs/clap/latest/clap/), the most widely used one, with its derive API. If you've used Go's `cobra` or Python's `click`, the idea is similar: you define your CLI structure as data types, and the library generates the parser.

Here's what the binary entry point looks like:

```rust
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "relish", version, about = "Reliaburger CLI")]
struct Cli {
    #[arg(long, default_value = "human", global = true)]
    output: OutputFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Apply { path: PathBuf },
    Status,
    Logs { name: String },
    Exec {
        app: String,
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    Inspect { name: String },
}
```

Two attributes do the heavy lifting here.

`#[derive(Parser)]` generates the argument parsing code at compile time. It reads the field names and types from your struct, figures out what flags and positional arguments to expect, and writes the parser for you. No runtime reflection, no code generation step — the Rust compiler does it during the normal build. If you add a field with the wrong type, you get a compile error, not a runtime crash.

`#[derive(Subcommand)]` does the same for the command enum. Each variant becomes a subcommand. The variant's fields become that subcommand's arguments. `Apply { path: PathBuf }` means `relish apply <path>` — clap figures out that `path` is a positional argument because it doesn't have `#[arg(long)]` or `#[arg(short)]`.

Compare this to Go's standard `flag` package, where you manually define each flag, parse them, then check which subcommand was used with `os.Args`:

```go
// Go: manual, repetitive, easy to forget a flag
applyCmd := flag.NewFlagSet("apply", flag.ExitOnError)
outputFlag := applyCmd.String("output", "human", "output format")
switch os.Args[1] {
case "apply":
    applyCmd.Parse(os.Args[2:])
    // ...
}
```

Or Python's `argparse`, where the parser is built at runtime:

```python
# Python: runtime construction, easy to get wrong
parser = argparse.ArgumentParser()
subparsers = parser.add_subparsers()
apply_parser = subparsers.add_parser("apply")
apply_parser.add_argument("path")
apply_parser.add_argument("--output", default="human")
```

With clap's derive API, you get the same result by writing a struct. The compiler catches typos. Adding a new subcommand means adding an enum variant. If you forget to handle it in your `match`, the compiler tells you.

### ValueEnum for flag types

The `--output` flag isn't a `String`. It's an `OutputFormat`:

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    #[default]
    Human,
    Json,
    Yaml,
}
```

`#[derive(clap::ValueEnum)]` teaches clap how to parse this enum from command-line text. `--output human`, `--output json`, and `--output yaml` all just work. Anything else gets a clear error message. No string matching, no `if output == "json"` scattered through your code.

The `#[default]` attribute on `Human` means you don't even need to specify `--output` — it defaults to human-readable text. This is a Rust enum feature that works with the `Default` trait. In Go you'd check for an empty string; in Python you'd pass `default=` to `argparse`. Here the default is part of the type definition itself.

Why does this matter? Because we use `OutputFormat` in a `match` expression later:

```rust
pub fn format_output<T: Serialize + fmt::Display>(
    value: &T,
    format: OutputFormat,
) -> Result<String, RelishError> {
    match format {
        OutputFormat::Human => Ok(value.to_string()),
        OutputFormat::Json => serde_json::to_string_pretty(value)
            .map_err(RelishError::SerialiseJson),
        OutputFormat::Yaml => serde_yaml::to_string(value)
            .map_err(RelishError::SerialiseYaml),
    }
}
```

If we ever add a fourth format (say, `Table`), the compiler will refuse to build until we handle it in every `match`. A `String` flag can't do that.

### The plan pattern

The core of Phase 1's CLI value lives in `generate_plan`. It takes a parsed `Config` and produces a structured plan showing what would be deployed:

```rust
pub fn generate_plan(config: &Config) -> ApplyPlan {
    let mut entries = Vec::new();

    for (name, app) in &config.app {
        let mut summary = Vec::new();
        if let Some(ref image) = app.image {
            summary.push(("image".to_string(), image.clone()));
        }
        summary.push(("replicas".to_string(), app.replicas.to_string()));
        // ... port, health, memory, cpu, namespace
        entries.push(PlanEntry {
            resource: format!("app.{name}"),
            action: PlanAction::Create,
            summary,
        });
    }
    // ... jobs, namespaces, permissions

    ApplyPlan {
        to_create: entries.len(),
        entries,
        to_update: 0,
        to_destroy: 0,
        unchanged: 0,
    }
}
```

Notice `if let Some(ref image) = app.image`. This is pattern matching with a reference — `ref` borrows the inner `String` instead of moving it out of the `Option`. In Rust, `match` and `if let` can destructure values, and `ref` says "I want to look at this, not take ownership of it." Without `ref`, the compiler would complain that you're trying to move `image` out of a borrowed `app`.

The function iterates `config.app`, which is a `BTreeMap<String, AppSpec>`. We chose `BTreeMap` over `HashMap` back in the config chapter for deterministic ordering — and here's the payoff. The plan entries come out in alphabetical order. Tests can assert on ordering without fragility. The YAML and JSON output is stable across runs. Small decision, compound benefit.

In Phase 1 every resource gets `PlanAction::Create` because there's no cluster state to compare against. In Phase 2, when we have a running cluster, this function will diff the desired state (config file) against the actual state (cluster) and produce `Update`, `Destroy`, and `Unchanged` entries too. The structure is ready for that — we just haven't implemented it yet.

### One data structure, three output formats

`ApplyPlan` implements both `Display` (for human output) and `Serialize` (for JSON and YAML). One struct, three views:

```
$ relish apply cluster.toml

Relish apply plan:

  + app.web
      image     myapp:v1.4.2
      replicas  3
      port      8080
      health    /healthz

  + job.db-migrate
      image     myapp:v1.4.2
      command   npm run migrate

Plan: 2 to create, 0 to update, 0 to destroy.
```

```
$ relish --output json apply cluster.toml
{
  "entries": [
    {
      "resource": "app.web",
      "action": "create",
      "summary": [["image", "myapp:v1.4.2"], ...]
    }
  ],
  "to_create": 2,
  ...
}
```

YAML output is there for Kubernetes refugees who want the comfort of familiar syntax. `serde_yaml` makes it trivial — same `#[derive(Serialize)]`, different serialiser.

The `Display` implementation for `ApplyPlan` uses `write!` and `writeln!` macros, which work like `format!` but write directly to a formatter instead of allocating a new string. The `+` prefix on each entry is the `Display` implementation for `PlanAction`:

```rust
impl fmt::Display for PlanAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanAction::Create => write!(f, "+"),
        }
    }
}
```

If you've used Terraform, this will look familiar. Green `+` for create, yellow `~` for update (Phase 2), red `-` for destroy (Phase 2). Same idea, applied to container workloads instead of cloud infrastructure.

### Error handling: library vs binary

The Relish module has its own error type:

```rust
#[derive(Debug, thiserror::Error)]
pub enum RelishError {
    #[error("{0}")]
    Config(#[from] ConfigError),

    #[error("failed to serialise JSON: {0}")]
    SerialiseJson(serde_json::Error),

    #[error("failed to serialise YAML: {0}")]
    SerialiseYaml(serde_yaml::Error),

    #[error("{command} requires a running Bun agent (not available in single-node mode yet)")]
    AgentRequired { command: String },

    #[error("bun agent not reachable at localhost:9117 (is it running?)")]
    AgentUnreachable,

    #[error("API error (status {status}): {body}")]
    ApiError { status: u16, body: String },
}
```

Two things worth pointing out. First, `#[from] ConfigError` generates a `From<ConfigError> for RelishError` implementation, which lets the `?` operator automatically convert config errors. When `Config::from_file(path)?` fails inside `apply()`, the `ConfigError` gets wrapped into `RelishError::Config` without any manual conversion.

Second, the two serialisation variants (`SerialiseJson` and `SerialiseYaml`) don't use `#[from]`. They can't, because `#[from]` generates `From<E>` implementations, and both `serde_json::Error` and `serde_yaml::Error` would conflict — Rust's orphan rules don't allow two blanket `From` impls that could overlap. Instead, we use `.map_err(RelishError::SerialiseJson)` at the call site. A small bit of ceremony, but it makes the error's origin unambiguous.

The binary's `main()` function doesn't use `anyhow::Result` anymore. Instead, it returns `ExitCode`:

```rust
#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Apply { ref path } => commands::apply(path, cli.output).await,
        Command::Status => commands::status().await,
        // ...
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
```

`ExitCode` is from Rust's standard library (`std::process::ExitCode`). The `#[tokio::main]` macro sets up the async runtime, same as in `bun`. We need async here because the command functions talk to the bun agent over HTTP. `ExitCode` lets you return a proper exit code to the shell without going through `anyhow`'s default error printing, which includes a `Error:` prefix and the Debug representation. We want clean, lowercase error messages on stderr, so we format them ourselves with `eprintln!`.

The split is: `thiserror` in the library for structured, matchable errors. Manual `ExitCode` in the binary for clean user-facing output. The library defines *what* went wrong; the binary decides *how* to tell the user.

### Graceful stubs

The previous version of the binary had `todo!("Phase 1")` in every match arm. That compiles, but it panics at runtime with a stack trace. Not a great user experience.

Now, the stub commands return structured errors:

```rust
pub fn status() -> Result<(), RelishError> {
    Err(RelishError::AgentRequired {
        command: "status".to_string(),
    })
}
```

Running `relish status` prints:

```
error: status requires a running Bun agent (not available in single-node mode yet)
```

No stack trace, no panic, proper exit code. The error message tells you *what's* wrong and *why*. When Phase 2 adds the agent, we'll replace the error return with real logic, and the function signature won't change.

### Testing CLI argument parsing

You might wonder: how do you test a `main()` function? You don't. You test the argument parsing separately:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    #[test]
    fn parse_apply_command() {
        let cli = parse(&["relish", "apply", "config.toml"]).unwrap();
        assert!(matches!(cli.command,
            Command::Apply { ref path } if path.to_str() == Some("config.toml")));
    }

    #[test]
    fn invalid_output_format_rejected() {
        assert!(parse(&["relish", "--output", "csv", "status"]).is_err());
    }
}
```

`Cli::try_parse_from` parses from a string slice instead of reading `std::env::args()`. Same parsing logic, no process spawning needed. The test for invalid output proves that clap rejects unknown formats at parse time — our `ValueEnum` derive handles it.

The command functions themselves are tested through the library. `apply()` gets tested with temporary files (created by the `tempfile` crate), exercising the full pipeline: read file, parse TOML, validate config, generate plan, format output. The stub commands get one test each, proving they return the right `AgentRequired` error variant.

With the Relish CLI skeleton, we've added 36 tests (bringing the total to 233). The CLI parses five subcommands with a global `--output` flag. `apply` does real work — it reads, validates, and plans. The other four commands fail gracefully instead of panicking. Three output formats — human, JSON, YAML — work through a single `format_output` function that dispatches on a type-safe enum. And the error handling follows the pattern we'll use everywhere: `thiserror` in the library, manual formatting in the binary.

## A process-based runtime

We have a `Grill` trait that knows how to create, start, stop, and query containers. We have an OCI spec generator that produces the right cgroup limits and port mappings. We have a state machine that tracks what's Pending, Running, or Stopped. But none of it does anything yet. Time to give Grill a body.

The obvious choice would be to jump straight to `runc`. But runc only works on Linux, and half the development is happening on macOS. We need something that works everywhere so the test suite runs on every developer's machine.

The answer is `ProcessGrill`: a `Grill` implementation that spawns child processes instead of containers. Each "container" is just an OS process. No namespaces, no cgroups, no rootfs. But it implements the same trait, so the agent doesn't know the difference.

```rust
pub struct ProcessGrill {
    processes: Arc<Mutex<HashMap<InstanceId, ProcessEntry>>>,
}

struct ProcessEntry {
    spec: OciSpec,
    child: Option<tokio::process::Child>,
    state: ContainerState,
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    exit_code: Option<i32>,
}
```

Two things worth noting here. First, the `Arc<Mutex<HashMap<...>>>` pattern. `Arc` is Rust's atomic reference counter — it lets multiple owners share the same data. `Mutex` (from `tokio::sync`, not `std::sync` — never use the standard mutex in async code, it blocks the runtime) guards mutation. Together they give you shared mutable state across async tasks. In Go you'd use a `sync.Mutex` with a map; in Python you'd just rely on the GIL. Rust makes you be explicit about every shared mutation, which is verbose but prevents data races at compile time.

Second, we store `Option<tokio::process::Child>` rather than always having a child. A container can be created without being started (the OCI spec separates `create` from `start`). The `Option` reflects this: `None` before start, `Some(child)` after. If you try to start something that already has a child, that's a bug, and we reject it.

The `start()` method builds a `tokio::process::Command` from the OCI spec:

```rust
let mut cmd = Command::new(&effective_args[0]);
if effective_args.len() > 1 {
    cmd.args(&effective_args[1..]);
}

for env_str in &entry.spec.process.env {
    if let Some((key, value)) = env_str.split_once('=') {
        cmd.env(key, value);
    }
}

cmd.stdout(std::process::Stdio::piped());
cmd.stderr(std::process::Stdio::piped());
```

`tokio::process::Command` is the async version of `std::process::Command`. Same API, but the process is managed by tokio's event loop instead of blocking a thread. We pipe stdout and stderr so we can capture logs — each gets a spawned task that reads into a buffer:

```rust
let stdout_buf = entry.stdout_buf.clone();
if let Some(stdout) = child.stdout.take() {
    tokio::spawn(async move {
        let mut reader = stdout;
        let mut buf = vec![0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let mut out = stdout_buf.lock().await;
                    out.extend_from_slice(&buf[..n]);
                }
                Err(_) => break,
            }
        }
    });
}
```

`tokio::spawn(async move { ... })` has two parts worth unpacking. `tokio::spawn` takes a future and runs it as an independent task on the runtime — like launching a goroutine in Go or starting a thread, but cooperatively scheduled. The `async move` block is how you create that future. `async` makes it a future (code that can be paused and resumed at each `.await` point). `move` tells the compiler to *move* captured variables into the block, transferring ownership. Without `move`, the block would try to borrow `stdout_buf` and `stdout` from the surrounding scope. But the surrounding function might return before the spawned task finishes, which would leave the task holding dangling references. Rust's borrow checker catches this at compile time and refuses to compile it. `move` fixes it by giving the task its own copy of `stdout_buf` (which is an `Arc`, so "copy" means incrementing a reference count) and full ownership of `stdout`. The spawned task now owns everything it needs and can outlive the function that created it.

`child.stdout.take()` is a pattern you'll see a lot. `take()` moves the value out of the `Option`, leaving `None` behind. We need ownership of the stdout handle to send it into the spawned task — but we also need the `child` struct to keep existing. `take()` lets us split the child from its stdout cleanly.

Stopping a process uses Unix signals via the `nix` crate:

```rust
if let Some(ref child) = entry.child
    && let Some(pid) = child.id()
{
    let pid = nix::unistd::Pid::from_raw(pid as i32);
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
}
```

That `if let ... && let ...` syntax is a Rust 2024 edition feature called *let chains*. It lets you chain multiple pattern matches in a single `if`. The previous edition would have required nested `if let` blocks.

`kill()` is simpler — it uses `child.kill().await` from tokio, which sends SIGKILL. And `state()` calls `child.try_wait()` to check if the process has exited without blocking:

```rust
match child.try_wait() {
    Ok(Some(_status)) => { entry.state = ContainerState::Stopped; }
    Ok(None)          => { /* still running */ }
    Err(_)            => { entry.state = ContainerState::Stopped; }
}
```

`try_wait` returns `Ok(None)` if the process is still running, `Ok(Some(status))` if it's exited, and `Err` if something went wrong checking. We map all terminal cases to `Stopped`.

ProcessGrill now has 8 tests covering every lifecycle transition. They run in milliseconds, on any platform, without root privileges. That's the whole point: by abstracting the runtime behind a trait, we can test everything at full speed and add real containers later.

## Real containers: runc and Apple Container

ProcessGrill is a cheat — it spawns processes, not containers. For Phase 1 that's fine, but we also want to prove that the OCI specs we've been generating actually work with real container runtimes.

### Runc on Linux

`runc` is the reference OCI container runtime. Docker uses it under the hood. It reads an OCI bundle (a directory with `config.json` and a `rootfs`), creates Linux namespaces, sets up cgroups, and runs the container. `RuncGrill` calls the `runc` binary directly via `tokio::process::Command`:

```rust
pub struct RuncGrill {
    bundle_base: PathBuf,
    entries: Arc<Mutex<HashMap<InstanceId, RuncEntry>>>,
}
```

The implementation is straightforward shell-out:

```rust
async fn runc_command(
    &self,
    args: &[&str],
    instance: &InstanceId,
) -> Result<std::process::Output, GrillError> {
    tokio::process::Command::new("runc")
        .args(args)
        .output()
        .await
        .map_err(|e| GrillError::StartFailed {
            instance: instance.clone(),
            reason: format!("failed to run runc: {e}"),
        })
}
```

Creating a container means writing the OCI spec to disk and calling `runc create`:

```rust
// Write the OCI spec as config.json
let spec_json = serde_json::to_string_pretty(spec)?;
tokio::fs::write(bundle_dir.join("config.json"), spec_json).await?;

// Create rootfs directory
tokio::fs::create_dir_all(bundle_dir.join("rootfs")).await?;

// Call runc create
self.runc_command(&["create", "--bundle", &bundle_str, &instance.0], instance).await?;
```

Querying state parses the JSON that `runc state` returns:

```rust
let state_json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
let status = state_json["status"].as_str().unwrap_or("unknown");

match status {
    "created" => Ok(ContainerState::Pending),
    "running" => Ok(ContainerState::Running),
    "stopped" => Ok(ContainerState::Stopped),
    other => Err(/* unknown state */),
}
```

As we discussed earlier, we call runc directly rather than going through containerd. One binary, no daemon, no protobuf.

`RuncGrill` is Linux-only, gated behind `#[cfg(target_os = "linux")]`. Its tests require root privileges and `runc` installed, so they're further gated behind an environment variable: `RELIABURGER_RUNC_TESTS=1`.

### Apple Container on macOS

Apple shipped a `container` CLI tool that runs Linux containers in lightweight VMs on Apple Silicon. It's OCI-compatible, pulls standard images from Docker Hub, and has a Docker-like command set: `container create`, `container start`, `container stop`, `container inspect`.

`AppleContainerGrill` maps our `OciSpec` to `container` CLI flags:

- `spec.process.env` becomes repeated `-e KEY=VALUE` flags
- `spec.linux.resources.memory.limit` becomes `--memory {bytes}`
- `spec.linux.resources.cpu` becomes `--cpus {count}`
- `spec.mounts` become `--volume host:container` flags

The state query parses `container inspect` JSON, similar to runc. Like RuncGrill, it's platform-gated (`#[cfg(target_os = "macos")]`) and its tests are behind `RELIABURGER_APPLE_CONTAINER_TESTS=1`.

### The AnyGrill dispatch pattern

Now we have three implementations. The bun binary needs to pick one at startup. In Go, you'd reach for an interface and use it directly. In Rust, you'd normally use a trait object (`Box<dyn Grill>`). But our `Grill` trait has methods that return `impl Future`, which makes it not *object-safe*. You can't put non-object-safe traits behind `dyn`. This is one of those places where Rust's type system forces you to think about what you're doing.

The fix is an enum that delegates:

```rust
pub enum AnyGrill {
    Process(ProcessGrill),
    #[cfg(target_os = "linux")]
    Runc(RuncGrill),
    #[cfg(target_os = "macos")]
    Apple(AppleContainerGrill),
}

impl Grill for AnyGrill {
    async fn create(&self, instance: &InstanceId, spec: &OciSpec) -> Result<(), GrillError> {
        match self {
            AnyGrill::Process(g) => g.create(instance, spec).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.create(instance, spec).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.create(instance, spec).await,
        }
    }
    // ... same for start, stop, kill, state
}
```

Every method is just a match-and-delegate. Mechanical, repetitive, but dead simple. The `#[cfg]` attributes mean the enum only has the variants available on the current platform. On macOS, there's no `Runc` variant. On Linux, there's no `Apple` variant. `Process` is always there.

Runtime detection happens at startup:

```rust
pub async fn detect_runtime() -> AnyGrill {
    #[cfg(target_os = "macos")]
    if which_exists("container").await {
        return AnyGrill::Apple(AppleContainerGrill::new());
    }

    #[cfg(target_os = "linux")]
    if which_exists("runc").await {
        return AnyGrill::Runc(RuncGrill::new(/* ... */));
    }

    AnyGrill::Process(ProcessGrill::new())
}
```

Check for the platform-native runtime. If it's there, use it. Otherwise, fall back to processes. A `--runtime` CLI flag lets you override this.

## Probing for health

The health checker from earlier in this chapter makes pure decisions: given a sequence of probe results, should the container be considered healthy or unhealthy? But it doesn't actually probe anything. Time to add the effectful half.

```rust
pub async fn probe_health(config: &HealthCheckConfig, host: &str) -> HealthStatus {
    let url = format!("{}://{}:{}{}", config.protocol, host, config.port, config.path);

    let client = reqwest::Client::builder()
        .timeout(config.timeout)
        .danger_accept_invalid_certs(true)
        .build();

    match tokio::time::timeout(
        config.timeout + Duration::from_secs(1),
        client.get(&url).send(),
    ).await {
        Ok(Ok(response)) => {
            if response.status().is_success() {
                HealthStatus::Healthy
            } else {
                HealthStatus::Unhealthy
            }
        }
        Ok(Err(e)) => {
            if e.is_timeout() { HealthStatus::Timeout }
            else { HealthStatus::ConnectionRefused }
        }
        Err(_) => HealthStatus::Timeout,
    }
}
```

There's a double timeout here, and it's deliberate. `reqwest` has its own timeout (`config.timeout`), and we wrap the whole thing in `tokio::time::timeout` with an extra second. The outer timeout is a safety net: if reqwest's timeout mechanism fails (which can happen with certain connection states), we still give up. Belt and suspenders.

`danger_accept_invalid_certs(true)` looks alarming, but it's correct for health probes. We're probing localhost containers that might use self-signed certs. We're checking "is the process alive and responding," not "is the TLS certificate valid." Certificate validation belongs in the security layer (Phase 4).

The return type maps directly to the `HealthStatus` enum the health checker already understands. No new types, no adapters. The pure logic processes results the same way whether they came from a real HTTP request or a test fixture.

Testing health probes against mocks would miss the point. The whole purpose of this function is to do real I/O. So we test against real TCP listeners:

```rust
#[tokio::test]
async fn healthy_on_200() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        let mut buf = vec![0u8; 1024];
        let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
        socket.write_all(response.as_bytes()).await.unwrap();
    });

    let config = test_config(port);
    let status = probe_health(&config, "127.0.0.1").await;
    assert_eq!(status, HealthStatus::Healthy);
}
```

Bind to port 0 (the OS picks an ephemeral port), spawn a minimal HTTP server that sends a canned response, and probe it. Five tests cover the interesting cases: 200, 500, timeout (accept but never respond), connection refused (nothing listening), and correct path verification. Each runs in milliseconds because it's all localhost.

## The main event loop

Every piece we've built so far is inert. The supervisor tracks state but doesn't drive transitions. The health checker makes decisions but doesn't probe anything. The grill can start containers but nobody tells it to. The agent ties them all together.

```rust
pub struct BunAgent<G: Grill> {
    supervisor: WorkloadSupervisor<G>,
    command_rx: mpsc::Receiver<AgentCommand>,
    shutdown: CancellationToken,
}
```

Three fields. The supervisor manages workload state and the container runtime. The `command_rx` receives instructions from the API server. The `CancellationToken` coordinates shutdown across all tasks.

Commands arrive as an enum with `oneshot` response channels:

```rust
pub enum AgentCommand {
    Deploy {
        config: Config,
        response: oneshot::Sender<Result<ApplyResult, BunError>>,
    },
    Stop {
        app_name: String,
        namespace: String,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    Status {
        response: oneshot::Sender<Vec<InstanceStatus>>,
    },
    Logs {
        app_name: String,
        namespace: String,
        response: oneshot::Sender<Result<String, BunError>>,
    },
}
```

Each variant carries the data it needs plus a `oneshot::Sender` for the result. The sender is a one-use channel: you send one value, the receiver gets it, and the channel is consumed. This gives us request-response semantics over `mpsc`. The HTTP handler sends a command and `await`s the oneshot; the agent processes the command and sends the result back. No shared state between the API layer and the agent.

The event loop uses `tokio::select!`:

```rust
pub async fn run(&mut self) {
    let mut health_interval = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = self.shutdown.cancelled() => {
                self.shutdown_all().await;
                break;
            }
            Some(cmd) = self.command_rx.recv() => {
                self.handle_command(cmd).await;
            }
            _ = health_interval.tick() => {
                self.run_health_checks().await;
                self.check_jobs().await;
                self.drive_pending_restarts().await;
            }
        }
    }
}
```

`tokio::select!` is like Go's `select` statement but for Rust futures. It polls all branches simultaneously and runs whichever one completes first. The others are cancelled (dropped). Three things can happen:

1. **Shutdown requested** — the `CancellationToken` fires. Stop all instances, break the loop.
2. **Command received** — dispatch to the appropriate handler.
3. **Health check timer fires** — probe all instances that have health checks configured.

The order matters. Rust's `select!` checks branches in the order they appear, so shutdown always takes priority. In practice, since all three are polled simultaneously and only one can fire per iteration, this mainly matters during shutdown: if a command arrives at the same time as the cancellation, we process the cancellation.

When a deploy command arrives, the agent drives instances through the state machine:

```rust
async fn drive_instance_startup(
    &mut self,
    instance_id: &InstanceId,
    app_name: &str,
    namespace: &str,
    spec: &AppSpec,
) -> Result<(), BunError> {
    // Pending → Preparing
    instance.state = instance.state.transition_to(ContainerState::Preparing)?;

    // Generate OCI spec and create the container
    let oci_spec = generate_oci_spec(app_name, namespace, spec, host_port, &cgroup_str);
    self.supervisor.grill().create(instance_id, &oci_spec).await?;

    // Preparing → Starting
    instance.state = instance.state.transition_to(ContainerState::Starting)?;
    self.supervisor.grill().start(instance_id).await?;

    // Starting → HealthWait
    instance.state = instance.state.transition_to(ContainerState::HealthWait)?;

    // If no health check configured, jump straight to Running
    if spec.health.is_none() {
        instance.state = instance.state.transition_to(ContainerState::Running)?;
    }

    Ok(())
}
```

Each `transition_to` call validates the transition against the state machine. Try to go from Pending to Running directly? The state machine rejects it. This isn't defensive programming — it's the compiler and the state machine working together to ensure the lifecycle always follows the expected sequence.

Health checks happen on the timer tick. For each instance in `HealthWait` or `Running`, the agent probes the health endpoint and feeds the result to the health checker. If the health checker says the instance is now healthy (enough consecutive successful probes), the agent transitions it to `Running`. If it's unhealthy, the agent stops and restarts it, applying backoff between retries.

The health tick also does two more things:

```rust
_ = health_interval.tick() => {
    self.run_health_checks().await;
    self.check_jobs().await;
    self.drive_pending_restarts().await;
}
```

`check_jobs` monitors running jobs for process exit and handles retry logic. `drive_pending_restarts` picks up instances that have been reset to Pending after a failure and drives them through startup again. We'll look at both of these next.

## Jobs, init containers, and restarts

### Monitoring jobs

Apps run forever. Jobs exit. The agent needs to notice when a job process has stopped and decide what to do about it. That's `check_jobs`:

```rust
async fn check_jobs(&mut self) {
    let running_jobs: Vec<InstanceId> = self
        .supervisor
        .list_instances()
        .iter()
        .filter(|i| i.is_job && i.state == ContainerState::Running)
        .map(|i| i.id.clone())
        .collect();

    for id in running_jobs {
        let grill_state = match self.supervisor.grill().state(&id).await {
            Ok(s) => s,
            Err(_) => continue,
        };

        if grill_state == ContainerState::Stopped {
            let exit_code = self.supervisor.grill().exit_code(&id).await;

            // Transition Running → Stopping → Stopped
            // ...

            if exit_code == Some(0) {
                continue;  // success — stays Stopped
            }

            // Failed — attempt restart
            match self.supervisor.maybe_restart(&id, now).await {
                Ok(true) => { /* now Pending */ }
                Ok(false) => { /* backoff not elapsed */ }
                Err(_) => { /* exceeded limit — mark Failed */ }
            }
        }
    }
}
```

The pattern is: collect, iterate, check. We collect all running job IDs into a `Vec` first, then iterate. Why not iterate directly over `list_instances()`? Because inside the loop we call `self.supervisor.grill().state()`, which borrows the supervisor. If we were iterating over a reference to the supervisor's internal data, the borrow checker would reject the second borrow. Collecting into a `Vec` gives us owned data that doesn't conflict with later borrows. This is a common pattern in Rust when you need to read a collection and then mutate based on what you found.

For each running job, we ask the grill what the process is actually doing. If it's still running, we move on. If it's stopped, we check the exit code. Exit code 0 means success. Anything else means failure, and we try to restart.

### Exit code tracking

How does the grill know the exit code? ProcessGrill stores it when the process exits:

```rust
async fn state(&self, instance: &InstanceId) -> Result<ContainerState, GrillError> {
    let mut procs = self.processes.lock().await;
    let entry = procs.get_mut(instance).ok_or(/* ... */)?;

    if entry.state == ContainerState::Running {
        if let Some(ref mut child) = entry.child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    entry.state = ContainerState::Stopped;
                    entry.exit_code = status.code();
                }
                // ...
            }
        }
    }

    Ok(entry.state)
}
```

`try_wait()` is a non-blocking check. If the child process has exited, it returns `Ok(Some(status))`. If it's still running, `Ok(None)`. We call this on every state check rather than spawning a background waiter, because the health tick already gives us a regular polling interval. `status.code()` returns `Option<i32>`: `Some(0)` for success, `Some(n)` for failure with exit code n, and `None` if the process was killed by a signal (on Unix).

The `exit_code()` method on the `Grill` trait has a default implementation that returns `None`:

```rust
fn exit_code(
    &self,
    instance: &InstanceId,
) -> impl std::future::Future<Output = Option<i32>> + Send {
    let _ = instance;
    std::future::ready(None)
}
```

This is a trait method with a default body. Types that implement `Grill` can override it (ProcessGrill does), or accept the default (MockGrill can configure it per test). The `let _ = instance` suppresses the unused-variable warning while keeping the parameter in the signature. `std::future::ready(None)` creates a future that immediately resolves to `None`, which is the async equivalent of `return None` for a synchronous function.

### Init containers

Some apps need setup work before they can start. A database migration, a config file download, a certificate generation. Kubernetes calls these "init containers": processes that run to completion before the main container starts. If any init container fails, the main container never starts.

The agent handles init containers during the startup sequence, between Preparing and Starting:

```rust
if !spec.init.is_empty() {
    instance.state = instance.state.transition_to(ContainerState::Initialising)?;

    for (i, init_spec) in spec.init.iter().enumerate() {
        let init_id = InstanceId(format!("{}-init-{i}", instance_id.0));
        let init_oci = generate_init_oci_spec(
            &init_spec.command, namespace, app_name,
            spec.image.as_deref(), &cgroup_str,
        );

        self.supervisor.grill().create(&init_id, &init_oci).await?;
        self.supervisor.grill().start(&init_id).await?;

        // Wait for init container to complete
        let failed = loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let state = self.supervisor.grill().state(&init_id).await?;
            if state == ContainerState::Stopped {
                let exit_code = self.supervisor.grill().exit_code(&init_id).await;
                break exit_code != Some(0);
            }
        };

        if failed {
            instance.state = instance.state.transition_to(ContainerState::Failed)?;
            return Err(BunError::InitContainerFailed {
                instance_id: instance_id.clone(),
                init_index: i,
            });
        }
    }
}
```

Each init container gets its own `InstanceId` (e.g., `web-0-init-0`, `web-0-init-1`) and a minimal OCI spec. They run sequentially. `.enumerate()` gives us both the index and the value from the iterator, so we can report which init container failed.

The `loop { ... break ... }` pattern is how you write a polling loop in Rust. The `break` expression evaluates to a value (the boolean `failed`), which becomes the value of the `let failed = loop { ... }` expression. This is another case of "everything is an expression": `loop` produces a value via `break`, just like `if`/`else` produces a value via its branches. In C you'd declare `bool failed` before the loop and assign it inside, which is more fragile because the compiler can't verify that the variable is always assigned before the loop exits.

The state transition path for apps with init containers is: Pending → Preparing → Initialising → Starting → HealthWait → Running. Without init containers, it's: Pending → Preparing → Starting → HealthWait → Running. The state machine validates both paths because we added Preparing → Initialising and Initialising → Starting as valid transitions.

### Restart re-drive

When a health check fails or a job exits with a non-zero code, the agent calls `maybe_restart()`. If the restart policy allows it, `maybe_restart` transitions the instance back to Pending, increments the restart counter, and applies exponential backoff. But that leaves the instance sitting in Pending. Something needs to drive it through startup again.

That's `drive_pending_restarts`:

```rust
async fn drive_pending_restarts(&mut self) {
    let pending_restarts: Vec<(InstanceId, OciSpec)> = self
        .supervisor
        .list_instances()
        .iter()
        .filter(|i| i.state == ContainerState::Pending && i.restart_count > 0)
        .filter_map(|i| i.oci_spec.as_ref().map(|spec| (i.id.clone(), spec.clone())))
        .collect();

    for (id, oci_spec) in pending_restarts {
        // Pending → Preparing → Starting → HealthWait → Running
        // (same sequence as initial startup, using stored OCI spec)
    }
}
```

The filter logic is specific: only instances that are Pending *and* have been restarted at least once. Fresh deploys are driven by `deploy()` directly. Restarted instances are driven by this method on the next health tick.

`.filter_map()` combines filtering and transforming. For each instance that passes the filter, it tries to extract the OCI spec. `i.oci_spec.as_ref()` converts `Option<OciSpec>` to `Option<&OciSpec>` (borrowing the inner value without moving it), then `.map(|spec| ...)` transforms the `Some` case while leaving `None` unchanged. `filter_map` drops the `None`s and unwraps the `Some`s. In Go, you'd write an `if` inside a `for` loop. The Rust version chains iterators, which avoids the intermediate `Vec` until `.collect()` materialises the results at the end.

This is called the "collect-then-process" pattern. We gather everything we need to act on (releasing the borrow on the supervisor), then iterate and mutate. The alternative would be indexing into the instances map by position, which is both less readable and fragile if the map changes size during iteration.

## A local API

The agent runs as a loop processing commands. Something needs to feed those commands in. That's the HTTP API: a thin axum server that translates HTTP requests into `AgentCommand` values and sends them over the channel.

```rust
pub fn router(cmd_tx: mpsc::Sender<AgentCommand>) -> Router {
    let state = ApiState { cmd_tx };

    Router::new()
        .route("/v1/health", get(health_handler))
        .route("/v1/apply", post(apply_handler))
        .route("/v1/status", get(status_handler))
        .route("/v1/status/{app}/{namespace}", get(status_app_handler))
        .route("/v1/stop/{app}/{namespace}", post(stop_handler))
        .route("/v1/logs/{app}/{namespace}", get(logs_handler))
        .with_state(state)
}
```

axum is Rust's answer to Express or Gin. Routes map HTTP methods and paths to handler functions. `.with_state(state)` makes the `ApiState` available to every handler via the `State` extractor. If you've used dependency injection in other frameworks, this is similar — but resolved at compile time, not runtime.

Each handler follows the same pattern:

```rust
async fn status_handler(
    State(state): State<ApiState>,
) -> impl IntoResponse {
    let (resp_tx, resp_rx) = oneshot::channel();
    if state.cmd_tx.send(AgentCommand::Status { response: resp_tx }).await.is_err() {
        return /* 500 error */;
    }
    match resp_rx.await {
        Ok(statuses) => Json(statuses).into_response(),
        Err(_) => /* 500 error */,
    }
}
```

Create a oneshot channel. Send the command with the sender half. Await the receiver half. Return the result as JSON. The handler doesn't know how the agent processes commands — it just sends and waits. This separation means we can test the API layer independently from the agent logic.

The `apply` handler does a bit more work: it parses the TOML body and validates the config before sending the command, returning 400 for invalid input:

```rust
async fn apply_handler(State(state): State<ApiState>, body: String) -> Response {
    let config = match Config::parse(&body) {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e.to_string() }))).into_response(),
    };

    if let Err(e) = config.validate() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e.to_string() }))).into_response();
    }

    // ... send to agent
}
```

The API listens on `127.0.0.1:9117` — localhost only. In Phase 1, there's no cluster, no mTLS, no authentication. Opening it to the network would be a security hole. We'll add proper auth in Phase 4 and bind to all interfaces when the cluster needs it.

## Relish talks to Bun

With the API running, the CLI can stop returning "agent required" errors and start doing real work. `BunClient` is a simple HTTP client:

```rust
pub struct BunClient {
    base_url: String,
    client: reqwest::Client,
}

impl BunClient {
    pub fn default_local() -> Self {
        Self::new("http://127.0.0.1:9117")
    }

    pub async fn health(&self) -> Result<(), RelishError> {
        let url = format!("{}/v1/health", self.base_url);
        self.client.get(&url).send().await
            .map_err(|_| RelishError::AgentUnreachable)?;
        Ok(())
    }
}
```

Each CLI command now tries the agent first. `apply` has a clever fallback: if the agent is unreachable, it still parses and validates the config and shows the dry-run plan. You can review what *would* happen before starting the agent.

```rust
pub async fn apply(path: &Path, output: OutputFormat) -> Result<(), RelishError> {
    let config = Config::from_file(path)?;
    config.validate()?;

    let client = BunClient::default_local();
    match client.health().await {
        Ok(()) => {
            let result = client.apply(&config).await?;
            println!("deployed {} instance(s): {}", result.created, result.instances.join(", "));
            Ok(())
        }
        Err(_) => {
            let plan = generate_plan(&config);
            let formatted = format_output(&plan, output)?;
            println!("{formatted}");
            println!("\n(dry run — bun agent not reachable, showing plan only)");
            Ok(())
        }
    }
}
```

The two new `RelishError` variants we saw earlier handle the failure modes: `AgentUnreachable` (can't connect at all) and `ApiError { status, body }` (connected but got an error response). The error enum grows with each phase, but every variant carries enough context to display a useful error message.

## Integration tests: the full lifecycle

Unit tests prove that each piece works in isolation. Integration tests prove they work together. Our test harness starts a real agent with a real ProcessGrill, binds the API to an ephemeral port, and talks to it over HTTP:

```rust
struct TestHarness {
    client: BunClient,
    cmd_tx: mpsc::Sender<AgentCommand>,
    shutdown: CancellationToken,
}

impl TestHarness {
    async fn start() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let shutdown = CancellationToken::new();

        let grill = ProcessGrill::new();
        let port_allocator = PortAllocator::new(40000, 41000);
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, shutdown.clone());

        tokio::spawn(async move { agent.run().await; });

        // Bind API to ephemeral port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = api::router(cmd_tx.clone());

        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { shutdown.cancelled().await; })
                .await.ok();
        });

        let client = BunClient::new(&format!("http://127.0.0.1:{port}"));

        // Wait for API readiness
        for _ in 0..20 {
            if client.health().await.is_ok() { break; }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        Self { client, cmd_tx, shutdown }
    }
}
```

Port 0 is the key trick here. Binding to port 0 tells the OS to assign any available ephemeral port. This means tests don't fight over port numbers, and you can run the whole suite in parallel without conflicts.

The `Drop` implementation cancels the shutdown token, which stops the agent and the API server. Tests don't need explicit cleanup — when the harness goes out of scope, everything shuts down.

Here's what the integration tests cover:

**`deploy_app_reaches_running`** — send an apply with a no-health-check config, verify the instance reaches Running state. This tests the full pipeline: HTTP request → TOML parsing → agent command → supervisor deploy → process start → state transition.

**`health_check_healthy_app_transitions_to_running`** — deploy an app with a health check pointing at a `TestApp` (a built-in test HTTP server). Wait a few seconds for the health check timer to fire and the probe to succeed. Verify the instance transitions through HealthWait to Running.

**`health_check_failing_app_marked_unhealthy`** — deploy against a TestApp that always returns 500. Verify the instance stays in HealthWait (never gets healthy enough to transition).

**`stop_app_transitions_to_stopped`** — deploy, verify running, send stop, verify stopped.

**`deploy_multiple_apps`** — deploy a config with two apps, verify both appear in status.

**`status_empty_when_nothing_deployed`** — verify that a fresh agent reports no instances.

**`logs_for_deployed_app`** — deploy an app, fetch logs, verify they contain the app name.

**`relish_status_returns_expected_output`** — deploy via the command channel directly (not HTTP), verify the status API reflects the expected app name.

**`job_runs_to_completion`** — deploy a job with `command = ["echo", "migration complete"]`. Wait a few seconds for the process to exit, then verify the instance reaches Stopped state. This tests the full job pipeline: deploy, `drive_job_startup`, process exit detection in `check_jobs`, and the success path.

**`job_failed_retries_then_fails`** — deploy a job with `command = ["false"]`. The `false` command exits immediately with code 1. Wait for all retries to exhaust (3 retries with exponential backoff takes about 12 seconds), then verify the instance reaches Failed state and `restart_count > 0`.

**`init_container_success_allows_app_start`** — deploy an app with an init container that runs `echo "init done"`. Verify the deploy succeeds and the app reaches Running. Tests the Preparing → Initialising → Starting path.

**`init_container_failure_prevents_start`** — deploy an app with an init container that runs `false`. Verify the deploy returns an error. The app should never start.

**`health_check_triggers_restart`** — deploy an app with a health check pointing at a TestApp in `UnhealthyAfter(3)` mode. The app passes 3 health checks (reaching Running), then starts failing. Wait long enough for the failure threshold to trip and the restart to happen. Verify `restart_count > 0`. This is the full lifecycle test: healthy, unhealthy, restart.

Each test is end-to-end: real HTTP, real process spawning, real state machine transitions. The only thing that's fake is the container runtime (ProcessGrill instead of runc). That's exactly the line we want: test the orchestration logic with real I/O, but don't require root privileges or a container runtime to be installed.

## The bun binary

The bun binary ties everything together with clap argument parsing:

```rust
#[derive(Parser)]
#[command(name = "bun", version, about = "Reliaburger node agent")]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,

    #[arg(long, default_value = "127.0.0.1:9117")]
    listen: String,

    #[arg(long, default_value = "auto")]
    runtime: String,
}
```

Startup is sequential: load config, create port allocator, detect runtime, create command channel and shutdown token, spawn the agent task, start the API server, wait for SIGINT. When the signal arrives, the cancellation token fires, the agent stops all instances, the API server drains connections, and the binary exits.

The `--runtime` flag controls which `Grill` implementation to use. `auto` checks what's available on the platform. `process` forces ProcessGrill. `runc` and `apple` select the platform-specific runtimes and error out if they're not available.

## Pulling real images

Up to this point, runc could only create containers with an empty rootfs directory. That's fine for testing the OCI spec generation pipeline, but it means `runc create` fails the moment the container process tries to do anything useful. The `container` CLI on macOS handles image pulling internally, so Apple Container works out of the box. On Linux, we need to do this ourselves.

### How OCI image distribution works

Container images aren't monolithic files. They're a stack of layers, each one a gzipped tarball containing filesystem changes. When you `docker pull alpine:latest`, here's what actually happens:

1. The client contacts the registry (e.g. `registry-1.docker.io`) and requests the *manifest* for `library/alpine:latest`.
2. The manifest lists the image's layers by their SHA-256 digest, plus a config blob containing the default command, environment variables, and other metadata.
3. The client downloads each layer blob and verifies its digest.
4. The layers are unpacked bottom-up into a directory, with each layer's files overlaying the previous one.

The manifest can also be a *manifest index* (sometimes called a "fat manifest") that lists platform-specific manifests for linux/amd64, linux/arm64, and so on. The client picks the one matching its host architecture.

### Parsing image references

Docker Hub has convenient shorthand. You type `alpine`, but the actual reference is `docker.io/library/alpine:latest`. The `ImageReference` type normalises this:

```rust
pub struct ImageReference {
    pub registry: String,
    pub repository: String,
    pub tag: String,
}
```

The parser handles bare names (`alpine`), names with tags (`alpine:3.19`), user repos (`myuser/myimage:v1`), and custom registries (`ghcr.io/org/image:sha`). It distinguishes registries from user names by checking for a `.` or `:` in the first path component.

### The `oci-distribution` crate

We use the `oci-distribution` crate for the registry protocol. It handles HTTP authentication (we only need anonymous pulls for now), manifest parsing, and blob downloads. The key call is:

```rust
let (manifest, digest, config) = client
    .pull_manifest_and_config(&reference, &RegistryAuth::Anonymous)
    .await?;
```

This returns an `OciImageManifest` with the layer descriptors. If the registry returns a manifest index (multi-platform), the client's built-in platform resolver automatically selects the right architecture.

### Layer unpacking and whiteouts

Each layer is a gzipped tarball. We extract them base-first (the first layer in the manifest is the bottom of the filesystem). The `flate2` crate handles gzip decompression, and `tar` handles extraction. We run the unpacking inside `tokio::task::spawn_blocking` since tar extraction is CPU-bound and would block the async runtime.

OCI images use a convention called *whiteout files* to handle deletions between layers. Think of each layer as a transparent sheet. You can add new files, but you can't erase what's underneath. Whiteout markers solve this:

- `.wh.filename` means "delete `filename` from whatever layer created it"
- `.wh..wh..opq` means "this entire directory is opaque — ignore everything from lower layers"

The unpacker watches for these markers and removes the corresponding files before continuing with the rest of the layer.

### Content-addressed caching

Every blob is stored by its SHA-256 digest under `{store_root}/blobs/sha256/{hash}`. If the file already exists, we skip the download entirely. Tags always re-fetch the manifest (since `latest` might point to a new digest tomorrow), but if all the layers are already cached, the second pull only costs one HTTP request for the manifest.

### Going rootless

On Linux, runc normally requires root to create namespaces and set up cgroups. Rootless mode uses user namespaces instead: the kernel maps your unprivileged UID to UID 0 *inside* the container, giving the container process the illusion of being root without any actual privileges on the host.

The `rootless::make_rootless` function adjusts an OCI spec for this:

```rust
pub fn make_rootless(spec: &mut OciSpec, instance_name: &str) {
    // Add user namespace
    spec.linux.namespaces.push(OciNamespace {
        ns_type: "user".to_string(),
        path: None,
    });

    // Remove network namespace (share host network for Phase 1)
    spec.linux.namespaces.retain(|ns| ns.ns_type != "network");

    // Map current user to container root
    let uid = nix::unistd::getuid().as_raw();
    spec.linux.uid_mappings = Some(vec![OciIdMapping {
        container_id: 0, host_id: uid, size: 1,
    }]);
    // ... gid_mappings, /sys adjustments, cgroup path
}
```

Three things change from the root spec:

1. **User namespace added.** This is what makes rootless work. The kernel creates a new user namespace where UID 0 maps to your real UID.
2. **Network namespace removed.** Creating a network namespace rootlessly requires tools like `slirp4netns` or `pasta`. We'll handle container networking properly in Phase 3. For now, containers share the host network.
3. **`/sys` becomes a bind mount.** Mounting `sysfs` requires `CAP_SYS_ADMIN`, which we don't have outside the user namespace. A read-only bind mount of the host's `/sys` works instead.

The `OciIdMapping` type is new in our OCI spec:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciIdMapping {
    #[serde(rename = "containerID")]
    pub container_id: u32,
    #[serde(rename = "hostID")]
    pub host_id: u32,
    pub size: u32,
}
```

The `serde(rename)` attributes match the OCI runtime spec's JSON field names. Without them, Rust's snake_case field names would produce `container_id` instead of `containerID`, and runc would reject the spec.

### Wiring it together

`RuncGrill` now has an `ImageStore` and a `rootless` flag. When `create()` is called, it checks whether the spec's `root.path` looks like an image reference (doesn't start with `/` or `.`). If it does, it pulls and unpacks the image, then symlinks the resulting rootfs into the bundle directory:

```rust
if looks_like_image_ref(&spec.root.path) {
    let rootfs = self.image_store
        .pull_and_unpack(&spec.root.path).await?;
    tokio::fs::symlink(&rootfs, bundle_dir.join("rootfs")).await?;
    spec.root.path = "rootfs".to_string();
}
```

The `detect_runtime()` function now auto-detects rootless mode and configures paths accordingly. Non-root users get `~/.local/share/reliaburger/` for images and bundles; root uses `/var/lib/reliaburger/`.

### Testing image pulling

The unit tests for `ImageReference::parse` and layer unpacking run unconditionally. They create synthetic gzipped tarballs in temp directories and verify that whiteouts, symlinks, and multi-layer ordering work correctly.

The integration tests for actual image pulling are gated behind `RELIABURGER_IMAGE_PULL_TESTS=1` since they require network access. They pull `alpine:latest` from Docker Hub and verify that `/bin/sh` exists in the unpacked rootfs.

## What we built

Phase 1 started with parsing TOML and ended with a working container lifecycle that can pull real OCI images from Docker Hub and run them rootlessly on Linux. We can now:

- `cargo run --bin bun` starts a node agent that auto-detects the runtime, spawns containers (or processes), and runs health checks on a timer
- `cargo run --bin relish -- apply app.toml` deploys workloads to the running agent
- `cargo run --bin relish -- status` shows what's running
- `cargo run --bin relish -- logs web` shows captured output
- `cargo run --bin relish -- apply app.toml` without an agent falls back to a dry-run plan
- `cargo run --bin testapp -- --mode healthy --port 8080` runs the test server for demos
- RuncGrill pulls real OCI images from Docker Hub (e.g. `alpine:latest`) and unpacks them into a rootfs
- Rootless runc runs containers without sudo using user namespaces and UID/GID mapping
- Jobs run to completion with exit code tracking and retry on failure
- Init containers run sequentially before the main app, with failure halting the deploy
- Unhealthy apps get restarted automatically with exponential backoff

321 tests verify all of it: config parsing, validation, state machine transitions, OCI spec generation, cgroup computation, port allocation, health check decisions, HTTP probing, process management, exit code tracking, job lifecycle, init container execution, restart re-drive, image reference parsing, layer unpacking with whiteouts, rootless spec modifications, the agent event loop, the API server, the CLI with its init command, streaming apply progress via SSE, and 16 integration tests that exercise the full stack end to end.

What we deferred: real multi-node clustering (Phase 2), network namespaces (Phase 3), mTLS and authentication (Phase 4), the Pickle registry (Phase 5). ProcessGrill doesn't provide real isolation, and there's no scheduler, no gossip protocol, no persistent state. All of that is coming.

The foundation is solid. The trait boundaries (`Grill`, the state machine, the health checker) were designed so that adding real implementations doesn't change the orchestration logic. When we add runc support, the agent doesn't know the difference. When we add a scheduler in Phase 2, it sends the same `AgentCommand::Deploy` that the API sends today. That's the payoff of getting the abstractions right early: each phase adds new capabilities without rewriting what came before.
