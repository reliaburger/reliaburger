# Changing the Tyres at Full Speed

Everything we've built so far assumes the Reliaburger binary itself stays put. Apps roll, nodes join and leave, leaders come and go, but `bun` — the process running the whole show on each node — has been immortal. It isn't, of course. We ship bug fixes. We add features. Sooner or later every node in the cluster needs a new binary, and "SSH in, stop everything, copy the file, start everything" is exactly the kind of operational folklore this project exists to kill.

This chapter builds self-upgrade: the cluster replaces its own binary, node by node, while the workloads keep serving. The pieces, assembled one at a time:

- a way for a binary to know and prove **what version it is** (this section),
- **dual-signature verification**, so a node never executes a binary it can't trace to a release key *and* to the operator's own key,
- an on-disk **binary store** with atomic symlink activation and rollback retention,
- a node-level **state machine** that survives crashes mid-upgrade and reverts on its own,
- **workload adoption**, so containers and processes sail through the swap untouched,
- and finally the leader-side **rolling orchestration**: workers first, council one at a time, leader last.

One warning before we start. The single most important syscall in this chapter is `exec()`, and the single most important fact about it is what it *preserves*. We'll get there properly in the adoption section. But it casts a shadow over even this first, innocent-looking one.

## 14.1 What version am I?

A version sounds like the least interesting thing a program can know about itself. It's a string in `Cargo.toml`; Cargo exposes it at compile time; you print it in the banner. Done?

Not quite, twice over. First, versions need to be *compared* — the whole rolling upgrade turns on questions like "is this node already at the target?" — and comparing version strings lexically is a classic bug factory (`"0.10.0" < "0.9.0"` as strings, since `1` sorts before `9`). Second, our integration tests will need two binaries that behave identically but *report different versions*, without paying for two full compiles of a 70,000-line project per test run. That second requirement leads somewhere genuinely instructive.

### Semver, and why we don't hand-roll it

Reliaburger versions follow [semantic versioning](https://semver.org): `MAJOR.MINOR.PATCH`, with optional pre-release tags like `1.0.0-rc.1`. Most of the comparison rules are obvious. The pre-release rules are not: `1.0.0-rc.1` comes *before* `1.0.0`, pre-release identifiers compare segment by segment, numeric segments compare numerically but alphanumeric ones lexically... it's a page of spec that is very easy to get subtly wrong. So we don't write it. The `semver` crate — maintained by the people who maintain Cargo, which lives and breathes this format — does it for us:

```toml
semver = "1"
```

What we *do* write is a newtype around it:

```rust
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BinaryVersion(semver::Version);
```

You met the newtype pattern back in Chapter 1 with `NodeId`. Here it earns its keep three ways: it gives us somewhere to hang Reliaburger-specific behaviour (like generating file names — `bun-v0.2.0`), it keeps `semver` out of our public API so we could swap the crate later without touching callers, and it lets us control the serialised form. Note what `#[derive(PartialOrd, Ord)]` does on a one-field tuple struct: it delegates to the field. Our ordering *is* semver's ordering, pre-release rules included, for free.

### Parsing and printing

Users type `v0.2.0`; the semver crate wants `0.2.0`. Files on disk are named `bun-v0.2.0`. We settle the ambiguity at the edges — accept an optional `v` on the way in, always print one on the way out:

```rust
impl FromStr for BinaryVersion {
    type Err = UpgradeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let stripped = trimmed.strip_prefix('v').unwrap_or(trimmed);
        // ... parse with semver, wrapping errors in UpgradeError::InvalidVersion
    }
}

impl fmt::Display for BinaryVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}
```

`FromStr` and `Display` are the standard traits for string conversions — implementing them is what makes `"v0.2.0".parse::<BinaryVersion>()` and `format!("{version}")` work. Coming from Go, they're roughly `encoding.TextUnmarshaler` and `fmt.Stringer` with compiler enforcement; from Python, `__str__` and a classmethod constructor, except the caller names the target type and the compiler checks the whole chain.

One new trick in this chapter: we implement `Serialize` and `Deserialize` *by hand* instead of deriving them. Derived serde on a tuple struct would expose the inner struct's shape — `{"major":0,"minor":2,...}` — which is miserable to read in an API response or a marker file. Six lines get us `"v0.2.0"` instead:

```rust
impl serde::Serialize for BinaryVersion {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)   // reuses our Display impl
    }
}
```

Deserialisation is the mirror image: read a string, run it through the `FromStr` impl we already tested. One parser, one printer, used everywhere — JSON, TOML, CLI arguments, file names.

### The trap: why the version override is a file, not an environment variable

Now the interesting part. Integration tests for self-upgrade need a "v0.1.0 binary" and a "v0.2.0 binary". Building the project twice with different `Cargo.toml` versions works, but costs minutes per test run. The obvious cheap alternative: copy the compiled binary twice and tell each copy what to claim, say via `RELIABURGER_VERSION_OVERRIDE=v0.2.0`.

Can you see the problem? Recall how the upgrade will actually happen: the running process calls `exec()` on the new binary, replacing itself in place. And `exec()` **preserves the environment**. The old binary was started with `RELIABURGER_VERSION_OVERRIDE=v0.1.0`; the new binary inherits that variable and dutifully reports... v0.1.0. Your test passes the swap, then fails the version check, and the failure points at everything except the actual cause. The same trap springs in reverse when the supervisor restarts a reverted binary.

The fix is to attach the override to the *artefact* instead of the *process*: a sidecar file next to the binary. `bun-v0.2.0` looks for `bun-v0.2.0.version`; whichever binary ends up being exec'd finds its own truth sitting beside it on disk:

```rust
pub fn resolve_running_version(exe_path: &Path) -> BinaryVersion {
    if cfg!(debug_assertions)
        && let Some(version) = sidecar_version(exe_path)
    {
        return version;
    }
    compiled_version()
}
```

Two bits of Rust worth pausing on. `cfg!(debug_assertions)` is a compile-time boolean: `true` in debug builds, `false` in release builds, where the optimiser deletes the whole branch. Release binaries physically do not contain the sidecar-reading path — this is a test hook, and we'd rather not ship a way to lie about versions. (Contrast with the `#[cfg(...)]` attribute, which removes code from compilation entirely; `cfg!` keeps both branches compiling, so the test-only code can't silently rot.)

And that `if cfg!(...) && let Some(version) = ...` line is a *let chain*, stabilised in the 2024 edition: boolean conditions and pattern-match bindings mixed in one `if`. Before this you'd nest an `if let` inside an `if`, one indentation level deeper for no gain.

Two implementation details, both future bug reports pre-empted. We canonicalise the executable path before looking for the sidecar, because `std::env::current_exe()` resolves symlinks on Linux (it reads `/proc/self/exe`) but isn't guaranteed to elsewhere — and the sidecar lives next to the real versioned file, not next to the `bun` symlink. And we build the sidecar name by *appending* `.version` rather than calling `Path::with_extension`, because `with_extension` replaces everything after the last dot: `bun-v0.2.0` would become `bun-v0.2.version`. The unit test `sidecar_path_appends_rather_than_replacing_extension` exists so nobody "simplifies" that back.

### What we decided not to do

- **Hand-rolled comparison.** Tempting for something this small; the pre-release rules alone justify the dependency.
- **An env-var override.** See above. It isn't just inelegant, it's *wrong* under exec.
- **Overriding in release builds.** A signed binary that can be told to misreport its version undermines the signature story we're about to build.

The tests for this section read like a specification: parsing with and without the `v`, rejection of garbage, the `0.2.0 < 0.10.0` ordering that string comparison gets wrong, pre-release precedence, the serde round-trip, and the sidecar behaviour through a symlink. Run them with `cargo test --lib upgrade::version`.

Next: nobody should run a binary just because it showed up claiming to be v0.2.0. Signatures.
