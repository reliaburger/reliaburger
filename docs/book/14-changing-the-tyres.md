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

An attacker has to compromise both, and they don't live in the same place. That's the whole design. Air-gapped upgrades (`relish upgrade start --binary`, where an operator hand-carries a file to the cluster) require only the embedded signature — the operator's approval is implicit in the hand-carrying — matching how `UpgradeConfig` was specced in the design doc. That holds on one node. In a cluster the other nodes fetch the hand-carried file from the registry, which is the network again, so they want both signatures.

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
    &["ed25519:zSUgsFfmv0WohbjRJE7FJf/xgLIgMuK7AbnDgOdduRM="];
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

### Countersigning without the release key

For a long while the only signing tool was `relish dev sign-binary --key release.key [--external-key operator.key]`. Look at who holds which key and you'll spot the problem: the release key belongs to the project, the external key to the operator, and the one command that could add the operator's signature demanded both. Nobody outside the project could countersign a release. Our own V02 soak harness walked straight into it: it started every upgrade walk with a release-signed soak build, each node asked for the second signature, and every walk was refused.

So there's a second command, `relish dev countersign-binary --external-key operator.key bun-v0.2.0`, built on one small function:

```rust
pub fn countersign(
    envelope: &SignatureEnvelope,
    external_pkcs8: &[u8],
    bytes: &[u8],
) -> Result<SignatureEnvelope, UpgradeError> {
    let actual = sha256_hex(bytes);
    if !actual.eq_ignore_ascii_case(&envelope.sha256) {
        return Err(UpgradeError::HashMismatch { expected: envelope.sha256.clone(), actual });
    }
    Ok(SignatureEnvelope {
        external: Some(sign(external_pkcs8, bytes)?),
        ..envelope.clone()
    })
}
```

`..envelope.clone()` is the struct update syntax from Chapter 1: every field not named comes from the envelope, so the release signature is copied, never recomputed. The hash check stops you countersigning a `.sig` that belongs to a different binary, which would otherwise produce an envelope that fails on every node.

One more wrinkle. `ring` has two ways to load a PKCS#8 private key: `from_pkcs8`, which insists on version 2 (the document carries the public key, and ring checks it matches), and `from_pkcs8_maybe_unchecked`, which also takes version 1. `openssl genpkey -algorithm ed25519` writes version 1. Operators should be able to make their key with whatever tool they trust, so signing now uses the second. Nothing is lost: a signature that doesn't match the operator's real public key fails verification on every node anyway.

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

Making the marker's *absence* the "settled" signal has a consequence that bit us once. `upgrade_in_flight()` is literally `marker_path.exists()` — one stat — and the whole cluster reads it to decide an upgrade is done. So whatever `commit()` still has left to do after it removes the marker happens in a window where an outside observer already believes the node has settled. Our first `commit()` removed the marker and *then* ran retention GC (§14.3), which deletes each aged-out binary and its `.sig` sidecar as two separate syscalls. A retention test that polled for "settled" and immediately checked the store would, on a loaded CI runner, occasionally catch the prune half-done: the binary gone, the sidecar not yet. It passed a thousand times locally because the window is microseconds on an idle machine. The fix is just an ordering rule — **do all the work first, flip the visible flag last**: run GC (best-effort; pruning an old binary must never fail a verified upgrade), then remove the marker. Once "settled" means the marker is gone, "settled" must also mean everything the commit promised has already happened.

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

Every successfully started instance now writes a record to `{data_dir}/instances/{id}.json`: pids, ports, the OCI spec it was started from, the app spec (to rebuild health checks), where its logs go and, for rootless runc, the userspace network owner. On startup — before any reconciliation — the agent scans the records and asks the runtime to `adopt` each one. Adopted instances are seeded into the supervisor as `Running`; the reconcile loop then sees nothing missing and starts nothing twice. Records for dead processes are deleted, and those instances reschedule through the normal path.

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

The fix is the one runc used from day one: redirect stdout/stderr to *files*. A file doesn't care who reads it or whether the reader is alive; the workload appends through the swap without noticing, and the new bun just keeps reading from the recorded path. The binary builds its ProcessGrill with `with_owner`, which gives it file-backed logs and the durable process owners described below. This is the quiet lesson of the section: in a system where processes replace themselves, *shared state belongs in the filesystem, not in process plumbing*.

### Problem 3: reaping — waitpid, ECHILD, and the two afterlives

An adopted process has no `Child` handle, so someone must still collect its exit status when it dies, otherwise it lingers as a zombie. Our first answer was a poller, `poll_adopted_process`, that knew about the two histories an adoptee can have:

- **After an exec** (the upgrade path): it's still our child (same PID, remember). `waitpid(pid, WNOHANG)` works: it reports "still alive", or reaps the zombie and returns the exit code.
- **After a full restart** (the crash path: the supervisor spawned a *new* bun process): the orphaned workload was reparented to init. `waitpid` returns `ECHILD` ("not your child"), and we fall back to `kill(pid, 0)`, the classic no-op signal that only answers "does this process exist?". The exit *code* is unknowable in this history; init reaped it.

```rust
match waitpid(nix_pid, Some(WaitPidFlag::WNOHANG)) {
    Ok(WaitStatus::Exited(_, code)) => Ok((false, Some(code))),
    Ok(WaitStatus::Signaled(..)) => Ok((false, None)),
    Ok(_) => Ok((true, None)),
    Err(Errno::ECHILD) => match kill(nix_pid, None) {
        Ok(()) => Ok((true, None)),               // alive, someone else's child
        Err(Errno::ESRCH) => Ok((false, None)),   // gone
        Err(error) => Err(error.into()),
    },
    Err(error) => Err(error.into()),
}
```

(The real function also rejects a PID of zero, since `waitpid(0)` means "any child in my process group", and re-checks the start time before trusting the answer.) If you've only ever managed processes from Go's `os/exec` or Python's `subprocess`, this is the machinery those libraries hide from you; it stops being hideable the moment the process that called `spawn` isn't the process calling `wait`.

It works, and it has two holes. A job that finished while Bun was down has no exit code, so "did my job succeed?" becomes "unknown". And there's a gap between spawning a process and writing its record: crash there and nobody knows the process exists. So in production, neither Bun nor its successor is the parent any more. Every workload, in process mode and under runc alike, runs beneath a small *process owner*: a helper that outlives Bun, records its child before letting it run, reaps it, and writes the real exit code to disk. Chapter 1 walks through how it works for runc and Chapter 8 for process workloads. For this chapter, the point is that Bun's exec can't lose a reaper it never was. After an exec or a restart, Bun reconnects to each owner over its private socket and asks. The poller above survives only in the owner-less, file-backed mode the unit tests use.

Adoption still cross-checks the adoption record against the owner. For runc, the owner must report the `runc run` launcher as running, and its PID and start time must match the record. A rootless container's network helper is an owned process too, so the same machinery covers it.

Apple Container adoption drops the pid check entirely. An Apple workload runs *inside a VM* managed by the `container` daemon; it was never a child of bun, so there's no pid to fingerprint. The recoverable handle is the container itself: `container inspect <id>` reporting `running` means the VM sailed through our exec, so we re-track the entry and re-discover its IP. A vanished container declines adoption; an inspection that *fails* is an error, not absence, and stops startup rather than deleting a record we might still need. (The Apple adapter isn't part of the 0.1.0 release, for reasons Chapter 1 explains, but its adoption tests still run behind `make test-apple`.)

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

After the exec, the new binary's boot does what §14.4 and §14.5 built: startup recovery returns `Continue { verify: Some(marker) }`, workloads get adopted, and a task waits out `boot_grace_secs` (default 30 — surviving the grace period is itself part of the proof, since crashing inside it burns a boot attempt). Then it commits: write history, delete the marker, GC old binaries (protecting the one we'd revert to — the §14.3 `protect` list in action). A genuinely broken new binary never reaches this point — it crash-loops inside the grace window and the boot-attempt budget (§14.4) reverts it first.

There's a subtlety about *what* counts as a broken upgrade, and it bit us in the cluster tests. On a **single node**, the agent also checks the marker's inventory: every workload that was `Running` before the swap must be `Running` after. A vanished workload there really is a failed swap — nothing else could have moved it — so it triggers `mark_revert_pending` + `exit(1)`. But in a **cluster**, workload placement belongs to the scheduler, not the node: while the node bounces, the leader may legitimately reschedule an app elsewhere. A node that reverted its own (perfectly good) binary because the cluster moved a workload would be confusing correctness for coincidence. So in cluster mode the inventory mismatch is logged, not fatal — liveness through the grace window is the proof, and the "containers survive" guarantee rests on adoption (§14.5) plus the crash-loop budget, not on a placement census the node no longer owns. (The inventory skips jobs either way — a run-to-completion job finishing *during* the upgrade is success, not a casualty.)

### The first-ever upgrade

A fresh install is a plain `bun` binary, no versioned files, no symlink. The first `prepare` on such a node detects that the running version has no file in the store and copies the current executable in as `bun-v0.1.0` before staging the new one — so rollback has something to return to even on a node that has never upgraded. (The copy gets a stub signature envelope; it isn't re-verified locally — we're already running it, that ship has sailed.) The chapter's opening honesty applies here too: a v0.1.0 *cluster* has no upgrade code at all, so the first deployment of upgrade-capable binaries is a manual rollout. Everything in this chapter applies from that version onward.

### API surface

Three protected endpoints and one public one land with this step: `POST /v1/upgrade/apply` (admin; the directive as JSON; 202 then exec), `POST /v1/upgrade/rollback` (admin; optional version, defaulting to the newest installed version older than the running one — no download, no re-verify, the binary was verified when it was staged), `GET /v1/upgrade/status` (marker + history), and public `GET /v1/version`. That last one is deliberately *not* routed through the agent's command channel: the upgrade orchestrator will poll it to gate every rolling step, and it must answer even when the agent loop is busy deploying. It reads the manager directly — one version string and one `stat()`.

Tests: `cargo test --lib upgrade::manager`. The ones worth reading are `apply_verifies_before_staging` (a bad signature leaves *zero* trace — no staged file, no marker, symlink untouched) and `prepare_stages_binary_and_writes_staged_marker` (staging is visible, activation hasn't happened). The exec path itself gets its reckoning in the next section, with real binaries and a real supervisor.

## 14.7 Testing a program that replaces itself

Everything so far has been unit-testable because we kept carving the untestable parts away — the pure state machine, the prepare/execute split. The bill for that carving now comes due: nothing has actually exec'd anything. Time to run the real binary and watch it eat itself.

`tests/self_upgrade.rs` is a different kind of test file. It doesn't call functions; it plays *operator*. Cargo helps more than you'd expect: for every `[[bin]]` target, integration tests get an env var with the compiled path — `env!("CARGO_BIN_EXE_bun")` — so the tests always drive the exact binary the build just produced, no `target/debug` guessing.

Each test builds a miniature production node in a temp directory:

```
{tmp}/bin/bun -> bun-v0.1.0      # the store, symlink and all
       bun-v0.1.0                # both "versions" are copies of the same
       bun-v0.2.0                #   build, told apart by .version sidecars
{tmp}/data/                      # marker, records, history
{tmp}/node.toml                  # throwaway keys via release_keys_override
```

and then — the piece that makes the whole file honest — a **supervisor loop**, fifteen lines of tokio that impersonate systemd:

```rust
loop {
    let mut child = Command::new(&symlink).args(...).spawn()?;
    tokio::select! {
        _ = child.wait() => {
            if *stop_rx.borrow() { break; }
            sleep(Duration::from_millis(200)).await;   // Restart=always
        }
        _ = stop_rx.changed() => { let _ = child.kill().await; break; }
    }
}
```

There's a lovely subtlety hiding in `child.wait()`. When the upgrade *succeeds*, the supervisor never notices: exec keeps the PID, so as far as `wait()` is concerned the child simply hasn't exited. The supervisor only wakes for crashes — which is exactly systemd's view of the world too. The test can even assert on it: the bun process's pid before and after a successful upgrade is *the same pid*.

The five tests are the roadmap's five promises, verbatim:

- **`single_node_upgrade_preserves_running_containers`** — deploy a testapp, note its pid, post a signed directive, wait for `/v1/version` to say v0.2.0. Assert the workload's pid didn't change and it still answers HTTP. The console output of this test is the whole chapter in four lines: `upgrading to v0.2.0` → `reliaburger node agent v0.2.0` → `adopted 1 running instance(s) from a previous process` → `upgrade to v0.2.0 verified and committed`.
- **`single_node_rollback_reverts_to_previous_version`** — upgrade, then `POST /v1/upgrade/rollback`. Same workload pid across *both* swaps.
- **`failed_upgrade_triggers_automatic_rollback`** — the star witness. The test drops a `bun-v0.2.0.fail-boot` sidecar (the §14.2-style debug-only hook; it fires *after* startup recovery, so each crash burns a boot attempt). Then: exec → crash → supervisor respawn → crash → respawn → attempts exhausted → symlink reverted → old binary boots → history records `Reverted`. No assertion in the test lifts a finger to help; it just waits for `/v1/version` to read v0.1.0 again. And the workload? Its parent died repeatedly, it got reparented to init, and the reverted binary adopted it back through the `ECHILD` path from §14.5 — same pid, never stopped serving. That single test exercises the marker, the boot-attempt budget, the symlink revert, reparenting, and adoption in one story.
- **`version_retention_gc_keeps_last_three`** — four consecutive upgrades; asserts exactly v0.3.0–v0.5.0 remain on disk, sidecars gone with their binaries.
- **`upgrade_rejects_bad_external_signature`** — right key, wrong bytes: `409`, and the node is byte-for-byte untouched.

Two practical notes that will save you an afternoon. First, everything binds ephemeral ports — including the Pickle registry, whose default port turned out to be squatted on macOS dev machines (AirPlay sits on 5000; ask us how we know). Second, the tests are **serialised** with a file-local `tokio::sync::Mutex`: each one runs a full node with real processes, and five at once starve each other into flaky timeouts on a laptop. A `static SERIAL: Mutex<()>` and one `lock().await` per test is the cheapest fix that doesn't add a dependency.

What we decided not to do: mock the supervisor (its behaviour under exec — *not waking up* — is part of what's under test), and share one harness across tests (isolation is the entire point; 40 seconds per test is the price and it's worth paying).

Run them with `make test-upgrade` (they're gated behind `RELIABURGER_UPGRADE_TESTS=1` — spawning real binaries under a supervisor takes minutes, too slow for the default CI test job, so they live in their own workflow job like the runc and eBPF suites). Next: teaching the *cluster* about upgrades — state in Raft, and a leader that walks the fleet.

## 14.8 Where upgrade state lives

A rolling upgrade across a cluster is a long-running process with a coordinator — and the coordinator is a node that will itself be upgraded before the run ends. If the plan lived in the leader's memory, "leader upgrades last" would be a paradox: transferring leadership would forget the plan. So the plan lives where everything durable lives in Reliaburger: the Raft-replicated `DesiredState`.

```rust
/// The rolling binary upgrade in progress, if any (at most one).
#[serde(default)]
pub active_upgrade: Option<ClusterUpgradeState>,
/// Completed/abandoned cluster upgrades, newest last (bounded to 20).
#[serde(default)]
pub upgrade_history: Vec<ClusterUpgradeState>,
```

`ClusterUpgradeState` is the whole plan as data: target version, signatures, worker parallelism, direction (upgrade or rollback), the cluster phase (`Preparing → UpgradingWorkers → UpgradingCouncil → TransferringLeadership → UpgradingLeader → Completed`, with `Paused { reason }` as the escape hatch, and `Aborted { reason }` for a pause the operator ended; see "A pause with no way out" at the end of the chapter), and one `NodeUpgradeRecord` per node — role, address, observed version, per-node phase. When leadership moves mid-run, the new leader reads this and continues from exactly where the old one stopped. No handover protocol; the handover *is* the replication that already happened.

Two new log entries drive it, and their design follows the deploy machinery from Chapter 7: `UpgradeUpdate { state }` (last-writer-wins full replacement — only the leader's orchestrator writes, so merging semantics would be complexity without a customer) and `UpgradeClear { upgrade_id }` (archive to bounded history). The clear checks the id: a stale clear racing a newer upgrade must not delete the wrong run.

### The rule that can brick a cluster

Here's the part to read twice. The Raft log entries and the council's Raft RPC both carry `RaftRequest`, and `RaftRequest` is serialised as **self-describing JSON** — serde tags each enum variant by its *name* (`{"AppSpec": {...}}`), not by a numeric index. (This is the same reason the snapshot uses JSON, and the same lesson Chapter 2 learned the hard way: `RaftRequest` embeds config types like `replicas = "*"` that need serde's `deserialize_any`, which a positional format like bincode can't drive. So the log had to become self-describing.) Which sets the compatibility rule:

**A parseable entry is not a compatibility contract.** Renaming a variant breaks every old log entry with that tag. Adding one is subtler: a newly elected leader may emit it while an old follower is still running, and that follower can't decode it. Leader-last ordering makes that less likely; it can't stop an election. So we don't pretend that "it parses" means "it's compatible".

For 0.1.0 the promise is deliberately narrow:

- **Fresh clusters only.** 0.1.0 doesn't load data written by pre-release development builds. Each node writes a `state-format.json` stamp into its data directory before opening any subsystem, and refuses unmarked or mismatched data rather than guessing, leaving it untouched so you can keep a copy.
- **Two generations, not one version.** Every binary advertises a *protocol* generation (what it speaks on the wire) and a *state* generation (what it writes to disk: Raft snapshots, backups, the stamp). They're plain integers in `src/compatibility.rs`, independent of the product version.
- **Rolling upgrades and rollbacks need an exact match of both.** Product versions may differ; the generations may not. An incompatible change bumps the relevant generation, and migration between generations is designed separately when we need it.

```rust
/// Formats understood by one binary. Equality is the initial rolling-upgrade policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compatibility {
    /// Cluster protocol generation, independent of the product version.
    pub protocol: u32,
    /// Durable state generation, including Raft snapshots and sealed backups.
    pub state: u32,
}
```

`Copy` in the derive list means the struct is copied bit-for-bit on assignment, like a C struct, instead of being moved. That's fine for two integers, and it lets `require_current(self)` take the value without anyone losing it.

Every boundary checks the pair. Raft requests carry both generations, gossip rejects a mismatch before touching membership, reporting frames carry a fixed header, and a joining node has to match before it reveals its one-time token. Absent evidence is a refusal.

A candidate binary gets checked too, before it's staged. Run `bun --compatibility` and it prints the pair as JSON without loading config or starting a runtime. The upgrade manager verifies the release signature first, writes those verified bytes to a private temporary file, runs *that* copy with `--compatibility`, and caps the output at 4 KiB with a ten-second deadline. Why a copy? Checking the download path and executing it later would let the file change in between. `NamedTempFile::into_temp_path` hands us a path that deletes the file when dropped, and closes our writable descriptor first, because Linux refuses to execute a file that's open for writing (`ETXTBSY`, "text file busy"). Even then, the full test suite caught the occasional `ETXTBSY`: another thread forking at the wrong moment briefly inherits the descriptor. That's a [known race in process launching](https://github.com/rust-lang/rust/issues/114554), so the probe retries that one error within the same deadline. Retrying can't turn an incompatible binary into an accepted one.

Tests cover mismatched gossip and Raft messages, refused development state, rejected joins, signed-but-incompatible executables and rollback refusal. Test fixtures that model a *compatible* peer take their pair from `compatibility::CURRENT` rather than hard-coding numbers. We learnt that one when a state bump turned ten unrelated tests red at the compatibility check instead of at the behaviour they were meant to test.

### Cordoning

While a node swaps binaries, the scheduler shouldn't hand it new work. The mechanism is two small pieces: `ClusterUpgradeState::is_node_cordoned` says whether a node's record is in an actively-swapping phase (`Directed` or `Verifying` — a `Pending` node keeps taking work until its turn actually comes; cordoning the whole fleet at once would be a self-inflicted capacity outage), and `meat::filter::apply_upgrade_cordon` flips those nodes' `ready` flag in the scheduler's cache, which the existing filter already respects. No new filter logic — readiness was designed as the "don't place here" bit, and upgrades are just one more thing that clears it.

Tests: `cargo test --lib council::state_machine` (the update/clear/bounded-history/compat set) and the `scheduler_skips_nodes_mid_upgrade` filter test. Next: the orchestrator that actually walks the fleet.

## 14.9 Workers first, leader last

Why that order? Blast radius, then arithmetic. Workers are individually expendable — the scheduler routes around any one of them, so they absorb a bad release cheapest, in configurable batches (`--parallel`). Council members carry Raft, so they go strictly one at a time: with 3 voters, quorum is 2, and one member mid-restart is exactly as many as you can spare (5 voters can spare 2, but one at a time keeps reasoning simple — a second is only directed after the first reports healthy). The leader goes last because it's the orchestrator; by the time its turn comes, every other node has proven the new binary works *in this cluster*, and the machine that directs its upgrade is already running it.

There's a trap hiding in "with 3 voters you can spare one". You can spare one *live* voter. If a voter is *already* down — a hardware fault, a network partition, whatever — then a 3-voter council is really running on 2 live votes, and taking a second down for its swap drops you to 1, below quorum, and the cluster loses its leader for the whole bounce (or forever, if the swap goes wrong). So the quorum check counts **live** voters, not configured ones. It cross-references the Raft voter set against the gossip `Alive` view (bridging Raft's numeric ids to gossip names through the same stable hash the rest of the cluster uses), and refuses the next council step whenever taking one more down would leave fewer than a quorum standing. The leader always counts itself live — it's the thing running the check. `live_quorum_headroom_ok(configured, live)` is a two-line pure function, and its unit tests pin exactly the boundaries that matter: 3 voters with one already dead is refused, 5 voters with two dead is refused, and the fully-live cases proceed.

The orchestrator (`src/upgrade/orchestrator.rs`) has three layers, testability decreasing as consequence increases — the same pattern as §14.6:

1. **`step`** — one tick of the walk, generic over a `NodeControl` trait (probe a node, send a directive). Every behaviour test runs against a mock cluster in a `HashMap`; not one of them opens a socket.
2. **`HttpNodeControl`** — the trait's boring production body: reqwest calls to each node's API with the cluster service token.
3. **`run_orchestrator`** — the driver loop, spawned on every cluster node: every 3 seconds, *if I am the leader and an upgrade is active*, run `step`, persist to Raft if anything changed.

### Idempotency is the resume story

Every `step` begins by *polling reality* — `/v1/version` on each node it cares about — rather than trusting its own records. This one habit collapses a pile of failure modes into non-events:

- A `Pending` node that's somehow already at the target (leader crashed after directing it, before recording it)? First poll says so; it's marked `Healthy` without ever being directed. There's a test named for it: `resume_skips_nodes_already_at_target`.
- A directive sent but lost (node rebooted at the wrong moment)? `Directed` nodes that don't show an upgrade in flight get the directive re-sent next tick — and re-sending is safe because the node side is idempotent on `upgrade_id` (§14.6). We *chose* send-then-record over the classic record-then-send write-ahead: with idempotent directives, the worst outcome of either crash window is a retry, and retries are free.
- A leadership change mid-walk? The new leader's copy of the loop reads the same Raft state and its first `step` re-polls everything. Resuming *is* the normal path; there is no special recovery code.

Failure detection also falls out of polling. A node that was `Verifying` and reappears on its *old* version, no upgrade in flight, has self-reverted (§14.4 did its job) — mark it `Failed`, pause the whole run. A node that vanishes entirely trips a stuck-node timeout. Per the design doc, cluster-level reaction to failure is deliberately *pause and tell the operator*, not auto-rollback of the fleet: one node's revert is contained, but yanking N healthy nodes back because one disagreed multiplies the blast radius on the worst day. `upgrade resume` returns `Failed` nodes to `Pending` (where the poll-first habit sorts out what actually needs doing) and re-enters the earliest unfinished group.

One more thing has to be true before a node counts as *done*, and it's easy to miss: the node has to be back in the **gossip mesh**. HTTP-healthy at the target version is necessary but not sufficient — a process can come up, answer `/v1/health`, and still be isolated from the mesh (a firewall that didn't reopen, an interface that didn't come back). An isolated node takes no service traffic and its vote doesn't count; calling it "upgraded" and moving on would quietly shrink the cluster one node at a time. So the `Healthy` transition needs all three: target version, HTTP-healthy, *and* `Alive` in the current gossip snapshot. A node that's healthy-but-not-in-gossip is held in `Verifying` until it rejoins — and if it never does, the stuck-node timeout catches it and pauses the run, which is exactly the operator's cue that something's wrong with that box. The two unit tests spell out the difference: same probe (healthy, on target), one with the node in the gossip-alive set and one without — the first completes, the second doesn't.

### The replacement must prove rejoin locally too

The leader's check isn't the only one. The upgraded node itself decides whether to commit its new binary or revert, so it needs its own proof. It removes its upgrade marker only after it has survived `boot_grace_secs` *and* its own gossip transport has received a direct acknowledgement from a peer within `gossip_rejoin_secs`. Membership restored from disk or a configured seed list doesn't count. The observation lives in a `watch::Receiver<bool>` that starts at `false` and is never saved, and `tokio::join!` waits on the grace timer and the gossip deadline at the same time, so a long grace period can't stretch the rejoin deadline. If either fails, the node writes `RevertPending` and exits, and the supervisor brings back the old binary, which adopts the same workloads.

### The leader goes last — in place

The original design called for the leader to *transfer* leadership to an already-upgraded member, then upgrade as a follower. We built that, and it failed the integration test in an instructive way. openraft 0.9 has no `transfer_leader`; the closest primitive is `Raft::trigger().elect()`, "call an election on yourself". So the old leader asked an upgraded member to campaign — and it never won. Raft has an *anti-disruption* rule (leader-stickiness): a follower that's recently heard the leader's heartbeat refuses to vote for a challenger, precisely so a flaky node can't unseat a healthy leader. A live leader replicating happily is unseatable this way by design.

(Amusingly, an earlier version of this test *passed* — because at that point nothing else wrote to Raft, so once the workers and council were done the log went quiet, the heartbeats were the only traffic, and the challenger squeaked through. The moment the cluster grew background writers — a scheduler, an autoscaler — the log stayed busy and the transfer wedged forever. A test that passes for a reason you didn't intend is a landmine.)

The honest fix is to stop fighting Raft: **the leader upgrades in place, last.** It execs itself like any other node. Exec is sub-second; a council of three or more keeps quorum through the bounce (losing one voter of three still leaves two, a majority); and when the node comes back — reclaiming leadership, or yielding to whoever the followers elected in the interim — the current leader's orchestrator does what it always does, polls reality, sees the former leader now healthy on the target version, and marks the run complete. No special path, just the poll-first idempotency from earlier doing its job. `TransferringLeadership` survives as a one-tick pass-through to `UpgradingLeader` (and a place to hang this explanation); `/v1/cluster/elect` survives as a manual admin tool for moving leadership before planned maintenance. The whitepaper's "transfers leadership and upgrades last" became "upgrades last, in place" — same guarantee (the leader is the last thing to move, and quorum never drops), simpler mechanism.

The lesson is worth more than the feature: when a distributed primitive resists you, check whether you're working *against* a safety property. Leader-stickiness wasn't in our way by accident — it's the same rule that stops a partitioned node from disrupting a healthy cluster. The design that respects it is shorter than the one that fought it.

### What the API gained

`POST /v1/upgrade/start` (admin, leader): the plan — target version, hashes and signatures, `parallel`, the registry address nodes should fetch from, and the node list. The client names *which* nodes to upgrade, but it does **not** get to say what those nodes are. The leader rebuilds each node's role (from the Raft voter set and current leader) and address (from gossip membership) server-side and validates the request against its own view. Why bother? Because the roles decide the rolling order, and the leader-last invariant is load-bearing: a caller who could label a live leader "worker" (or a worker "leader") could make the real leader upgrade first-and-disruptively, or point directives at another host entirely. So a spoofed address, or any claim that crosses the leader boundary, is rejected; a harmless worker↔council relabel among non-leaders is quietly corrected to the authoritative role. Either way the plan the orchestrator walks is built from the leader's truth, never the client's claim. `GET /v1/upgrade/cluster` reads the replicated state from any node. `resume` and `cluster-rollback` do what they say — rollback runs the same walk with `direct_rollback` directives and no distribution step, since every node still has the previous binary on disk (§14.3's retention earning its keep).

There's a catch in "the leader's truth", and a CI run found it for us. The rollback test upgrades a four-node cluster, then immediately asks the leader to roll it back. It got a 400: `address for node "n1" is "127.0.0.1:36685" but the cluster sees "127.0.0.1:45611"`. Port 45611 appeared nowhere in the logs. n1 had listened on 36685 before and after its upgrade. So where did 45611 come from?

From arithmetic. The leader upgrades last, so it had just restarted, and a restarted node learns its peers in two steps. First, some member sends it a membership sync, which says "n1 is alive at gossip port 57888". Later, n1's own gossip arrives, stamped with the API address n1 advertises. Between the two, the membership table fills the gap by shifting the gossip port by the leader's *own* gossip-to-API offset: 57888 + (41165 − 53442) = 45611. That guess is right on a production fleet, where every node uses the same ports, and wrong on any host running several nodes with independently picked ports. The validation compared the client's correct address with the guess and called the client a liar.

The fix keeps the guess where it's harmless and stops it being treated as an identity. `NodeMembershipInfo` now says whether its address was advertised, and the authoritative view carries `address: Option<String>`, built with `member.api_advertised.then(|| member.address.to_string())`. `derive_upgrade_nodes` refuses a node whose address it doesn't know yet with `PlanError::AddressNotAdvertised`, and `PlanError::is_transient` maps that to a 503 ("retry shortly") rather than a 400, because the request isn't wrong, only early. `/v1/cluster/nodes` stopped publishing guesses too, so relish reports "no advertised API endpoint" instead of quietly building a plan around one. Would accepting the client's address when we have nothing better have been simpler? Yes, and it would also undo the whole point of UPG2.

`cargo test --lib upgrade::orchestrator`. Next: the operator's steering wheel — `relish upgrade`.

## 14.10 Driving it from relish

Everything so far is machinery an operator never touches directly. The interface is six subcommands, and the design goal for all of them is the same: an upgrade is a scary operation, so the CLI should make the state of the world *legible* at every step.

```
relish upgrade check                   # anything newer than what I'm running?
relish upgrade plan v0.2.0             # what would happen, in what order?
relish upgrade start v0.2.0            # do it (network)
relish upgrade start --binary ./bun-v0.2.0   # do it (air-gapped)
relish upgrade status                  # where are we?
relish upgrade rollback v0.1.0         # undo it
relish upgrade resume                  # carry on after a pause
```

`check` fetches the **release metadata** — a static JSON file listing versions and per-platform artefacts (`{os}-{arch}` keys from `std::env::consts`, so the binary knows its own platform). The metadata is served over HTTPS but deliberately *not* signed: it can at worst advertise versions that don't exist, because nothing executes without the per-binary dual signatures from §14.2. Signing the metadata (TUF-style freshness guarantees) is real hardening we consciously deferred; the trust anchor is the binary signature, full stop.

`start` has two personalities. The network flow downloads the artefact, checks its hash against the metadata, and takes the signatures from the metadata entry. The air-gapped flow reads a local file and its `.sig` envelope. Either way relish then asks the connected node a question that shapes everything after: *are you a cluster?* (`GET /v1/upgrade/cluster` answers 503 on a single node.) Single node → build a `LocalFile` directive and POST it straight to §14.6's node-level endpoint. Cluster → push the binary as a content-addressed blob to the leader's Pickle registry (a plain monolithic Distribution-API upload — the same registry that serves container images happily serves *our own* binary, which has a pleasing circularity), then POST the plan to `/v1/upgrade/start` and let the orchestrator walk.

Notice what relish deliberately does **not** do: verify the signatures itself. It could — it embeds the same release keys — but the nodes *must* verify regardless (relish is outside their trust boundary), and a relish-side check would give integration tests signed with throwaway keys a false failure. One verification, in the place that matters.

relish still assembles a node list for the start request, but it's no longer the source of truth. The leader rebuilds each node's API address from its own gossip membership table (the address each node advertised over gossip, never a port-offset guess) and its role from the Raft voter set, then validates relish's list against that. So a stale or hand-edited list can't upgrade a node under a false identity — the leader corrects what it safely can and rejects what it can't (see "What the API gained"). relish's job shrinks to *which* nodes and *how many workers at once*; the leader owns *what those nodes are*.

`plan` and `status` are the legibility half. Both are pure functions from data to a string, which makes them perfect **snapshot test** material — `insta::assert_snapshot!(render_plan("v0.2.0", 5, 2, 1, 2))` stores the rendered output in a `.snap` file under version control, and any change to the wording shows up as a reviewable diff instead of a broken `assert_eq` on a multi-line string literal. (First time we've used insta in this book: the workflow is run the test, eyeball the generated `.snap.new`, accept it. The eyeballing is the point — an earlier draft's single-node plan promised a "leadership transfer" with nobody to transfer to, and the snapshot diff caught it.)

```
$ relish upgrade plan v0.2.0 --cluster-size 8 --parallel 2
upgrade plan to v0.2.0 (8 node(s))
  1. workers: 5 node(s) in 3 batch(es) of up to 2
  2. council members: 2 node(s), strictly one at a time
  3. the leader, in place (last)
estimated duration: ~5 min (assuming 45s per node)
workloads keep running throughout (adoption across exec)
```

`rollback` mirrors `start`'s cluster/single-node split, with one asymmetry worth explaining: on a single node the version is optional (the node knows its own previous version — §14.6 defaults to the newest installed version older than the running one), but a *cluster* rollback demands an explicit version, because the leader has no single answer to "previous" across a fleet that may have been paused mid-upgrade. Forcing the operator to name the target is friction in exactly the place friction belongs.

Tests: the render functions and address/role derivation under `cargo test --lib relish::upgrade`, metadata parsing under `upgrade::metadata`. Next, the dress rehearsal: a real multi-node cluster upgrading itself end to end.

## 14.11 The four-node dress rehearsal

`tests/self_upgrade_cluster.rs` is §14.7's harness scaled up: four real bun processes, each under its own supervisor loop, forming a real gossip+Raft cluster on localhost — node 0 bootstraps, the rest join, the council reconciler promotes voters, a leader emerges. Nothing is mocked below the HTTP API; the tests drive exactly the endpoints relish drives.

One honesty note up front. With four nodes and a council cap of seven, *every* node becomes a Raft voter — a genuine non-voter worker would need an eight-node harness, which is a lot of laptop for one assertion. So the test labels one voter "worker" in its start request, and the leader's server-side derivation *corrects* that to `Council` (both roles precede the leader, so it's a harmless relabel, not a rejection). The mechanics under test — batch-then-serial ordering, quorum-gated council steps, leader-last in-place upgrade — are untouched by the distinction, and the correction is exactly the behaviour a unit test pins directly.

**`rolling_upgrade_walks_workers_council_then_leader`** is the milestone test. Deploy a workload, push the signed blob to the leader's Pickle, POST the plan, and then just *watch* `/v1/upgrade/cluster` — any node can serve it, it's replicated — recording when each node first reports `Healthy`. The assertions read like the design doc: worker first; old leader last; the cluster still has a leader at the end; all four nodes report v0.2.0; and the app stays *reachable* across the whole roll. Note that last one is an **availability** assertion, not a same-pid one: unlike the single-node case (§14.7), a cluster's scheduler may legitimately reschedule an app while its host node bounces, so pid-identity is the wrong thing to demand here — "still serving, still has a running instance" is the honest cluster guarantee.

The milestone test caught its own subtle bug, in the *observer* rather than the machinery: the driver used to write `UpgradeClear` the moment a step returned `Completed` — without persisting that final step first. The archived history therefore showed a stale mid-walk snapshot in which the old leader never reached `Healthy`, and the test's ordering assertion rightly refused to believe the leader had finished. (The bun logs said otherwise, which is how we knew where to look.) The fix — persist the final state, *then* clear — is one of those changes that's obviously correct in hindsight and invisible until a test insists on reading history back.

**`upgrade_failure_pauses_cluster_and_reverts_node`** poisons only the worker's staged binary with the fail-boot sidecar. The worker crash-loops and reverts itself (§14.4); the leader notices and pauses; the other three nodes are asserted untouched. Then the test cures the binary, POSTs `resume`, and the run completes. Writing this test flushed out the best bug of the phase — a genuine distributed-systems livelock:

The worker's crash loop is *fast* (a couple of seconds); the leader polls every three. So the leader often never observes the upgrade in flight — from its chair, the node is `Directed` and silent. When the reverted node comes back, the leader's lost-directive retry helpfully re-sends the directive... and the node, whose marker was cleaned up by the completed revert, *accepts it again*. Crash loop, revert, re-send, forever. Two idempotency mechanisms, each locally correct, composing into a perpetual-motion machine.

The fix has three interlocking parts, and the pattern is worth keeping: **make failure a fact, not an inference.** Nodes remember the upgrade ids they've reverted (it's already in the history file) and refuse to re-attempt them — the loop is now impossible by construction. They advertise those ids in `/v1/version`, so the leader learns "I tried, it failed" as reported truth rather than guessing from version numbers. And `resume` renames the run (`up-…-retry`), because a retry the operator asked for must not look like the re-delivery the node is now rightly refusing. Unit tests pin each part; the integration test proves the composition.

**`cluster_rollback_returns_every_node_to_previous_version`** completes a full upgrade, then walks the whole fleet back to v0.1.0 through the same machinery in `Rollback` direction — no downloads, no distribution, just §14.3's retained binaries earning their keep at cluster scale.

These are the slowest tests in the repository — a couple of minutes each, serialised for the same starvation reasons as §14.7 — and the cheapest confidence per line in the whole phase. When someone asks whether the cluster can really upgrade itself, the answer is a test name.

One honest operational note: they run on a *real* machine (`make test-upgrade-cluster`), not in CI. Four real `bun` processes, each with its own Raft TCP server and gossip, need enough cores to converge; on a contended 2-core shared CI runner the membership-change RPC times out under load and the council never forms. That's a property of *four real processes competing for two cores*, not of the upgrade logic — the single-node real-binary suite (§14.7) does run in CI, and the cluster mechanics are exercised deterministically by the mock-driven `step` unit tests (§14.9). The full-process cluster test is the belt-and-braces layer you point at a dev cluster, not the one that gates every push.

What remains is bookkeeping: progress ticked, READMEs updated, and this chapter closed out with the lessons that only showed up in the doing.

## 14.12 Lessons learned

**The kernel does the hard part, if you let it.** The headline feature — workloads surviving the upgrade — is mostly a property of `exec()` we chose not to break. Children survive; memory doesn't. Everything we built (records, adoption, file-backed logs) exists to reconstruct *knowledge*, not to keep processes alive. Knowing precisely what a syscall preserves turned a scary feature into a bookkeeping exercise.

**Attach state to artefacts, not processes.** The chapter's recurring trap, in three costumes: the env-var version override (env survives exec — wrong version reported), piped logs (pipes die with their reader — SIGPIPE kills the workload), and in-memory upgrade state (memory dies at exec — no revert possible). The fix was the same every time: put it in a file next to the thing it describes. In a system where processes replace themselves, the filesystem is the only memory you can trust.

**Decide, then act.** The two scariest pieces — the startup rollback decision and the rolling-walk step — are (nearly) pure functions returning data, with thin imperative shells around the unrunnable parts (`execv`, real HTTP). Twenty-six unit tests cover logic that would otherwise need a crashed process or a live cluster per assertion. The shells stayed dumb enough that the eight real-binary integration tests could carry them.

**Idempotency by observation beats bookkeeping by memory.** The orchestrator re-polls reality (`/v1/version`) before every decision, so leader crashes, lost directives, and resumes all collapse into the same code path: look, then act. And where two idempotency mechanisms composed into a livelock (§14.11's revert/re-send loop), the cure was making failure a *reported fact* — nodes advertising "I tried that id and reverted it" — rather than something the leader infers from version numbers.

**Wire formats are one-way doors.** Half the design decisions in this phase (D5, D6, the don't-rename-`RaftRequest`-variants rule) were dictated by how each format evolves: bincode's index-positional gossip datagrams and the Raft log's name-tagged JSON impose *different* rules, but both bind precisely at the moment old and new binaries must talk. Those constraints were laid down long before upgrades existed. If your system will ever upgrade itself, mixed-version compatibility is a constraint on your *first* release, not your second.

**What we'd do differently.** A self-describing wire format for membership would have let version info ride along in gossip for free (instead of the per-node `/v1/version` poll), and the boot-attempt budget plus grace timers are wall-clock heuristics that a supervisor-integration API (sd_notify-style readiness) would make crisp. Both deferred with eyes open — each would have doubled the phase for a marginal improvement to a path that now has a wall of end-to-end tests standing on it.

The milestone holds: `relish upgrade start v0.2.0` rolls a live cluster onto a new binary — signatures checked twice, workloads never blinking, leadership handed over mid-flight, and a poisoned release walking itself back without a human in the loop. The cluster changes its own tyres at full speed.

Next, Phase 15: teaching Reliaburger to test, benchmark, and diagnose itself — `relish test`, `relish bench`, and the long-promised `relish wtf`.


## Release integration: one metadata document per executable

The upgrade reader already understands a version, a platform and a signed
binary. We keep that schema. `metadata.json` contains Bun, and
`cli-metadata.json` contains Relish. If both appeared in the same platform map,
an older Bun could select the wrong executable. Separate documents make that
mistake impossible without changing the existing reader.

The release workflow builds each platform natively, checks the complete set of
artefacts, and signs their raw bytes with the existing Ed25519 release identity.
It refuses to publish if the supplied private key doesn't match a trusted
public key in the source. Checksums help detect incomplete downloads; signatures
prove who approved the bytes. The distinction matters when metadata and binaries
travel through the same hosting service.

The packaging tests generate temporary keys, validate a real signature, corrupt
the binary, and check that verification then fails. They also reject missing
platforms and an untrusted key. The public release key stays out of the test
fixture. See [the release procedure](../releasing.md) for the operator steps and
remaining candidate-qualification gates.

### Shipping 0.1.0

A tag and a lockfile don't identify the compiler, so release builds pin Rust 1.98.0 and CI separately checks that the declared minimum, 1.97, builds every target with and without default features. Running the whole suite again on 1.97 would repeat the stable run, so we don't. Changing either is a reviewed build-policy change, followed by a new candidate with its own checksum. We never swap the bytes behind an existing tag.

The first release also needed a new signing identity: the development key's private half was gone. That was only possible because 0.1.0 starts with fresh clusters. After a supported release, swapping the public key would strand every installed node, which would reject our next binary. That's why `EMBEDDED_RELEASE_KEYS` is a slice rather than a single key: a future rotation ships a release trusting both old and new keys, then switches the signer, then drops the retired key a release later.

Adoption itself got stricter on the way. The original startup loop treated anything other than "yes, it's running" as a dead workload. If the runtime merely failed to answer, Bun deleted the instance record and swept its identity files while the process kept running. Adoption now returns `Result<usize, BunError>`: the count of adopted instances on success, and an error that stops startup before the API listener opens. Only an explicit "not running" from the runtime allows cleanup, and each runtime call has a ten-second deadline. Reading records follows the same rule: a missing directory is an empty inventory, but a malformed file, a symlink or a FIFO named `.json` is an error. Ten other journals and checkpoints had grown their own copies of that careful read, each with slightly different gaps, so they now all share one function in `src/durable.rs`:

```rust
let record: OwnerRecord = durable::read_json(&path, RECORD_LIMIT, Access::Regular)?;
```

Its signature is `read_json<T: DeserializeOwned>(...) -> io::Result<T>`. `DeserializeOwned` is serde's trait for types that can be built from bytes without borrowing from them, and the compiler picks `T` from the type annotation on the left: the same call returns an `OwnerRecord` here and a checkpoint somewhere else. `Access` says how private the file has to be. Startup also refuses records whose stored instance ID doesn't match their app, namespace and replica fields, rather than inventing a second name for a running workload and later failing to find it.

Finally, the upgrade test suite runs all of this with real binaries and real runc. It signs a copied Bun with a throwaway test key, upgrades and rolls back through the real API, and requires the workload to keep its instance ID, PID and host port through both execs, with its main command running exactly once. A poisoned candidate must revert on its own. A three-node variant upgrades each node, leader last, rolls them all back, and checks that every node still sees the service afterwards. The throwaway key and the failure trigger are test-only; release binaries contain neither.


## Two addresses and one version number

Planning the 0.1.0 soak test meant reading `relish upgrade start` as an operator would, from a laptop, against a quickstart cluster. Two things didn't survive the read.

### Where relish pushes isn't where nodes fetch

The cluster flow used to take one registry address, `{api host}:5050`, and use it twice: relish pushed the binary there, and the same string went into the start request as the place every node should download from. On a quickstart cluster the API host from the Mac is `127.0.0.1`, so relish pushed to `127.0.0.1:5050`. Nothing listens there. The registry forward sits on host port 15050. Pass `--registry 127.0.0.1:15050` and the push works, but now every node is told to fetch from *its own* loopback port 15050, which is empty. The same bug bit anyone running relish on a node against `https://127.0.0.1:9117`: four nodes, each fetching from itself, three of them finding nothing.

It's one address doing two jobs for two audiences. relish stands wherever the operator is; the nodes stand on the cluster network. So `start` now resolves a `RegistryRoute` with two fields:

```rust
pub struct RegistryRoute {
    /// Origin relish pushes to, `scheme://host:port`.
    pub push_origin: String,
    /// `host:port` the nodes fetch from.
    pub fetch_address: String,
}
```

The fetch address comes from the node relish is connected to. relish asks it for its capability report, *as the node reports it* (a managed connection normally swaps in its host forwards, so there's a new `capabilities_as_reported` that doesn't), which gives the node's id and its real registry listener, typically `https://0.0.0.0:5050`. A wildcard listener says nothing about how peers reach it, so relish looks the node up in cluster membership and pairs its gossip IP with the listener's port. The push origin is the connection's declared registry forward when there is one (quickstart writes `https://127.0.0.1:15050` into the local context), and otherwise the API host with that same port. `--registry` still names one address for both jobs, for anyone who wants to decide.

`resolve_registry_route` is a pure function from those facts to a route, so the unit tests read like a map of where relish might be standing: on the Mac through forwards, on a node through loopback, on a remote machine, and against a listener bound to a specific IPv6 address. The cluster suite adds `relish_pushes_through_a_forward_while_nodes_fetch_from_the_cluster_address`. It drives the real `relish::upgrade::start` through a one-shot TCP forward that closes after the push, so a node that tried to fetch through the forward would fail its download, and the walk would never complete.

### An upgrade that swaps nothing

The second gap is quieter. The orchestrator marks a node `Healthy` when `/v1/version` reports the target version, and the node's binary store names files by version. Now build a new bun without bumping the version and `relish upgrade start --binary` it. Every node already reports the target, so the walk marks them all `Healthy` on the first poll and reports success. Nothing was swapped. On a single node it was worse: `prepare` staged the new bytes over `bun-v0.1.0`, the very file the node was running and would revert to.

The fix works at three layers, because each one can be reached without the others:

- **The binary store** refuses to put different bytes under an existing version (`VersionContentConflict`). Identical bytes are accepted and left alone.
- **Nodes report what they run.** `/v1/version` gains `binary_sha256`. Hashing a whole bun is CPU work, so the manager does it once, on the blocking pool, and caches it in a `tokio::sync::OnceCell`. `OnceCell::get_or_try_init` takes an async closure; the first caller runs it and everyone else awaits the same result. If the closure fails, the cell stays empty and the next caller tries again, which is what you want for an I/O error.
- **One pure gate, `upgrade::plan::check_target`,** compares the target with what every node runs. Same version and different (or unknown) bytes: `SameVersionDifferentBinary`, which tells the operator to give the candidate a new version. Same version and identical bytes everywhere: `TargetCheck::AlreadyRunning`, and relish prints "nothing to do" and exits cleanly. The leader runs the gate in `/v1/upgrade/start` after probing every planned node, before anything goes into Raft. Each node runs it again in `prepare`. And the orchestrator re-checks the digest when a node reports the target version, so a node that changed underneath the walk fails the run instead of passing it.

The API handler maps "already running" to a 200 rather than an error, with a match arm that binds and filters at once:

```rust
Ok(Err(crate::bun::BunError::Upgrade(
    error @ crate::upgrade::UpgradeError::AlreadyRunning { .. },
))) => ...
```

`name @ pattern` is a Rust binding: it matches only if the value fits the pattern on the right, and then gives you the whole matched value under `name`. Go and C have nothing like it; you'd match the variant and then re-borrow the value. Here it lets us render `error` into the response body without destructuring its fields.

### Downgrades need asking for

Nothing used to refuse moving to an *older* version with `upgrade start`. That matters because semver sorts pre-releases before their release: `0.1.0-soak.1 < 0.1.0`. The soak's private build is a downgrade by that ordering, and so is any accidentally older binary. We decided a downgrade through `start` should be deliberate, so it now needs `--allow-downgrade`. The flag rides in the start request, into the replicated `ClusterUpgradeState`, and into every node's directive, because each node checks for itself. Both new fields are `#[serde(default)]`, so state recorded by an older leader still reads back.

`relish upgrade rollback VERSION` is unchanged and needs no flag. It returns to a binary that is already on every node's disk and was verified when it arrived, which is a different operation from installing a new one. So the soak runs `relish upgrade start --binary bun-v0.1.0-soak.1 --allow-downgrade` to move onto the soak build, and `relish upgrade rollback v0.1.0` to come back.

The real-binary suites grew two more tests. `same_version_upgrade_never_swaps_silently` posts both kinds of same-version directive to a live node and checks the running file's bytes afterwards. `start_refuses_same_version_other_bytes_and_unrequested_downgrades` does the same through relish against the four-node cluster and asserts that no upgrade was ever recorded.

### Scoped admins stay in their lane

While we were in these handlers, a separate review of token scopes found that every mutating `/v1/upgrade/*` route checked for the Admin *role* and stopped there. An Admin token scoped to one namespace (`relish token create --namespaces team-a`) could therefore start a cluster-wide upgrade, which replaces the binary under every tenant. The upgrade handlers, and `/v1/cluster/elect` for the same reason, now go through one helper:

```rust
fn authorize_cluster_admin(
    auth: Option<&crate::sesame::auth::AuthContext>,
) -> Result<(), Response> {
    crate::sesame::auth::authorize(auth, crate::sesame::types::ApiRole::Admin)?;
    crate::sesame::auth::require_unscoped(auth)
}
```

The `?` after the first call returns its error response early, so the function reads as the two rules it enforces, in order. The service token still passes, which matters: it's what the orchestrator presents when it directs each node. A unit test posts to all six routes with a scoped Admin and expects 403, another checks an unscoped Admin gets through, and a source-scanning test in `bun::authz` fails if any of those handlers stops calling the helper.

## A pause with no way out

The V02 soak found the next hole on its first night. The harness ran `relish upgrade start --binary …` against a cluster whose nodes had no `upgrades.external_signing_key`. The leader accepted the plan and recorded it in Raft. Then the first node refused its directive with a 409, "network upgrades require upgrades.external_signing_key in node.toml", and the run paused, exactly as §14.9 says it should.

It stayed paused for twelve hours. `resume` would only repeat the refusal. Every later `relish upgrade start` got "an upgrade is already in progress", and so did `relish upgrade rollback v0.1.0`, because both handlers refused to touch the active slot while anything sat in it. The pause that was meant to hand control back to the operator had taken it away. Nothing but hand-editing Raft state could clear it.

Two changes fix it, one on each side of the pause.

### Don't record a run the nodes will refuse

Every cluster directive fetches the binary from Pickle, so every node treats it as a network upgrade and demands the operator's external signature and a key to check it with. The leader can know that before it writes anything. `/v1/version` now reports `accepts_network_upgrades` (true when the node has an external key), the start handler reads it in the same probe that already fetches each node's version and digest, and a pure gate in `upgrade::plan` decides:

```rust
pub fn check_network_prerequisites(
    external_signature: Option<&str>,
    nodes: &[NetworkReadiness],
) -> Result<(), UpgradeError> {
    if external_signature.is_none_or(str::is_empty) {
        return Err(UpgradeError::ExternalSignatureRequired);
    }
    let unready: Vec<&str> = nodes
        .iter()
        .filter(|node| node.accepts_network_upgrades == Some(false))
        .map(|node| node.node.as_str())
        .collect();
    ...
}
```

`Option::is_none_or` is true for `None`, and otherwise asks the closure about the value inside, so one call covers "no signature" and "an empty one". `str::is_empty` is passed as a function rather than written as a closure: any function with the right signature works where a closure is expected. `accepts_network_upgrades` is a plain `bool`. Our first draft made it an `Option<bool>` so a node too old to report it wouldn't block a start, but there are no older nodes: every 0.1.0 binary reports the field, and we don't carry compatibility shims for development builds. So the probe reads a missing field as `false` (`value["accepts_network_upgrades"].as_bool().unwrap_or(false)`), and a node with no upgrade manager at all says `false` outright. A node that can't tell us it will accept is a node we don't record a run for.

The probe used to return one `Vec`. It now builds a pair per node and splits them with `Iterator::unzip`, which turns an iterator of `(A, B)` into an `(Vec<A>, Vec<B>)` in one pass. So a start that would have paused on its first node now fails straight away with "node n1 cannot accept a cluster upgrade: set upgrades.external_signing_key in node.toml on every node first", and nothing reaches Raft.

### A way out of a pause

A pre-check can't catch everything. A node can still refuse for its own reasons, or crash-loop and revert. So the pause needs exits, and there are now three:

- `relish upgrade resume` retries, as before.
- `relish upgrade abort` (new, `POST /v1/upgrade/abort`) ends the run and leaves every node where it is.
- `relish upgrade rollback <version>` now *replaces* a paused run instead of being refused by it.

Abort is only safe when no node moved. `orchestrator::abort` is another pure function over the replicated state. It refuses a run that isn't `Paused`, and it refuses one where any node is `Healthy` (on the target), `Directed` or `Verifying` (told to swap, and maybe still swapping). Dropping the plan then would leave those nodes on a different version with nothing tracking them. The refusal names them and points at `rollback`. Failed, rolled-back and never-directed nodes are all on their old binary, so for everything else, abort really is "as if we never started".

A rollback doesn't need that condition, because it walks every node to its own target, moved or not. Its handler now asks `orchestrator::supersede` whether the active run may be replaced (only a paused one may) and archives it before recording the rollback. The archive happens *after* the rollback plan has been validated, so a malformed request leaves the paused run where it was.

Both paths end the old run the same way. We added a phase:

```rust
pub enum ClusterUpgradePhase {
    // ...
    Paused { reason: String },
    Aborted { reason: String },
}
```

It goes last for the reason the `RaftRequest` comment spells out: the Raft log is bincode, which writes an enum variant as its index, so inserting a variant in the middle would make every stored entry after it decode as its neighbour. The handler writes the run with its `Aborted` phase, then clears it into history, so `relish upgrade status` shows what happened to it instead of a bare "paused". Those are two Raft writes. If the leader dies between them, the orchestrator loop finds an `Aborted` run in the active slot and archives it on its next tick, the same recovery it already had for `Completed`.

Two small things changed on the way. The 409 for a start or rollback against a paused run now names the run and all three exits rather than "an upgrade is already in progress". And `abort` goes through `authorize_cluster_admin` like every other upgrade route, which the source-scanning test in `bun::authz` now checks too.

The tests follow the layers. `plan::tests` covers the gate (no signature, empty signature, nodes that refuse), and `orchestrator::tests` checks that a `/v1/version` without the field probes as "can't accept". The same module covers abort on a clean pause, refusal for each of the three "moved" phases, refusal when not paused, supersede over a node that did move, and a `step` that leaves an aborted run alone. The cluster suite gets two real-binary tests. `start_refuses_when_a_node_cannot_verify_network_upgrades` boots two nodes, one without an external key, and checks that `relish upgrade start` fails naming that node with nothing recorded. `paused_upgrade_can_be_aborted_or_replaced_by_a_rollback` poisons a worker's binary so the run pauses, aborts it through relish, starts again (accepted, now that the slot is free), lets it pause a second time, and replaces that one with `relish upgrade rollback v0.1.0`, which completes.

Running the whole upgrade suite twice in a row turned up a leak of our own. A single-node test deploys a workload on a fixed port, and workloads run under detached process owners precisely so they survive Bun's `exec`. They survived the test too. The next run found port 46071 already serving and its own instance never appeared. The harness now kills every process whose command line names its temporary directory, both in `shutdown` and in a `Drop` implementation. `Drop` is Rust's destructor: the compiler calls `drop(&mut self)` when a value goes out of scope, including while a panic unwinds the stack, so a failed assertion can't skip the cleanup the way it skips a `shutdown().await` at the end of the test.

## One blip is not a refusal

The V02 soak's next finding came from a chaos step, not a misconfiguration. Mid-walk, the harness SIGKILLed the leader's Bun. Node 3 had already upgraded. systemd brought the leader back three seconds later, and within the same second its orchestrator (the state lives in Raft, so the restart is just a resume) sent node 2 its directive. Node 2 asked the leader's Pickle registry for the binary. The registry wasn't listening yet: in the journal, "Pickle registry listening" comes eight lines *after* the pause. The fetch failed with "error sending request", node 2 answered 409 like any other refusal, and the orchestrator did what §14.9 told it to on a refusal. It paused. Ten minutes later the harness gave up and rolled back.

Nothing was wrong with the binary, the signatures or node 2. The registry was simply three seconds late. So the question is: which failures mean "no", and which mean "not right now"?

### Two kinds of failure, in the types

"No" is anything that will give the same answer next time: a hash or signature that doesn't verify, a missing external key, a version the policy refuses, a registry that answers 404 because it doesn't hold the blob. "Not right now" is anything about reachability: a connection refused or reset, a body cut off halfway, a 5xx, a 408 or a 429. One helper, `upgrade::is_transient_status`, draws that line for HTTP statuses, and both sides of the directive use it.

On the node, `fetch_binary` used to return `FetchFailed` for everything. It now has a sibling variant, `FetchUnavailable`, and `UpgradeError::is_transient()` is a one-line `matches!` over it. The node rides out an unavailable source itself for a short budget (10 s, backing off from 500 ms), because the fetch runs while the agent holds its command loop and the orchestrator is waiting on the HTTP answer. If the source is still down after that, the API answers **503** instead of 409.

Holding the command loop also means a registry that *accepts* the connection and then says nothing is worse than one that refuses it. reqwest has no timeout by default, so the first version of the retry would have waited on that registry forever, with the whole agent stuck behind it. Each attempt now runs under `tokio::time::timeout`: 5 s to connect and get the response headers, 60 s for the whole attempt, body included (plenty for a ~100 MB binary on a LAN), and 75 s for the whole fetch, retries and backoff included. `timeout` wraps any future and returns `Err(Elapsed)` if the deadline passes first, dropping the inner future. In Rust, dropping a future cancels it, so the half-read connection is closed as well. A timeout counts as `FetchUnavailable`, because a registry that hangs is still a "not right now". A guard on a match arm does it:

```rust
Ok(Err(crate::bun::BunError::Upgrade(error))) if error.is_transient() => (
    StatusCode::SERVICE_UNAVAILABLE,
    Json(serde_json::json!({ "error": error.to_string() })),
)
    .into_response(),
Ok(Err(e)) => (StatusCode::CONFLICT, /* … */).into_response(),
```

The `if` after the pattern is a *match guard*: the arm only matches when the pattern fits *and* the condition holds, otherwise matching falls through to the next arm. Order matters, so the more specific arm goes first.

On the leader, `NodeControl::direct_upgrade` used to return `Result<(), String>`. A `String` can't tell you whether to retry without someone parsing it, which is exactly the stringly-typed API the project guide warns about. It now returns a two-variant error:

```rust
pub enum DirectiveError {
    Transient(String),
    Refused(String),
}
```

A Go programmer would reach for a sentinel error and `errors.Is`. The Rust version is stronger in one specific way: the orchestrator `match`es on the result, and the compiler refuses to build it until both variants have an arm. Nobody can add a third kind of failure later and forget to decide what the walk does with it. A failure to reach the node at all is `Transient` too, since a node that is itself restarting looks just like that.

### Retrying without losing your place

A transient failure leaves the node `Pending` and fills in a new `directive_retry` field on its record: attempts so far, when the first one failed, when the last one did, and what it said. Because that record lives in Raft, a leader that changes mid-retry carries on with the same count and the same window rather than starting over. The orchestrator re-sends when the backoff has passed (3 s, doubling, capped at 30 s) and gives up after `DIRECTIVE_RETRY_WINDOW`, two minutes from the first failure. Only then does the node go `Failed`, with a reason that says how long it tried, and the run pauses as before. A refusal skips all of that and pauses on the spot.

The subtle part is the concurrency budget. A council member waiting out its backoff still *holds its slot*. If it didn't, the next tick would see a free slot and direct the next council member, and the walk would quietly reorder itself around a node that is owed its turn. So pass 2 takes the slot before it even looks at the backoff:

```rust
slots -= 1;
if record
    .directive_retry
    .as_ref()
    .is_some_and(|retry| !retry_due(retry, context.now))
{
    continue;
}
```

`as_ref()` turns an `&Option<DirectiveRetry>` into an `Option<&DirectiveRetry>`, so we can look inside without moving the value out of the record, and `is_some_and` is `false` for `None` and the closure's answer for `Some`. Writing this turned up an older bug in the same loop. A refused directive marked the node `Failed` but didn't use up its slot, so with `parallel = 2` the loop went on to direct the *next* worker in the same tick, past the failure that was about to pause the run. A refusal now ends pass 2, the same rule pass 1 already applied.

`set_phase` clears `directive_retry` on every transition out of `Pending`, and `resume` clears it too, so a resumed run gets a fresh two minutes. The new field changes what the Raft log stores, and the 503 changes what a directive can answer, so `compatibility::CURRENT` moved to protocol 27 and state 43.

### What we decided not to do

We thought about letting a node fetch the blob from *any* Pickle node rather than the one address in the directive. It would have dodged this particular outage. It isn't simple, though. `relish upgrade start` pushes the binary to one registry, and nothing guarantees the other nodes hold that raw blob by the time the walk reaches them. A node would also need a list of peer registries it doesn't have today. The retry fixes the failure we actually saw, a registry that is late, and a registry that is *gone* is still a pause the operator should see.

We also didn't make the orchestrator wait for its own registry after a restart. The registry in the directive needn't be the leader's, and a retry covers that case and every other kind of blip with one mechanism.

### Tests

`orchestrator::tests` scripts the mock node's answers. `transient_directive_failure_keeps_the_node_pending_and_retries` walks the clock through two transient failures, checks that no attempt happens inside a backoff and that the third one succeeds. `transient_failures_past_the_retry_window_pause_the_run` ticks every three seconds for two minutes (between four and ten attempts, never paused) and then checks the pause names the last error. `refused_directive_pauses_at_once_and_starts_no_sibling` pins the refusal path, including the sibling bug. `a_node_retrying_holds_its_place_in_the_rolling_order` checks the slot. Three more point the real `HttpNodeControl` at a canned 503, a canned 409 and a closed port.

In `manager::tests`, `flaky_registry` is a tiny TCP server that follows a script (hang up, answer a status, or serve the blob) and counts requests. `prepare_rides_out_a_registry_that_is_briefly_unavailable` gets a hang-up, then a 503, then the blob, and stages it on the third request. A 404 fails after exactly one request and isn't transient. A registry that never comes back is reported transient with nothing staged. So is one that accepts and never answers, or stalls halfway through the body, and `a_hanging_registry_is_a_transient_failure_within_the_ceiling` checks that it gives up within the ceiling instead of hanging. Finally, and bytes that don't verify aren't transient even though they came over the network. An API test checks the 503/409 split end to end.

The cluster suite gets `a_registry_outage_at_directive_time_does_not_pause_the_upgrade`. It puts a TCP proxy in front of the leader's registry that hangs up on everything for the first 25 seconds, longer than a node's own 10 s budget, so the orchestrator has to re-send. It points the upgrade at the proxy and requires the run to reach `Completed` with every node on v0.2.0, and requires that the outage actually turned fetches away. Otherwise the test would prove nothing.
