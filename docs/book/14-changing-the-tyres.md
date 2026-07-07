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

## 14.2 Trust, but verify twice

Here is the threat we're defending against. An upgrade means a node downloads an executable from the network and *replaces itself with it*. If an attacker can slip a malicious binary into that pipeline — a compromised CDN, a poisoned mirror, a man-in-the-middle on a badly configured network — they don't get a foothold, they get everything, on every node, wearing the orchestrator's own uniform. Image signing (Chapter 10) protected the workloads. This is the same idea pointed at ourselves, with less room for error.

Reliaburger requires **two** signatures on every network-distributed binary, from keys with different owners and different failure modes:

1. **The embedded release key.** A set of Ed25519 public keys compiled into the binary itself. Signature by one of these proves the file came out of the Reliaburger release process. If this key leaks, the *project* has a problem.
2. **The external key.** An Ed25519 public key the operator generates themselves and puts in `node.toml` (`upgrades.external_signing_key`). Signature by this proves *this cluster's operator* approved *this specific binary*. If this key leaks, one organisation has a problem — and rotating it is a config change, not a re-release.

An attacker has to compromise both, and they don't live in the same place. That's the whole design. Air-gapped upgrades (`relish upgrade start --binary`, where an operator hand-carries a file to the cluster) require only the embedded signature — the operator's approval is implicit in the hand-carrying — matching how `UpgradeConfig` was specced in the design doc.

Why Ed25519, when Chapter 10's image signing used ECDSA P-256? The image path needed X.509 certificate *chains* — delegation, intermediates, revocation. Binary signing needs none of that; it's a fixed set of raw keys, and for raw keys Ed25519 is the boring, fast, hard-to-misuse choice. We already have an implementation in the tree: `ring`, which has been signing our OIDC tokens since the identity work. No new dependency, no new audit surface.

### The envelope

Signatures are *detached* — the binary stays byte-identical to what the release process produced, and the proof travels alongside as `bun-v0.2.0.sig`:

```json
{
  "schema": 1,
  "sha256": "9f2c…",
  "embedded": "base64 signature from a release key",
  "external": "base64 signature from the operator key, or null"
}
```

Verification runs in a fixed order, cheapest and most-diagnostic first:

```rust
pub fn verify_binary(
    bytes: &[u8],
    envelope: &SignatureEnvelope,
    release_keys: &[PublicKey],
    external_key: Option<&PublicKey>,
    network: bool,
) -> Result<(), UpgradeError> {
```

Hash first — a corrupted download fails with `HashMismatch` and a retry is the fix, no need to wonder about attackers. Then the embedded signature, accepted if it verifies against *any* key in the release set. A set rather than a single key is what makes rotation survivable: ship a version trusting old+new, sign the next release with new only, drop old a release later. No flag day.

Then the external signature, and here the type system does something worth noticing. The function takes `network: bool`, and the external logic is one `match` over three facts:

```rust
match (network, external_key, envelope.external.as_deref()) {
    (true, None, _) => Err(UpgradeError::ExternalKeyRequired),
    (true, Some(_), None) => Err(UpgradeError::ExternalSignatureInvalid),
    (_, Some(key), Some(sig)) => { /* verify, either way */ }
    (false, _, _) => Ok(()),
}
```

Tuple matching like this is why Rust people keep banging on about exhaustiveness: every combination of "is this a network upgrade / is a key configured / did a signature arrive" is visibly handled, and adding a fourth input later makes the compiler list every arm that needs rethinking. Note the third arm's `_` for `network` — even on an air-gapped upgrade, if an external key *and* an external signature are both present, a mismatch is an error. Silently ignoring a failed check because it wasn't strictly required is how verification code rots.

### Keys in source code, on purpose

`src/upgrade/keys.rs` contains the release *public* key as a plain constant:

```rust
pub const EMBEDDED_RELEASE_KEYS: &[&str] =
    &["ed25519:kdNmHSKOupiiF2i5vCyNrNMmEeagWZzB4DOm/w3a1IY="];
```

Public keys are public; committing one is fine and pinning it in the binary is the point — a config file must never be able to widen what a production binary trusts. The *private* key lives outside the repository (generated with `relish dev keygen`, which chmods it 0600 and prints a warning to that effect).

Which raises the testing problem. Integration tests need to sign binaries, and they obviously don't get the real private key. So `node.toml` grows `upgrades.release_keys_override` — and the code that honours it is gated the same way as the version sidecar from §14.1:

```rust
if let Some(override_keys) = &section.release_keys_override {
    if cfg!(debug_assertions) {
        return override_keys.iter().map(|k| parse_public_key(k)).collect();
    }
    eprintln!("bun: warning: upgrades.release_keys_override is ignored in release builds");
}
```

In a release build that branch collapses to the warning. A production binary's trust anchor is in its text segment, full stop.

### What we decided not to do

- **Certificate chains for binaries.** Sesame has a whole CA hierarchy we could have reused. It solves delegation problems we don't have here, at the cost of parsing X.509 in the most security-critical path we own.
- **Signed release metadata (TUF-style).** The metadata file (`upgrade check`) travels over HTTPS unauthenticated-beyond-TLS. It can lie about what versions exist; it cannot make a node run anything, because the binary signatures gate execution. Full TUF adds freshness and rollback-attack protection — noted as future work, deliberately not built today.
- **A single dual-purpose key.** Two signatures from keys in the same drawer is theatre. Different owners or don't bother.

The tests are the specification again: correct dual signatures verify; a wrong hash fails before any signature work; tampered bytes fail even with a "fixed-up" hash; an unknown release key fails; the second key of a rotation-window set passes; a network upgrade without the external key or signature fails with the right error; air-gapped skips what it may skip and still rejects a present-but-wrong signature. `cargo test --lib upgrade::signing`.

Next: where verified binaries live on disk, and how to swap one in atomically.

## 14.3 The symlink two-step

A node that upgrades itself needs somewhere to keep binaries — plural, because rollback means the previous version must still be on disk when the new one turns out to be a lemon. The layout is old Unix wisdom, nothing clever:

```
{binary_dir}/
  bun            -> bun-v0.2.0     (symlink, the entry point)
  bun-v0.1.0                       (previous version, kept for rollback)
  bun-v0.2.0                       (current version)
  bun-v0.2.0.sig                   (detached signature envelope)
```

Every version is a separate immutable file; "which version runs here" is a single symlink; changing the version is changing the symlink. `BinaryStore` (in `src/upgrade/store.rs`) wraps this directory with five operations: `stage`, `activate`, `current_target`, `installed_versions`, `garbage_collect`.

### Why not just overwrite the binary?

Because you can't do it atomically, and this is the one file on the node where a half-written state is fatal. If the node loses power halfway through `cp new-bun /usr/local/bin/bun`, it now has *no working orchestrator binary* and no way to fix itself. (There's a separate, funnier failure on Linux: overwriting a binary that's currently executing gets you `ETXTBSY`, and "fixing" that by truncating first crashes the running process. Ask me how people learn this.)

So writes never touch a live name. `stage` writes the new binary to a hidden temp file, `fsync`s it, sets the permission bits, and only then `rename(2)`s it to `bun-v0.2.0`. Rename within a filesystem is atomic: any observer sees the old state or the new state, never a torn one.

`activate` plays the same trick one level up, on the symlink itself:

```rust
let tmp_link = self.binary_dir.join(format!(".{}.link-{}", self.stem, std::process::id()));
let _ = std::fs::remove_file(&tmp_link);        // stale leftover from a crashed attempt
std::os::unix::fs::symlink(&target, &tmp_link)?; // create pointing at bun-v0.2.0
std::fs::rename(&tmp_link, self.symlink_path())?; // atomically replace `bun`
```

The naive `rm bun && ln -s bun-v0.2.0 bun` has a window — between the `rm` and the `ln` — where `bun` doesn't exist. Crash there and the supervisor's next restart fails with `ENOENT`. The temp-symlink-plus-rename dance has no window at all. You'll find this exact pattern in every serious deployment tool; now you know why.

Two smaller touches. The symlink target is *relative* (`bun-v0.2.0`, not `/usr/local/bin/bun-v0.2.0`) so the directory survives being moved or mounted at a different path — which our tests, running out of temp directories, immediately rely on. And after every rename we `fsync` the *directory*: file renames are directory mutations, and a power cut can otherwise undo a rename whose file data had long since hit the platter. `File::sync_all` on a `File::open` of the directory is the slightly odd-looking Rust spelling of `fsync(dirfd)`.

### Rust bits worth a look

Setting the executable bit is our first brush with the `std::os::unix` extension traits:

```rust
use std::os::unix::fs::PermissionsExt;
std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))?;
```

Rust's portable `std::fs` API has no concept of Unix permission bits, so the Unix-only parts live in extension traits you import explicitly. It's the same philosophy as `#[cfg(unix)]` (which guards this block): platform-specific code is allowed, but it announces itself.

The GC's core is a nice little exercise in iterator thinking — sort ascending, and the deletion *candidates* are everything except the newest `retain`:

```rust
versions.sort();
let candidate_count = versions.len().saturating_sub(retain as usize);
for version in versions.into_iter().take(candidate_count) {
    if protect.contains(&version) { continue; }
    // delete binary + .sig sidecar
}
```

`saturating_sub` is subtraction that stops at zero instead of panicking — with 2 versions installed and `retain = 3`, `2 - 3` on a `usize` would otherwise abort the process (in debug builds) or wrap around to a number with eighteen digits (in release builds, which is much worse). The sort works on `BinaryVersion` directly because of that derived `Ord` from §14.1; the semver rules quietly do the right thing when a pre-release is among the candidates.

The `protect` list is the subtle part of GC, and it's the caller's job to fill it. Retention says "keep the newest three", but a live upgrade marker may reference an *older* version as its rollback target — deleting that one to satisfy a retention count would saw off the branch the node plans to retreat along. Passing protection explicitly (rather than having the store peek at marker files) keeps the store dumb and testable; `gc_never_deletes_protected_versions` pins the contract.

### What we decided not to do

- **Hard links or copies instead of a symlink.** Both make "which version is live?" a forensic question. `readlink` is self-documenting — `current_target` is five lines.
- **Keeping binaries in Raft or Pickle only.** Distribution goes through Pickle (that's later in this chapter), but the *local* store must work with zero cluster dependencies: rollback happens at the exact moment the node is least able to talk to anyone.
- **A manifest file listing installed versions.** The directory *is* the manifest. `installed_versions` scans for `{stem}-v*` names and ignores everything else; a manifest would just be a second copy of that truth, one crash away from disagreeing with it.

Tests: staging writes content, signature, and mode bits; activation swaps atomically and keeps the old file; activating a missing version refuses; GC keeps the newest three of five, respects protection, removes `.sig` sidecars, and does nothing when there's nothing to do; foreign files in the directory are ignored. `cargo test --lib upgrade::store`.

Next: the state machine that decides, at every startup, whether this process is a fresh boot, a just-upgraded binary that must prove itself, or a crash-looping mistake that should put the old version back.

## 14.4 A state machine you can trust your process to

Here's the uncomfortable property of self-upgrade: the code that must recover from a failed upgrade runs *inside the thing that failed*. There is no adult in the room. If the new binary crashes on boot, whatever puts the old binary back has to be a fresh start of... the new binary, again, restarted by the supervisor, with no memory of the last attempt.

No memory *in the process*, that is. So we give it memory on disk: a single marker file, `{data_dir}/upgrade/marker.json`, that exists exactly while an upgrade is in flight on this node. It records where we came from, where we're going, how the attempt is progressing, and — the crucial field — `boot_attempts`, how many times a binary has started with this marker present.

Why a file and not Raft, where all our other state lives? Timing. The marker matters most in the exact moments when the runtime *isn't up*: between `exec()` and a working gossip mesh, or halfway through a crash loop. Raft needs a quorum; the marker needs `open(2)`. (Written atomically, of course — temp file, `fsync`, rename, same recipe as §14.3. A half-written marker would be a lie about the one thing we must not lie about.)

### The phases, and the missing one

```rust
pub enum MarkerPhase {
    Staged,        // binary + sig written, symlink NOT yet swapped
    Executed,      // symlink swapped, exec called
    Verifying,     // new binary booted, running self-checks
    RevertPending, // give up: revert on next startup
    Reverted,      // old binary back in control, failure not yet reported
}
```

Notice what's *not* there: `Committed`. A committed upgrade deletes the marker. This is deliberate and worth stealing for your own designs: if "everything is fine" were a marker state, then every startup would have to distinguish "fine" from "stale leftover saying fine", and any bug that forgot to write the final state would leave a zombie marker changing future behaviour. Absence can't go stale. No marker, no upgrade in flight, one source of truth.

### One pure function decides everything

Every bun startup begins the same way: load the marker (if any), and hand it to a pure function together with two facts — what version am I (§14.1) and how many boot attempts are allowed:

```rust
pub fn decide_startup(
    marker: Option<UpgradeMarker>,
    running: &BinaryVersion,
    max_boot_attempts: u32,
) -> StartupDecision
```

It returns one of five instructions:

```rust
pub enum StartupDecision {
    NormalBoot,
    VerifyUpgrade { marker: UpgradeMarker },          // I'm the new version: prove myself
    RevertAndExecPrevious { marker: UpgradeMarker },  // I've crash-looped: put the old one back
    CompleteRevert { marker: UpgradeMarker },         // I'm the old version, back in control
    ArchiveStaleMarker { reason: String },            // marker makes no sense: move it aside, boot
}
```

The logic reads like a table. Marker says `Staged` — the process died *between* staging and the symlink swap, so nothing actually changed; archive the attempt and boot. Marker says `Executed` or `Verifying` and I'm the target version — count this boot: within budget, verify; past budget, flip to `RevertPending` and revert. Same phases but I'm the *previous* version — someone (an operator, probably) already put the old binary back; complete the revert so the failure gets reported instead of silently forgotten. `RevertPending` and I'm the previous version — the revert worked, complete it. And any combination that doesn't add up — running a version the marker never mentions — is `ArchiveStaleMarker`, because the one thing bookkeeping must never do is brick the node. The marker is moved to `marker.json.stale-1` (kept for the post-mortem), not deleted, and boot continues.

Trace the crash-loop story through it, because this is the automatic rollback promised at the start of the chapter. Upgrade swaps and execs: `Executed`, attempts 0. New binary boots, decision increments to 1 — under the limit of 2 — `VerifyUpgrade`. It crashes. The supervisor (systemd in production, the test harness in tests) restarts the symlink, which still points at the new binary. Attempts 2: still `VerifyUpgrade`. Crashes again. Attempts 3: over budget — `RevertAndExecPrevious`. Startup flips the symlink back and execs the old binary, which wakes up, sees `RevertPending`-and-I'm-the-previous-version, and completes the revert: report the failure to the leader, archive the marker, get on with life. No human involved, and every step of that story is a unit test with the same name.

### Why pure?

`decide_startup` touches no filesystem, spawns nothing, execs nothing. All I/O happens before it (loading the marker) or after it (persisting the mutated marker the decision carries, then acting). That split is what makes the scariest logic in the chapter *boring to test*: the crash-loop test doesn't crash anything, it calls a function with `boot_attempts: 2` and asserts on the returned enum. Fourteen tests cover every phase-times-version combination, and they run in six milliseconds.

The pattern deserves a name on the wall: **decide, then act**. Compute the decision as data with a pure function; let a thin imperative shell carry it out. You met a milder version in the scheduler (Chapter 2's filter/score pipeline). Here it's load-bearing, because the "act" part includes `execv` — which we can't unit-test at all (it replaces the test process!) and therefore want as thin and dumb as possible. The real exec-crash-revert cycle does get tested end to end, with real binaries and a real supervisor loop, in §14.7.

One Rust note for the road. The decision arms use *match guards* — `MarkerPhase::Executed | MarkerPhase::Verifying if is_target =>` — combining or-patterns with a boolean condition. Guards cost you the compiler's exhaustiveness guarantee (it can't reason about the `if`), which is why the function ends with an explicit `_ => ArchiveStaleMarker` arm: the fallback isn't an oversight, it's the designed answer for "anything I didn't plan for".

### What we decided not to do

- **Keeping attempts in the supervisor.** systemd's `StartLimitBurst` could count restarts, but then the logic lives in a unit file we don't ship and can't test, and macOS/test environments behave differently. The marker makes the binary self-contained: any dumb `while true; do bun; done` loop is an adequate supervisor.
- **Reverting on the *first* failed boot.** Transient failures exist (a port not yet released, a slow disk). One retry is cheap; the budget is configurable (`upgrades.max_boot_attempts`, default 2).
- **Deleting stale markers.** Archiving costs nothing and the file is exactly what you'll want when diagnosing "why did node 7 revert last night".

`cargo test --lib upgrade::marker`.

Next, the prerequisite that makes the swap invisible to workloads: teaching the runtimes to *adopt* processes and containers they didn't start.

## 14.5 Your children survive exec()

Time to pay off the warning from the top of the chapter. When bun upgrades itself, it calls `execve(2)`: the kernel throws away the process's entire memory image and loads the new binary into the *same process*. Not a child, not a replacement — the same PID, mid-flight.

What survives an exec, and what doesn't, is the single most important table in this chapter:

| Survives | Destroyed |
|---|---|
| The PID | All memory — every struct, every `HashMap`, every `Child` handle |
| Parent/child relationships — **your children are still your children** | All threads (including the entire tokio runtime) |
| File descriptors *without* `O_CLOEXEC` | FDs *with* `O_CLOEXEC` — which is everything Rust's std and tokio open |
| The environment (the §14.1 trap) | Signal handlers |
| Working directory, resource limits | Locks tied to closed FDs (redb's, usefully) |

Read the first column again. The workload processes ProcessGrill spawned, and the foreground `runc run` processes driving our containers, are children of bun. Exec doesn't touch them. **The workloads survive the upgrade by default.** The kernel does the hard part of "containers survive" for free.

What doesn't survive is our *knowledge* of them. The `tokio::process::Child` handles, the entry maps, the supervisor state — all of it was memory. The new binary wakes up with running children it has never heard of. So the real work of this section isn't keeping workloads alive; it's rebuilding the bookkeeping. Three problems, each with a sharp edge.

### Problem 1: remembering — instance records

Every successfully started instance now writes a record to `{data_dir}/instances/{id}.json`: pids, ports, the OCI spec it was started from, the app spec (to rebuild health checks), where its logs go. On startup — before any reconciliation — the agent scans the records and asks the runtime to `adopt` each one. Adopted instances are seeded into the supervisor as `Running`; the reconcile loop then sees nothing missing and starts nothing twice. Records for dead processes are deleted, and those instances reschedule through the normal path.

The sharp edge: **pids get reused**. A record saying "web-0 is pid 4242" proves nothing — 4242 might now be someone's text editor. Adopting it would mean health-checking, signalling, and eventually SIGKILLing an innocent process. So records also store the process's *start time*, and adoption requires both to match:

```rust
pub fn is_live(record: &InstanceRecord) -> bool {
    match process_start_time(record.pid) {
        Some(started_at) => started_at.abs_diff(record.pid_started_at) <= 2,
        None => false,
    }
}
```

A pid plus its start time is, for practical purposes, a unique process identity. (The `±2s` slack exists because platforms round start times differently depending on when you ask. `abs_diff`, note, is the panic-free way to ask "how far apart" for unsigned integers — `a - b` on `u64` aborts in debug if `b > a`.)

### Problem 2: the pipe trap — logs must be files

ProcessGrill used to capture workload output with pipes: spawn with `Stdio::piped()`, read the other end in a tokio task. Follow the pieces through an exec. The reading task: gone (all threads). Bun's read-end FD: closed (CLOEXEC). The workload's write end: now points at a pipe nobody will ever read. The workload keeps serving happily until the pipe buffer fills or the kernel notices — and then its next `println!` gets **SIGPIPE, whose default action is process death**. The workload survives the upgrade and is then murdered by its own logging.

The fix is the one runc used from day one: redirect stdout/stderr to *files*. A file doesn't care who reads it or whether the reader is alive; the workload appends through the swap without noticing, and the new bun just keeps reading from the recorded path. `ProcessGrill::with_log_dir` enables this mode (the binary uses it always; in-memory pipes remain for unit tests). This is the quiet lesson of the section: in a system where processes replace themselves, *shared state belongs in the filesystem, not in process plumbing*.

### Problem 3: reaping — waitpid, ECHILD, and the two afterlives

An adopted process has no `Child` handle, so someone must still collect its exit status when it dies — otherwise it lingers as a zombie. Here Unix hands us a fork in the road, because an adoptee has two possible histories:

- **After an exec** (the upgrade path): it's still our child — same PID, remember. `waitpid(pid, WNOHANG)` works: it reports "still alive", or reaps the zombie and returns the exit code.
- **After a full restart** (the crash path: the supervisor spawned a *new* bun process): the orphaned workload was reparented to init. `waitpid` returns `ECHILD` — "not your child" — and we fall back to `kill(pid, 0)`, the classic no-op signal that answers only "does this process exist?". The exit *code* is unknowable in this history; init reaped it.

```rust
match waitpid(nix_pid, Some(WaitPidFlag::WNOHANG)) {
    Ok(WaitStatus::StillAlive) => (true, None),
    Ok(WaitStatus::Exited(_, code)) => (false, Some(code)),
    Ok(_) => (false, None),                       // killed by signal, etc.
    Err(Errno::ECHILD) => match kill(nix_pid, None) {
        Ok(()) => (true, None),                   // alive, someone else's child
        Err(_) => (false, None),                  // gone
    },
    Err(_) => (false, None),
}
```

This is `poll_adopted_process`, and it's called from `state()` — which the supervisor polls continuously anyway, so polling doubles as reaping and no separate reaper task is needed. If you've only ever managed processes from Go's `os/exec` or Python's `subprocess`, this is the machinery those libraries hide from you; it stops being hideable the moment the process that called `spawn` isn't the process calling `wait`.

RunC adoption is the same story one level up, with one extra check: besides the `runc run` pid being live, `runc state <id>` must report the *container* as `running` — the pid check authenticates the process, the state check authenticates the container. Apple Container adoption is deferred (`TODO(Phase 14 follow-up)`); macOS still exercises every line of this via ProcessGrill.

### What we decided not to do

- **A pidfd-based watcher.** Linux's `pidfd_open` gives race-free process handles and would beat polling — and doesn't exist on macOS. Polling from `state()` is portable and already on a loop we pay for.
- **Persisting restart-backoff counters.** Adopted instances restart their backoff history from zero. Defensible (the new binary is a new regime) and one less thing to keep consistent; documented behaviour.
- **Re-registering cluster routing at adoption time.** The reconcile paths rebuild routing anyway; doing it twice invites disagreement.

The tests to read: `records::liveness_rejects_reused_pid` (the start-time fingerprint doing its job), `process::state_detects_adopted_instance_exit` (a real child, deliberately never `wait()`ed by the test, reaped through adoption polling), and `agent::startup_adopts_recorded_instances_instead_of_restarting` (the mock grill proving the supervisor calls `adopt`, and never `create`/`start`, for a recorded instance).

With workloads able to out-live the process that started them, we can finally build the thing that kills that process on purpose: the node-level upgrade manager.

## 14.6 Replacing yourself without dying

All the pieces exist: versions, signatures, the store, the marker, adoption. The `UpgradeManager` strings them into the actual node-level upgrade, and its design pivots on one uncomfortable fact — somewhere in the middle of this sequence is a function call that, if it works, *never returns*.

You can't unit-test `execv`. It replaces the test process. So the manager splits the sequence at exactly that line:

- **`prepare(directive, inventory)`** — everything testable: check no upgrade is in flight (idempotent on `upgrade_id`, so a re-delivered directive is a no-op), fetch the binary (local file, or Pickle blob by its sha256), verify the dual signatures, stage into the store, write the `Staged` marker with the pre-upgrade workload inventory. If any of it fails, the error propagates and *the running system is untouched* — the symlink never moved.
- **`execute(prepared)`** — the point of no return, kept almost too dumb to break: write the `Executed` marker, `activate` the symlink, `execv`. Three lines of consequence. It returns only on failure, and then puts the symlink back before reporting.

The ordering inside those three lines is not negotiable, and it's worth spelling out why. Marker *before* symlink: if we crash between them, the marker says `Executed` but the previous version is still what runs — startup recovery reads that as a failed upgrade and reports it. Symlink *before* the marker would invert the failure: a crash leaves the *new* binary active with a marker claiming nothing was executed; the stale-marker path would archive it and the node would silently run an unverified version. Same three operations, opposite safety, purely from order.

### The exec itself

```rust
let path_c = CString::new(path.to_string_lossy().into_owned())?;
let mut argv_c = vec![path_c.clone()];
for arg in self.original_argv.iter().skip(1) { argv_c.push(CString::new(arg.as_str())?); }

// execv only returns on failure.
let err = nix::unistd::execv(&path_c, &argv_c).err();
```

Three Rust-meets-Unix notes. `CString` is the NUL-terminated string C expects — and `CString::new` *fails* if the input contains a NUL byte, a check C callers routinely forget and Rust makes unskippable. We exec the *symlink* path, not the versioned file, so the new process's identity is the stable entry point (and §14.1's canonicalisation finds the real file behind it). And we replay the original argv — captured at startup — so `--config`, `--cluster` and friends survive into the new regime; the new binary re-parses them like any boot.

What about all the open sockets, the redb database, the log files? This is where a decision Rust's std made years ago quietly pays off: every file descriptor Rust opens is `O_CLOEXEC` — closed atomically by the kernel *during* exec. No shutdown code runs (there's no code left to run), yet the listener port is free for the new process to bind, and redb's file locks (which live on the fds) evaporate with them. The new binary just... boots, like any boot. There's a sub-second blip where the API answers nothing; gossip shrugs it off (the incarnation number bumps on restart, existing behaviour).

One genuinely awkward wrinkle: the upgrade arrives over HTTP, and the response must escape the process before exec destroys the socket. The agent replies `202 Accepted` after `prepare` succeeds, then sleeps 200ms before `execute`. Yes, a sleep. The alternatives (hooking response-flush completion through axum's internals) buy precision nobody needs — the caller polls `/v1/version` to observe the outcome anyway, so a lost response is survivable; the sleep just makes it rare.

### Draining, verifying, committing

While an upgrade is staged, the agent sets a **drain flag**: new deploys are refused with a clear message; running workloads are untouched. It's one `AtomicBool` — the cluster-level version of "don't schedule onto an upgrading node" comes later, in the orchestrator; this is the node defending itself.

After the exec, the new binary's boot does what §14.4 and §14.5 built: startup recovery returns `Continue { verify: Some(marker) }`, workloads get adopted, and a task waits out `boot_grace_secs` (default 30 — surviving the grace period is itself part of the proof, since crashing inside it burns a boot attempt). Then the agent checks the marker's inventory: every workload that was `Running` before the swap must exist and be `Running` now. All present → `commit`: write history, delete the marker, GC old binaries (protecting the one we'd revert to — the §14.3 `protect` list in action). Anything missing → `mark_revert_pending` and `exit(1)`, handing the baton to the supervisor and the crash-loop machinery. The upgrade doesn't get to shrug off a lost workload as someone else's problem; a swap that eats a workload *is* a failed swap.

The inventory deliberately skips jobs — a run-to-completion job finishing *during* the upgrade is success, not a casualty.

### The first-ever upgrade

A fresh install is a plain `bun` binary, no versioned files, no symlink. The first `prepare` on such a node detects that the running version has no file in the store and copies the current executable in as `bun-v0.1.0` before staging the new one — so rollback has something to return to even on a node that has never upgraded. (The copy gets a stub signature envelope; it isn't re-verified locally — we're already running it, that ship has sailed.) The chapter's opening honesty applies here too: a v0.1.0 *cluster* has no upgrade code at all, so the first deployment of upgrade-capable binaries is a manual rollout. Everything in this chapter applies from that version onward.

### API surface

Three protected endpoints and one public one land with this step: `POST /v1/upgrade/apply` (admin; the directive as JSON; 202 then exec), `POST /v1/upgrade/rollback` (admin; optional version, defaulting to the newest installed version older than the running one — no download, no re-verify, the binary was verified when it was staged), `GET /v1/upgrade/status` (marker + history), and public `GET /v1/version`. That last one is deliberately *not* routed through the agent's command channel: the upgrade orchestrator will poll it to gate every rolling step, and it must answer even when the agent loop is busy deploying. It reads the manager directly — one version string and one `stat()`.

Tests: `cargo test --lib upgrade::manager`. The ones worth reading are `apply_verifies_before_staging` (a bad signature leaves *zero* trace — no staged file, no marker, symlink untouched) and `prepare_stages_binary_and_writes_staged_marker` (staging is visible, activation hasn't happened). The exec path itself gets its reckoning in the next section, with real binaries and a real supervisor.
