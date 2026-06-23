# Appendix: Rust for C, Python, and Go Programmers

You can read this book two ways. Front to back, picking up Rust as it appears — each chapter explains new syntax the first time it shows up. Or, if you hate surprises, start here. This appendix is a self-contained tour of the Rust you need to read Reliaburger's source, written for people who already program in C, Python, or Go but haven't used Rust. Read it first and the scattered "here's a new bit of Rust" boxes in the chapters become reinforcement rather than first contact.

Either way, the examples are real. Almost every snippet below is lifted or lightly trimmed from Reliaburger's actual source, with a file reference so you can go and read the surrounding code.

Rust's reputation precedes it: fast like C, safe like a garbage-collected language, and famously hard to learn. The "hard" part is mostly one idea — ownership — applied relentlessly. Once it clicks, the rest is a well-designed language with excellent tooling. Let's start with the idea everything else hangs off.

## 1. The memory model: ownership, borrowing, lifetimes

Here's the central bargain. C gives you manual memory management: you `malloc` and you `free`, and if you get it wrong you get use-after-free, double-free, leaks, and the security bugs that follow. Python, Go, Java, and friends take the keys away: a garbage collector frees memory for you, at the cost of a runtime that pauses your program to do it. Rust does neither. It works out *at compile time* when memory can be freed, inserts the frees for you, and refuses to compile code where the answer is ambiguous. No GC, no manual `free`, no leaks. The catch: you have to write code the compiler can reason about, and that's what ownership rules are.

### Ownership and moves

Every value in Rust has exactly one *owner* — the variable binding responsible for it. When the owner goes out of scope, the value is dropped (freed). That's it. One owner, freed when the owner dies.

The twist that surprises everyone: assigning a value *moves* ownership, it doesn't copy.

```rust
let a = String::from("redis");
let b = a;            // ownership MOVES from a to b
// println!("{a}");   // compile error: a no longer owns anything
```

In Python or Go, `b = a` gives you two names for the same object (or a copy for value types), and both keep working. In Rust, after `let b = a`, the variable `a` is *dead*. The string wasn't copied (that would mean allocating and duplicating the heap buffer), and it isn't shared, because shared ownership of mutable data is exactly the bug Rust is trying to prevent. So `a` simply hands the value to `b` and stops being usable. Try to use `a` afterwards and the compiler stops you.

This is why, all through the book, you see things cloned before they're sent down a channel:

```rust
// the value is moved into the channel; clone first if you still need it
self.driver.start_instance(app_id, &node, image)?;
```

Cloning across a channel boundary or a `tokio::spawn` isn't a code smell in Rust — it's the explicit acknowledgement that two parts of the program now each need their own copy. Reliaburger's coding guide says exactly this: "Clone across channel boundaries. This is expected, not a code smell."

Small types that are cheap to duplicate opt out of move semantics with the `Copy` trait — integers, booleans, `char`, and small structs of copyable fields. A `Copy` type is duplicated bit-for-bit on assignment, so the original stays alive. Reliaburger marks its rollup aggregate `Copy` because it's just three `f64`s and a `u32`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RollupAggregate {
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    pub count: u32,
}
```

(from `src/mayo/rollup_store.rs`)

`Copy` is the exception. Most types (anything owning a heap allocation, like `String` or `Vec`) move.

### Borrowing: references without giving up ownership

Moving everything everywhere would be miserable. You usually want to *lend* a value to a function and get it back. That's borrowing, written with `&`.

```rust
// borrows the path, doesn't take ownership
pub fn validate_build(spec: &BuildSpec) -> Result<PickleDestination, BuildError> { ... }
```

A `&BuildSpec` is a *shared reference*: read-only access, and the caller keeps ownership. A `&mut BuildSpec` is a *mutable reference*: you can change the value through it. The rule that makes this safe is the one to tattoo on your arm:

> At any given time, you can have **either** one mutable reference **or** any number of shared references to a value, but not both.

This is the "borrow checker", and it's the thing that fails your first dozen Rust programs to compile. But look at what it buys you. A data race needs two threads, at least one writing, to the same memory, unsynchronised. Rust makes "one writer xor many readers" a *compile-time* property of all code, single- or multi-threaded. Data races become impossible to express. Go's race detector finds these at runtime if you're lucky and have a test that triggers them; Rust rejects them before the program runs.

In practice the rule shapes APIs. Reliaburger's guide says: take `&str` not `String`, `&Path` not `PathBuf`, `&[T]` not `Vec<T>` — borrow by default, own only when you must. A function that just reads a path should borrow it (`&Path`), leaving the caller free to use it afterwards.

### Lifetimes: how long a borrow is valid

A reference must never outlive the thing it points to. A C programmer knows the failure mode: return a pointer to a local variable, the stack frame vanishes, the pointer dangles, undefined behaviour. Rust tracks how long every reference is valid (its *lifetime*) and refuses to compile code where a reference could outlive its referent.

Most of the time lifetimes are inferred and invisible. Occasionally you write one explicitly, with a `'tick` syntax:

```rust
fn find_peer_for_layer<'a>(holders: &BTreeSet<u64>, peers: &'a [Peer]) -> Option<&'a Peer> { ... }
```

(from `src/pickle/pull.rs`)

The `<'a>` declares a lifetime named `a`. The signature says: the returned `&'a Peer` lives as long as the `peers` slice it was borrowed from. The compiler now guarantees nobody can use that returned reference after the `peers` slice is gone. You're not allocating anything or managing anything — you're handing the compiler a proof obligation, and it checks the proof. There's no runtime cost; lifetimes are erased entirely after compilation.

The payoff for the whole section: Reliaburger has no garbage collector and not one manual `free`, yet it can't leak memory, can't use-after-free, and can't race on data. The compiler inserts every deallocation at exactly the point the owner goes out of scope. You pay for it in the borrow checker arguing with you while you learn. You stop paying once it clicks; the runtime never pays at all.

## 2. Modelling data: structs, enums, and pattern matching

### Structs

A `struct` is a record, like a C struct or a Python dataclass:

```rust
pub struct BackendInstance {
    pub instance_id: String,
    pub node_ip: Ipv4Addr,
    pub host_port: u16,
    pub healthy: bool,
}
```

(from `src/onion/service_map.rs`)

`pub` makes a field visible outside the module; without it the field is private. Nothing surprising here for anyone coming from C or Go.

### Enums are sum types, and this changes everything

This is where Rust departs hard from C and Go. A C `enum` is a glorified integer. A Rust `enum` is a *tagged union*: each variant can carry its own data, and a value is exactly one variant at a time.

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

(from `src/grill/state.rs`)

That looks C-like, but variants can hold data, which is where it gets powerful:

```rust
pub enum AlertState {
    Inactive,
    Pending { since: SystemTime },   // breaching, but not yet long enough
    Firing { since: SystemTime },    // breached for the full duration
}
```

(from `src/mayo/alert.rs`)

An `AlertState` is *one* of these, and the `Pending` and `Firing` variants carry a `since` timestamp that `Inactive` doesn't. Coming from Go you'd model this with a struct full of fields and a "kind" tag, trusting yourself never to read `since` when the kind is `Inactive`. Coming from Python you'd use a class hierarchy or a dict and hope. Rust's enum makes the illegal states unrepresentable: there is no way to have a `since` timestamp unless you're actually `Pending` or `Firing`.

Reliaburger models every lifecycle as an enum — container state, deploy phase, alert state, fault type. The coding guide is blunt about it: "State machines as enums. No sentinel values, no stringly-typed states." When a deploy can be `Pending`, `Rolling`, `Reverting`, or six other things, it's an enum, not a string you compare with `==` and misspell.

### `match`: exhaustive by force

You take an enum apart with `match`, Rust's switch on steroids. The crucial property: `match` must be *exhaustive*. Handle every variant, or the code doesn't compile.

```rust
let kind = match self.workload_type {
    WorkloadType::App => "app",
    WorkloadType::Job => "job",
};
```

(from `src/sesame/types.rs`)

Two variants, two arms. Add a third variant to `WorkloadType` and this `match` (and every other `match` on it across the entire codebase) becomes a compile error until you handle the new case. In C, a `switch` that forgets a case compiles fine and falls through to whatever's next. In Go, you add a `case` to a `switch` and hope you found all the switches. In Rust, the compiler hands you the list.

This is the single most-cited reason Reliaburger leans on enums so hard. From the deploy chapter: "Add a tenth state and every `match` that doesn't handle it becomes a compile error. You can't ship a deploy that forgets what to do when a new phase is reached."

`match` also binds data out of variants and can guard with conditions:

```rust
let new_state = match (&state, breaching) {
    (AlertState::Inactive, true) => AlertState::Pending { since: now },
    (AlertState::Pending { since }, true) => {
        if now.duration_since(*since).unwrap_or_default() >= rule.for_duration {
            AlertState::Firing { since: *since }
        } else {
            state.clone()
        }
    }
    (AlertState::Firing { .. }, true) => state.clone(),
    (_, false) => AlertState::Inactive,
};
```

(from `src/mayo/alert.rs`)

Match on a *tuple* of `(state, breaching)` and you've got a transition table the compiler checks for completeness. The `(_, false)` arm is a wildcard: `_` matches anything, so "whatever state we're in, if not breaching, go Inactive." The `{ since }` syntax pulls the `since` field out of the variant and binds it to a local.

### Newtypes: making the compiler catch mix-ups

A pervasive Rust habit is wrapping a primitive in a one-field struct to give it meaning:

```rust
pub struct NodeId(pub String);
pub struct DeployId(pub u64);
pub struct VirtualIP(pub Ipv4Addr);
```

These are *newtypes*. A `NodeId` is just a `String` at runtime (zero overhead), but the compiler treats it as a distinct type. You can't pass a `NodeId` where an `AppId` is expected, even though both wrap a `String`. In C or Go, an ID is a `string` and a port is an `int`, and nothing stops you passing them in the wrong order. Reliaburger's guide mandates newtypes "so the compiler prevents mix-ups." The classic bug, swapping two string arguments, becomes a type error.

## 3. Traits and generics

### Traits are interfaces (with more reach)

A `trait` defines behaviour a type can implement. If you know Go interfaces or Java interfaces, you're 80% there:

```rust
pub trait DeployDriver {
    fn start_instance(&self, app: &AppId, node: &NodeId, image: &str)
        -> Result<(InstanceId, u16), DeployError>;
    fn await_healthy(&self, instance_id: &str, timeout: Duration) -> Result<(), DeployError>;
    fn stop_instance(&self, instance_id: &str) -> Result<(), DeployError>;
    // ...
}
```

(from `src/meat/orchestrator.rs`)

Any type that implements all the methods *is a* `DeployDriver`. Reliaburger has two: `MockDriver` for tests and the real driver for production. The orchestrator is written against the trait, so the same deploy logic runs against a mock in microseconds or against real containers in production.

The big difference from Go: in Go a type satisfies an interface implicitly, just by having the methods. In Rust you write `impl DeployDriver for MockDriver { ... }` explicitly. More typing, but the intent is on the page, and the compiler tells you exactly which method you forgot.

The book's central trait is `Grill`, the container-runtime abstraction. `ProcessGrill`, `RuncGrill`, and `AppleContainerGrill` all implement it, so the agent runs processes, runc containers, or Apple containers through one interface (`src/grill/mod.rs`).

A rule worth absorbing from the coding guide: "Don't write a trait until you have two implementations." Traits are for genuine polymorphism, not speculative abstraction. `DeployDriver` exists because there really are two drivers; write the concrete version first and extract the trait when the second implementation arrives.

### Generics and monomorphisation

Generics let you write code once that works for many types, with the concrete type filled in by the caller:

```rust
pub struct DeployOrchestrator<D: DeployDriver> {
    state: DeployState,
    driver: D,
}
```

(from `src/meat/orchestrator.rs`)

`<D: DeployDriver>` reads "for any type `D` that implements `DeployDriver`." Build a `DeployOrchestrator<MockDriver>` and the compiler stamps out a dedicated copy with every `driver.start_instance(...)` call wired straight to `MockDriver`'s method. This is *monomorphisation*: one generic, compiled into one concrete version per type actually used. C++ templates work the same way. There's no runtime dispatch and no boxing — the generic version is exactly as fast as if you'd written it by hand for that type.

This is the opposite trade-off from Java generics (which erase to `Object` and dispatch at runtime) and from Go's older interface-based polymorphism. Rust pays in compile time and binary size (N copies of the code) and gets zero runtime cost.

### Trait objects: when you need runtime variety

Sometimes you genuinely don't know the type until runtime, or you want a collection of mixed types behind one interface. Then you reach for a *trait object*, written `dyn Trait`, usually behind a pointer:

```rust
pub type SecretDecryptor = Box<dyn Fn(&str) -> Result<String, String>>;
```

(from `src/grill/oci.rs`)

`Box<dyn Fn(...)>` is a heap-allocated, type-erased closure: the concrete type is forgotten, and calls go through a vtable (a pointer table) at runtime — exactly how Go interfaces and C++ virtual methods work. The trade-off is a pointer indirection per call in exchange for runtime flexibility.

The rule of thumb the book uses: a generic (`<D: DeployDriver>`) when each value is one known type for its whole life; a trait object (`Box<dyn ...>`) when you need a runtime-varying mix. The deploy chapter even records switching from `Box<dyn DeployDriver>` to a generic once it was clear each orchestrator owns exactly one driver.

## 4. Error handling: no exceptions, just values

Rust has no exceptions. A function that can fail says so in its return type, and the caller has to deal with it. Two enums carry the whole scheme.

### `Option<T>`: maybe a value

```rust
pub fn resolve(&self, name: &str) -> Option<&ServiceEntry> { ... }
```

`Option<T>` is either `Some(value)` or `None`. It's Rust's answer to null — except you can't accidentally use a `None` as if it held a value, because the type is `Option<&ServiceEntry>`, not `&ServiceEntry`, and the compiler won't let you reach inside without checking. Tony Hoare called null his "billion-dollar mistake"; `Option` is the fix. Python's `None` and Go's nil pointers are the mistake; they're values you can dereference and crash on. `Option` forces the check.

The coding guide insists on it: "`Option<T>`, not sentinels. No `-1` meaning 'not set', no empty strings meaning 'absent'."

### `Result<T, E>`: success or a typed error

```rust
pub fn write_blob(&self, data: &[u8], expected: &Digest) -> Result<(), PickleError> { ... }
```

`Result<T, E>` is either `Ok(value)` or `Err(error)`. The error type is part of the signature, so callers know exactly what can go wrong. There's no invisible control flow, no stack-unwinding exception that skips ten frames. A failure is a value you return.

### The `?` operator

Checking every `Result` by hand would be as tedious as Go's `if err != nil { return err }` after every call. The `?` operator is the cure: it unwraps an `Ok`, or returns the `Err` early from the current function.

```rust
let data = tokio::fs::read(&upload_path).await?;   // on error, return it
let actual = compute_sha256(&data);
if actual.as_str() != expected_digest.as_str() {
    return Err(PickleError::DigestMismatch { expected, actual });
}
tokio::fs::rename(&upload_path, &blob_path).await?;
Ok(())
```

(from `src/pickle/blob_store.rs`)

Each `?` says "give me the value or bail with the error." It's Go's error-check boilerplate compressed to one character, and because it's part of the type system the compiler ensures you can't forget it — a `Result` you ignore is a warning.

### Defining errors: `thiserror` and `anyhow`

Library code defines precise error enums with the `thiserror` crate, which generates the boilerplate `Display` and `Error` implementations from attributes:

```rust
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("build context {path:?} does not exist")]
    ContextNotFound { path: PathBuf },
    #[error("builder failed: {reason}")]
    BuilderFailed { reason: String },
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}
```

(from `src/pickle/build.rs`)

Each variant carries the context needed to diagnose it. The `#[error("...")]` attribute defines the message; `{path:?}` interpolates a field. The `#[from]` on the `Io` variant auto-generates a conversion from `std::io::Error`, so a `?` on a file operation turns an I/O error into a `BuildError::Io` for free.

Binaries and CLIs, where you just want "something went wrong, here's a chain of context," use `anyhow` instead, with `.context(...)` to annotate failures as they bubble up. The split is deliberate: `thiserror` for libraries whose callers match on specific errors, `anyhow` for top-level code that just reports them. And the guide bans `Box<dyn Error>` and `.unwrap()` in production code — an error is either a typed enum or an `anyhow::Error`, never a panic.

## 5. Async and concurrency

Reliaburger is a network service: it juggles gossip, Raft, HTTP, health checks, and timers all at once. It does this with *async* Rust on the **tokio** runtime, not with a thread per task.

### `async` / `await`

An `async fn` returns a *future* — a computation that hasn't run yet. Calling it does nothing until something `.await`s it:

```rust
let data = tokio::fs::read(&path).await?;
```

`.await` means "suspend here until this completes, and let the runtime do other work meanwhile." If you know Python's `async`/`await` or JavaScript's, this is the same model. The difference: Rust's futures are *zero-cost* and don't run on a hidden event loop you can't see — you pick the runtime (tokio) and spawn tasks onto it explicitly. A suspended `.await` doesn't block a thread; tokio parks the future and runs thousands of others on a handful of OS threads.

### `tokio::select!`: waiting on several things

The agent's main loop waits on whichever of several events fires first:

```rust
loop {
    tokio::select! {
        _ = shutdown.cancelled() => break,
        Some(cmd) = command_rx.recv() => self.handle_command(cmd).await,
        _ = health_interval.tick() => self.run_health_checks().await,
    }
}
```

`select!` polls each branch and runs the one that's ready, looping forever. This one structure multiplexes shutdown, incoming commands, and a periodic health timer onto a single task. In Go you'd use `select` over channels; tokio's `select!` is the direct analogue, extended to any future (timers, I/O, cancellation).

### Channels: how tasks talk

Tasks don't share memory and lock it; they send messages. The coding guide: "Subsystems communicate via channels, not shared mutexes." Tokio gives you four, and choosing the right one is a design decision:

- **`mpsc`** (multi-producer, single-consumer) — the default for command queues. Many senders, one receiver. The agent's command channel is `mpsc`.
- **`watch`** (single-producer, multi-consumer, latest-value-only) — for config or routing-table updates where subscribers only care about the current value. The rollup worker watches council membership over a `watch` channel.
- **`oneshot`** — a single request/response. An HTTP handler sends a command plus a `oneshot` sender, then awaits the reply on the receiver.
- **`broadcast`** — every subscriber sees every message. Rarely needed; prefer `watch`.

Channels take ownership of what you send, which is why you clone before sending — the value moves to the receiver.

### Shared state: `Arc`, `Mutex`, `RwLock`

When tasks genuinely must share data, you wrap it so sharing is safe. `Arc<T>` is an *atomically reference-counted* pointer — multiple owners, freed when the last one drops. It's how you get shared read access across tasks (like Python's implicit refcounting or a Go pointer, but thread-safe by construction).

For shared *mutable* state, wrap the inner value in a lock:

```rust
let registry: Arc<tokio::sync::Mutex<FaultRegistry>> = ...;
```

(from `src/smoker`, via `src/bun/agent.rs`)

`Arc<Mutex<T>>` means "shared ownership of a mutex protecting a `T`." `RwLock` is the read-heavy variant: many readers or one writer (Reliaburger uses `Arc<RwLock<RoutingTable>>` so in-flight requests read the routes while a rare rebuild takes the write lock).

One rule the book repeats because it bites everyone: **never use `std::sync::Mutex` in async code.** Use `tokio::sync::Mutex`. A standard-library mutex, if held across an `.await`, blocks the OS thread — and since tokio runs many tasks per thread, you can deadlock the whole runtime. The tokio mutex yields the thread instead of blocking it. The guide states it flatly: "No `std::sync::Mutex` in async code."

The combined effect: fearless concurrency. The borrow checker from Section 1 extends to threads via two marker traits, `Send` (safe to move to another thread) and `Sync` (safe to share by reference). A type that isn't safe to share simply won't compile into a multi-threaded context. The data races you debug at 3am in Go or C are compile errors here.

## 6. `unsafe` and FFI

Rust's safety guarantees rest on rules the compiler can check. Sometimes you need to do something it can't verify — talk to the kernel, lay out a struct for a C ABI, dereference a raw pointer. That's what `unsafe` is for. It doesn't turn off the borrow checker; it lets you do a handful of extra things (deref raw pointers, call C functions, implement unsafe traits) and promises *you've* checked the invariants.

Reliaburger keeps `unsafe` to a minimum. The place it matters is eBPF: the kernel reads bytes out of BPF maps at fixed offsets, so the Rust structs that describe those maps must have a guaranteed memory layout. By default Rust reorders struct fields for packing efficiency. `#[repr(C)]` forces C-compatible layout:

```rust
#[repr(C)]
pub struct BackendKey {
    pub vip: u32,     // network byte order
    pub port: u16,    // network byte order
    pub _pad: u16,
}
```

(from `src/onion/ebpf` types)

The `_pad` field makes the alignment padding explicit instead of leaving it to the compiler, so the Rust struct's bytes match the C struct's bytes exactly. Get this wrong and the kernel reads garbage. The book pairs every such struct with a compile-time size assertion (a test like `connect_fault_key_size` that asserts `size_of` is exactly what the C side expects) so a missing pad field fails a test before any kernel code runs. And every `unsafe` block carries a `// SAFETY:` comment explaining why it's sound — that's a hard rule in the coding guide.

The eBPF loading itself goes through the `aya` crate, which wraps the genuinely unsafe BPF syscalls behind a safe API, and it's gated behind a Cargo feature so the unsafe machinery only compiles on Linux when you ask for it. Most of Reliaburger contains no `unsafe` at all.

## 7. The smaller pieces

A few features you'll see constantly, in brief.

### Iterators and closures

Rust's iterators are lazy and chainable, like Python generators or Java streams, and a closure (`|args| body`) is its lambda:

```rust
let hash: u32 = node_id
    .bytes()
    .fold(5381u32, |acc, b| acc.wrapping_mul(33).wrapping_add(b as u32));
```

(from `src/grill/netns.rs`)

`|acc, b| ...` is a two-argument closure; the pipes delimit the parameters, like `lambda acc, b:` in Python. `.fold` is reduce. Iterator chains (`.iter().filter(...).map(...).collect()`) compile down to the same machine code as a hand-written loop — zero-cost again.

### Derive macros

`#[derive(...)]` auto-generates trait implementations so you don't hand-write them:

```rust
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppId { pub name: String, pub namespace: String }
```

`Debug` gives you printable output, `Clone` an explicit deep copy, `Hash`/`Eq` lets the type be a `HashMap` key, and `Serialize`/`Deserialize` (from the `serde` crate) handles JSON/TOML conversion. The guide says derive `Debug, Clone` by default and add the rest only when needed — don't derive `Hash` if nothing hashes it.

### `Default`

The `Default` trait gives a type its "empty" value, via `#[derive(Default)]` or a hand-written `impl`. Reliaburger's `DeployConfig::default()` encodes the sensible defaults for a deploy (roll one at a time, 60s health timeout, auto-rollback on). Combined with struct-update syntax (`DeployConfig { auto_rollback: false, ..Default::default() }`) it's how you say "mostly defaults, with these overrides."

### Modules and visibility

Code is organised into modules (`mod`), and everything is private by default until marked `pub`. Reliaburger's subsystems are top-level modules with food codenames (`sesame` for security, `pickle` for the registry, `mayo` for metrics, `meat` for the scheduler), each exposing a small public API and keeping the rest private. This is enforced by the compiler, not convention: a non-`pub` function simply can't be called from outside its module.

### Cargo and crates

`cargo` is the build tool and package manager, `npm`/`pip`/`go mod` rolled into one, but the part of the toolchain people consistently praise. `cargo build`, `cargo test`, `cargo fmt`, `cargo clippy` (the linter). Dependencies ("crates") are declared in `Cargo.toml`. Reliaburger leans on the ecosystem rather than reinventing: `tokio` for async, `axum` for HTTP, `serde` for serialisation, `clap` for the CLI, `ring` and `rustls` for crypto, `datafusion` for SQL. Optional features gate heavy dependencies (the `kubernetes` feature pulls in `k8s-openapi`, the `ebpf` feature pulls in `aya`), so you compile only what you use.

## 8. The trade-offs, honestly

No language is free. Here's the deal Rust offers, weighed against the languages you already know.

**What you give up.**

- *Compile times.* Monomorphisation and heavy compile-time checking aren't free. Rust compiles slower than Go, much slower than running Python. Incremental builds and `cargo check` (type-check without codegen) take the edge off, but a clean release build of a project this size is a coffee break.
- *The learning curve.* The borrow checker will reject correct-looking code while you internalise ownership. Everyone goes through it. It lasts weeks, not months, and then mostly stops.
- *Verbosity.* Explicit error handling, explicit `.clone()`, explicit lifetimes occasionally. More ceremony than Python. The flip side is that the ceremony is the documentation — the signature tells you what can fail and what's borrowed.

**What you get.**

- *No garbage collector.* No GC pauses, predictable latency, low and steady memory. For an orchestrator that must answer health checks on time, this matters. Go's GC is good, but it's still a pause; Python's is refcounting plus stop-the-world cycles.
- *Memory safety without a runtime.* C's speed with none of C's use-after-free, double-free, or buffer-overflow bugs. The class of vulnerability that dominates CVE lists simply doesn't compile.
- *Fearless concurrency.* `Send`/`Sync` and the borrow checker make data races a compile error. You refactor threaded code aggressively because the compiler catches the mistakes Go would only find under the race detector, if at all.
- *Exhaustive types.* Enums and `match` push whole categories of bug (forgotten cases, illegal states, null derefs) from runtime to compile time. The compiler becomes a to-do list when you change a type.
- *Tooling.* `cargo`, `rustfmt`, `clippy`, and built-in tests are first-class and consistent across every project. No bikeshedding the build system.

**When to reach for something else.** A quick script? Python. A small CLI where GC pauses don't matter and you want to ship in an afternoon? Go. Rust earns its keep when you need performance *and* reliability *and* concurrency at once — systems software, infrastructure, anything long-running where a crash or a data race is expensive. Which is precisely what a container orchestrator is, and why Reliaburger is written in it.

If you came here first: now go read the chapters. The Rust will look familiar, and you can spend your attention on the distributed-systems ideas instead of the syntax. If you arrived here last: you've now seen the language whole, and the pieces you met scattered through the book should fit together. Either way — that's Rust. The hard part was ownership, and you've already got it.
