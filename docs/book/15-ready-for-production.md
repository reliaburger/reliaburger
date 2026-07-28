# Ready for Production

We had more than two thousand tests. One of the CI jobs still took ten minutes, a
three-node cluster occasionally contained two nodes, and a failed run left `sleep`
processes behind for GitHub to kill. That's a useful reminder: a test count isn't a quality
metric. It is, at best, an inventory.

This chapter is about making the inventory honest.

## What does a green test mean?

Consider this tempting pattern:

```rust
#[tokio::test]
async fn btrfs_snapshot_round_trips() {
    if std::env::var("RUN_BTRFS_TESTS").is_err() {
        return;
    }

    // The actual test.
}
```

`#[tokio::test]` is an *attribute macro*. An attribute is metadata written above an item;
the `tokio::test` macro transforms this asynchronous function into a normal Rust test with
a Tokio runtime. The function returns successfully when the variable is absent, so Cargo
prints a dot. Nothing involving Btrfs happened. Green, but useless.

Rust gives us two honest ways to express the distinction.

```rust
#[cfg(target_os = "linux")]
#[test]
fn linux_path_uses_mount_namespaces() {
    // ...
}

#[test]
#[ignore = "requires Linux root; run by make test-linux"]
fn btrfs_snapshot_round_trips() {
    assert!(nix::unistd::geteuid().is_root(), "this suite requires root");
    // ...
}
```

`#[cfg(...)]` is conditional compilation. On macOS the compiler doesn't build the first
function at all. This is the right answer when the types, system calls or feature simply do
not exist on that target. It is *compiled out*, not passed and not ignored.

`#[ignore]` keeps the second test in the test binary but excludes it from an ordinary run.
The reason appears in source, and `make test-linux` selects ignored tests explicitly. The
assertion is a preflight. If somebody asks for the privileged suite without providing root,
we fail with an explanation. We don't smile politely and do nothing.

These states need separate numbers:

- executed tests made their assertions;
- ignored tests compiled but belong to another named suite;
- platform-specific tests did not compile on this target;
- benchmarks measured performance and are not correctness tests.

Adding them together produces an impressive number with no stable meaning. So we stopped.

## One test, one promise

Some tests were duplicates of pure scheduler unit tests. Others checked that a GPU struct
field could be read after Rust had already type-checked the field access. An identity
"test" printed a demonstration without asserting production behaviour. Deleting those
tests reduced the headline and improved the suite.

That sounds backwards. It isn't.

A useful test protects a promise that a caller can observe. The new Relish black-box tests
run the compiled binary and inspect its exit status, standard output and standard error.
They prove, amongst other things, that `apply --dry-run` does not need an agent and that a
missing input file fails on stderr. Unit tests of Clap parsing still help, but they can't
prove that `main` maps an error to the right process exit code.

Names must describe that promise. A test called `health_check` used to deploy an app and
assert only that it appeared in status. Now the health tests wait for the relevant state
transition. A log-follow test used to treat a timeout as success. Now the expected line must
arrive before the bounded deadline. The GitOps webhook test had a subtler problem: it sent a
webhook for the initial commit, but the sync loop always processed that commit on startup.
The test passed even if webhooks were broken. The replacement waits for the initial sync,
creates a second commit while the timer is an hour away, sends the webhook, and observes the
new app. Same general shape. Completely different evidence.

## Stop sleeping and observe the event

This is a race disguised as patience:

```rust
start_three_nodes().await;
tokio::time::sleep(Duration::from_secs(3)).await;
assert_eq!(members().await.len(), 3);
```

Three seconds is wasteful on a fast machine and insufficient on a slow one. CI managed to
demonstrate both sides, which was considerate of it.

For mocks we can expose the event directly. Tokio provides several useful synchronisation
types:

- `Notify` wakes a task when an event occurs;
- `Barrier` releases a known number of tasks together;
- `watch` stores the latest value and wakes receivers when it changes;
- `mpsc` carries every message to one consumer;
- a semaphore represents a finite number of permits.

The mock Grill now blocks `create` on a semaphore. A test waits until the mock reports that
creation started, sends another command, proves the command loop still responds, and then
releases creation. No three-second guess. The proxy uses notifications to hold an HTTP
request open while it inspects drain state. TCP reporting waits for a watch value to change.
Council replication tests repeatedly inspect the replicated state with a 20 ms bounded
predicate rather than sleeping for half a second.

Why retain any timeouts? Because a missing event must eventually fail instead of hanging the
runner forever. A timeout is a safety boundary around an observation. A sleep is the
observation. That's the difference.

For pure clock-driven code, Tokio can do even better:

```rust
#[tokio::test(start_paused = true)]
async fn retry_happens_after_backoff() {
    let task = tokio::spawn(run_retry_loop());
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(attempts(), 2);
    task.abort();
}
```

The paused clock advances logical time without waiting for the wall clock. This works only
when every relevant operation uses Tokio time. Process exits, TCP stacks and Raft elections
involve the real world, so their acceptance tests keep real deadlines in `make test-slow` or
the cluster suite. We haven't weakened those assertions merely to make the quick suite look
quick.

## A harness owns what it starts

Spawning a task transfers ownership of its captured values into that task. Tokio returns a
`JoinHandle`, which represents ownership of the running computation. Dropping the handle
detaches the task; it does not stop it. The same idea applies to `std::process::Child`.

Our old harnesses often kept a cancellation token but discarded the join handle:

```rust
tokio::spawn(async move { agent.run().await });
```

If an assertion panicked, background work and ProcessGrill children could outlive the test.
The primary harness now stores the handles. Its `Drop` implementation cancels the tasks,
waits for a bounded grace period so the agent can stop its children, then aborts only if
shutdown did not complete. Reporting tests join listeners, aggregators and workers on both
success and failure paths. Ports come from binding `127.0.0.1:0`, which asks the operating
system for an unused port, and every filesystem fixture lives in a temporary directory.

This matters more with nextest. Cargo's built-in runner executes the tests in one test binary
in a process. Nextest runs each test in its own process and schedules tests from different
binaries concurrently. Isolation exposes assumptions about global ports, shared paths and
orphan processes very quickly. Good. Those assumptions were bugs waiting for a second
worktree.

The nextest configuration defines small serial resource groups for host networking,
clusters, upgrades and child-process-heavy tests. It also records JUnit output, reports slow
tests and terminates a hung test after a bounded interval. Retries are zero. Automatically
rerunning a flaky distributed test makes the dashboard greener while preserving the race.
We're trying to remove the race.

## Alive isn't ready

An HTTP listener can answer while the agent command loop is dead. It can also answer while
DNS, Raft or the registry has fallen over. Returning `{"status":"ok"}` in those cases tells
the truth about the process and lies about the node. Those are two different questions, so
we now give them two different endpoints.

`GET /v1/health` remains the public liveness probe. If it answers, a process manager knows
the binary is alive and accepting HTTP. Authenticated callers use `/v1/readiness` for the
stronger claim. It returns 200 only when every critical subsystem is `Ready`, and 503 with
the evidence when any of them is still `Starting`, has become `Degraded`, or has `Stopped`.
Each record includes when its state changed and the latest error and error time. No more
searching the logs to discover that a green node lost half its control plane.
The combined capability report names the node and gives the snapshot a 15-second expiry;
after that, a diagnostic must call the state unknown rather than replaying an old green.

The four states are a Rust `enum`:

```rust
pub enum SubsystemState {
    Starting,
    Ready,
    Degraded,
    Stopped,
}
```

An enum is better than four booleans because only one variant can exist at a time, and a
`match` must consider every variant. The tracker sits behind an `Arc<RwLock<_>>`. `Arc`
gives the API, agent and task wrappers shared ownership; Tokio's asynchronous `RwLock`
allows concurrent readers while serialising a transition. We copy a snapshot out before
returning it, so a slow HTTP client never holds the lock.

Readiness also travels to the leader in its own reporting message. Why another message?
Reliaburger uses positional bincode frames between nodes. Adding a field to an existing
frame would make an older binary run out of bytes while decoding it. Appending a new enum
variant preserves every existing discriminant. An old node may reject the new extension,
but it still decodes the ordinary report. The leader treats a missing or expired readiness
extension as unready. Rolling compatibility doesn't mean optimistic guessing.

There is one more trap here: “supervise” doesn't automatically mean “restart”. A gossip
task owns a UDP socket. A report worker owns the receiving end of a channel. Starting a
second copy may fail to bind, steal messages or leave two authorities alive. Those owners
never auto-restart. Bun records their death and fences scheduling.

The security-state refresher is different. Its factory owns only cloneable handles and can
recreate its timer from scratch, so Bun may reconstruct it. Even then the policy names a
maximum retry count, a delay, a recovery window and a shutdown deadline. The factory takes
a child cancellation token for each attempt. Rust's ownership rules help us state the real
question: can this closure build a completely new owner without borrowing the dead one? If
the answer isn't obviously yes, we don't restart it.

The scheduler consumes the same evidence under an independent receive-time lease. A fresh
metrics report can't keep stale readiness alive, and a leader change starts with no inherited
lease. Until the node proves every critical owner again, it receives no new work. Briefly
under-scheduling is inconvenient. Scheduling onto a node whose control plane is half dead is
worse.

## Correctness is not a benchmark

One test transferred 100 MB over the P2P path and asserted that it finished in under five
seconds. On what CPU? With what filesystem load? Was the debug build warm? The test answered
none of those questions, but it could block a patch because somebody else's runner was
busy.

The correctness replacement uses several small, deterministic layers and asserts that every
layer arrives through parallel fetches. Criterion owns the performance question. Criterion
runs a function repeatedly, warms it up, samples its distribution and stores results under
`target/criterion`. The fast and large gossip benchmarks now share one seeded simulation.
Setup and convergence are timed separately, and failure to converge is an error rather than
a magic duration.

The 10,000-member check asks a different question. Can one real Mustard node hold the full
membership table, ingest it through fixed-size protocol messages, choose a peer and
disseminate every update in bounded batches? Running 10,000 complete nodes in one process
creates 100 million membership records. That's a single-machine stress test masquerading as
a distributed-systems result (and it didn't finish inside 90 minutes). Full convergence
remains covered through 1,000 real in-memory nodes; the 10k tier checks the per-node scale
invariant honestly.

We upload the data in CI but don't enforce a percentage regression yet. Hosted runners are
noisy. Once the measurements settle on comparable hardware, a threshold will mean
something. Until then it would be another confident number with weak evidence. We've had
enough of those.

## A test that proves the door is locked

Here's a bug that a passing test hid for a while. Bun can run a workload as a plain host
process, not just a container. You tell it a binary path and it runs it. Obviously you don't
want any config you deploy to be able to run *any* binary on the node, so there's an
allowlist in `node.toml`: `[process_workloads] allowed_binaries`. Only those run.

The allowlist worked. The tests passed. And yet an empty allowlist allowed everything.

The check was `self.allowed_binaries.is_empty() || self.allowed_binaries.contains(binary)`.
Read it as English: "allowed if the list is empty, or if the binary is in it." The empty-list
case was meant as a convenience default, but it inverts the security posture. A fresh node
with no `[process_workloads]` section would happily run whatever host binary a deploy named.
The supervisor never even received the parsed config, so *every* production node ran the
permissive default. The design doc had promised "deny-by-default" on page one. The code did
the opposite, and the tests agreed with the code.

The fix is one line of logic and a change of mind. Deny by default:

```rust
pub fn is_binary_allowed(&self, binary: &std::path::Path) -> bool {
    self.allowed_binaries.iter().any(|b| b == binary)
}
```

An empty list now matches nothing. A binary runs only if an operator named it. The same rule
extends to inline scripts, because a script is just host execution of `/bin/sh` -- so the
shell has to be allowlisted too, or the script is refused like any un-listed binary.

The interesting part is the test. It isn't enough to assert that an allowlisted binary runs;
that was already true. The test that earns its keep asserts the *refusal*, and asserts that
the policy came from config rather than a built-in default:

```rust
#[tokio::test]
async fn host_exec_not_allowlisted_is_refused() {
    let mut sup = test_supervisor();               // no allowlist configured
    let spec = exec_app_spec("/usr/bin/python3");
    let err = sup.deploy_app("job", "default", &spec, Instant::now())
        .await.unwrap_err();
    assert!(matches!(err, BunError::DeployFailed { .. }));
    assert_eq!(sup.list_instances().len(), 0, "nothing must be created");
}
```

Two assertions, two promises: the deploy is refused, and nothing was created as a side effect
before the refusal. A companion test allowlists the same binary and asserts it runs, so we
know the gate opens as well as closes. This is the shape of every good security test in the
suite. Don't just prove the happy path works. Prove the door is locked when it should be, and
prove it's the operator's key that opens it.

The same admission gate refuses two other silent lies while we're here: a workload asking for
a GPU on a node that has none (or has `gpu_enabled = false`), and a workload asking for
cpu/memory limits on a rootless node that can't enforce them. In each case the old behaviour
was to accept the work and quietly deliver less than asked. The new behaviour is a clear
error. A refusal you can see beats a guarantee you can't.

## The gate that runs on real hardware

A unit test is only as honest as its fixture. If you invent the shape of the world and then
assert your code handles that shape, all you've proved is that your code agrees with your
imagination. Two bugs slipped through exactly this way, and the thing that caught them was a
final acceptance gate that ran the whole programme against the real world before we called it
done.

The first was a chaos fault that did nothing. Reliaburger can inject a DNS NXDOMAIN fault, so
you can test how your app behaves when a dependency stops resolving. The fault wrote its
target into an eBPF map. The unit tests wrote to that map and read it back, and everything
agreed. But DNS resolution had long since moved to a userspace resolver, and that resolver
never looked at the map. So the fault wrote a note nobody would ever read. Every test passed;
the feature was a placebo. The fix moved the fault into the userspace resolver, where DNS
actually happens, and the test now drives a real query through the resolver and asserts it
comes back NXDOMAIN. Ask the thing that answers, not the thing that used to.

The second only showed up on an actual Mac. Apple's `container inspect` tells you whether a
container is running, and after a self-upgrade Bun re-adopts its surviving workloads by asking
exactly that. The parser had been written against a guessed JSON shape -- `State.Status`, an
object -- and the fixtures matched the guess, so the unit tests were green. The real `container`
CLI returns an *array*, and puts the status at a lowercase top-level `status`. On real
hardware the parser matched none of its paths, decided a running container was "unknown", and
declined to adopt it. No fixture could have caught this, because the fixture *was* the bug.
Only `make test-apple`, run on Apple silicon, exercised the real CLI -- so that's where it
surfaced, and where we captured the true schema and pinned the fixtures to it.

Neither bug was subtle once you saw it. Both were invisible from inside the test suite,
because both suites tested a model of the world rather than the world. That's the whole reason
the acceptance gate exists: run the portable suite, then the cluster and upgrade suites
in-process, then the privileged Linux suite in CI, then the Apple suite on a real Mac, and
only *then* believe the green. A passing test earns trust in proportion to how much of the
real world it touched.

## Coverage is a map, not a target

`cargo-llvm-cov` instruments the compiled programme and records which source regions execute.
Our `make coverage` runs both the default feature set and `--no-default-features`, merges the
profiles, and emits LCOV plus an HTML report. The first combined Linux CI measurement covered
79.65% of lines, so CI starts at 78.65%, one percentage point lower, and can ratchet upwards.

Coverage finds unvisited code. It does not tell us whether an assertion is useful, whether a
webhook test accidentally exercised startup, or whether a five-second performance limit is
portable. The audit found all three in a suite with lots of coverage. Read the uncovered
lines, but read the covered tests too.

## Examples are tests when we run them

Twenty-one configuration files sat under `examples/`. The Make target labelled itself a
dry-run, then called `relish apply` without `--dry-run`, discarded both output streams and
reported every file as broken because no agent was running. It managed to test the absence
of Bun 21 times. Two configs really were stale, but the useful errors went into the same
bin.

The repaired target builds Relish once, sends every file through the real parser, validator
and planner, and keeps a failed command's diagnostic. Successful plans stay quiet. That
makes `make examples` cheap enough for every pull request, and it catches the same drift a
reader would hit after copying an example.

We apply the same rule to platform promises. The `ebpf` Cargo feature can be selected on
macOS even though Aya and the kernel hooks exist only on Linux. An Aya-using branch therefore
needs both conditions:

```rust
#[cfg(all(feature = "ebpf", target_os = "linux"))]
mod maps;
```

You have met `#[cfg]` already. `all(a, b)` is its Boolean AND: the compiler includes the
item only when both predicates are true. The matching fallback uses
`not(all(...))`, so an all-feature macOS build still gets the unsupported stub. Hosted macOS
now runs the all-target, all-feature Clippy command. That's a compile-time check of the
boundary, not a hopeful comment saying the code is portable.

## Dependencies are code too

The lockfile is part of the programme. A perfectly tested call into a vulnerable archive
parser remains a vulnerable call, and an optional crate can sit in `Cargo.lock` without
appearing in the compiled graph. We need both facts. `cargo audit` compares every locked
crate with the current RustSec database; `cargo tree -i package` walks backwards from a
package to show why it exists.

That distinction mattered immediately. `tar` handled real image and build archives, so we
patched it. `quick-xml` arrived through the cloud object-store client and parsed remote
responses, so we upgraded `object_store` and migrated its changed Rust API. `quinn-proto`
was locked behind reqwest's optional HTTP/3 support, but `cargo tree --target all` found no
enabled path. We patched it anyway. Cheap uncertainty is still uncertainty.

Two findings had no clean compatible fix. Parquet brings in an unpatched Thrift allocation
issue, and ratatui brings in an `lru::IterMut` soundness issue. The latter affected method
isn't called by ratatui's layout cache. The former can parse a crafted Parquet object when an
operator points the remote log-query command at it, so trusted storage is a compensating
control, not a fix. We wrote both decisions down, named an owner and gave them an expiry.

The repository audit denies every new vulnerability and maintenance warning. Its short
exception list expires on 18 August 2026, and the Make target fails after that date until we
review it. CI runs the audit on changes and releases; a weekly job catches a new advisory
even when nobody changes the source. An exception without an owner and an alarm is just a
quieter way to forget a problem.

Now a green portable run means something narrow and valuable: the portable behaviour ran,
without retries, on this machine. The privileged, cluster, upgrade, slow and benchmark jobs
make their own claims. Smaller claims. Better evidence.

## Asking the cluster what it can do

Everything above is about *our* tests — the ones that run in CI, against code, before it
ships. The rest of this chapter is about a different animal: tests that run against a
cluster that's already up, from the outside, as a user.

`relish test` deploys real workloads onto a real cluster and checks they behave. That
raises a question CI never had to answer. Our CI knows exactly what it built. A running
cluster is whatever the operator configured — eBPF on or off, ingress bound or not,
a council or a single node. So what does a test do when the thing it tests isn't there?

There are three states, and they're easy to conflate:

1. The subsystem works.
2. The subsystem is switched off.
3. The subsystem is broken.

Guessing from responses collapses all three. A 404 from `/v1/metrics` looks identical
whether Mayo was never configured or has fallen over. Get that wrong in a test runner and
you produce the two worst outcomes available: a failure that isn't one (noise, which
teaches people to ignore red), or a pass that isn't one (a hollow green, which is the very
thing this chapter opened by complaining about).

So the cluster tells you. `GET /v1/capabilities`:

```json
{
  "version": "0.1.0",
  "environment": "staging",
  "container_runtime": "runc",
  "cluster": true,
  "node_count": 3,
  "metrics": true,
  "council": true,
  "ebpf": false,
  "ingress": true,
  ...
}
```

A test that needs eBPF sees `"ebpf": false` and reports **skipped, with the reason**,
which is honest in a way that neither red nor green would be.

### Derived, never asserted

The whole value of this endpoint is that it's true, so not one field is a literal:

```rust
let wired = WiredSubsystems {
    metrics: state.mayo.is_some(),
    logs: state.log_store.is_some(),
    council: state.council.is_some(),
    // …
};
```

`ApiState` carries `Option<Arc<RwLock<MayoStore>>>` and friends. That `Option` isn't
defensive coding — it's the wiring itself. A node built without metrics has `None` there,
and no amount of configuration can make it `Some`. Reporting `is_some()` reports what was
built. This is a small illustration of something Rust does well: the type already
encodes "might not exist", so the capability report is a rename of information the
program was carrying anyway. In a language where everything is nullable, you'd be
maintaining a separate registry of what's switched on, and it would drift.

Two fields resisted the pattern, and both are worth the detour.

**eBPF** is configured *and* observed. `[ebpf] enabled = true` says the operator wants it;
whether the programs actually loaded and attached is a different fact, and a node that
tried and failed logs a warning and carries on without enforcement. So the capability is
`ebpf.is_attached()` at load time, not the config flag. Reporting intent as achievement is
exactly the lie this endpoint exists to prevent.

**Fault injection** was going to be `true`, because the Smoker API is always mounted. Then
it's not information — a caller learns nothing from a field that's always the same. What
they actually want to know is whether any fault can *do* something, and that varies:
cgroup faults need Linux, network faults need eBPF, node-level faults need a cluster plane
to disturb. So:

```rust
fault_injection: statics.cgroup_faults || statics.ebpf || cluster,
```

If a field would always be `true`, it isn't a capability. Either derive it from something
real or delete it.

### Policy decides whether we're allowed to break things

`[cluster] environment` remains useful descriptive metadata. It is a poor
authorisation boundary. It's free-form, absent by default and easy to misspell.
The first proposal treated an untagged cluster as non-production and let a
client-side `--override` bypass the check. Both failures point towards doing
more damage.

The capabilities response now carries the server's typed `[testing]` policy.
Its safety class defaults to `unknown`, and unknown is protected. Each operation
class has its own allowlist entry and minimum authenticated role. Protected
mutation needs a second server-side gate. A confirmation flag can record the
operator's acknowledgement, but it cannot add a permission the server didn't
grant. This is an interlock, not a warning label.

## A workload in your pocket

`relish test` needs something to deploy. The obvious answer is a public image
— `nginx`, or a hello-world container — and it's the wrong one twice over.

It makes the test suite depend on the internet, so a registry outage becomes a
failing cluster test and everyone learns to distrust the result. And it decouples
the workload from the orchestrator: you'd be testing whatever `nginx:latest`
means today against whatever `bun` means today, and when the pair stops working
you get to find out which moved.

So the test workload ships *inside* the orchestrator. `bun testapp` runs a small
HTTP server that every node already has, because every node already has `bun`.
Version-locked by construction, no registry involved.

It's a hand-rolled TCP server rather than axum, which looks like the wrong call
until you remember where it runs: inside a container, in its own network
namespace, on a node under deliberate stress. It should have no opinions and no
dependencies.

### The bind address is not a detail

The original bound `127.0.0.1`. As an in-process test fixture that's exactly
right — nothing else should reach it.

As a *workload*, it's fatal. A container gets its own network namespace, so
loopback inside the container is not loopback on the node. The agent's health
check would connect to the node's own loopback, find nothing, and mark a
perfectly healthy app dead. Worse, it would do so consistently, which reads as a
real bug in health checking.

Binding `0.0.0.0` fixes it and costs nothing: it accepts on every interface,
loopback included, so the in-process tests are unaffected. When code moves from
"test fixture" to "thing that runs in production shapes", its assumptions about
the network are the first thing to re-examine.

### Two paths that ignore the mode

The app's `TestAppMode` is its identity: a `Hang` app hangs, an `UnhealthyAfter`
app starts failing on cue. Every path gets the same treatment, which is what
makes the modes useful.

Two paths break the rule, because tools need them whatever behaviour is being
simulated:

```rust
pub fn special_route_response(path: &str) -> Option<String> {
    // /payload?bytes=N  → exactly N bytes, for throughput measurement
    // /env/NAME         → the variable's value, or 404
}
```

`/env/NAME` earns its place. Chapter 4 decrypts `ENC[AGE:...]` secrets at
container start — but how does a *test* prove the workload got the plaintext?
Asking the API is circular: it tells you what it believes it did. Reading the
variable from inside the process is the workload's own testimony, which is the
only evidence that counts.

`Option<String>` is doing the routing here, and it reads nicely: `Some` means
"this is a special path, here's the response", `None` means "not mine — let the
mode decide". No sentinel, no flag, no separate `is_special_path()` that could
disagree with the handler.

There's one exception to the exception. `Hang` hangs on the special paths too,
because a hanging app that helpfully answers `/payload` isn't hanging, and a
test that relies on it hanging would quietly stop testing anything.

### A size on the wire is a claim

`/payload?bytes=N` takes its size from the query string. That's the same shape as
the Raft frame reader in chapter 4: a number from a stranger, used to decide how
much memory to allocate. So it gets the same treatment — clamped to a ceiling,
not honoured.

It would be easy to argue this one doesn't matter. It's a test app; who would
attack it? But it runs as a real workload on real clusters, sometimes on the
same node as real work, and "who would attack it" is a question with a poor
track record. The bound is one `.min()` call.

### One parser, two front doors

`bun testapp` and the standalone `testapp` binary run identical code, and they
used to have identical-looking `match` statements over the mode string. Note the
tense: they had *drifted*. The library grew an `exit-after` mode; the standalone
binary's parser never learned about it, so passing `--mode exit-after` there
printed "unknown mode" for a mode that existed.

Duplicated logic doesn't stay duplicated, it diverges — and the divergence is
invisible until someone uses the path you forgot. Both now call one
`parse_mode`, and its error message lists the valid modes from a single place,
so the next mode can only be added once.

## A test framework in four types

`relish test` needs to describe a run: which cases exist, what each one did,
and what the whole thing amounts to. Four types carry that, and two of them
teach something about Rust.

### An outcome with data attached

The tempting shape is a boolean, or `Result<(), Error>`. Both are wrong here.
A case still has four top-level states, but a timeout is not a fifth verdict:

```rust
pub enum TestOutcome {
    Pass,
    Fail { reason: String },
    Skipped { capability: Capability, reason: String },
    Unknown { kind: UnknownKind, reason: String },
}
```

This is a Rust enum — a *sum type*, not the integer constants C calls an enum.
Each variant can carry different data. `Fail` explains the assertion,
`Skipped` names the capability which was proven absent, and `Unknown` says why
the runner couldn't establish a verdict.
Coming from Go you'd model this as `(bool, error)` and rely on a convention
about which combinations are legal; coming from Python you'd raise different
exception classes and hope every caller catches the right ones. Here the
illegal states can't be written down: there is no `Pass` with a failure
message.

`Skipped` only means one thing: fresh capability evidence says the requested
facility isn't available. A timeout isn't a skip. A collector error isn't a
skip. A case deciding at runtime that it would rather not run isn't a skip
either. Those are all `Unknown`, because we don't have enough evidence to say
pass or fail. This sounds fussy until a production gate turns green because
the API was down. Then it sounds obvious.

Cleanup gets an independent outcome (`Confirmed`, `NotRequired`, `Failed` or
`Unknown`). A case can pass its assertion and still leave a workload running.
Keeping cleanup outside `TestOutcome` records both facts instead of letting
one overwrite the other.

The serde attributes matter for the same reason:

```rust
#[serde(rename_all = "snake_case", tag = "status")]
```

`tag = "status"` produces `{"status": "fail", "reason": "..."}` rather than
serde's default nesting. It's the shape a `jq` one-liner in someone's CI
expects, and once shipped it's an API — hence `schema_version` on the report
and a snapshot test pinning the whole thing.

### Counters that can't lie

`TestReport` carries `total`, `passed`, `failed`, `skipped` and `unknown`
alongside the full results list. Two representations of the same facts is an invitation for
them to disagree, and a summary line contradicting the list beneath it
destroys trust in both.

So the counters are *derived* in one place, from the results, at construction:

```rust
let passed = results.iter().filter(|r| r.outcome == TestOutcome::Pass).count();
```

Not incremented as tests finish. Incrementing works right up until an early
return or a `?` skips one, and then the arithmetic is quietly wrong forever.
A test asserts the four parts sum to the whole.

### Why a test case can't just be an async fn

Here's a piece of Rust that surprises people arriving from Go, where you'd
write `[]func(ctx) error` and move on.

```rust
pub type TestFn = fn(TestContext)
    -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
```

That's a lot of machinery for "a list of test functions". Every layer is
load-bearing.

An `async fn` isn't really a function that runs — it's a function that
*returns a future*, and the compiler generates an anonymous type for that
future, unique to each `async fn`. Two async functions with identical
signatures return two different types. So there is no `fn` type they can share
and no way to put them in a `Vec` directly.

The escape is a trait object: `dyn Future`, which erases the concrete type and
keeps only the behaviour. But a trait object has no size known at compile time,
so it must live behind a pointer — hence `Box`.

And `Pin` is the one with a real story. A future generated from an `async fn`
can hold references *into itself*: if you write `let x = something(); foo(&x).await;`
the generated state machine has a field for `x` and a field pointing at `x`.
Move that struct in memory and the pointer dangles. `Pin` is the type-level
promise that it won't move once polled. Rust makes you say this out loud;
languages with a garbage collector and heap-allocated coroutines never have to
because everything is already behind a pointer.

We wrap it in a small macro so the catalogue stays readable:

```rust
TestCase {
    name: "schedule_fixed_replicas_across_nodes",
    group: TestGroup::Scheduling,
    requires: &[Capability::Cluster, Capability::MultiNode],
    run: testkit_case!(schedule_fixed_replicas_across_nodes),
}
```

`requires` is the graceful-skip mechanism from earlier in this chapter, in its
final form: the runner compares it against `/v1/capabilities` *before* running
the body, so an unsupported case is skipped without side effects rather than
failing halfway through setup.

### A prefix as a safety net

One more small thing with a large consequence. Every namespace the runner
creates is `rbtest-{run}-{seq}`, and teardown checks the prefix before
stopping anything:

```rust
pub fn is_test_namespace(namespace: &str) -> bool {
    namespace.strip_prefix(TEST_NAMESPACE_PREFIX)
        .is_some_and(|rest| rest.starts_with('-'))
}
```

Note the second condition. `starts_with("rbtest")` alone would match
`rbtestingground`, which might be somebody's real namespace — and the worst
thing this tool could do is stop an operator's apps because a name looked
similar. Requiring the separator makes the match exact in the way that
matters. The check is a named function with its own test rather than an
implication of how the name was built, because "we construct them correctly so
they'll always match" is an assumption, and this is not a place for
assumptions.

## Running them all without a stampede

We have a catalogue of cases and a way to select from it. Now something has to
actually run them. That something is the runner, and it exists so a case body
never has to. A case is a plain `async fn` that applies some config and asserts.
Three concerns that would otherwise clutter every one of them live in the runner
instead: how many run at once, what happens when one hangs, and who cleans up.

Start with concurrency. Forty cases against a three-node cluster, all deploying
apps at the same instant, is not a test — it's a load spike that makes every
case flaky. So the runner caps how many run at once with a semaphore:

```rust
let semaphore = Arc::new(Semaphore::new(config.parallel.max(1)));
```

A semaphore is a counter with a waiting room. `parallel` permits go in; a task
that wants to run takes one and gives it back when it finishes; a task that
finds none waits. The subtle part is *where* you take the permit. Acquire it
before spawning and you've bounded how many tasks you queue, which is not the
same thing at all — you'd spawn all forty and they'd all start. Acquire it
*inside* the spawned task and you've bounded how many actually run:

```rust
set.spawn(async move {
    let _permit = semaphore.acquire().await.expect("never closed");
    run_one(&case, client, namespace, capabilities.as_ref(), timeout).await
});
```

The `_permit` is held for the case's lifetime and dropped when the task ends,
which is what hands the permit to the next waiter. There's no explicit release
call — the `Drop` does it — which is the same ownership story as a mutex guard,
one of the places where Rust's "resources are freed when their owner goes out of
scope" rule quietly does the bookkeeping a Go `defer` would do by hand.

Next, hanging. A health check that never returns, a deploy that never
completes — a case can wedge, and one wedged case must not take the run with it.
Every case gets one absolute deadline. Polls, API calls and assertions inherit
the remaining budget; they don't each start a fresh two-minute timeout:

```rust
match deadline.run("case", body).await {
    Ok(Ok(Ok(())))      => TestOutcome::Pass,
    Ok(Ok(Err(reason))) => TestOutcome::Fail { reason },
    Ok(Err(panic))      => TestOutcome::Unknown {
        kind: UnknownKind::Panicked,
        reason: panic.to_string(),
    },
    Err(_)              => TestOutcome::Unknown {
        kind: UnknownKind::TimedOut,
        reason: "case exceeded its deadline".into(),
    },
}
```

The case body runs in a nested Tokio task. That detail is easy to miss. If the
outer task owns both the body and cleanup, a panic kills the owner before it
can clean anything. The nested task turns the panic into evidence and leaves
the owner alive.

Then teardown, which is the whole reason this is safe to point at a real
cluster. It runs after every case:

```rust
let cleanup = context.teardown(cleanup_deadline).await;
```

Note "after every case" — pass, fail *or* timeout. It's tempting to only clean
up after a pass, but that's exactly backwards: the case that failed halfway is
the one that left a workload running. Teardown is the runner's job precisely so
a case body can `return Err(...)` the moment something's wrong without a pile of
cleanup code first. Teardown then asks `/v1/status` until the resources have
actually gone. We record timeout, API failure and a workload which remains
present as cleanup evidence. Discarding the result with `let _ =` would make
the happy path shorter and the report less true.

Two smaller decisions round it out. Cases finish whenever they finish — a
250-millisecond case beats a 30-second one to the join — but a report where the
rows jump around between runs is a report nobody trusts. So each case carries
its catalogue index, and the results are sorted back into order at the end.
There are two panic boundaries. The nested body catches a case panic while the
owner still has its context for cleanup. The outer join set keeps a second
identity map in case the runner itself panics:

```rust
let mut identities: HashMap<tokio::task::Id, (usize, String, TestGroup)> = ...;
```

An outer task which panics can't return its name and index, so we record them
out here, keyed by task id, and look them up when the join comes back as an
`Err`. One mishandled case shouldn't blank a forty-case run.

One thing the runner deliberately does *not* do is pause the clock. Elsewhere in
this book we drove time with `tokio::test(start_paused = true)` to make a
health-check test instant. The runner spawns tasks, and a spawned task can
advance the virtual clock out from under the code driving it — a trap we've hit
before in this codebase. So the runner is timed against the real clock, and its
own tests use small real durations: a 50-millisecond timeout against a case that
sleeps for 30 seconds proves the timeout fires without making anyone wait.

## A green result needs a profile

Should a missing eBPF capability fail the run? On a developer's Mac, no. On the
rootful-runc acceptance job which promised to test the Linux data plane, yes.
The result alone can't answer that question, so the report records one of four
profiles: `development`, `full-runc`, `full-apple` or `process-grill`.

The development profile may accept a typed skip for a facility the node proved
absent. A full profile marks the cases it requires. A required skip, any
`Unknown`, missing observed evidence, or failed/unknown cleanup makes the run
non-zero. We keep the skipped row as `Skipped`; we don't rewrite history as
`Fail` just because the profile rejects the run.

Safety has a similar split. The old proposal used a free-form
`[cluster].environment` and let `--override` weaken a production check from the
client. A typo such as `prodution` was enough to make the cluster look safe.
Now `node.toml` owns a typed policy:

```toml
[testing]
safety_class = "development"
allowed_operations = [
  "read_diagnostics",
  "provision_isolated_workloads",
]
max_lease_seconds = 900
```

The default safety class is `unknown`, and unknown is protected. The server
checks the authenticated role, the operation allowlist, protected-cluster
policy and explicit acknowledgement. Acknowledgement records consent; it
doesn't grant permission. There is deliberately no `relish test --override`.

## The exit code is the message

`relish test` is the operator's door into all of this. It builds a client, asks
the node `/v1/capabilities`, selects the cases that fit, runs them, and prints a
report. The interesting design decision is what it *returns*.

Every other Relish command returns `Result<(), RelishError>`, and the binary
maps it the obvious way: `Ok` is exit 0, `Err` prints to stderr and exits 1.
That's fine for `apply` or `stop` — either it worked or it didn't. But a test
run has a third state. "The suite ran and everything passed" and "the suite ran
and two cases failed" are *both* `Ok` as far as the tool is concerned: nothing
went wrong with `relish test` itself. Yet CI has to tell them apart, because the
whole point of running the suite in a pipeline is to fail the pipeline when a
case fails.

So the diagnostic commands return a small enum instead:

```rust
pub enum CommandOutcome {
    Clean,     // ran fine, nothing wrong — exit 0
    Problems,  // ran fine, found failures — exit 1
    Warnings,  // ran fine, only warnings — exit 2
}
```

`relish test` maps a report rejected by its profile to `Problems` and a clean
one to `Clean`. That includes assertion failures, unknown evidence, required
skips and unconfirmed cleanup. (`Warnings` is for `wtf`, later — a cluster
that's degraded but not broken.) The binary keeps a second little function
next to the original `finish`:

```rust
fn finish_outcome(result: Result<CommandOutcome, RelishError>) -> ExitCode {
    match result {
        Ok(outcome) => ExitCode::from(outcome.exit_code()),
        Err(e) => { eprintln!("error: {e}"); ExitCode::FAILURE }
    }
}
```

Read the two arms carefully, because they encode the distinction. An `Err` is
still a *tool* failure — the agent was unreachable, a flag was malformed — and
exits 1 with a message. An `Ok(Problems)` is the tool succeeding and reporting
bad news, and *also* exits 1, but silently, because the report already said
everything. Same exit code, two very different events, and the code says which
is which. This is the sort of thing Rust's enums make pleasant: the states are
named, the `match` is exhaustive, and there's no magic integer floating around
that a reader has to decode.

Two smaller choices round out the command. Selection: `--filter
scheduling,firewall` parses to a list of groups and picks those; no filter runs
everything. And the human report renders as plain aligned text with word labels
rather than colour:

```
running 4 tests against a 3-node cluster

scheduling
  PASS  schedule_fixed_replicas_across_nodes  (120 ms)
  SKIP  schedule_respects_required_placement_label  (0 ms)  requires multi_node
service-discovery
  FAIL  resolve_returns_vip_and_healthy_backends  (80 ms)  expected 2 backends, saw 1
health-checks
  UNKN  hanging_health_check_marks_instance_unhealthy  (120000 ms)  case exceeded its deadline

4 tests: 1 passed, 1 failed, 1 skipped, 1 unknown  (120.0s)
```

No colour crate, no terminal detection — a report that reads identically in a
terminal and in a CI log is one fewer thing to reason about, and `--output json`
is there for anything that wants to parse rather than read. The renderer is a
pure `&TestReport -> String`, so it snapshot-tests against a fixed report
without a cluster anywhere in sight.

## What the tests actually deploy

An empty runner runs cleanly and proves nothing. Now it needs cases, and a case
needs a workload to deploy. That workload turned out to be the most interesting
decision in the whole chapter, because it forced a question we'd been able to
dodge: *where does `relish test` actually run, and what can it run there?*

The obvious answer was the `testapp` binary we've used in unit tests all along —
a tiny server with modes (`healthy`, `unhealthy-after`, `hang`, `slow`) that let
a case provoke exactly the behaviour it wants to observe. But `relish dev`
creates a cluster running the **runc** container runtime, and runc wants an OCI
image. Handing it `testapp` as a loose binary doesn't work. Packaging `testapp`
as an image is a real project of its own: the dev nodes have no image builder,
their registry is bound to loopback, and `testapp` is dynamically linked, so a
one-binary scratch image won't even start under runc.

The way out was already sitting on every node. `testapp` is a subcommand of
`bun` (`bun testapp --mode healthy --port 8080` runs the identical server), and
`bun` is installed at `/usr/local/bin/bun` on every dev node. So instead of an
image, the harness deploys a **process workload** — a plain command — and lets
the node run its own `bun`:

```toml
[app.web]
command = ["/usr/local/bin/bun", "testapp", "--mode", "healthy", "--port", "40817"]
```

That needs the cluster to run the *process* runtime rather than runc, so
`relish dev create` grew a `--runtime` flag (`runc` still the default). This is
the honest engineering trade: a process-runtime cluster isn't bit-for-bit what
production runs, but it exercises the scheduler, health checks, deploys, service
discovery and jobs — all the things the suite is actually testing — without a
container-image supply chain the harness has no business owning. `testapp_spec`
builds that TOML, deriving a per-app port (an FNV hash of the name) so two apps
in one case don't fight over a socket.

### One way to skip

A case that needs the process runtime shouldn't *fail* on a runc cluster — it
should skip, the same way a case needing eBPF skips where eBPF is off. That's the
`requires` list from earlier, and every `testapp` case lists
`Capability::ProcessRuntime`. The runner checks it before the case runs.

The first implementation also let a case skip itself after it started. That
was convenient and ambiguous, so the current helper names the state honestly:

```rust
let Some((node, key, value)) = labelled_node else {
    return unknown("no node advertises a label to target");
};
```

Was the label genuinely absent, was the collector stale, or did the API fail?
A string can't prove which. The runner records
`Unknown(MissingEvidence)`. If a case needs a dynamic prerequisite, the
capability API must report it with fresh evidence before the case starts.

### Status is node-local

One assumption in the cases bit back immediately. `/v1/status` reports the
instances *this node* runs, not the cluster's. Deploy an app with three replicas
across three nodes and ask any one node — it sees one. There's no cluster-wide
instance list endpoint; the reporting tree aggregates to the leader internally
but doesn't expose the raw list.

So a case that reasons about placement fans out itself. `node_clients()` builds a
`BunClient` for every node — each node's API address is its gossip IP with the
entry node's API port, since every `bun` serves its API on the same port — and
`cluster_instances(app)` gathers the app's instances from all of them. The
"three replicas on at least two nodes" case counts how many nodes report a
running replica; the health-check cases, whose single replica could be anywhere,
wait on the cluster-wide view rather than the local one. It's more code than a
single GET, but it matches reality: a distributed system's state is distributed,
and a test that forgets that is testing one node while claiming to test a
cluster.

### The cases are the tests

The ordinary catalogue is now 39 cases across 13 groups: scheduling, service
discovery, deployments, health checks, secrets and config, firewall, workload
identity, ingress, volumes, process workloads, jobs, image registry and cluster
coordination. Each name is a behaviour sentence
(`rolling_deploy_keeps_the_app_running`, `failing_job_retries_then_fails`).
They run against a live cluster, so they can't run in `make ci`; what `make ci`
checks is the scaffolding around them — unique names, group coverage, typed
requirements, valid generated TOML, verdict aggregation and cleanup behaviour.
The cases themselves earn their keep at the acceptance milestone, on a real
cluster. That split — unit-test the harness, acceptance-test the cluster — is
the same honesty this chapter opened with: a green check should mean something
specific, and never more than it can back up.

## The line the workload draws

Part A was scheduling, deploys, health and jobs — all of which a *process*
workload exercises perfectly well. Part B is the rest of the catalogue, and here
the choice we made in Part A shows its edge.

Some part-B groups are about the **control plane**, and the control plane doesn't
care what runtime it's on. Service discovery is a good example: deploy two
replicas, ask `/v1/resolve/{app}`, and check the VIP has two healthy backends;
scale to three and watch the backend list grow; stop the app and watch it leave.
That's the userspace service map answering questions about registrations — no
container required. Cluster-coordination is the same shape: every node reports
alive, the council has a leader, every member answers `/v1/health` directly.
And workload identity is API-level auth: the JWKS endpoint serves a well-formed
signing key, and a token scoped to one namespace is refused when it tries to
write to another. Mint the token, point a second client at it, watch the write
bounce with a 403. None of that needs a container either.

But the other part-B groups — firewall, ingress, volumes, mounted secrets,
image-registry deploys — are exactly the ones that *do*. Firewall enforcement
lives in eBPF, which needs a container network namespace. A managed volume is a
bind mount into a container's root; a process workload has no mount namespace to
bind into, so it just writes to the host. Ingress needs the proxy bound and a
real listener behind it. These aren't test-harness problems; they're the honest
consequence of running workloads as host processes. The same decision that let
Part A run cleanly is the one that keeps these particular cases from running on a
process cluster.

So the catalogue gates them the way it gates everything else — on capabilities —
and they'll skip where the capability is absent, ready to run the day a
container-workload path exists. It would have been easy to write them anyway and
let them quietly pass against a process runtime that isn't actually enforcing
anything. That's the failure mode this whole chapter is about: a green that
didn't test what it claims. Better a labelled skip and a note in the plan than a
check that lies.

## The flags that were already wired

Before we can build a chaos *suite*, the `relish fault` command it drives has to
actually be honest, and it wasn't quite. This is a different flavour of the same
problem: not a test that lies, but a CLI that quietly does less than it says.

The clearest case: `FaultRequest` — the struct sent to the agent — has carried
`target_instance`, `target_node`, `reason` and `override_safety` fields for a
long time, and the agent honours them. But every one of the fifteen CLI handlers
built the request with those fields hardcoded to `None`/`false`. The plumbing ran
from the agent all the way up to one layer below the command line, and stopped.
So `relish fault kill redis --instance redis-1` didn't exist; you could kill the
service but not one instance of it, even though the agent knew how.

Wiring them through is the kind of change that's easy to do badly — four extra
parameters on fifteen functions, sixty chances to fat-finger a `None`. So instead
of threading four arguments everywhere, the flags became one struct that clap
*flattens* into each subcommand:

```rust
#[derive(clap::Args, Clone, Default)]
pub struct FaultTargeting {
    #[arg(long)] pub instance: Option<String>,
    #[arg(long)] pub node: Option<String>,
    #[arg(long)] pub reason: Option<String>,
    #[arg(long)] pub override_safety: bool,
}
```

`#[command(flatten)]` splices those four flags into every fault subcommand as if
they'd been declared inline, and one `make_request` helper reads them into the
request. Add a flag once, get it on all fifteen commands, and the handlers shrink
to almost nothing.

A few smaller honesty fixes rode along. `relish fault dns redis banana` used to
inject NXDOMAIN regardless of what you typed after the service name — the
positional was read into a variable named `_fault_type` and thrown away. Now an
unknown type is rejected before anything is sent. `relish fault clear` learned to
take a service name as well as a numeric id (a bare number is an id, anything
else is a service), which finally gave the registry's `clear_by_service` — dead
code waiting for a caller — its first one, via a new `?service=` query on the
clear endpoint. `fault run` became an alias for `fault scenario`, because the
docs called it `run` and the binary shipped `scenario`. And `fault resume` got a
CLI path at last: the `Resume` fault type existed and the agent implemented it,
but nothing on the command line could construct it, so a paused service could
only be un-paused by clearing the fault, not by resuming it.

Last, a rounding bug that made a diagnostic lie. The bandwidth parser correctly
reads `1mbps` as one megabit per second — 125,000 bytes/s. But the `Display` that
echoes a fault back divided bytes/s by 1024², so it printed `bandwidth 0mbps` for
exactly the throttle you'd just set. The number was right on the wire and wrong
on the screen, which is the worst place for it, because the screen is how you
check your work. Inverting the parser's own arithmetic — `bytes_per_sec * 8 /
1_000_000` — makes the echo match the input. None of these are big changes. They
just move the tool a little closer to meaning what it says, which is the whole
job before we start breaking things on purpose.

## Every fault must expire

There's one rail that matters more than the rest: a fault has to end. A chaos
experiment that outlives the person who started it isn't chaos engineering, it's
just a broken cluster. The code already had a backstop — `FaultRule::new` clamps
any duration to a hard 24-hour ceiling — but 24 hours is a safety net, not a
policy. The real limit an operator wants is "faults auto-expire in ten minutes
unless I say otherwise, and nobody gets to inject a day-long one by accident."

That's two numbers, and they belong in config:

```toml
[smoker]
default_duration_secs = 600   # applied when a fault names no duration
max_duration_secs = 3600      # a fault asking for longer is rejected
```

The decision worth dwelling on is *where* to enforce them. The CLI already
defaulted a missing `--duration` to ten minutes, so it would have been easy to
call it done there. But the CLI is one client among possible many — a script
that POSTs straight to `/v1/fault` skips it entirely. A limit that only the
friendly front door respects isn't a limit. So the enforcement lives in the
agent, in the inject handler, where every fault converges regardless of how it
arrived:

```rust
match effective_duration(request.duration, request.fault_type.is_instantaneous(), &self.smoker_config) {
    Ok(effective) => request.duration = effective,
    Err(reason)   => { respond(FaultRejected { reason }); return; }
}
```

`effective_duration` is a pure function — requested duration in, effective
duration or a rejection out — so it unit-tests without an agent at all. Three
rules: an instantaneous fault (a kill, a resume) has no duration to bound and
passes through; a zero duration means "unspecified" and becomes the configured
default; anything over the maximum is rejected with *both* numbers in the
message, because "too long" without saying "the limit is 3600s" just makes the
operator go read the config. And underneath it, the 24-hour clamp still stands
as the backstop for an absurd config — a maximum of 48 hours is still an
experiment that ends within a day. Layered limits: a policy you set, and a floor
you can't remove.

## The other half of the catalogue

Part B's control-plane groups ran on a process cluster because they don't touch
a container. The rest — firewall, ingress, volumes, mounted config — do, and we
left them deferred with a note. Now we come back for them, and the question is
the same one that shaped Part A: what workload, and where does it come from?

The Part A answer (run `testapp` as a host process via the node's `bun`) is no
help here, because the whole point of these cases is the container: a volume is a
mount into a container's root, `allow_from` is enforced by eBPF in a container's
network namespace, ingress routes to a container listening behind the proxy. A
process has none of that. So these cases need a *real* image on a runc cluster.

Building one turned out to be a rabbit hole — the dev nodes have no image
builder, their registry is loopback-only, and our own binaries are dynamically
linked, so a hand-rolled image is a project in itself. The way out was to stop
building anything. `busybox` is a two-megabyte public image that every container
runtime can pull, and it carries `sh`, `httpd` and `wget` — exactly the three
tools these cases need. A volume case runs `busybox sleep infinity` and `exec`s a
shell into it to write and read a file. A firewall case runs `busybox httpd` as
the target and `wget`s it from another container. An ingress case puts `httpd`
behind the proxy and sends it an HTTP request with the right `Host` header. No
image to build, no registry to push to — just `image = "busybox:latest"` and a
capability gate:

```rust
Capability::ContainerRuntime => self.container_runtime != "process",
```

That gate is the mirror of `ProcessRuntime` from Part A, and together they draw
the honest line: the `testapp` cases run on a process cluster and skip on runc;
the `busybox` cases run on runc and skip on a process cluster. Full coverage
means two acceptance runs, one per runtime — which is exactly right, because the
two runtimes really are different environments and a test suite that pretended
otherwise would be hiding the seam, not testing it.

Some cases still skip even on runc, and they say so honestly. Firewall
enforcement needs eBPF, off by default on a dev cluster, so those cases require
`Capability::Ebpf` and skip without it. Two of the three secrets cases need to
*encrypt* a value with the cluster's age public key — and there's no API that
hands it out, so they `skip` from inside their bodies with that exact reason
rather than pretend. The config-file case, which only needs to mount a file and
read it back, runs. One group is still missing entirely: image-registry, whose
cases push a synthetic OCI image to the node's loopback registry, needs the
harness to speak the raw `/v2` protocol from *on* a node — a genuinely different
piece of plumbing, left for its own day. Twelve of thirteen groups, and the
thirteenth's absence is written down rather than papered over.

## The thirteenth group

That last group — image-registry — is the one we said needed its own day. The
day came. Its cases push an image to the cluster's Pickle registry and check it
comes back, and the reason it was awkward is worth stating plainly: the harness
has no image to push and no push method to call. `BunClient` can *list* images
but not upload one, and the dev registry binds to loopback, reachable only from
on a node. So the harness has to become, briefly, a registry client.

Building the image is the interesting half. An OCI image is not a magic format;
it's three blobs and a bit of JSON. A **config** blob describing the platform, a
**layer** — a gzipped tar of a filesystem, here a single marker file — and a
**manifest** that names the other two by their SHA-256 digests and sizes. The
digest *is* the identity: content-addressed, so the same bytes always produce
the same `sha256:...` name, which is exactly what lets a round-trip test assert
"what I pulled is what I pushed" by comparing one string. Constructing all this
is pure code with no cluster in sight, so it unit-tests directly — the digests
are well-formed, the manifest references the blobs, the same salt gives the same
digest and a different salt a different one.

Pushing it is the raw `/v2` dance the OCI distribution spec defines and buildah
speaks: POST to open an upload and read back a `Location`, PATCH the bytes to
that location, PUT with `?digest=` to seal it — once per blob — then PUT the
manifest under a tag. Reliaburger's own registry answers this protocol (that's
how `buildah push` works against it), so the harness just plays the client side
over `reqwest`. Two of the three cases run: push-and-pull-back-by-digest, and
"does it show up in `relish images`". The third — actually *deploying* from the
cluster registry — skips, honestly, because our synthetic image is one marker
file with no binary in it; you can store it but you can't run it, and a deploy
case that can't run its workload has nothing to prove. Staging a genuinely
runnable image in Pickle is a job for another day, and it says so.

That completes the catalogue: thirteen groups, thirty-nine cases. Not all of
them run everywhere — the process-runtime cases skip on runc and the container
cases skip on a process cluster; firewall wants eBPF, ingress wants the proxy,
the registry cases want to be on a node. But every skip names its reason, every
group that can be exercised is, and the shape of what the cluster promises is now
written down as tests that either hold it to that promise or say, out loud, why
they couldn't. Which was the whole point of the chapter.
