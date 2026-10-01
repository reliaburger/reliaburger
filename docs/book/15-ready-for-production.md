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

### Waiting out a timeout is still sleeping

A sleep hides in plenty of tests that never call `sleep`. When we sorted the portable suite
by duration, nineteen tests took an exact multiple of five seconds: 231 seconds between them,
spent proving that a stalled peer, an ignored SIGTERM or a silent socket hits its deadline.
The assertion was right. The wait was the production deadline, served in full, every run.

We used three tools, and choosing between them is the interesting part.

**Pause the clock, when every deadline in play is a Tokio timer.** A registry route that must
answer 408 to a body that never arrives, a cluster status request to a member that accepts
the connection and says nothing, a compatibility probe of a binary that sleeps forever: in
each, the only thing that can end the wait is a `tokio::time::timeout`. Calling
`tokio::time::pause()` just before the request makes Tokio jump straight to the next timer
whenever the runtime has nothing else to do. The outer guard in the test is a later timer, so
a missing deadline still fails instead of passing. We call `pause()` mid-test rather than
using `start_paused`, after the fixtures are up, and only where nothing in flight needs real
time to make progress. Paused time treats "waiting on a socket" as idle, so a test whose
server must genuinely answer first can't use it: the clock would expire the request before
the reply arrived, and the test would pass for the wrong reason.

**Inject the deadline, when real time is involved.** The agent's ten-second stop grace, the
placement reconciler's I/O deadline, the registry forwarder's proposal deadline, the Apple CLI
inspection bound and the renewal worker's retry pause are all now fields with the production
value as their default. Tests whose peer stalls *permanently* set a short one:

```rust
pub fn set_stop_grace(&mut self, grace: std::time::Duration) {
    self.stop_grace = grace;
}
```

It's a setter, not a global and not an environment variable, so one test's short grace can't
leak into another running in the same process. The rule we held to: shorten a deadline only
when the thing it bounds never happens in the test. A runtime that ignores SIGTERM for the
whole test proves escalation just as well after 200 milliseconds as after ten seconds. The
`relish wtf --watch` refresh became a real `--interval` flag rather than a test-only knob,
since an operator watching an incident wants a faster refresh too.

**Observe the end of the work, when proving that something didn't happen.** The hardest four
checked that a cancelled process-owner mutation can't touch the generation that replaced it.
Dropping the caller's future doesn't cancel a `spawn_blocking` closure that's already queued,
which is the whole point of the test, so the old version watched the owner record for twenty
seconds. A shorter window would have let a slow regression pass silently. Instead, the process
control layer now counts its blocking operations, and the count lives inside the closure:

```rust
struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

let in_flight = InFlight::enter(&self.in_flight);
tokio::task::spawn_blocking(move || {
    let _in_flight = in_flight;
    operation(this, id)
})
```

`Arc<AtomicUsize>` is a reference-counted pointer to an integer that several threads can
update without a lock; `fetch_add` and `fetch_sub` are atomic increments and decrements.
`impl Drop` is Rust's destructor hook, the same idea as a C++ destructor or Go's `defer`
attached to a value instead of a function. The `move` closure takes ownership of the guard,
so it's dropped when the closure finishes, not when the caller stops waiting. The leading
underscore in `_in_flight` keeps the binding alive to the end of the closure; a bare `_`
would drop it immediately. When the count reaches zero, nothing queued can still change the
record, so the test checks it at once. We confirmed it still bites by reintroducing the old
bug: the start, stop and kill variants each failed within half a second.

Not everything in the slow list was a timeout. Most of the API authorisation tests spent
their time in Argon2id, which is slow by design and much slower again without optimisation. Cargo lets one
crate be optimised inside an otherwise debug build, and we already did that for SHA-256, so
`argon2` and `blake2` joined it. The slowest of those tests fell from nearly nine seconds to
a third of a second. One test unpacked 65,537 empty files to prove the entry cap; the cap
counts entries, not files, so the same directory repeated 65,537 times proves the same limit
without half a minute of file creation.

A few stayed slow on purpose. Two job tests wait four seconds to prove no spurious retry
follows a success, and the retry backoff they cross runs on `std::time::Instant`, which no
paused clock can move. The rollout ownership tests are bounded by the two-second force-kill
confirmation, which has its own configuration work ahead of it.

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

Production code had the same blind spot, one level down. GitOps ran `git` with
`Command::output()` inside `spawn_blocking`, so a stuck remote held the only sync loop, and
shutdown waited with it (B19, Chapter 7). A timeout around the task would have freed the loop
and left `git` running. The fix gives each child an owner that can end it: its own session and
process group, a deadline, the shutdown token, and a group kill and reap however it finishes.
The tests prove the ending, not the waiting. A fetch pointed at an upload-pack that never
answers must fail inside its budget, the sleeping descendant it left must be gone, and the
next fetch must work. Cancelling shutdown during that fetch must finish the loop's
`JoinHandle` within five seconds.

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

Per-test processes also let us cut the build down. Cargo compiles every file directly under
`tests/` as its own crate, and each one links the entire `reliaburger` library, so 77 test
binaries meant 77 links and about 9 GB of debug executables. The small, ungated files now
live as modules of one `tests/suite/` binary, and only the suites a Makefile target or a
nextest group picks out by `binary(...)` keep a binary of their own.

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
It carries its own expiry, so a fresh state report can't keep a dead subsystem looking alive.
The leader treats a missing or expired readiness report as unready. No optimistic guessing.

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

Spawning a task doesn't mean it has bound its socket or loaded its state, yet the first supervisor marked every owner `Ready` the moment it spawned it. Now each owner receives a signal and fires it itself, once its resources are in hand:

```rust
pub struct ReadySignal(tokio::sync::oneshot::Sender<()>);

impl ReadySignal {
    /// Publish resource readiness. A retired owner's acknowledgement is ignored.
    pub fn ready(self) {
        let _ = self.0.send(());
    }
}
```

`ReadySignal` is a *tuple struct*: a struct whose single field has no name and is reached as `self.0`. It wraps the sending half of a `oneshot` channel. `ready` takes `self` by value rather than `&self`, so calling it consumes the signal and the compiler won't let an owner report ready twice. A restarted owner gets a brand-new channel, so a slow signal from the attempt that just died can't mark its replacement ready. `let _ =` deliberately discards the send result: if the supervisor has already moved on, nobody is listening, and that's fine.

Wiring this up produced a proper async deadlock, without any thread holding a lock forever. The supervisor drives the owner's future itself, in a `tokio::select!` alongside the ready signal. When the signal won, the supervisor ran that branch's body, which awaited a write to the readiness tracker. Meanwhile the owner was itself queued for the same tracker, first in line, and a future only makes progress while someone polls it. The supervisor *was* that someone, and it was busy awaiting inside a branch body, which doesn't keep polling the other branches. Nobody moved. The fix makes publishing readiness another future inside the `select!`, so the owner keeps getting polled while the supervisor waits. It's worth remembering: in `select!`, code in a branch body runs alone.

The scheduler consumes the same evidence under an independent receive-time lease. A fresh
metrics report can't keep stale readiness alive, and a leader change starts with no inherited
lease. Until the node proves every critical owner again, it receives no new work. Briefly
under-scheduling is inconvenient. Scheduling onto a node whose control plane is half dead is
worse.

Readiness isn't the whole capability story. Pickle can have a healthy TCP listener and still
be useless to peers, or have enough members for two copies while one layer has only one. Its
capability record therefore reports both inputs and outcomes: the actual `SocketAddr`, TLS
and P2P state, target copy count, active members and under-replicated layers. The listener is
an `Option<SocketAddr>` because an unbound registry has no honest address; `None` says that
directly instead of smuggling absence through `0.0.0.0:0`.

In cluster mode the default loopback setting derives the gossip-advertised IP. An explicit
bind must be that IP or a wildcard. Otherwise Bun refuses to start. This is a useful pattern
for configuration defaults: a safe standalone default can become a derived cluster value,
but it must never survive into a mode where its guarantee is false.

Identity evidence needs the same discipline. `[cluster].name` is the SPIFFE
trust domain for app, job and build-signer certificates. Generated configs now
persist it and Bun rejects malformed domains at startup. The acceptance test
uses a non-default name, signs a real workload CSR with that cluster's Workload
CA, validates the chain and checks the URI SAN. A string-format assertion alone
would have missed the original wiring bug.

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

### `Err(_)` is not a locked door

A later audit found the runner's own cases breaking exactly this rule. The namespace-scope
case sent a write it expected to be refused and matched the outcome with `Err(_) => Ok(())`.
Any error was proof of enforcement — a connection reset, a TLS failure, a 500 from a
half-started agent. The test named a security property and would go green on a network blip.
Worse: even asserting the status wasn't enough. The real refusal is a 403 whose body says
`token scope does not allow …`, and a *role* failure is also a 403 — so the case now matches
the message, not just the code. And a transport failure while probing an enforcement boundary
is neither pass nor fail; it becomes `Unknown`, the runner's "no verdict" outcome, because
"couldn't reach the door" says nothing about whether it was locked.

The quota case had the more interesting disease: its premise was wrong. It expected the
second `apply` to be rejected — and quota enforcement doesn't live there. Apply admits an app
to desired state; the *scheduler* enforces quota, by declining to place the over-budget app.
So on a healthy cluster the case's expected rejection never happens, and the only reason it
ever passed was the same `Err(_)` sponge soaking up unrelated failures. The rewrite asserts
the enforcement that actually exists: both applies succeed, and the scheduling evidence shows
the second app pinned at zero scheduled replicas — re-checked after a settle window, because
a *late* grant is precisely the bug. When a test can't say what mechanism it's testing, check
whether the mechanism exists.

The placement case rounds out the set: it swept per-node status errors into "not hosting"
(`unwrap_or(false)`) and only ever asserted the *negative* — no wrong node hosts the replica.
An unreachable node is the likeliest home of a misplaced replica, and a sweep that can't see
it proves nothing. It now returns `Unknown` on any node it can't inspect and asserts the
positive: the set of hosting nodes is exactly the labelled one.

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
Our `make coverage` runs the portable suite once under instrumentation and emits LCOV plus an
HTML report. On Linux CI that same run is the test gate, so we don't pay for the suite twice. The first combined Linux CI measurement covered
79.65% of lines, so CI starts at 78.65%, one percentage point lower, and can ratchet upwards.

Instrumented programmes write their counters to a `.profraw` file as they exit. Our
crash-recovery tests SIGKILL instrumented Bun processes on purpose, and one day a kill landed
mid-write: all 4,708 tests passed, then `llvm-profdata` refused the truncated file and failed
the whole report. The report steps now pass `--failure-mode all`, which skips an unreadable
profile with a warning and only fails when none can be read. Skipping one killed process's
partial counts can pull coverage down a hair. It can't push it up, so the floor still means
what it says.

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

The repaired check sends every file through the real parser, validator and planner, and
keeps a failed command's diagnostic. Successful plans stay quiet. It started as a Makefile
target that paid for its own `cargo build`; it's now `tests/examples.rs`, an ordinary
integration test that finds the binary through `env!("CARGO_BIN_EXE_relish")`. Cargo sets
that variable at compile time to the path of the `relish` it built for the tests, so the
check costs nothing extra and runs on every platform `make test` does. It catches the same
drift a reader would hit after copying an example.

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

## A pipeline you're willing to wait for

By the time the review tiers landed, a green push took half an hour and about 212
runner-minutes, and more than half of those minutes went to runs that a newer push had
already cancelled. Nobody waits half an hour for feedback. They push again, which cancels the
run, which makes the wait longer. So we measured where the time went before touching anything.

Two-thirds of it was compiling. The portable suite, about 4,150 tests that take five minutes to
run, ran seven times per push: default and no-default features in three jobs, plus macOS. The
only default feature is `kubernetes`, and nothing in the code depends on it being *off*, so the
four no-default runs repeated the same 4,110 tests to prove the crate builds without one
dependency. A second `clippy --no-default-features` proves that in seconds. The coverage job
compiled and ran the suite again under instrumentation, beside the uninstrumented run it
duplicated. The 10,000-member scale test had its own 21-minute release build, because
everyone assumed it was slow. Timed in a debug build, it took 1.3 seconds.

The rest of the fix follows one idea: build once, then fan out.

- **One instrumented run.** Portable Linux runs the suite once under `cargo llvm-cov`, and that
  single run is the test gate, the coverage floor and the JUnit report.
- **One build for the acceptance suites.** A `build-tests` job compiles every test binary and
  packs them with `cargo nextest archive`. The cluster, upgrade and wall-clock jobs download
  the archive instead of each spending nine minutes compiling all 75 binaries to run a dozen
  tests. It works because GitHub checks the repository out at the same path in every job, so
  the paths that `env!("CARGO_BIN_EXE_bun")` baked into the test binaries still point at real
  files after extraction.
- **One cache per build, saved from `main`.** Thirteen per-job caches overflowed GitHub's
  10 GB limit, so every pull request evicted the last and most jobs started cold. Jobs that
  build the same profile now share a key, and only `main` writes it.

The last piece is choosing what a run needs. A small script diffs a pull request against its
base and sets three outputs, and every expensive job asks one of them:

```yaml
  cluster:
    needs: [changes, build-tests]
    if: needs.changes.outputs.heavy == 'true'
```

`needs` makes a job wait for others and gives it their outputs; `if` skips it when the
expression is false, and a skipped job counts as passing. A pull request that touches only
book chapters sets `code` to false and runs no Rust at all, unless it touches chapter 2 or 4,
the READMEs or the manual. Those count as code, because `documentation_first_run` checks
their snippets and the manual is compiled into `relish`. A pull request stacked on another
branch skips the acceptance suites unless someone labels it `full-ci`; it gets them when it
targets `main`. Benchmarks run on `main`, nightly, and on changes to the gossip protocol,
since nothing gates on their numbers yet.

Speed is only half of it. A flaky suite teaches everyone to press "re-run" without reading,
and then a real race looks exactly like noise. Our retries stay at zero, so a failure that
passes on a re-run goes into a register (now `docs/flakes.md`) with its cause the same day. Of
the seven we chased down, three were product races, not test problems: a node back from a
fault spread stale suspicions about healthy peers, a rollout interrupted by a crash dropped the
reservation its own retirement needed, and a two-second kill deadline was too short for a busy
host. We fixed those in the product. Pressing re-run would have hidden all three.

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
Both expiries did their job: ratatui 0.30 later brought a fixed `lru`, and moving to
DataFusion 55 removed Thrift from the graph altogether (Chapter 6 tells that story).

A later `rkyv` advisory showed why the compiled graph matters too. Cargo locked
`rust_decimal`'s optional `rkyv` 0.7 dependency, but `byte-unit` disables the defaults that
would enable it. `cargo tree --target all -i rkyv` found no active path, and Reliaburger reads
no rkyv archives. The advertised fix starts at the incompatible rkyv 0.8 API while
`rust_decimal` 1.x deliberately pins its optional integration to 0.7. We recorded a temporary,
expiring exception instead of pretending that changing an unused lockfile version was a fix.
The exception has a condition attached: `make audit` asks Cargo for the active `rkyv`
graph first, and fails if any feature or target activates it, or if Cargo can't answer.

The repository audit denies every new vulnerability and maintenance warning. Its short
exception list expires on 18 November 2026, and the Make target fails after that date until we
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

How does a case body say "unknown" rather than "failed"? The first version used a convention: an error message starting with `__unknown__:` was downgraded. That meant a workload printing `__unknown__:connection refused` could turn its own failure into missing evidence, which is a lot of power to hand a string. Case bodies now return a typed error:

```rust
pub enum CaseError {
    /// An observed assertion or operation failed.
    #[error("{0}")]
    Failed(String),
    /// Available evidence cannot establish a verdict.
    #[error("{0}")]
    Unknown(String),
}

impl From<String> for CaseError {
    fn from(reason: String) -> Self {
        Self::Failed(reason)
    }
}
```

`impl From<String> for CaseError` teaches Rust how to turn a `String` into a `CaseError`, and the `?` operator uses exactly that conversion when it propagates an error. So a case can keep writing `something().map_err(|e| e.to_string())?` and every such error becomes `Failed`, whatever it says. The only way to get `Unknown` is to construct it on purpose, through a small `unknown(reason)` helper. The runner never reads the message to decide.

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

Production runs acquire server-owned leases for their apps and namespaces.
The server checks exact ownership before cleanup; a familiar name is not proof
of ownership. Names still use `rbtest-{run}-{seq}` for recognition. The
lease-free path that focused unit tests use also checks that prefix:

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

Then teardown. The runner attempts it after every case and records a separate
cleanup outcome:

```rust
let cleanup = context.teardown(cleanup_deadline).await;
```

Note "after every case" — pass, fail *or* timeout. It's tempting to only clean
up after a pass, but that's exactly backwards: the case that failed halfway is
the one that left a workload running. Teardown is the runner's job precisely so
a case body can `return Err(...)` the moment something's wrong without a pile of
cleanup code first. Teardown reverses exactly the faults it injected, releases the
server-owned lease and checks runtime absence. Timeout or unreachable peers
mean unknown cleanup; they do not prove the resources are gone. The server's
expiry reaper can still finish later. We record timeout, API failure and a
workload which remains present as cleanup evidence. Discarding the result with `let _ =` would make
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
read another namespace's logs. Mint a leased token, point a second client at it,
and watch the read bounce with the scope gate's own 403. None of that needs a container either.

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

## The last fault that lied

The service partition arm was the last obvious silent success. Ask Bun to block
`web` from `payments` on a node without eBPF and it added a registry row,
returned `200 OK`, and changed no connection. The excuse was a test: the
three-node quorum test used that same `Partition` value as shorthand for a Raft
transport partition. Tightening the service path would break the test.

That's not a reason to keep the lie. It's evidence that we'd put two different
operations in one enum variant.

A control-plane partition blocks gossip and Raft addresses between nodes. It
can remove a council voter, so the quorum rail must count it. A service
partition blocks `connect()` from a workload cgroup to a service VIP. It can't
remove a Raft voter and should never consume the quorum budget. The fix starts
by naming them separately:

```rust
pub enum FaultType {
    Partition { source_app: Option<String> },
    CouncilPartition { peers: Vec<String> },
    // ...
}
```

Where did the source cgroup id go? An earlier version carried a
`source_cgroup_id` on the wire and refused any non-zero value. Nothing had
shipped, so we deleted the field instead: a client can't name a cgroup at all.
That's a question of trust. The kernel compares the key against
`bpf_get_current_cgroup_id()`, and only Bun can reliably tie that number to the
instances it supervises. Trusting an arbitrary integer from a caller would let
the caller fault some other cgroup or create a convincing no-op with a made-up
one.

For a named source app, Bun finds every running local instance, asks the runtime
for its PID, resolves `/proc/<pid>/cgroup` to the cgroup-v2 inode id, and writes
one key per instance. The important bit isn't the loop. It's the ownership
record:

```rust
pub enum FaultReversal {
    BpfConnectKeys(Vec<(u32, u16, u64)>),
    // ...
}
```

That `Vec` (Rust's growable array) is the exact set this fault installed:
network-order VIP, network-order port and source cgroup id. If the third map
write fails, Bun deletes the first two before it reports the injection failure.
If activation succeeds, clear and expiry delete those same keys rather than
trying to reconstruct them from whatever the service map looks like later. The
service may have been redeployed or removed by then. Cleanup must own facts,
not guesses.

The Linux acceptance test loads the real cgroup connect program, publishes a
VIP backed by a local listener, and resolves the test process's own cgroup id.
The control connection succeeds. After inserting the source-scoped partition
key, the kernel returns `EPERM` and the backend sees no connection. Delete the
key and the connection succeeds again. The three-node quorum test now drives
the actual `CouncilPartition` transport operation, so neither mechanism
borrows credibility from the other.

That pass uncovered two more lies hiding behind the phrase "requires eBPF".
Delay wrote an action that the connect program explicitly ignores: a cgroup
socket-address hook can't sleep. Bandwidth wrote a map no loaded program
defines or reads, and discarded the resulting `MapNotFound`. Both commands now
return an error even on an eBPF-capable node. They need a TC packet hook,
lifecycle ownership and effect tests before they can claim success. Parsing a
future contract is fine. Pretending it ran isn't.

Memory pressure follows the same rule from the other direction. An OOM kill
can't be reversed, so there's no `oom` form at all. A Kill fault is the honest
way to test restart after abrupt termination.

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
behind the proxy and sends it an HTTP request with the right `Host` header.

There's a catch with running `sleep` or `httpd` as a container's first
process. PID 1 is special: the kernel drops any signal it has no handler for,
and neither command installs one for SIGTERM. So every stop sat out bun's full
ten-second grace before the SIGKILL. Bun stops workloads on its single command
loop, so a node retiring several test apps couldn't even answer `/v1/status`,
and the V02 soak's catalogue pulse reported six passing cases with cleanup
"not confirmed within 30 s". The fixtures now run under a shell that traps the
signal:

```sh
trap 'kill $! 2>/dev/null; exit 0' TERM; /bin/busybox sleep infinity & wait
```

The command runs in the background because a trapped signal interrupts `wait`
but not a foreground child. `$!` is the background job's PID, so the trap takes
it down too.

A tag isn't an identity, though. `busybox:latest` can point at different bytes
between two runs, which makes a failure impossible to reproduce and lets the
runtime architectures drift apart. The catalogue uses BusyBox 1.37.0's OCI
index digest instead. An OCI index maps one immutable name to platform-specific
manifests, so runc selects `linux/amd64` and Apple Container selects
`linux/arm64` without changing the test configuration. The runtime gates do
more than pull it: they create, start and execute the workload, and any failure
fails the test. No image to build, no registry to push to, and no moving tag.
The capability gate stays explicit:

```rust
Capability::ContainerRuntime => self.container_runtime != "process",
```

That gate is the mirror of `ProcessRuntime` from Part A, and together they draw
the honest line: the `testapp` cases run on a process cluster and skip on runc;
the pinned BusyBox cases run on runc or Apple Container and skip on a process
cluster. Full coverage means separate acceptance runs for ProcessGrill, runc
and Apple Container. That's exactly right. The runtimes really are different
environments, and a test suite that pretended otherwise would hide the seam,
not test it.

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
but not upload one. Originally the dev registry was reachable only from its
node; declared endpoints and managed forwards now remove that assumption. The
harness still has to become, briefly, a registry client.

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
over `reqwest`. The first version ran two of the three cases: push-and-pull-back
by digest, and "does it show up in `relish images`". The synthetic image contains
one marker file and no executable, so it couldn't prove deployment. The third
case now copies the digest-pinned BusyBox image, verified blob by blob, into
Pickle under the lease's namespace, deploys it by digest and checks that its
HTTP server answers from inside the container. A successful upload alone can't
pass it.

That completes the catalogue: thirteen groups, thirty-nine cases. Not all of
them run everywhere — the process-runtime cases skip on runc and the container
cases skip on a process cluster; firewall wants eBPF, ingress wants the proxy,
the registry cases need a declared reachable endpoint. But every skip names its reason, every
group that can be exercised is, and the shape of what the cluster promises is now
written down as tests that either hold it to that promise or say, out loud, why
they couldn't. Which was the whole point of the chapter.

## Ask each node what it can prove

A test runner shouldn't infer the data plane from a config file.
`ebpf.enabled = true` says what the operator wanted. It doesn't prove that the
hooks attached, that the workload entered its cgroup before it started, or that
the observation is still current.

Bun now exposes an authenticated `/v1/capabilities` snapshot which combines
startup facts with live readiness and placement evidence. Each capability has
one of three states: `Available`, `Unavailable` or `Unknown`. The snapshot
expires after 15 seconds.

Why three? Imagine that Bun opened its metrics store but doesn't publish the
latest sample time. Calling metrics unavailable throws away a useful fact.
Calling them fresh turns hope into evidence. `Unknown` says exactly what we
know: the store exists, but we can't prove freshness yet. The future `wtf`
check needs that timestamp before it earns a green result.

This changes how the runner gates a case. A known absent capability can become
a typed skip when the selected profile allows it. Unknown or expired evidence
becomes `Unknown`, which fails acceptance. Silence isn't evidence of absence.

The snapshot also fingerprints the build target and profile, runtime and
version, rootless mode, kernel, architecture and cluster identity. It publishes
the server's operation policy separately from the caller's role. These facts
let us decide whether two reports describe comparable systems before we
compare their outcomes.

Cluster collection has another tempting trap: only returning nodes which
answered. `/v1/capabilities/cluster` instead creates one future per expected
peer and awaits them concurrently with `join_all`. A future represents work
which may not have completed yet; `join_all` polls all of them rather than
waiting for one slow peer before contacting the next. Every request shares one
absolute five-second deadline.

Peer calls present the internal service token through the same cluster HTTP
client as the control plane, including mTLS where configured. There is no
anonymous retry. Responses have a 1 MiB limit and must carry the expected
schema, node id and an unexpired observation. Any failure creates an explicit
`Unknown` entry for that node. Missing evidence stays visible. That's the
point.

## Make the server own the mess

Suppose the test runner creates an app, loses its network connection and gets killed by CI.
Who deletes the app? If the answer is "the runner's `finally` block", nobody does. The code
which should clean up is already dead.

Bun now gives Phase 15 apps a server-owned lease. A Deployer asks for a
lifetime and receives a random identifier plus an `rbtest-*` namespace. The
lease records both the readable token name and a fingerprint of the exact
credential, because two credentials may share a name. The server policy must
permit isolated workloads, and the requested lifetime can't exceed the
operator's configured maximum or the hard one-day ceiling. Bun also stops one
caller filling the control plane with polite-looking garbage: at most 64 leases
may exist, and each owns at most 128 resources.

A leased apply sends the identifier in an HTTP header. In standalone mode Bun writes the app
identity to `test-leases.json`, calls `sync_all`, and only then starts deployment. In cluster
mode one Raft entry inserts both the desired app and its lease resource. There's no gap where
the app exists but ownership doesn't. An ordinary apply can't use the reserved prefix, even
when no matching lease exists, so two concurrent requests can't sneak through opposite sides
of an ownership check.

The resource collection is a `BTreeSet<LeasedResource>`. We met `BTreeSet` earlier for the
safety allow-list. Here its second useful property matters: inserting the same app twice is
idempotent. The enum owns apps and the matching namespace quota declaration:

```rust
pub enum LeasedResource {
    App { app_id: AppId },
    Job { job_id: AppId },
    Namespace { name: String },
    ApiToken { name: String, fingerprint: [u8; 32] },
}
```

A derived ordering follows declaration order, so apps and jobs sort before
namespaces and cleanup removes every workload before its quota record.
`[u8; 32]` is a fixed-size array of 32 bytes, stored inline: a SHA-256
fingerprint of the exact token, because a name alone could point at a
replacement credential. Each kind of resource needs its own cleanup
operation. Pretending a namespace owned them all would produce green reports
and leaked state, which is quite an achievement for a test system.

Expiry moves a lease to `Cleaning`. Standalone cleanup waits for the agent to confirm the app
stopped. Cluster cleanup removes each owned app from replicated desired state and lets the
ordinary reconciler converge the runtimes. The record disappears only after those control
plane operations succeed. Every cleanup step has a ten-second limit; a failure leaves the
resource list and attempt count on disk or in Raft. A reaper checks once per second, so a Bun
restart or Raft leadership change resumes the same cleanup instead of inventing a fresh one.

Followers forward create, renew, release and leased apply requests to the
leader. They preserve the user's credential, and the leader authenticates it
again. Using the cluster service token here would be convenient, but it would
also turn any compromised follower into the owner of every test lease.
Convenience doesn't get a vote on authority.

A forwarded create has one more wrinkle. The leader answers once a quorum
has committed the lease, and that quorum doesn't have to include the follower
that forwarded it. The caller's very next request is an apply under the new
lease, usually sent to the same follower, which checks the lease against its
own replica before forwarding. A multi-node CI run caught it answering "lease
not found" for a lease it had handed out a millisecond earlier. So the
follower now holds the `201 Created` until its own replica has applied the
lease, for up to five seconds: read-your-writes for anyone who stays on the
same node.

The runner now creates one lease after capability gating and before it runs a
case. It asks for the case budget plus the fixed cleanup budget; if the server's
maximum can't cover both, the case becomes `Unknown` without touching the
cluster. Every app or namespace-quota apply carries the lease id. Pass,
failure, panic and timeout all reach the same release path, and killing Relish
merely leaves the server reaper to do the same job. `--namespace` is a readable
base now, with a per-case suffix, because sharing one namespace between
concurrent cases was never isolation. After release, Relish polls every node
using the original authenticated and CA-pinned client. A missing Raft record
isn't proof that a runtime has stopped.

Container cases now use BusyBox 1.37.0 by immutable OCI index digest. The
provisioned runc gate resolves and executes the `linux/amd64` manifest; the
Apple Container gate does the same with `linux/arm64`. The Apple proof caught a
nice final trap: its `container exec` command doesn't accept Docker's `--`
separator, so our old adapter asked the runtime to execute a programme
literally named `--`. A test which merely logged that error had been green.
The new test fails on create, start, state or exec, and the adapter now emits
the command shape Apple's CLI actually accepts.

### Every owner, not just the latest one

The lease above answered "who deletes the app?". It took a surprising amount of work to answer "and how do we know it's gone?"

Move a test app from worker A to worker B while A is disconnected. Deleting the app from Raft removes the *desired* state; it tells us nothing about the process still running on A. If we also dropped the lease at that point, we'd have lost our only reminder to go back and check. So the lease remembers every node that has ever been assigned its apps, and every node that accepted an upload into one of its registry repositories:

```rust
pub struct TestLease {
    // ...
    /// Resources atomically associated with this lease.
    pub resources: BTreeSet<LeasedResource>,
    /// All possible runtime owners, removed only by confirmed retirement.
    pub placements: BTreeSet<LeasedPlacement>,
    /// Repositories and every node that may hold uploads or metadata for them.
    pub repositories: BTreeMap<String, BTreeSet<u64>>,
    /// Confirmed workload retirement, required before repository deletion.
    pub workloads_retired: bool,
}
```

`LeasedPlacement` is a small struct of an `AppId` and a `NodeId` that derives `Ord`, so a `BTreeSet` can hold each pair once, in a deterministic order. The scheduler adds a pair in the same Raft entry that publishes the placement, and rescheduling adds a new owner rather than replacing the old one. `BTreeMap<String, BTreeSet<u64>>` nests one collection in another: each repository name maps to the set of node numbers that might hold its bytes.

Cleanup then waits for each of those owners to say, in so many words, that it's done. A worker acknowledges retirement only after it has observed the runtime exit, removed the instance's identity directory and adoption record, unmounted and deleted any test volume, and saved its own checkpoint. The acknowledgement carries the lease's ID, so a late reply can't release a replacement lease's resources. Registry uploads wait behind the `workloads_retired` flag: there's no point deleting an image that a still-running container might need. The HTTP API tells the caller where it stands. `202 Accepted` means cleanup is under way; `204 No Content` means every recorded owner has confirmed. Relish polls on 202, retries a 503 from a follower that has briefly lost its leader, and puts one deadline around the whole lot, so a retry never quietly earns a fresh one.

How long should that deadline be? We first picked 30 seconds, and the V02 soak showed why a round number was the wrong answer. A chaos case killed a worker, watched its replicas move, healed the node and passed. Then the release sat at 202 for the full 30 seconds and the report said cleanup was `unknown`. Nothing was stuck. The owner, the leader, was still rolling out the frontend replicas it had inherited from the dead node, and a node's reconcile tick finishes its deploys before it starts retirements. The test instances vanished within 55 seconds of the release. Meanwhile the product's own deadline for a *single* retirement was already 60 seconds: ten for queueing behind other agent commands, plus `stop_completion_bound`, the longest a stop may take when a workload ignores SIGTERM. The runner was giving up on the server sooner than the server gives up on itself.

So the number is no longer ours to pick. `lease_retirement_bound` in `src/cluster/orchestrate.rs` adds up what an owner does after a release (wait for its next placement poll, retire, acknowledge to the leader) from the same constants the reconciler uses. Relish's `lease_release_budget` is that bound with the default runtime configuration, and the case runner's teardown is that budget plus 30 seconds for reversing faults and checking the runtime independently. `pub fn` rather than `const` here isn't a style choice: `RuntimeSection::default()` builds a struct at run time, and Rust only lets a `const` call functions marked `const fn`, which `Default::default` isn't. If someone later lengthens the stop grace, the runner's patience grows with it instead of silently turning green runs into `unknown` ones.

That bound still had a hole: it assumed nothing else held the owner's reconcile cycle, and a deploy did. So the reconciler now retires before it deploys, and keeps retiring while it waits on one (Chapter 7, "Retire first, deploy second"). The report changed too. It used to say "within 30 s" whatever the budget was; it now prints the budget it actually had.

What if a worker will never come back? Waiting forever keeps the record honest but leaves the operator stuck. `relish decommission-node worker-a --workloads-stopped --reason "powered off"` is the escape hatch, and it's deliberately a *different* kind of evidence. Only an unscoped Admin can submit it. Raft records who said it, why and when, releases that node's outstanding placements, and permanently retires the node identity, so an old disk can't come back with an old certificate and resume work the cluster has already forgotten. Bun observing a process exit and an operator promising they've pulled the plug are both valid reasons to finish cleanup. They're not the same reason, and the audit record says which one it was.

Tokens, jobs and faults needed their own owners too. A test token is minted inside the lease, scoped to its namespace, never Admin, and it expires no later than the lease does. Jobs in 0.1 run on the node that received them, so their leases live on that node (`scope = "node_jobs"`) and a local reaper retires both running jobs and cron registrations that haven't fired yet. Chaos faults get a receipt *before* the request goes out, because the server may accept a fault whose response never reaches us; an unresolved receipt makes cleanup `Unknown` rather than `NotRequired`.

One last trap. The reaper used to wait on each lease's operation lock in turn, so a single lease still being deployed stopped every other expired lease from being cleaned. It now asks without queueing:

```rust
let operation_guard = operation_lock
    .try_lock_owned()
    .map_err(|_| LeaseError::Busy)?;
```

`try_lock_owned` returns immediately, with either a guard or an error, where `lock_owned().await` would wait its turn. The "owned" part means the guard holds its own `Arc` reference to the mutex rather than borrowing it, so it can be stored in a struct or moved to another task. A busy lease becomes `LeaseError::Busy` (HTTP 409 to an explicit caller), the reaper moves on to the next one, and it comes back on its next tick.

## Who may break production

Before this tranche, `/v1/fault` checked only that the caller was a Deployer.
Relish also copied `$USER` into `injected_by`. That environment variable tells
you which local account launched the client. It says nothing about the bearer
token Bun authenticated, and a raw HTTP caller could put any name in the JSON
body. Useful display hint, terrible audit trail.

The corrected decision has independent gates:

```text
authenticated role
  + server operation allowlist
  + protected-cluster policy
  + explicit acknowledgement
  = permission to inject
```

For ordinary workload faults the minimum role is Deployer and the operation is
`inject_workload_faults`. An Admin still needs the operation grant. Otherwise
changing a role would silently enable an operation which the server owner
disabled. Unknown and production clusters also require the server's
`allow_protected_mutation` switch. Finally, `--acknowledge` records the
operator's intent. It grants nothing by itself.

Node drain, node kill and council partitions remain
stricter: Admin plus `alter_node_state`. Node pressure uses Admin plus
`saturate_capacity`. The target node repeats these checks after forwarding, so
a permissive source can't confer authority on a stricter target.

Reversal has a deliberately different method:

```rust
policy.authorise_reversal(operation, &caller)?;
```

It still checks the operation's minimum role and server grant. It doesn't
require acknowledgement or the protected-cluster mutation switch. Those
checks stop escalation. Requiring either before removing an active fault could
trap the cluster in the dangerous state after an operator tightened policy.
The agent receives separate booleans for workload, node and pressure reversal,
then checks the actual stored fault type. Authority to remove one class never
leaks into another.

Bun now replaces the compatibility `injected_by` field with the authenticated
token's readable name. Audit events use the credential's stable principal id
instead. `ClusterEvent` gained three backwards-compatible fields:

```rust
pub action: Option<String>,
pub principal: Option<String>,
pub details: BTreeMap<String, String>,
```

`Option<String>` lets old non-audit events omit the two scalar fields.
`BTreeMap` keeps action-specific facts deterministic when serialised. Serde
defaults all three while deserialising older events and skips empty values
while serialising, so existing consumers keep working. A successful injection
records `fault.injected` with fault id, type, duration and target. Every
specific, service-wide, all-workload and council clear path records its own
stable action. The human message remains for people; automation reads the
fields.

## Make a node disappear without killing Bun

What does a node failure mean in an in-process test? Killing Bun would
certainly look realistic, but the process which owns the timer and cleanup
would be gone too. It also makes a portable acceptance test responsible for
restarting an external service manager. That's a different test.

The node-kill primitive instead closes the three channels which make a Bun
process part of a cluster: gossip, Raft and the reporting tree. The local
management API stays open. Peers stop receiving SWIM acknowledgements, Raft
traffic fails, reports stop, and the normal failure machinery reacts. From the
cluster's point of view the node has gone. From the test runner's point of view
there is still a narrow recovery path.

All three transports share a `NodeTransportGate`. It uses an `AtomicUsize`,
which is Rust's lock-free integer for state touched by several tasks. Each
fault increments the count and each reversal decrements it. The transports
open only when the count reaches zero, so clearing one of two overlapping
faults can't accidentally heal the other one. A three-node acceptance sends
the request through one node, watches another disappear from SWIM, clears it
through the target's management API, and watches it rejoin. Before healing, it
also tries to fail a second voter and proves the quorum rail says no.

Drain is gentler. It adds a critical degraded entry to the node's live
readiness report but keeps every transport open. The leader sees the node as
alive but ineligible, re-plans placements elsewhere, and admits it again when
the final overlapping drain reverses.

These are deliberately privileged operations. The JSON field
`acknowledged: true` records intent, but it grants nothing. Bun also requires
an authenticated Admin and the server-owned `alter_node_state` permission.
When one node forwards a request, it preserves the user's credential; the
target authenticates and authorises it again. It also replaces the
client-supplied `injected_by` value with the authenticated token name before
recording the fault; the event uses the stable credential principal.

Every node fault needs a non-zero TTL. Clearing one manually uses:

```sh
relish fault clear 7 --node worker-2
```

Reversal still needs Admin plus `alter_node_state`, but it needs neither
destructive acknowledgement nor the protected-cluster mutation switch. A
Deployer with `inject_workload_faults` may clear workload faults and leaves
node faults alone. Fault IDs and timers still live in the target process,
though. A node that gossip has marked suspect or dead is still reachable
this way: forwarded reversals (and the node relay) look it up among every
member gossip still knows, not just the live ones. Only once gossip has
forgotten the node entirely must the operator point Relish at its still-open
API to clear it; expiry needs no route and will restore it automatically. Durable lease ownership for node state remains
unfinished. The chaos catalogue must account for that rather than turning
cleanup uncertainty into a cheerful skip.

## Pressuring a node without pressuring Bun

A workload CPU fault edits that workload's cgroup. Useful, but it doesn't test
what happens when one whole node runs short of capacity. C4 in the chaos
catalogue needs the latter: consume a bounded share of one worker's CPU, bring
the node towards a memory-usage target, and check that the rest of the cluster
stays healthy.

The tempting implementation is to start a burn loop inside Bun. That would put
the control plane in the experiment's blast radius and make the process
responsible for cleaning up the resource starvation it is suffering. A fine
way to create a memorable afternoon, but not a useful primitive.

Bun now owns `/sys/fs/cgroup/reliaburger-chaos` instead. Each pressure fault
gets one child cgroup and one hidden helper process. Bun stays in its original
cgroup. The child writes its PID to `cgroup.procs`, allocates the required
resident memory and starts one burn thread per available core. The cgroup's
`cpu.max` limits those threads to the requested percentage of total machine
capacity:

```text
quota = period × cores × percentage / 100
```

One ordering detail earned its comment the hard way. Our first version wrote
`cpu.max` before spawning the helper. Sensible-looking, and wrong: the helper
faults hundreds of megabytes of ballast into residency *inside* that cgroup,
so a 5% quota turned a sub-second startup into a crawl on shared CI hardware
and tripped the four-second readiness timeout. The parent now writes `cpu.max`
only after the helper prints `ready`. The burn threads run unthrottled for one
pipe round-trip, which is bounded and harmless. The memory ceiling still goes
in before spawn, because it is the safety bound on the allocation itself.

Memory needs a more careful definition. `--memory 90%` means "bring total node
usage to 90%", not "allocate another 90%". The helper reads Linux `MemTotal`
and `MemAvailable` *after* joining its cgroup, then allocates only the
difference. The kernel-enforced `memory.max` is the requested percentage of
physical memory. That ceiling doesn't depend on a momentary usage reading, so
the parent's setup and the child's later calculation can't race into an
accidental OOM. `memory.swap.max = 0` also keeps the evidence about resident
pressure rather than swap throughput.

The helper sizes its ballast once. It doesn't chase the target afterwards: if
other processes free memory, node usage drifts below 90%, and if they grow, it
drifts above. We considered a loop that tops the ballast up and gives it back,
and said no. A ballast that shrinks whenever the workload grows hands the
workload the very memory the experiment was meant to take away, and a control
loop fighting the kernel's reclaim is a new source of flakiness rather than a
fault. The acceptance test learned this the hard way. It used to assert the
node-wide `MemTotal - MemAvailable` after apply, and on a CI runner still
reclaiming the previous suite that figure came in 100 MB short of a target the
helper had hit exactly. It now checks what the controller promises: the
helper cgroup's `memory.current` holds the delta measured just before apply,
and stays under `memory.max`.

This helper runs before Bun constructs Tokio's runtime. We replaced
`#[tokio::main]` with an ordinary `main` which handles the hidden synchronous
subcommand first and explicitly builds the normal multi-thread runtime for the
agent path. Otherwise Tokio's worker threads would exist before the helper
joined its cgroup, muddying process ownership and wasting memory in a programme
which only needs to park.

Linux gives us one more safety net: `prctl(PR_SET_PDEATHSIG, SIGKILL)` asks the
kernel to kill the helper when the thread which created it exits. Note *thread*,
not process: that thread can be a Tokio worker, and Linux doesn't wait for the
whole of Bun to die. `prctl` is a C system call, so Rust requires an `unsafe`
block, whose safety comment explains the invariant: the call changes process
metadata and doesn't dereference Rust memory. Bun records the kernel thread ID
immediately before spawning, with no `.await` in between (an await could resume
the task on a different worker thread), and the helper refuses to start unless
that thread still exists inside its parent. A fresh Bun sweeps any `fault-*`
cgroups left by a hard crash.

The storage helpers use the same trick through `Command::pre_exec`, a closure
that runs in the child after `fork` and before `exec`. In a multi-threaded
program, that window is hostile: another thread may have held the allocator's
lock at the moment of the fork, and the child inherits the locked lock with no
thread left to release it. So the closure may only make async-signal-safe
calls, and allocating memory isn't one. Our closure broke that rule on its
error path: `std::io::Error::other("…")` boxes a string. It now returns
`std::io::Error::from_raw_os_error(ESRCH)`, which stores the error code inline,
and the parent turns that code back into a readable message after `spawn`
returns. The `// SAFETY:` comment now says so, because the comment is the part
a reviewer actually checks.

None of this is enabled by merely running as root. The server policy needs the
independent `saturate_capacity` operation plus non-zero CPU or memory ceilings;
both ceilings have a hard 90% maximum. The caller needs an authenticated Admin
role and explicit acknowledgement, and the target repeats those checks after
cross-node forwarding. Only one helper may run on a node, so two individually
safe requests can't add up past the configured bound.

Clearing pressure is deliberately easier than creating it, but not a role
shortcut. It still needs Admin plus `saturate_capacity`; it doesn't need
acknowledgement or the protected-cluster mutation switch. That operation
doesn't let the same caller reverse a drain or node kill. Clear, expiry and
graceful shutdown kill the child and remove its cgroup. Parent death and
startup sweep cover the paths where normal cleanup code never gets a turn.

The privileged acceptance test checks the real hierarchy rather than a mock.
It verifies `cpu.max`, observes the helper PID inside the fault cgroup and the
test/Bun process outside it, confirms the total-memory target, clears the
fault, then drops the controller and proves a restarted owner removes the
stale cgroup. On macOS, rootless Linux or a cgroup hierarchy without both
controllers, capability evidence says `Unavailable`. Honest again.

## Chaos with a safety catch

We had five recovery scenarios on paper. Running them wasn't the difficult
part. The difficult part was making sure a failed test didn't leave the
cluster in a worse state than the failure it had found.

`relish test --chaos` now checks these five claims, in this order:

1. another council member becomes leader after the leader fails;
2. three live replicas recover after their worker fails;
3. the council majority keeps serving while its minority is isolated;
4. the cluster stays observable under bounded whole-node pressure; and
5. a rolling deployment reaches a real terminal outcome when a node dies
   during an observed active operation.

Each case also reverses its fault and proves the affected node rejoins or the
expected replicas return. The fifth case is worth spelling out. If the deploy
finishes before the runner observes an active operation, we haven't tested
"node death during a deploy". The result is `Unknown`, not an optimistic pass.

Could we run the five in parallel to save time? Individually acceptable blast
radii don't compose. Failing one worker while another case isolates a council
minority can turn two tidy experiments into one accidental outage. The chaos
suite therefore ignores the ordinary parallel width and uses one permit.
After a case gets that permit, it fetches capability evidence again. The
evidence expires after 15 seconds; checking it before waiting in the queue
would let a destructive decision outlive the facts behind it.

The preflight requires at least three nodes, an available container runtime,
and the union of capabilities and server operations used by the selected cases.
With no filter, all five run and require node kill, node pressure,
`provision_isolated_workloads`, `alter_node_state` and `saturate_capacity`.
Protected clusters need their server-owned mutation gate.
The interactive command asks the operator to type exactly `yes`; automation
uses `--yes`. This records consent. It doesn't invent authority, and there is
no `--override`.

Why fail an unfiltered invocation instead of skipping the pressure case on a
rootless machine? Because "five chaos tests passed" must mean we ran five chaos
tests. Select a supported subset explicitly with exact scenario names, for
example `relish test --chaos --filter dead_worker_node_has_workloads_rescheduled`.
This needs node kill and `alter_node_state`, but neither node pressure nor
`saturate_capacity`. Selecting the pressure case still requires its capability
and grant. Unknown names and empty comma-separated selections are errors.
The report names the cases that actually ran; a subset is not a full run.
ProcessGrill stays separate for the same reason: fixed host ports can't restore three replicas onto two surviving
nodes. The catalogue uses the digest-pinned BusyBox OCI workload which runs
under both runc and Apple Container, although the full pressure scenario still
needs rootful Linux cgroup v2.

Now, cleanup. A `TestContext` is cloned into the case task. When that task
times out, the runner aborts it before cleanup; when it panics, Tokio returns a
`JoinError`. In both cases the outer context must still know which faults the
inner task created. The guard holds:

```rust
Arc<Mutex<Vec<OwnedFault>>>
```

`Arc<T>` is an atomically reference-counted shared owner. Unlike an ordinary
borrow, it can cross a spawned task boundary and outlive the stack frame which
created it. `Mutex<T>` permits one task at a time to mutate the vector. We use
Tokio's asynchronous mutex because waiting for it must not block a runtime
thread. Cloning the guard clones the `Arc`, not the vector, so the runner and
case always see the same ownership ledger.

Every ledger entry contains the target-local fault id, owning node and the
direct client which created it. Teardown removes entries newest first and
calls the specific delete endpoint. If a delete fails, the entry stays in the
ledger for a retry and cleanup becomes `Unknown`. We never call blanket
`fault clear`: it could reverse an operator's unrelated experiment. A council
partition is an ordinary node fault on `POST /v1/fault`, so it hands back the
same fault summary as every other injection. It used to have its own
`/v1/chaos/partition` endpoint with a `chaos heal` that cleared everything.
Nothing had shipped, so we deleted both rather than keep a second path.

The regression tests drive that guard through both timeout and panic, observe
two exact injections and two exact reversals, and require confirmed cleanup.
Another test starts with deliberately expired evidence, serves a fresh
snapshot from Bun, and proves a chaos case receives the fresh one. These are
small tests for an unpleasant class of failure. That's exactly why they're
there.

## When faster is worse

Suppose one benchmark falls from 100 ms to 80 ms and another falls from 100
requests per second to 80. The arithmetic is identical. The verdict isn't.

Every benchmark metric therefore carries `higher_is_better`. Comparison uses
that field rather than guessing from the unit. Latency rising by more than 10%
is a regression; throughput falling by more than 10% is a regression. Exactly
10% is the boundary, not a failure. Metrics missing from either report get
listed separately because absence isn't performance data.

The value alone still isn't enough. Fifty resolver requests and two hundred
resolver requests don't describe the same experiment, even if both results say
`ms`. Each metric carries a sorted map of parameters such as request count and
payload bytes. Rust's `BTreeMap` keeps keys ordered, which makes JSON output
and compatibility diagnostics deterministic. Different units, direction or
parameters refuse comparison.

The report also fingerprints the environment:

- cluster node and council-member counts;
- every node's compilation target and profile;
- runtime name, version and rootless mode; and
- kernel and architecture.

What isn't a compatibility check? The binary version and Git SHA. Comparing
two different builds is the point. We record both, then permit them to differ.
Node names and cluster identity may differ too; two otherwise identical
clusters can provide a useful comparison.

Hosted workers are a special case. Their neighbours, CPU allocation and noisy
storage can change underneath a run. If either report says it came from a
hosted environment, we still calculate and show regressions, but mark the
whole comparison informational. A shared CI worker shouldn't veto a release
because somebody else's job woke up at the wrong moment.

The JSON parser is deliberately strict. `schema_version` is 2 and every report
type rejects unknown fields. An additive change therefore needs a schema bump
instead of being silently ignored by an older Relish. Duplicate metric names,
zero samples, negative or non-finite values and unknown node fingerprints also
refuse a verdict.

One small Rust detail prevents a surprisingly awkward bug. Percentage change
usually looks like this:

```text
(current - baseline) / abs(baseline) × 100
```

A zero baseline makes that percentage undefined. JSON has no portable
representation for infinity, so `change_percent` is `Option<f64>`: `Some(x)`
for a finite percentage and `None` (JSON `null`) for a zero baseline. The
directional result still works from the raw values. We keep the evidence
without producing invalid JSON. Tiny type, useful constraint.

## Measure the journey, not the shortcut

Our first benchmark sketch timed service discovery with the resolve API. It
timed an HTTP handler and a map lookup. Useful perhaps, but it didn't answer
the question an application cares about: how long does a DNS query from my
container take?

The same shortcut appeared in the network test. Fetching a backend's published
host port bypasses the service name, VIP and packet redirection. A wonderfully
fast result would merely prove that we hadn't measured Reliaburger.

The implemented paths are more prosaic:

1. deploy a source BusyBox container under a server-owned lease;
2. execute `nslookup target.namespace.internal` inside it for discovery;
3. execute `wget` against that name for throughput; and
4. let the request cross Onion's service VIP before it reaches a backend.

The number includes process-exec overhead. That's deliberate and recorded in
the metric's `parameters` map, so it can't be compared with a future raw-socket
method by accident. If exec overhead dominates the result, the next honest
step is a small benchmark client inside the pinned image. Switching back to
the control API isn't.

Every suite has a 60-second quick deadline or a five-minute full deadline. The
runner starts the suite with `tokio::spawn`, then waits with
`tokio::time::timeout`. A `JoinHandle<T>` is Tokio's owned reference to a
spawned task. Awaiting it produces either the task's value or a `JoinError`
(for example, when the task panics).

Dropping a `JoinHandle` does not cancel its task. That catches people coming
from ordinary scoped threads, and it would be dangerous here: a timed-out
capacity probe could keep creating workloads in the background. The runner
calls `abort()` and then awaits the handle so cancellation has actually
settled.

Now, the awkward bit. Aborting the suite also prevents its local cleanup code
from running. We keep lease IDs and chaos fault IDs in cloned guards outside
the task. The server persists each lease, while the client guard uses an
`Arc<Mutex<Vec<_>>>` for IDs added during the run. After success, error, panic
or timeout, the runner reverses exact owned faults and asks Bun to release
every lease. It accepts a metric only after both cleanup paths are confirmed.

This is why schema 2 has both `skipped` and `failed`. A missing optional
capability discovered before provisioning is a skip. Once a suite starts, a
timeout, API error, panic or uncertain cleanup is a failure. Calling the latter
a skip would make the report look healthier as the system became less
responsive. That's quite a trick, but not a useful one.

Two probes need another lock. State reconstruction really kills the observed
council leader, so it needs `--disruptive --yes`. Capacity deliberately fills
the scheduler with one-millicore, one-mebibyte workloads and needs
`--capacity --yes`. These flags record the operator's intent. The authenticated
role and server-owned operation policy still decide whether anything happens.
Relish adds a dedicated capacity-probe header to each saturating apply. Bun
accepts it only with a live lease, Admin role, `saturate_capacity` and the
protected-cluster mutation gate. The client isn't its own safety authority.

Image distribution has one remaining caveat. Pickle doesn't yet provide
lease-owned manifest deletion or deterministic cache eviction. Pushing a
fresh 16 MiB image per run would leave permanent catalogue entries. Instead,
the current metric deploys the pinned multi-architecture image once per node
and records `cache_state = uncontrolled`. That measures current image
availability, not a clean cold-cache replication experiment. The report says
so. We can build the stronger benchmark after we can clean up its evidence.

## Diagnosis starts with what we actually know

Imagine `relish wtf` says every certificate is healthy. Good news. Except it
never managed to read certificate metadata. That's not healthy. It's a blank
space wearing a green hat.

The diagnosis engine therefore accepts an evidence snapshot rather than a
pile of convenient values. Each source has one of four states:

```rust
enum Evidence<T> {
    Available { observed_at: u64, value: T },
    Degraded { observed_at: u64, value: T, reason: String },
    Unavailable { reason: String },
    Unsupported { reason: String },
}
```

The angle brackets make `Evidence` a generic enum: `T` stands for the type of
value a particular source returns. `Evidence<Vec<NodeObservation>>` and
`Evidence<CouncilObservation>` share the same availability rules without
turning their very different data into untyped JSON. The compiler still knows
which value belongs where.

`Degraded` carries facts we can still use, plus the reason the inventory isn't
complete. `Unavailable` means the source should work but didn't answer.
`Unsupported` means we don't yet expose the fact needed for an honest verdict.
All three produce an `Unknown` report entry. None produces OK. This sounds
fussy until a diagnostic command is the thing you reach for during an outage.
Then it's the whole point.

The snapshot also supplies `collected_at`. The pure function
`diagnose(&WtfInputs)` never reads the wall clock or makes a network request.
The same captured evidence always produces the same report, which makes every
time window deterministic in a unit test.

Take crashloops. A workload with a lifetime restart count of 30 may have run
perfectly for six months since its last failure. Calling that a current
crashloop is nonsense. We require three timestamped restart events inside the
last 15 minutes. When that pattern fires, the engine looks for a deploy of the
same application and namespace in the preceding 30 minutes, then attaches the
first recent error log. One finding now answers three questions: what failed,
what changed, and what did the process say?

The same rule keeps the other checks honest. Disk pressure needs used capacity
with a node and storage-domain attribution. CPU throttling needs an increase in
cgroup throttled time, not high CPU usage. Certificate expiry needs public
`not_after` and rotation state, never private key material. If Bun can't expose
one of those facts yet, `wtf` says so.

Output uses four separate lists: `critical`, `warnings`, `unknown`, and `ok`.
The report carries schema version 1 and rejects unknown top-level fields while
deserialising. This is less forgiving than Serde's default. That's useful for
an automation contract: adding a field requires us to decide whether an older
consumer can understand it, instead of silently pretending that it can.

Finally, application scope is structural. `relish wtf --app api` runs app
checks and filters alerts, restarts, services and deploys to `api`; it doesn't
manufacture `Unknown` rows for cluster evidence it deliberately didn't fetch.
Unknown means missing required evidence. It doesn't mean "we chose not to ask".

## A small diagnostic surface

Mayo already records CPU usage. Could `wtf` call an application throttled when
that number reaches its configured limit? No. A busy process can use its full
entitlement without the kernel ever delaying it, while a bursty process can
accumulate throttled time between ordinary usage samples. They are different
facts.

Bun's authenticated `/v1/diagnostics` endpoint reads the cgroup v2 `cpu.stat`
counter twice. The window defaults to one second and the server clamps it to
ten. We subtract the first cumulative `throttled_usec` value from the second
and report seconds of actual throttling. If an instance appears, disappears or
resets its counter during the window, the source becomes degraded or
unavailable. Churn isn't zero throttling.

The endpoint also resolves each configured storage domain to its containing
filesystem. It returns the domain attribution with byte counts
and a percentage, but never the configured host path. The distinction matters:
"disk is full" is less useful than "the filesystem holding image layers is
full", and an API response doesn't need to publish the server's directory
layout to say either.

Certificates get the same treatment. The wire carries only the certificate
kind, identity, issuer, serial, expiry time and rotation state. It does not
carry DER, PEM or private keys. Right now Bun can safely expose its node leaf,
but it doesn't expose a complete workload inventory and it can't hot-reload a
new node leaf into every cluster transport. The source therefore says
`degraded`. `wtf` can still warn if that known leaf approaches expiry, but it
won't claim that certificate rotation is healthy.

Notice what the endpoint doesn't do. It doesn't diagnose anything. It records
local facts with a schema version and collection state. Relish will fan those
facts out across the cluster and the pure engine will decide what they mean.
Keeping observation and judgement separate lets us test both halves without
teaching the API handler a second, subtly different diagnostic catalogue.

## Asking the whole cluster

The pure function is useful, but operators can't diagnose a cluster by writing
a Rust value by hand. Relish now builds that value from live APIs.

It starts with the configured Bun node. That first health check is special: if
it fails, Relish stops. Without an entry node it can't know which peers should
exist, so a partial report would look more authoritative than it is. Once the
entry answers, Relish reads the membership view and fans requests out to every
expected node concurrently. Each peer gets the same transport, credentials and
certificate authority as the entry client, and every request has a ten-second
limit.

This introduces another useful Rust pattern. We can create a `Vec` of futures
and pass it to `join_all`. The futures run concurrently, but the returned
results retain their input order. Each result is a `Result`, so one dead node
doesn't cancel evidence from the healthy ones. The collector turns the failed
part into `Evidence::Unavailable` or `Evidence::Degraded` and carries on.

The collector asks a separate question for services: what *should* exist? Bun's
authenticated `/v1/diagnostics/apps` view supplies desired and scheduled
replica counts. Relish compares those with the live resolver table. If we only
walked the resolver, a service with no entry would vanish from the diagnosis.
That is precisely the broken service we wanted to find. Absence is evidence
only when you also have the intended state.

We put bounds on correlation too. Relish fetches recent logs only for
applications that already have enough timestamped restart events to be
crashloop candidates. It caps the number of candidates and lines, and accepts
stderr or structured error levels rather than matching an alarming word in a
perfectly ordinary sentence. The restart and terminal deploy stores remain
bounded and process-local. Even an empty response therefore arrives as
`Degraded`, because restarting Bun can erase relevant history. Honest, if a
little annoying. That's better than green.

Finally, the shell contract is small enough to remember. `relish wtf` returns
0 only when every selected source was observed and healthy, 1 for a critical
finding, and 2 for warnings or unknown evidence. `--app` removes unrelated
cluster checks structurally. `--watch` repeats the human report every 30
seconds, while JSON and YAML remain one schema-versioned report per process.

One check arrived late, from the homepage tour. With one of three frontends
gone for five minutes, `wtf` reported that every service had a healthy
backend. Quite true: two healthy backends is still "a healthy backend". So
`wtf` now also compares each app's running replicas with its desired count,
using the placements the leader recorded and each node's own instance list,
and warns with the missing replicas' whereabouts: `1 replica placed on
rb-…-3 is not running (rb-…-3 is not a live member)`, or `1 replica has no
placement yet` when the scheduler hasn't found room.

### One door in

The first laptop cluster broke that fan-out without a single error message.
Each node advertises its API on its own guest address, say
`192.168.104.3:9117`. From inside the cluster that's fine. From a Mac, behind
Lima's user-mode network, only node 1's port is forwarded to `127.0.0.1`.
Every peer request timed out, and `wtf` dutifully reported two of three nodes
as unreachable on a perfectly healthy cluster. Honest about what it saw, and
wrong about the cluster.

We could have tried each advertised address first and fallen back when it
didn't answer. That means a ten-second wait per unreachable node on every run,
and two code paths whose difference only shows up on somebody's laptop. So
there's one path: every per-node request goes through the entry node. The
client builds a relayed client with `BunClient::via_node`, whose base URL is
`{entry}/v1/nodes/{node}/relay`, and every existing method (`health`,
`diagnostics`, `events`, `probe_path` and friends) just works, because each one
formats its path onto the base URL.

The relay on the server is deliberately not a proxy. It forwards a short list of `GET`
paths and two kinds of `POST` (`/v1/path`, and `/v1/exec/{app}/{namespace}`
since the V02 triage found `relish exec` could only reach instances on the
entry node), refuses everything else with a 404,
forwards the caller's own `Authorization` header and never the node's service
token. So a relayed request can't do anything the caller couldn't do by
dialling the node directly. A test proves it: a token scoped to one app asks a
peer's `/v1/status` through the relay and gets back only that app's
instances. If the relay had quietly used the node's identity, the peer would
have shown everything.

### The catalogue needed the same door

`wtf` got its door in; `relish test` didn't. The V02 soak ran
`relish test --profile full-runc --filter volumes,image-registry,workload-identity,ingress,deployments`
from the Mac against the quickstart cluster, every cycle for twelve hours, and
the same ten of fifteen cases failed every time. Two separate reachability
bugs, neither of them in the cases themselves.

The volume, ingress and identity cases timed out. Each waits for its app with
`wait_running_cluster`, which fans out to every node's `/v1/status` through
`TestContext::node_clients`, and `node_clients` dialled each node's advertised
API address. Including the entry node's own, which on a Lima guest is its
guest port and means nothing on the Mac. The poll loop treats an unreachable
node as "try again", so each case spun until its five-minute deadline, and
cleanup then couldn't confirm anything either ("could not inspect cleanup on
node rb-…-1"). The fix has two halves. The entry node is now the connection the
run already has, always. Peers go by a `PeerRoute`:

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PeerRoute {
    #[default]
    Direct,
    Relay,
}
```

`#[derive(Default)]` on an enum needs to be told which variant is the default,
and `#[default]` on `Direct` does that. It's what the bench suites and unit
fixtures get without having to say so.

Why not relay everything, as `wtf` does? Because inside the cluster network
the direct route is the one the chaos catalogue was designed around: it injects
a node fault through the target's own API and clears it the same way, which
keeps working even after every peer has forgotten the target. The relay
refuses fault `POST`s by design (there's a test that says so). So `relish test`
keeps the direct route wherever it works, and asks once per run. `PeerRoute::detect` tries each peer's `/v1/health` directly, and only if
one fails does it try the same peer through the relay. Direct everywhere means
`Direct`; a peer that answers only through the entry node means `Relay`; a peer
that answers neither way is simply down and decides nothing. That's one probe
per peer per run, and it only costs its five-second timeout when a peer really
is out of reach.

The chaos catalogue then needed three more things, because the soak's
`dead_worker_node_has_workloads_rescheduled` timed out from the Mac in exactly
the same way. First, a node fault can't go through the relay, but it doesn't
have to: Bun already routes a node fault, and its reversal, to the
`target_node` the request names. So `TestContext::fault_owner` hands the chaos
guard the target's own client on a direct route and the entry node's client on
a relayed one. Second, the observer that watches the target go `dead` and come
back may itself be a relayed peer, so `GET /v1/cluster/nodes` joined the relay's
reads. Third, and this one is on the server: a node-kill closes the target's
cluster transports but leaves its API open, and the relay only knew *live*
members. The moment gossip declared the target dead, every relayed status poll
and the fault reversal got "target node is not alive or is unknown". Bun now
keeps a second table, `KnownMembers`, with every member gossip still knows
(alive, suspect or dead, not left), and the relay and forwarded reversals fall
back to it. Injection still refuses a node the cluster has lost. The table
reaches the handlers as an axum `Extension` layer rather than another
parameter on a constructor that already takes about thirty, and a handler asks for
it as `Option<axum::Extension<KnownMembers>>`, so a router built without it
(most tests) behaves as before.

So the relay's allow-list grew by two reads: `GET /v1/cluster/nodes`, and
`GET /v1/deploys/history/{app}` because the deployment cases compare each
node's own history. One app segment, nothing nested, like the exec rule.

The next soak ran the case again with all of that in place. It timed out
again, at 600 s, and this time cleanup was confirmed. So was it the
rescheduler? Rescheduling after a node dies is about as core a promise as an
orchestrator makes, so we checked that first. A new cluster test,
`a_killed_worker_has_its_replica_rescheduled_on_the_survivors`, kills a
worker with the same fault the case uses (`NodeKill` with its containers) and
waits for three running replicas on the two survivors. It gets them in about
ten seconds.

The soak's own evidence said the same thing, if you knew where to look. The
status snapshots taken just before and just after the case show every
workload on every node with the same process id. A node-kill with
`kill_containers` would have changed all of them on its target. And the
leader, which logs "cannot place" every two seconds while a node holding a
pinned app is gone, logged nothing. The fault was never injected. The case
never got past its first line of real work: waiting for its three replicas to
run.

They never could. The chaos scenarios built their workload with
`httpd -f -p <port> -h /etc` and health-checked it on `/hostname`, expecting
`/etc/hostname`. The pinned BusyBox image has no `/etc/hostname` (Docker
bind-mounts one; we don't), and no `PATH` either. The ingress fixture learnt
exactly this on 17 September and was fixed then; the chaos module had its own
copy of the old spec and nobody ran it on a real runtime until the soak. The
script now lives in one place, `container_http_script`, which both fixtures
call: it writes its own `hostname` file, serves only that directory, and names
BusyBox by absolute path. A unit test parses each chaos spec and checks that
the file its health check asks for is one the script writes, and that every
external program is an absolute path.

Why did it take two soaks to see? Because the report said only "case
exceeded its 600000 ms deadline". The case's own wait knew precisely what it
was stuck on (three running replicas, and the states it last saw), but it
gives up on the same deadline the runner enforces, and the runner won the
race and cancelled it before the message came back. Now each poll helper also
leaves a note on the context as it goes, a `WaitNote`, which is a newtype
around `Arc<tokio::sync::Mutex<Option<String>>>` so every clone of the context
shares one note. On a timeout the runner appends it:

```rust
if let (Some(note), TestOutcome::Unknown { reason, .. }) =
    (context.wait_note.take().await, &mut outcome)
{
    reason.push_str("; it was still ");
    reason.push_str(&note);
}
```

That `if let` matches a *tuple* of two values at once, so the body runs only
when there's a note *and* the outcome is the `Unknown` variant. The `..` skips
the fields we don't need, and matching on `&mut outcome` binds `reason` as a
mutable reference into the enum, so we can extend the string in place without
rebuilding the variant. We didn't give the body a grace period to return its
own message instead: an existing test insists that a timed-out body stops at
its deadline, and a case that can keep acting after it would be a worse bug
than a terse report.

The registry cases failed faster: "blob upload POST failed … https://127.0.0.1:5050".
They push to `ctx.registry_base()`, the registry origin from the capability
report. A managed connection replaces the node's listener with the host
forward from the local context (quickstart writes `https://127.0.0.1:15050`
there). But the soak picks a live node for every call, so it passes
`--endpoint https://127.0.0.1:<that node's API forward>`, and an explicit
endpoint bypasses the context completely. What was left was the node's own
listener, `0.0.0.0:5050`, rewritten to the API host: a port nothing forwards.

The context's credentials should stay bypassed; that's the point of
`--endpoint`. Its forwards are a different kind of fact. They describe how this
host reaches that cluster's registry and ingress, and they stay true whichever
node's API you picked. The only question is whether it *is* that cluster. The
answer relish already has is the CA it pinned for the connection, so
`LocalContext::forwards_for_ca` hands the forwards over only when the
connection's CA is byte-for-byte the context's:

```rust
let connection_ca_pem = connection_ca_pem?;
let context_ca_pem = std::fs::read(&self.ca_cert).ok()?;
(context_ca_pem == connection_ca_pem).then(|| self.service_endpoints.clone())
```

`?` works on `Option` as well as `Result`: `None` returns `None` from the
function straight away. `.ok()` turns the read's `Result` into an `Option`,
dropping the error, which is right here: an unreadable CA file means "no
forwards", not a failed command. And `bool::then` runs the closure and wraps
its value in `Some` only when the bool is true. No CA, a different CA or a
missing file all mean no forwards, and relish behaves exactly as before.

The last change was a message. A lease release that got through but wasn't
confirmed within its 30-second budget used to say "could not reach the lease
owner", the same words as a connection refused. It now says the owner didn't
confirm cleanup in time, which points at the server rather than the network.

`testkit::context` has unit tests for both routes (entry node on its own
connection, peers at `{entry}/v1/nodes/{node}/relay`, a `/v1/status` answered
through a fake relay) and for detection (relay when a peer answers only that
way, direct when every peer answers, direct when a peer answers neither way).
`chaos::scenarios` checks that a relayed node-kill is injected and reversed
through the entry node, naming the target both times, and `bun::api` checks that
the relay reaches a member gossip no longer counts as alive.
`local_context` tests the CA match. The integration test
`registry_cases_push_through_the_contexts_forward_with_an_explicit_endpoint`
runs the real `relish test --filter image-registry` with `--endpoint` against
a secure single node, with a context whose registry forward is a counting TCP
proxy on a different port. Both registry cases must pass, and the proxy must
have seen them. What none of this proves is a three-node laptop run end to end;
that's the next V02 soak's job.

It was. The soak on candidate 1117dbb finally injected the node-kill: the
three replicas ran, the target went quiet, the survivors picked up its
replica. Then the case failed anyway, twice in five cycles, with a reason
that should have been impossible by now:

```text
owned fault cleanup failed: 2: API error (status 404):
target node rb-3fd6f1a419a3-2 is not alive or is unknown
```

That's the very message `KnownMembers` was meant to retire. The table's doc
comment promised "alive, suspect or dead". But Bun fills it from the gossip
membership watch, and gossip publishes `MembershipTable::snapshot()`, which is
`active_members()`: everything that isn't *down*. Dead counts as down. So a
node-killed peer sits in the table while it's suspect, and disappears from it
at the exact moment it's declared dead. And the case clears its fault only
after the survivors have rescheduled the replica, which the scheduler does
only once the node is dead. The fallback covered a window the case never
reached. The unit test hadn't noticed because it built the known table by
hand, with the dead node in it.

The cost was bigger than one failed case. The fault stayed in force until it
expired, and a chaos case sets expiry to its own timeout plus a 30-second
margin: 630 seconds. In the first cycle the node was still dead when the
soak's 300-second settle gave up (21:02:59 BST, six minutes after the kill)
and when the heavy check found it missing, along with the pinned
`soak-writer` it should have been running. Only the soak's next graceful stop
brought it back, by restarting Bun, which forgets in-memory faults. In the
fifth cycle the next fault's settle took 223 seconds instead of 18, ending
seconds after the 630-second expiry.

We could have made gossip publish dead members too. Half the cluster reads
that watch, though (the scheduler, the council, the upgrade orchestrator,
Pickle), and they all rely on "published" meaning "worth talking to". The
question here is narrower: where did this node last say it lives? That's
memory, and the table can keep it:

```rust
pub async fn refresh(&self, current: Vec<NodeMembershipInfo>) {
    let mut table = self.0.write().await;
    let remembered: Vec<_> = table
        .drain(..)
        .filter(|old| !current.iter().any(|member| member.node_id == old.node_id))
        .collect();
    *table = current;
    table.extend(remembered);
}
```

`drain(..)` empties the vector in place and hands its elements over one at a
time *by value*, so we can keep some of them without cloning. The `..` is a
range with no ends, meaning "all of it". `filter` keeps the members that
`current` doesn't mention (`any` stops at the first match), `*table = current`
moves the new list in, and `extend` appends the remembered ones behind it. A
member that comes back simply appears in `current` again and its fresh address
wins. A member that has really gone stays listed and fails to connect, which
is the honest answer for a reversal aimed at it.

Two tests pin it down. In `bun::api`,
`a_node_fault_clear_reaches_a_member_gossip_has_declared_dead` drops a node
from the live table *and* refreshes the known table without it, as gossip
does, then clears its fault through another node. Before the fix it failed
with the soak's own 404. The cluster test
`a_killed_worker_has_its_replica_rescheduled_on_the_survivors` now carries on
where it used to stop: it waits until the router node's gossip no longer
publishes the target, clears the fault through the router, and waits for
every survivor to see the target alive again. Against the old table it failed
with the same 404; with the new one the whole test, cluster start to rejoin, takes
about twenty seconds. The test harness now attaches a `KnownMembers` fed
from gossip the way Bun does, so that path is no longer something only a
laptop cluster exercises.

CI then found the next link in the chain on its first run. The clear reached
the dead node, the node reversed the fault, and then answered 504:

```text
fault 1 is reversed on this node, but the cluster has not yet released its
reservation; retry the clear before injecting again
```

A node fault holds a cluster-wide reservation (one experiment at a time), and
the leader releases it only once it sees the healed node alive again. The
node waits 4 s for that before answering; on a busy CI runner, gossip took
longer. The message says "retry", and a retry is safe: the node keeps the
reservation's sequence number until the leader fences it, so asking again just
waits again. But nothing retried. Not the test, and not the two real clients
either, `relish fault clear` and the chaos cleanup, which would have reported
the case as failed with a perfectly healthy cluster. So the retry went into
`BunClient::clear_fault` itself, where every caller gets it:

```rust
loop {
    match self.clear_fault_once(id, node, acknowledged).await {
        Err(RelishError::ApiError { status: 504, .. })
            if tokio::time::Instant::now() + FAULT_CLEAR_RETRY_PAUSE < deadline =>
        {
            tokio::time::sleep(FAULT_CLEAR_RETRY_PAUSE).await;
        }
        result => return result,
    }
}
```

The first arm is a *match guard*: the pattern matches only a 504, and the
`if` after it adds a condition the pattern can't express, here "and there's
still time for another go". Anything else, success included, falls through to
`result => return result`, which binds the whole value to a name and hands it
back unchanged. The budget is 30 seconds, after which the caller sees the 504
it would have seen before.

The next candidate, 77f366b, failed all three volume cases with a 404 that
looked like a product bug:

```text
exec in vol-persist failed: API error (status 404):
{"error":"app \"vol-persist\" not found in namespace \"rbtest-…-09\""}
```

Nothing between the two candidates touched exec, the scheduler or the
volume cases. The cases had always sent their exec to the entry node, and
the entry node only execs into instances it runs itself. Every earlier pass
was placement luck: the soak pins its own volume apps to nodes 2 and 3, so a
fresh one-replica app usually landed on node 1. `relish exec` already finds
the node running the app, and so did the other catalogue cases, through
`TestContext::exec_in_workload`. The volume cases were the last to call
`ctx.client.exec` directly. They now go through the same helper, and a unit
test serves a fake entry node that answers exec with the real 404 while the
app runs on a peer behind its relay. It failed with the soak's message before
the change.

Reading the restart case closely turned up a second problem. Since stop
became asynchronous, `stop` returns while the instance is still `stopping`.
The case redeployed straight away, which Bun refuses for a workload that's
still stopping, and if the redeploy had got through, the "running" instance it
waited for could have been the old one (the replacement reuses its id), so
the marker check would have passed without a restart at all. It now waits
until every instance is `stopped` or `failed` before redeploying.

With the exec going to the right node, the next candidate's fast tier failed
the restart case for real: the marker was gone. The case was right and the
product was wrong. A stopped app forgot which node held its volume, so the
redeploy could start it on another node with an empty one. Chapter 7 has the
fix.

The candidate after that, 0eb6071, failed the dead-worker case again, and the
504 was back with different words:

```text
owned fault cleanup failed: 2: API error (status 504):
node fault reversal request to rb-…-2 timed out
```

This time it wasn't the reservation. That message comes from the entry node,
which gives the owning node five seconds to answer a forwarded clear. Node 2
did reverse the fault (its journal says so at 21:15:12 BST), but it never
answered anyone in time again: seven minutes later `relish wtf` still reported
"rb-…-2 did not answer", and every agent query to it timed out. Its journal
showed why. The fault had killed all six of its containers, so every replica
was a pending restart, and every restart was stuck waiting for the cluster to
confirm its old endpoint was withdrawn. Each health tick walked all six, and
each one spent about 400 ms on runtime cleanup it couldn't finish yet. A tick
took two and a half seconds, and the tick interval is one.

The agent loop is one `tokio::select!` over the command channel, a few other
queues and that one-second tick. By default `select!` polls its branches in a
random order, which is fair when every branch is cheap. Here the tick was
always due, because ticks that fall behind fire at once, so every queued
command had to win a coin toss against a two-and-a-half-second tick, and lost
about half the time. The fault clear, the leader's fence and, worst of all,
the consumer-view update that would have let the restarts finish all queued
behind ticks and timed out. The node had talked itself into a corner: the
restarts kept the loop busy, and the busy loop kept the restarts from ever
finishing.

The fix is one word:

```rust
tokio::select! {
    biased;
    _ = self.shutdown.cancelled() => { /* ... */ }
    Some(req) = Self::recv_snapshot(&mut self.cluster) => { /* ... */ }
    Some(cmd) = self.command_rx.recv() => { /* ... */ }
    // stop completions, deploy operations ...
    _ = health_interval.tick() => { /* the periodic work */ }
}
```

`biased;` is a directive to the macro, not a branch: it tells `select!` to
poll the branches top to bottom every time. Shutdown wins over everything, a
waiting command over any background work, and the tick runs only when nothing
else is waiting. A command now waits for at most the tick already running.
The obvious risk is starvation, a branch near the top that's always ready and
never lets the tick run. Commands are requests with replies, so each caller
waits for its answer before sending the next; nothing floods the channel.

The test builds exactly that node: three replicas in pending restart, a mock
runtime that takes 400 ms to fail each cleanup, so every tick lasts 1.2 s. It
waits for a tick to start, queues eight status commands, and counts the
cleanups that run before all eight are answered. At most the rest of the
running tick is allowed. Before `biased;` it counted 57, nineteen whole ticks
for eight commands, and took 23 s; now it takes about three.

The same failure also showed a budget that didn't add up. The owning node's
clear handler waited for its agent without any deadline, then waited up to
4 s for the reservation release, while the forwarder gave it 5 s in total.
Any agent delay over a second meant the forwarder's timeout, not the owning
node's own answer. Both numbers now come from one constant:

```rust
const NODE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const NODE_FAULT_CLEAR_BUDGET: Duration =
    NODE_REQUEST_TIMEOUT.saturating_sub(Duration::from_secs(1));
```

`saturating_sub` is a `const fn`, so the compiler works out the budget and a
change to the forwarding timeout carries the clear's budget with it. The
handler sets one deadline on arrival, and both waits share it:

```rust
let deadline = tokio::time::Instant::now() + NODE_FAULT_CLEAR_BUDGET;
let Ok(cleared) = tokio::time::timeout_at(deadline, cleared).await else {
    return (StatusCode::GATEWAY_TIMEOUT, /* "...; retry the clear" */)
        .into_response();
};
```

`let ... else` is a refutable `let`: if the pattern (`Ok(cleared)`) matches,
the binding lives on in the rest of the function; if it doesn't, the `else`
block runs, and it must leave the function (`return`, `break` or a panic).
It's the early-return guard from Go, minus the separate `if err != nil`. A
busy agent now gets the client a 504 that says "retry", which `clear_fault`
already retries, and the clear that was queued still runs. The API test
stalls the agent completely and checks that the clear answers inside
`NODE_REQUEST_TIMEOUT`. Before, it never answered at all.

Was it the new connection timeouts in 0eb6071? No. The forwarded clear
reached node 2 and ran there; the request was slow on the far side, not lost
on a dead pooled connection. The earlier candidates were lucky: the loop was
as unfair then, and the case passes whenever the coin tosses go its way.

### Busy isn't dead

`biased;` bought fairness for commands, and the final tier of the soak on
ff854cb found who paid for it. During the `relish test` pulse, node 3 logged
`report worker: snapshot collection failed or timed out` every five seconds
from 09:12:08 to 09:13:48 BST, and node 2 did the same in bursts until
09:15:46. After 30 s without a report the leader marked each node stale, the
scheduler called it not ready, and it moved apps off perfectly healthy nodes.
Apps pinned by label had nowhere to go: node 1 logged `cannot place
default/soak-redis: no eligible nodes` every two seconds for 46 s.

The report worker doesn't read agent state itself. Once per interval it sends
the agent loop a snapshot request and gives it two seconds to answer. In the
biased order that request sat *below* the command channel, and the pulse keeps
that channel busy: four test cases polling instance status, the placement
reconciler, deploys and retirements, and every status request asks the runtime
for each instance's pid and exit code. The earlier argument that "each caller
waits for its reply, so nothing floods the channel" holds for one caller. It
doesn't hold for a dozen of them taking turns. The channel was rarely empty,
so the one request that decides whether the node is alive waited behind all
of it, missed its deadline, and was answered later into a closed channel,
which cost another inventory read.

There were two fixes, one at each end of the channel. In the loop, the snapshot
branch moved up to sit straight under shutdown, and a request the worker has
already given up on is dropped unbuilt (`req.response.is_closed()`). A report
arrives once every five seconds and costs a bounded amount of work, so it
can't starve commands. It now waits for at most the one operation already
running.

That operation can still be long. Some of the loop's work awaits other
machines: signing a follower's workload certificate goes to the leader with a
ten-second limit. So the worker no longer treats a slow loop as a dead node. A
report says two things: "I'm alive" and "here's what I run". Only the second
needs the loop. When the loop misses the deadline, the worker re-sends the
last snapshot it *did* answer, stamped with the time it was taken, and reads
readiness straight from the node's `ReadinessTracker`, which never needed the
loop in the first place:

```rust
let (snapshot, observed_wall) = match self.collect_snapshot().await {
    Some(snapshot) => { /* remember it, with both clocks */ }
    None => match self.recent_observation() {
        Some(observed) => (observed.snapshot.clone(), observed.observed_wall),
        None => return, // nothing honest to send
    },
};
```

Is re-sending old state a lie? It tells the leader nothing it didn't already
hold: the aggregator keeps a stale node's last report either way. The only
thing that changes is the verdict. The loop is the only writer of the agent's
state, so while it's stuck nothing it knows has changed either. A loop that
*never* answers again is wedged rather than busy, and a wedged node should
still be fenced. So `recent_observation` only offers a snapshot younger than
four stale windows (two minutes by default). After that the worker goes
quiet and the leader does what it did before.

`recent_observation` returns `Option<&ObservedSnapshot>`, a reference into the
worker's own field. The `&` means the caller borrows the snapshot rather than
taking it. The report builder consumes its snapshot by value, so the call
site clones it and the cached copy stays put for the next busy interval.

The tests come in two halves. In the agent, twenty `Status` commands queue up
against a mock runtime that takes 100 ms per pid read, about six seconds of
loop work, and a snapshot request must still come back inside the worker's
two seconds. It didn't before. A second test queues three abandoned requests
ahead of a live one with a runtime inventory that always hits its one-second
limit. Before, the live one waited four seconds. In the worker, a test
answers one snapshot and then goes silent: eight intervals later the leader
is still getting the same `web` instance and readiness from the tracker. The
guard rails get tests too. After the two-minute grace the worker sends
nothing, a worker that has never had an answer sends nothing at all, and
neither does one whose agent loop has exited (its end of the channel is
closed, so it's gone, not busy).

### Remembering, with a limit

The `KnownMembers` fix earlier in this chapter had a quiet flaw: it never
forgot anything. Every node that ever joined kept its last address in the
table until Bun restarted. That's fine for a node-killed peer, which is the
whole point. It's less fine for a node that left on purpose, or one the
operator decommissioned last week. Neither holds a fault anyone could clear,
and a relay aimed at them just waits for a connection timeout.

So when should a remembered member go? There are three answers, and the
table needs all of them.

A node that *left* announced it. Gossip marks it `Left`, holds that entry for
a minute so everyone hears, then reaps it. But the refresher couldn't see any
of that. It read the same watch the scheduler reads, and that watch publishes
only members that aren't down. A member that left and a member that died
vanished from it in exactly the same way. Rather than change what that watch
means (the scheduler, the council and Pickle all treat "published" as "worth
talking to"), gossip got a second one. `MembershipTable::roster()` returns
every member, down ones included, until they're reaped, and
`MustardNode::set_roster_watch` publishes it next to the old view. The
refresher reads the roster. A `Left` entry is forgotten on sight.

A node the operator *retired* is gone for good: its identity is on the CRL,
and Chapter 4's mTLS verifiers already refuse it. Every node keeps a
`CrlHandle` fresh from Raft, so the refresher asks it for
`retired_node_ids()` and drops those too.

Everything else gets a time limit, and the question is how long. The only
reason to keep a dead node's address is to clear a fault on it. A fault can
only be injected into a live node, and Smoker clamps every fault to 24 hours
(`MAX_FAULT_DURATION_NS`). So a node that nobody has heard from in 24 hours
can't be holding a fault anyone could still clear. That's the bound:

```rust
pub const KNOWN_MEMBER_RETENTION: std::time::Duration =
    std::time::Duration::from_nanos(crate::smoker::types::MAX_FAULT_DURATION_NS);
```

We could have typed `from_secs(24 * 3600)` and been right today. Deriving it
from Smoker's ceiling says *why* it's 24 hours, and if someone ever raises
that ceiling, the table follows. `from_nanos` is a `const fn`, which is what
lets it appear in a `const` at all: Rust evaluates constants at compile time,
so only functions marked `const fn` can be called there.

The clock the table measures from is "last heard alive or suspect", not "last
seen in the roster". A dead member sits in the roster for another minute
before gossip reaps it, and restarting the clock on every one of those
refreshes would quietly stretch the limit. Each entry carries a `last_heard:
Instant`, and `refresh` takes `now` as a parameter instead of calling
`Instant::now()` itself. That makes the 24-hour tests instant: they pass
`start + KNOWN_MEMBER_RETENTION` and one second past it, and check that the
entry survives the first and not the second. The comparison uses
`now.saturating_duration_since(last_heard)`, which returns zero rather than
panicking if the two instants arrive out of order.

One catch: a reaped member has already left the roster, so nothing gossip
publishes will ever change when its 24 hours run out, or when it's retired.
The refresher used to wake only on a new roster or directory. It now also
ticks once a minute, which is plenty for a one-day limit.

Five tests cover it: a dead member survives being reaped (the #244
behaviour), a returning member's new address replaces the old one, a `Left`
member is forgotten and doesn't come back as dead once gossip reaps it, a
retired one is forgotten whether gossip still lists it or not, and the
retention boundary itself. In gossip,
`roster_watch_reports_down_members_the_active_watch_drops` checks that the
old watch still hides Dead and Left members while the roster shows them, and
that both views lose them once they're reaped.

The same table fixed a smaller annoyance. `relish nodes` listed gossip's live
view, so a node that died didn't show up as `dead`. It just wasn't there, and
you had to notice the gap. The watch that hid it can't change, for the same
reason as before, but the known table already holds exactly the rows we
wanted. So `nodes_handler` asks it for its `down()` members and passes them
to the agent inside the `Nodes` command. The agent lists its live members
first, then any down member gossip doesn't list any more. It gives both the
same council and leader check against Raft, because a dead voter is still a
voter, and an operator staring at a dead node wants to know that first.

Handing the rows to the agent instead of appending them in the handler is
deliberate: the agent is where the Raft metrics live, so that's where the
council flags get decided, once, for every row. The destructuring in its
loop is worth a second look:

```rust
(node.is_council, node.is_leader) = roles(&node.node_id, false, false);
```

That's a *destructuring assignment*: a tuple pattern on the left of a plain
`=`, assigning both fields at once. Rust has had it since 1.59; before that
you needed a `let` and two more lines.

Showing dead nodes also meant finding who relied on them being hidden. Three
callers did. `relish upgrade` builds its rollout from the listing and would
have tried to upgrade a dead node (and refused the whole plan if it had no
address). The test harness's node inventory would have tried to inspect one.
Its route detection would have spent two timeouts probing one. Each now
skips members for which `NodeStatus::is_down()` is true, which keeps their
old behaviour exactly, and each has a test with a dead member in the listing.

### Everyone below the line

Moving the snapshot request up fixed the one branch we'd caught starving. It
didn't fix the order. Candidate 11's final tier showed what else lived under
the command channel.

At 19:49 UTC the harness SIGKILLed the leader's Bun on node 1. That restart
hung for eleven minutes (the capture-file replay from Chapter 6), and while
node 1 was gone the leader moved its soak apps elsewhere. Node 3 ended up
running ten of the fourteen. Nothing moved them back; nothing had to. Then
every `relish test` pulse for the next two hours failed the same way: eight of
fifteen cases timed out "still waiting for identity-app to reach 1 running
replica(s) cluster-wide; last saw 0 instance(s)". The apply had committed. The
leader had placed the apps. No instance ever appeared.

The placement part is ordinary. Our scheduler bin-packs, so every test workload
went to the fullest node, which was now node 3 (node 2 during one pulse, after
a power-off shuffled things around). Node 3's journal (BST, an hour ahead of
the UTC above) had the rest:

```text
21:34:26 orchestrator: deploy of ing-web/rbtest-…-06 failed (attempt 1),
         retrying in 5s: the deploy did not reach a terminal event within 300s
21:34:28 orchestrator: deploy of ing-listed/rbtest-…-08 failed (attempt 1), …
21:39:40 orchestrator: deploy of vol-b/rbtest-…-10 failed (attempt 1),
         retrying in 5s: the deploy did not reach a terminal event within 300s
```

A deploy runs on its own task, and every step that touches the agent's state
(check the image signature, record the spec, create the instances) goes back to
the loop as a `DeployOp` and waits for the reply. Those ops sat below the
command channel in the `biased;` order. Four cases polled every node's status
without a pause, node 3 read the runtime of ten instances for each answer, and
the channel was never empty. Status kept answering, so the cases saw "0
instances" rather than an error, while the deploy's first step waited all 300 s
for a turn that never came. The tell is in the timestamps: the first wave of
cases hit its deadline and stopped polling at 21:34:22, and two seconds later
the next deploy on the same node had run every step up to publishing its
service. (It then failed for want of the service's cluster address, and the
reconciler retried it; that part is ordinary.) The health tick, at the very
bottom, starved too: during one
pulse `registry-web` did get an instance, and it sat in `health-wait` for the
whole five minutes, because health probes run from the tick.

Why did the pulses before 19:49 pass, and the one after 22:55 (a graceful
restart of the whole cluster put node 1's apps back)? Load. The
earlier pulses took 40 to 110 s per case, which already says the deploy steps
were fighting for turns; they just won often enough. Ten instances on the node
under test tipped them into never winning.

The fix changes the order, not the budget:

```rust
tokio::select! {
    biased;
    _ = self.shutdown.cancelled() => { /* ... */ }
    Some(req) = Self::recv_snapshot(&mut self.cluster) => { /* ... */ }
    Some(outcome) = self.stop_waits.join_next_with_id(),
        if !self.stop_waits.is_empty() => { /* ... */ }
    Some(op) = self.deploy_ops_rx.recv() => { /* ... */ }
    Some(cmd) = self.command_rx.recv() => { /* ... */ }
    _ = health_interval.tick() => { self.run_health_tick().await; }
}
```

The argument for putting a branch above commands is the one we got wrong the
first time, so it's worth spelling out. A branch may sit above another only if
it can't flood. Stop completions come from a `JoinSet` of stops already
started, one each. Every deploy op comes from a deploy task that sends one and
waits for its reply before sending the next, so there are never more of them
waiting than deploys (plus health probes) in flight, and a command waits for at
most that many short steps. Commands are the opposite: any number of API
callers, the reconciler and the reporting paths all send them, and a dozen
polite callers taking turns are a flood.

The tick can't move up like that. It's always due again after a slow round, and
that's exactly what #260 was about. So it gets a floor instead: if no tick has
run for `HEALTH_TICK_STARVATION_BOUND` (five seconds), the loop runs one before
it looks at the queues at all:

```rust
let starved = last_health_tick.elapsed() >= HEALTH_TICK_STARVATION_BOUND;
if starved {
    self.run_health_tick().await;
    last_health_tick = tokio::time::Instant::now();
    health_interval.reset();
} else {
    tokio::select! { /* as above */ }
}
```

`tokio::time::Instant` rather than `std::time::Instant`: the tokio one follows
tokio's clock, so a test that pauses time controls it too. `reset()` pushes the
interval's next tick a full period away, so the forced tick doesn't get a
second, back-to-back run from the interval. Commands still come before a tick
that is merely due; they just can't hold it off for more than five seconds.

Both tests start four callers that ask for status again the moment they get an
answer, against a mock runtime that takes 20 ms per pid read. That's the pulse
in miniature. In the first, a fresh deploy must complete within ten seconds; it
never did before. In the second, a replica dies and must be restarted, which
only the tick does; before, nothing happened for fifteen seconds. The #260 test
still passes: eight queued commands are answered with at most the running
tick's cleanups in between.

### Shorter turns

`biased;`, the re-sent snapshot and the tick's floor make a long turn of the
loop survivable. They don't make it short. Both investigations left a list of turns that could
still run for seconds, and the next release took them one at a time.

The first was the tick from #260 itself. `drive_pending_restarts` walked
*every* pending restart on every tick, and each one that couldn't clean up yet
cost about 400 ms. Three of them made a 1.2 s tick. Thirty would make a
twelve-second one, and a command queued behind it waits all twelve. So the
tick now has a budget:

```rust
const PENDING_RESTART_TICK_BUDGET: Duration = Duration::from_millis(500);

let deadline = tokio::time::Instant::now() + PENDING_RESTART_TICK_BUDGET;
for (index, (id, state)) in retrying.into_iter().enumerate() {
    if index > 0 && tokio::time::Instant::now() >= deadline {
        break;
    }
    self.restart_rotation.cleanup = Some(id.clone());
    // ...
}
```

`enumerate()` pairs each item with its position, like Python's `enumerate`.
The `index > 0` guard means a tick always makes progress on at least one
restart, however slow. A budget on its own would starve somebody, though.
Collect the pending restarts in the same order every tick, stop after two,
and the same two get retried forever while the rest wait. So the agent
remembers the last instance each phase touched, and the next tick starts just
after it:

```rust
fn rotate_after<T>(
    mut items: Vec<T>,
    last: Option<&InstanceId>,
    id: impl Fn(&T) -> &InstanceId,
) -> Vec<T> {
    items.sort_by(|a, b| id(a).0.cmp(&id(b).0));
    if let Some(last) = last {
        let start = items.partition_point(|item| id(item).0 <= last.0);
        items.rotate_left(start);
    }
    items
}
```

`id: impl Fn(&T) -> &InstanceId` takes any closure that can pull an instance
id out of an item, so both phases (which carry different tuples) share one
helper. It's still a generic function, compiled once per closure type, with
no function pointer or boxing involved. `partition_point` is a binary search
on a sorted slice: it returns the index of the first item for which the
predicate is false, here the first id after the one we stopped at.
`rotate_left` then moves everything before that index to the back, in place.
An instance that disappeared since the last tick doesn't matter: the search
finds where it *would* be, and the walk carries on from there.

Two tests pinned it down. One put eight replicas into pending restart, each
spending 400 ms and failing, and checked that a single tick finished well short
of the 3.2 s the old walk took, without trying all eight. The other ran six
ticks over six slow restarts and checked that each got its turn and that no
instance got more than one attempt more than any other. (Both changed again
when the restarts themselves left the loop; see "Three awaits leave the loop"
below.)

The second long turn was the one #270 named: signing a follower's workload
certificate. Only the leader holds the Workload CA, so a follower sends its
CSR over mTLS and waits up to ten seconds for the answer. That wait ran inline
twice over, from the deploy worker's `ProvisionIdentity` request and from the
rotation tick. A slow leader meant ten seconds of nobody else getting an
answer from the node.

We already had the shape of the fix. #237 moved stop waits off the loop by
splitting a stop into the part that must happen on the loop, the part that
only waits, and the part that records the result back on the loop. Signing
splits the same way. The loop generates the CSR (the private key never leaves
the node), then spawns the signing into a `JoinSet`:

```rust
let task = self
    .identity_signing_tasks
    .spawn(async move {
        let result = tokio::time::timeout(WORKLOAD_SIGNING_TIMEOUT, request.sign())
            .await
            .unwrap_or_else(|_| Err("workload signing timed out".to_string()));
        SignedIdentity { spiffe_uri, private_key_der, result }
    })
    .id();
```

`async move` makes the future take ownership of everything it uses: the
`Arc` to the council, a clone of the leader transport, the CSR bytes. That's
what lets it outlive the function that spawned it, and the compiler insists
on it, because `spawn` requires a `'static` future: one that borrows nothing
from the caller's stack. In Go you'd capture the variables and hope nobody
changes them underneath the goroutine. Here the borrow checker won't compile a
future that could see them change.

A new `select!` branch collects finished signings with `join_next_with_id`,
writes the certificate files and records the identity, all on the loop, as
before. It sits next to the stop completions, above commands, and passes the
same test: a signing is only in flight for a provision the loop already
started, at most one per instance, so finished ones can't flood the loop. The deploy worker's reply travels with the signing and is answered
only when it's done, so the worker still waits, just not *on* the loop. Two
details came out of the split. The rotation tick runs every second and a
signing can take ten, so a request for an instance that already has a signing
in flight joins it rather than sending a second CSR. And a signature can now
arrive after its instance has been retired; retirement deleted the identity
directory, so writing the files would recreate it with nobody left to delete
it. The loop checks the instance still exists before it writes anything.

The tests stand up a follower with a "leader" that accepts the TCP connection
and then says nothing. The deploy worker's identity request must return in
under a second, with its reply still pending (it took the full ten before);
the rotation tick likewise. A leader that hangs up at once checks the other
end: two requests for one instance share one signing, and both get answered
when it fails. The last test hands the loop two finished signatures, one for
a live instance and one for a retired one, and checks that only the live one
gets files.

The third was status itself. Every `Status` command asks the runtime for each
instance's pid and exit code, and runc answers both under the instance's
lifecycle lock. A create that's pulling a slow image, or a stop waiting out
its grace, holds that lock for seconds, and `get_status` waited on it with no
deadline, one instance after another. One slow instance stalled the answer
for all of them, and the loop with it.

What should status say about an instance the runtime won't talk about yet?
Not nothing: the agent still knows its app, state, port and restart count.
Not "no pid" either, because that means something (the process has gone).
So the answer carries what's known and says which part isn't:

```rust
/// The runtime didn't answer for this instance before the status
/// deadline, so `pid` and `exit_code` are unknown rather than absent.
#[serde(default, skip_serializing_if = "std::ops::Not::not")]
pub runtime_unknown: bool,
```

The serde attributes keep the wire form backward compatible in both
directions: an older peer's JSON without the field reads as `false`
(`default`), and an unmarked status serialises exactly as before
(`skip_serializing_if` takes a function path, and `std::ops::Not::not` is the
`!` operator as a function). The CLI prints `?` in the PID column for a marked
instance, next to the `-` that means "no process".

The reads now share one deadline for the whole answer. They run eight at a
time with `join_all`, which polls a batch of futures together and returns
their results in order, each under `tokio::time::timeout_at(deadline, ...)`.
`timeout_at` polls the inner future once before it checks the clock, so an
instance that answers at once is never marked unknown just because an earlier
batch used up the time.

We first wrote this as a stream, `stream::iter(&instances).map(...).buffered(8)`,
and got 380 errors saying ``implementation of `Send` is not general enough``.
The closure passed to `map` takes a reference, which makes it generic over
that reference's lifetime, and the compiler can't yet prove a future built
from such a closure is `Send` for *every* lifetime, which `tokio::spawn`
needs. Plain `for` loops that push futures into a `Vec` sidestep the question,
and read just as well.

Three tests cover it. One instance of three holds its lock for thirty seconds:
status comes back well inside two seconds, with that instance marked, still
`running`, and the other two with their pids. Six instances all holding their
locks cost one deadline, not six. And the field round-trips, and stays out of
the JSON when it's `false`.

### A rule you can test

Look back over this story. Ten incidents, and every fix that held made a turn
shorter; every reorder of the `select!` fixed one starvation and uncovered the
next. The review we wrote after 0.1.1
([`docs/plans/2026-09-30-agent-loop-review.md`](../plans/2026-09-30-agent-loop-review.md))
weighed splitting the loop into actors against giving each instance its own
task, and we chose neither, for now. The single owner is why "withdraw the
backend before the runtime can reuse its address" is one line of code rather
than a protocol between two tasks. What we needed wasn't a new shape. It was a
way to notice, before a soak did, that somebody had put a slow `await` in the
wrong place. So "every turn is short" became three things you can run (#351).

The first is a number. Every turn is timed from the moment its branch fires
until the consumer view it may have marked stale is republished, and lands in
a histogram labelled by branch:

```rust
struct BranchHistogram {
    buckets: [AtomicU64; BUCKET_BOUNDS.len()],
    sum_nanos: AtomicU64,
    count: AtomicU64,
}

impl BranchHistogram {
    const fn new() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; BUCKET_BOUNDS.len()],
            // ...
        }
    }
}
```

`[x; N]` builds an array by copying `x` N times, which needs `x` to be
`Copy`, and an atomic isn't (two copies of one counter would defeat the
point). `const { ... }` is an *inline const*: a block the compiler evaluates at
build time, and an array can repeat a constant expression whether or not its
type is `Copy`. The loop increments with `Ordering::Relaxed`, as Wrapper's
round-robin counter does, because nobody needs the buckets to agree with each
other to the nanosecond, only to add up. The metrics collector holds the same
meter through an `Arc` and writes it into Mayo every interval as
`bun_agent_loop_turn_seconds_bucket{branch, le}`, plus `_sum` and `_count`,
the way Prometheus histograms look. Reading it is the usual game: the ratio
of `le="1"` to `le="+Inf"` for `branch="command"` is the share of command
turns that fit the 1 s budget. Any turn over 250 ms is also logged with the
command or deploy op that ran it, so the soak journal says `turn took 2514 ms
in command (inject_fault)` rather than "status timed out".

The second is a harness that provokes the slow turns on purpose. The review
listed every await the loop still made inline, and each gets a scenario:
make that one await slow, start the work that reaches it, queue a status
command, and require an answer within a second, with the meter's worst turn
under a second too. Three fakes make the awaits slow. `MockGrill` takes a
delay per kind of call (`set_call_delay(MockCall::State, ...)`); the council
gets a test hook whose writes never return, as they don't on a leader that
has lost its quorum; and a log follower gets a channel that nobody reads. The
disk and `nft` have no mock behind them, so the agent carries a test-only
`LoopStall` at those awaits:

```rust
#[cfg(test)]
self.loop_stalls.hold(LoopStall::Persist).await;
tokio::task::spawn_blocking(move || crate::grill::records::write_record(&dir, &record))
```

`#[cfg(test)]` works on a single statement, not just on items, so the stall
simply doesn't exist in a release build. A scenario whose await never
returns (a council without quorum) would never *finish* its turn, and a meter
that only records finished turns would call it a clean run. So under
`cfg(test)` the meter also remembers the turn in progress, and the worst turn
it reports counts that one's age so far.

Most scenarios fail today, which is the point: thirteen of the seventeen, from
`check_apps` reading ten instance states one at a time to a follow's tail
waiting on a client that stopped reading. They're `#[ignore]`d with the stage
of #351 that will fix them, and the fix un-ignores them. Four pass and stay
on: status with every pid read hung (the shared 500 ms deadline holds), a
council status over a busy desired state, a two-thousand-service catalogue,
and a slow disk. That last one mattered for a decision: we let fsync'd
persists stay inline rather than answer callers before their state is
durable, as long as a disk that takes 150 ms per write still leaves every
turn of a deploy, a job and a restart under the budget. It does, because no
turn persists more than once.

The third is a rule, and a test that reads the code for it. Clippy can't know
that `self.supervisor.grill().state(&id).await` might take seconds while
`self.routing_table.read().await` won't. So the loop's module doc states the
rule (no await on a turn without a deadline or a stated reason), and every
await that stays inline carries its reason where a reviewer will see it:

```rust
// LOOP-INLINE: stage 3 of #351: reading whole captures leaves the loop
let logs = self.supervisor.grill().logs(id).await.unwrap_or_default();
```

The test parses `agent.rs` and its submodules with `syn`, as the router
coverage test in Chapter 10 does. It starts at `run_loop`, follows every
`self.method(..).await` into that method's body, and treats any other await
as a leaf that needs either a `tokio::time::timeout` around it or a
`// LOOP-INLINE:` comment with something after the colon. `syn`'s `Visit`
trait has a default method for every kind of syntax node, each of which just
walks the children; the checker overrides the few it cares about. Overriding
`visit_expr_async` with an empty body is how it skips `async` blocks, whose
code runs wherever they're spawned, not on the loop. Except when a block is
awaited in place (`async { ... }.await`, a common way to scope `?`), which the
first version skipped too, and so missed the runtime read an execution fence
ran inside one. The test that checks the checker now has that case in it.

The first run found 125 awaits with neither. A third were in-memory locks and
now say so (`in-memory lock, no I/O`), seven are the persists we decided to
keep, a couple of dozen have one-line reasons of their own, and fifty name the
stage that moves them. Grep for `LOOP-INLINE: stage`
and you have the remaining work, in the code, next to the line it's about.

### Three awaits leave the loop

The harness handed us a to-do list with failing tests on it. Stage two of
#351 took the top of the list: the tick's state reads for apps and jobs, and a
restart's kill, create and start. All of them are runtime calls the loop made
while it held `&mut self`, so for as long as runc took, nothing else on the
node could change. And the most frequent caller of all, status, waited in the
queue behind them.

#### Status stops asking

Start with the caller. Why does `relish status` queue a command at all? The
loop knows the answer the moment a turn ends, so now it says so, every time:

```rust
// Status readers answer from this, not by queueing for a turn.
self.publish_status();
self.loop_meter.finish(turn);
```

`publish_status` builds one entry per instance (id, app, state, port, restart
count) and puts the lot in a `tokio::sync::watch` channel. We met `watch` in
Chapter 2 holding the membership table: one slot with the latest value in it,
which any number of receivers read without waiting for the sender.
`send_replace` swaps the new snapshot in. The snapshot sits behind an `Arc`, so
a reader that has started on one keeps it even after the loop publishes the
next; cloning an `Arc` copies a pointer and bumps a counter, not the entries.
The API holds a `StatusReader` with the receiving end, and `/v1/status` reads
the slot without sending the loop anything.

An answer from the loop came with two guarantees we got for free. An answer
from a snapshot has to earn them.

The first is freshness. A loop stuck in a 30-second turn used to make status
time out, which is the truth: this node isn't answering. A snapshot from 30
seconds ago would answer cheerfully and wrongly. So the reader refuses old
snapshots:

```rust
pub async fn read(&self) -> Result<Vec<InstanceStatus>, StatusUnavailable> {
    let deadline = tokio::time::Instant::now() + STATUS_FRESHNESS_WAIT;
    let mut snapshots = self.snapshots.clone();
    loop {
        let snapshot = Arc::clone(&snapshots.borrow_and_update());
        if snapshot.age() <= STATUS_SNAPSHOT_MAX_AGE {
            return Ok((self.read_evidence)(snapshot).await);
        }
        match tokio::time::timeout_at(deadline, snapshots.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(StatusUnavailable::AgentStopped),
            Err(_) => {
                return Err(StatusUnavailable::Stale {
                    age: snapshot.age(),
                });
            }
        }
    }
}
```

An idle loop still turns once a second for its health tick, so a live loop's
snapshot is never more than about a second old, plus whatever the current turn
takes. `STATUS_SNAPSHOT_MAX_AGE` is two seconds. Past that, the reader waits up
to four for the next publication (`changed()` resolves when the sender writes
again) and then gives up with the same 503 a timed-out status command used to
get. `borrow_and_update` marks the value as seen, so `changed()` only wakes for
a newer one. And Chapter 2's lesson about `changed()` comes back in our
favour: it returns `Err` once the sender has been dropped, which is how a
reader tells an agent that has stopped from one that is merely slow. Every
answer also carries `status_age_ms`, so you can see how old it was.

The second guarantee is about the dead. The loop learns that a container
crashed from `check_apps` on its tick, and status used to report the instance
`running` until then. A reader that trusted the snapshot would do the same, for
up to two seconds longer. So the reader keeps the runtime reads status already
made (pid and exit code, eight at a time, under one 500 ms deadline), now off
the loop, and for any instance the loop last saw alive it also asks the
runtime for its state. If the runtime says the process has exited, the answer
says `stopped`, even though the loop hasn't noticed yet. That's an improvement
on the old behaviour, not just parity.

The privileged CI run caught two mistakes in the first version. The state read
ran *before* the pid read, inside the same 500 ms, and rootless runc spends a
good part of that on each call. So an adopted container came back with its
pid marked unknown. The three reads now run side by side with `tokio::join!`,
which polls several futures together and waits for all of them. The second
mistake was quieter. The agent created its `watch` channel with an empty
snapshot stamped "now", before it had adopted anything, so a request that
arrived in that window got a fresh-looking "no instances". The placeholder
is now explicitly unpublished, with an infinite age, and readers wait for the
loop's first real publication.

There was a type problem in the middle of this. The reads need the container
runtime, and `BunAgent<G: Grill>` is generic over it, but the API state isn't
generic and we didn't want it to become so. The reader stores the one
operation it needs as a boxed closure instead:

```rust
type EvidenceReader =
    Arc<dyn Fn(Arc<StatusSnapshot>) -> BoxFuture<'static, Vec<InstanceStatus>> + Send + Sync>;
```

`dyn Fn(...) -> ...` is a *trait object*: any closure with that signature,
whatever its concrete type, called through a pointer and a vtable. It's the
nearest Rust gets to a Go interface with one method, without declaring a
trait. `BoxFuture<'static, T>` is the `futures` crate's name for
`Pin<Box<dyn Future<Output = T> + Send + 'static>>`, a future on the heap that
borrows nothing, so it can be awaited anywhere. The agent builds the closure
around a clone of its grill:

```rust
let read_evidence: EvidenceReader = Arc::new(move |snapshot| {
    let grill = grill.clone();
    Box::pin(async move { read_status(&grill, &snapshot).await })
});
```

The generic type `G` is inside the closure, and nobody outside needs to know
it. The cost is one allocation per status request.

The `Status` command still exists, for the metrics collector and anything else
that only holds the command channel. The loop answers it by publishing a fresh
snapshot and spawning the runtime reads, so it costs a turn of microseconds,
not the old 500 ms.

#### The tick reads in parallel

`check_apps` and `check_jobs` asked the runtime for every running instance's
state, one after another, inline. Ten instances behind a slow runc were a
2.5-second turn in the harness. Now the tick only plans the reads, and spawns
them:

```rust
pub(super) async fn sweep_states<G: Grill>(grill: G, reads: Vec<StateRead>) -> StateSweep {
    let deadline = tokio::time::Instant::now() + STATE_SWEEP_DEADLINE;
    let grill = &grill;
    let observations = futures_util::stream::iter(reads)
        .map(|read| async move {
            let observed = tokio::time::timeout_at(deadline, observe(grill, &read))
                .await
                .unwrap_or(Observed::Unknown);
            (read, observed)
        })
        .buffered(STATE_READ_CONCURRENCY)
        .collect()
        .await;
    StateSweep { observations }
}
```

`buffered(8)` turns a stream of futures into a pool of eight running at once
that still yields results in the original order. All of them share one
one-second deadline; a read that misses it comes back `Unknown` and the next
sweep asks again. The sweep goes into a `JoinSet`, at most one at a time, and a
new `select!` branch applies what it saw.

Earlier in this chapter a `buffered` stream gave us 380 errors about `Send`
not being general enough, and we went back to plain loops. Here it compiles,
because each `StateRead` is *moved* into its closure: there's no borrowed
argument for the compiler to be generic over. Where we do borrow, in the status
reader's walk over the snapshot's entries, we build the futures into a `Vec`
first and stream the `Vec`. Same idea, different place to put the borrow.

A result that comes back a turn later can be about a container that no longer
exists. Between the read and its result the instance may have been restarted
under the same id, and an "exited" meant for the old container would restart
the new one. So every read carries the instance's incarnation:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Incarnation {
    created_at: Instant,
    restart_count: u32,
}
```

A restart bumps `restart_count`; a fresh deploy under the same id gets a new
`created_at`. The loop applies an exit only to the same incarnation, still
`Running`. Deriving `PartialEq` is what lets `==` compare two of them field by
field, the way a C programmer would hope `memcmp` would. The cost of all this
is latency: a crash now restarts on the tick after the sweep reports, up to a
second later than before. The restart backoff is longer than that anyway.

#### A restart, one step at a time

The restart was the longest turn of them all. It killed whatever was left of
the old container and waited for the runtime to confirm the exit (up to twice
the stop-confirmation timeout, twenty seconds by default), then created the
replacement and started it. Stops (#237) and identity signings had already
shown the way out: the loop does the bookkeeping, a task does the waiting, and
the result comes back through a branch.

A restart doesn't split into two halves, though. There's loop work *between*
the runtime calls: the job ledger and the old artifacts after the kill, the
replacement's egress before it starts. So each runtime call became a step of
its own:

```text
 tick              branch              branch               branch
 Pending ─Clear─▶ (Pending) ─Create─▶ (Preparing) ─Start─▶ (Starting) ─▶ HealthWait
                                        └─refused─Refuse─▶ Failed
 Stopping ─Cleanup─▶ Stopped ─▶ Pending (backoff permitting)
```

The loop spawns a step into `restart_steps`, a `JoinSet`, and records it in
`restarts`, a map from instance to the step in flight. One entry per instance
is the whole of the "no double restarts" rule: the tick skips any instance
already in the map, however slow its kill. When a step finishes, the loop
picks the next move by matching on three things at once:

```rust
match (step, result, launch) {
    (RestartStep::Clear, StepResult::Done { .. }, Some(launch)) => {
        self.restart_after_clear(id, launch, gate).await;
    }
    (RestartStep::Create | RestartStep::Start, StepResult::Failed(error), _) => {
        self.record_failed_restart(&id, &error.to_string()).await;
    }
    // ...
}
```

A tuple of enums in a `match` reads like a decision table, and the compiler
checks it like one: leave out a combination and it won't build. At most
`RESTARTS_IN_FLIGHT_LIMIT` (eight) restarts run at once, so a node that lost
every container asks runc for eight kills, not eighty.

The hard part was ownership. While the restart ran inline, nothing could
happen between its kill and its start, so a stop for the same app simply went
first or second. Now a stop can arrive while a create is running. Suppose the
stop's kill lands before the create finishes. It kills nothing, sees no
process, reports "stopped". Then the create finishes, the restart starts the
container, and a process is running that its operator has just been told is
gone. The rule we wanted: a stop wins over a restart, but it waits for the
step already talking to the runtime. Each restart carries a gate for that:

```rust
pub(super) struct RestartGate {
    cancelled: CancellationToken,
    lane: Arc<tokio::sync::Mutex<()>>,
}
```

A step task locks `lane` before it calls the runtime and checks `cancelled`
once it has it. A stop, on the loop, cancels the token first (synchronously,
so no new step can begin) and then hands the gate to its exit wait, which
locks the lane before it sends SIGTERM. Locking means waiting for the step in
flight, if any. After that, no step of the cancelled restart touches the
runtime again, and the loop drops whatever result the last one reports.

`Mutex<()>` guards no data. It guards time: holding it means "I'm talking to
the runtime about this instance". C and Go programmers know this as a plain
critical section. It's the tokio mutex, not `std::sync::Mutex`, because the
step holds the guard across `.await`, and a `std` guard held across an await
would block a runtime thread while it waits. A rolling deploy's retirement
takes the same gate: `BeginRetire` hands it to the deploy worker, which
settles it before it drains the old instance. So does shutdown. Settling is
bounded by the stop-confirmation timeout, and a step that outlasts it fails
the stop, which the caller can retry. That beats signalling a container a
step might be creating.

The step names are deliberate. They are the events a per-instance supervisor
would report, the option (b) the review deferred to 0.4.0's migration work,
so that work can absorb this rather than replace it.

#### What the tests say now

The four stage-two scenarios run without `#[ignore]` and pass: ten 250 ms
state reads, ten job reads, a restart whose kill takes 2.5 s, and one whose
create and start take 1.25 s each. Status answers each within the second, and
no turn reaches it. Eight of the seventeen scenarios now pass; the other nine
wait for stage three, and `grep -rn 'LOOP-INLINE: stage'` lists the awaits
behind them.

New tests pin the ownership rules. A stop and a retirement each arrive while a
restart's create is blocked in the mock; neither may signal the runtime until
the create returns, and the replacement must never be started afterwards. A
restart whose kill hangs must not be started again by the ticks that follow.
A retirement drops a cancelled restart's late result. A state read from
before a restart leaves the replacement alone. And for status: it answers
from the snapshot while the loop is in a 1.5-second turn, refuses a snapshot
older than its bound, waits for a fresh one when one is coming, reports an
agent that has gone, and calls an exited container `stopped` before the loop
has noticed.

The budget tests from "Shorter turns" changed too. Starting a restart now
costs the tick microseconds, so the first test asserts that a tick returns
before a single kill could, and stops at the in-flight limit. The second runs
three rounds over twelve slow restarts and checks that each got its turn, and
no instance more than one attempt more than any other.

One test changed its mind rather than its timing. A job lifecycle test waited
for status to say `running` after a retry. The mock runtime keeps reporting
the exit the test gave it, and status now believes the runtime over the loop,
so the started retry reads `stopped`. The test had been checking what the
loop believed; now it checks what the runtime says.

### The rest of the list

Nine scenarios were still ignored after stage two, and `grep -rn 'LOOP-INLINE:
stage'` listed forty-odd awaits behind them: log reads, two council writes,
the upgrade download, `nft`, DNS, the retirement cleanup, a dozen fault
operations and a handful of runtime reads scattered through deploys and
retirements. Stage three of #351 moved every one of them. There was no single
trick for it. There were five, and which one fits an await depends on a
question worth asking first: once the slow thing finishes, does the loop
still have to do something?

#### Nobody needs the answer but the caller

`relish logs` read every instance's capture into one `String`, inside a turn.
The loop didn't need the logs. It needed to know which instances to read,
which is a walk over its own map. So that's all the turn does now:

```rust
let grill = self.supervisor.grill().clone();
tokio::spawn(async move {
    let logs = read_captures(&grill, &instance_ids).await;
    let _ = response.send(Ok(logs));
});
```

The task owns everything it touches: a clone of the runtime handle (an `Arc`
inside, so cloning is cheap), the ids, and the `oneshot` sender the caller is
waiting on. `async move` moves those into the future, which is what lets
`tokio::spawn` accept it: a spawned task may outlive the turn that started
it, so it can't borrow anything from the loop. If you try, the compiler tells
you the future "may outlive borrowed value", which is Rust's way of saying
what a Go programmer finds out from a data race.

`relish logs -f --tail 200` was the nastier one. The tail went into the API's
64-slot channel from the loop, so a client piping into `less` that stopped
reading held every caller on the node the moment the channel filled. Now the
task sends the tail and then starts the per-instance follows, in that order,
so the tail still comes first.

A join and an image signature are the same shape with a council write in the
middle. Without quorum, openraft's `client_write` waits until the leader steps
down, or longer. The task now has a deadline the loop never had to have:

```rust
tokio::spawn(async move {
    let result = tokio::time::timeout(COUNCIL_ANSWER_TIMEOUT, request)
        .await
        .unwrap_or_else(|_| Err(security_error(late)));
    let _ = response.send(result);
});
```

The deadline isn't for the loop any more; it's so a caller hears something.
And it needed an honest message. A join's token is consumed by that write,
and a write that timed out may still commit afterwards, so the error says that
a retry might be refused and you'll want a fresh token.

#### The loop has to finish the job

An upgrade is the opposite case. The loop must stop taking new work before the
download, and it's the loop that execs the new binary afterwards. Only the
middle, fetching, checking the signature and staging (up to 75 seconds against
a struggling registry), can go elsewhere. Restart steps and state sweeps had
each grown their own `JoinSet` and `select!` branch for this. Rather than a
third and a fourth, we gave the rest one between them:

```rust
pub(super) enum FollowUp {
    UpgradePrepared(UpgradePreparation),
    FirewallApplied {
        cluster_nodes: crate::firewall::rules::ClusterNodes,
        result: Result<(), crate::firewall::rules::FirewallError>,
    },
    NodePressure(super::node_pressure_work::PressureDone),
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    EgressResolved(Vec<super::egress_resolution::Resolution>),
}
```

A turn spawns the slow middle into `follow_ups: JoinSet<FollowUp>`, the task
returns a variant, and a `follow_up` branch in the loop's `select!` matches on
it. `#[cfg(...)]` on one variant is the same conditional compilation we've
used on functions: on a build without eBPF the variant doesn't exist, and
neither does the match arm that handles it, so there's nothing to dead-code
warn about.

The upgrade answers its caller from that branch, sleeps the 200 ms that lets
the HTTP response flush, and execs. That sleep is the one await in the whole
upgrade still on the loop, and it carries a `LOOP-INLINE` tag saying it's
there on purpose. A second `UpgradeApply` for the same upgrade id while the
first is still downloading gets `Ok` (as a re-delivered directive always did
once the marker was on disk), and anything else gets "already in flight".

Two things bit us. First, a panicking task never returns its variant, so the
loop can't tell from the result which piece of work just died. `JoinSet`
helps: `join_next_with_id` yields each result with its task's id, and the
`JoinError` of a panicked task carries the id too. The agent remembers the id
of the upgrade it's preparing and the firewall apply it's running, so a panic
in either clears the right flag, and an upgrade that died mid-download stops
draining the node. Second, a result can describe a world that has moved on.
DNS re-resolution takes a snapshot of every owned egress binding, resolves the
lot in a task, and by the time the answers land an instance may have
restarted into a new cgroup or been redeployed with a different allowlist.
So the loop applies an answer only to a binding that is still the one it
asked about: same owner, same cgroup, same allowlist. It's the incarnation
check from the state sweep again, in a different costume.

#### Whoever drove the runtime asks it

The deploy worker had just created a container when it asked the loop to
program its network, and the first thing the loop did was ask the runtime to
retain the container's network reference, under the same lifecycle lock the
worker had just released. Then, after the start, the loop asked for the pid,
the log files, the rootless port forward and the address, to write the
adoption record. Every one of those is a runtime call the worker could have
made itself, off the loop, where waiting costs nobody anything.

So now it does. The worker retains the reference and passes the result (or
the error) in the `ApplyNetworkPreStart` op; the loop only checks that it
belongs to this instance's generation and journals it. After the start, the
worker reads everything the record needs into one struct:

```rust
#[derive(Debug, Clone, Default)]
pub(super) struct LaunchEvidence {
    pub(super) pid: Option<u32>,
    pub(super) log_stem: Option<PathBuf>,
    pub(super) rootless_network: Option<RootlessNetworkRecord>,
    pub(super) container_ip: Option<Ipv4Addr>,
}
```

and hands it over with the step that writes the record. A restart's steps do
the same: `Create` comes back as `StepResult::Created { retained }` and
`Start` as `StepResult::Started(Box<LaunchEvidence>)`. The `Box` is there for
size. An enum is as big as its largest variant, so one variant carrying four
fields would make every `Done` and `Cancelled` that big too; boxing it costs
an allocation per start and keeps the rest one pointer wide.

The refused-hold case needed a moment's thought. If the loop refuses a
reference the runtime has already retained, nothing will ever release it,
so it hands it back. Doing that inline would be another runtime call, so a
detached task does it instead. That's safe because a reference names its
generation, and releasing an old generation can't touch a later retain of the
same instance.

#### A budget per turn

Some reads the loop can't hand to anyone: the namespace firewall wants every
instance's cgroup id, a fault wants its targets' pids, retirement wants the
runtime's view of a network reference. Each is usually milliseconds. Each can
take seconds behind a create holding the instance's lock. For these we
gave every turn a budget:

```rust
fn begin_turn(&mut self, branch: LoopBranch, detail: Option<&'static str>) -> Turn {
    self.turn_deadline = Some(tokio::time::Instant::now() + TURN_RUNTIME_BUDGET);
    self.loop_meter.begin(branch, detail)
}
```

`TURN_RUNTIME_BUDGET` is 500 ms, measured from the start of the turn, and
every such read in the turn waits until the same instant:

```rust
let deadline = self.turn_deadline();
let reads = ids.iter().map(|id| async move {
    tokio::time::timeout_at(deadline, grill.pid(id)).await.ok().flatten()
});
```

`timeout_at` takes an absolute deadline where `timeout` takes a duration,
and that's the point: ten reads in a row each get what's left of the same
half-second, not half a second each. One detail makes the shared deadline
safe to use: `timeout_at` polls the inner future before it looks at the
clock, so a read that's ready at the deadline still counts. What misses the
deadline fails its step the way a runtime error would, and something tries
again: the tick, every second, for most of them. The namespace firewall
needed a flag to make "something" true (a sync that gave up now marks the
maps stale, and the tick re-syncs until one completes), because a missed
mapping there means a cross-namespace connection the hook lets through.

A budget is a trade, so it's worth saying what it costs. A runtime that is
*always* slower than 500 ms for one of these reads would never get through.
The reads that stayed under a budget are cheap on every runtime we have
whenever nothing else holds the instance: a map lookup, or a small file read
under the instance's lock. Anything that forks a subprocess or waits for a
process to exit went to a task.

#### Start it now, collect it later

Retirement was the awkward one. A retired instance's identity directory has to
go (on Linux that's an `umount` subprocess first, then removing the files and
syncing the parent directory), and its adoption record with it, all *before*
the loop forgets the instance. That "before" is the invariant: an owner must
not disappear while its key material is still on disk. The cleanup sits in
the middle of a sequence the loop owns, and a dozen callers run that
sequence: stops, the tick's deferred retirements, rolling deploys, startup
recovery.

So the step starts the work in a task and waits for it, but only until the
turn's budget runs out:

```rust
let Some((task, started_now)) = self.off_loop_work.task(&key, incarnation, work) else {
    return Err(still_running());
};
let deadline = if started_now { turn_deadline } else { tokio::time::Instant::now() };
let joined = tokio::time::timeout_at(deadline, task)
    .await
    .map_err(|_| still_running())?;
```

On a healthy disk the cleanup finishes in a few milliseconds and the turn
carries on exactly as before. On a struggling one the step fails with
`BunError::StillRunning`, and the caller asks again later: a stop checks back
every 100 ms before it answers anyone, a deploy worker retries its op after a
short sleep, the tick retries a deferred retirement on its next pass. The
next attempt finds the same task in the map, so the work runs once however
many times someone asks, and the steps before it aren't repeated.

Here's the Rust detail that makes this work, and it surprises people coming
from Go's goroutines or Python's tasks alike. `task` is a `&mut JoinHandle`,
and awaiting a `&mut` to a future polls it without taking it. When
`timeout_at` gives up, it drops *its* future, which is the borrow, not the
handle. And even dropping a `JoinHandle` doesn't cancel the task it points to;
the task keeps running and its result waits to be collected. Dropping an
ordinary future cancels it on the spot. The two look identical at the call
site, so this is one of those places where the type tells you what you can't
see.

Each task belongs to the instance's incarnation, its `created_at`. A result
nobody collected (the owner went another way, say) is thrown away when a new
incarnation with the same id asks, and a new incarnation waits until an older
one's work has finished before it starts its own, so an old cleanup can never
delete a new instance's directory.

The same mechanism took two more awaits off the loop. Provisioning an app's
volumes became off-loop work keyed by the spec's volume list, and a snapshot
restore now waits while one is running. And the stage-two leftover: a job rerun
and an execution fence used to settle any in-flight restart step and confirm a
kill, up to twenty seconds, inside one turn. Both now kill through this path.
A rerun also fences the old run first (Stopping, no retries, no probes), so
nothing restarts it while the kill is in flight.

#### The faults

The fault code was where the five approaches met. A node-kill fault's
container kills nobody waits on, so a task does them. A kill, pause or resume
reads its targets' pids and then signals them; the first version read the pids
under the turn's budget, which kept the turn under a second but, on a slow CI
runner, made a status queued behind it wait for the turn *and* its own pid
reads, 1.47 s in all. So those faults became follow-ups too: the loop picks
the targets, a task reads and signals, and the caller hears once it's done. A
pause that lands after its fault was cleared thaws what it froze, since no
reversal ever will. Delays program a netem
tree inside every caller's network namespace, a handful of `tc` commands per
caller, and doing them one caller at a time added up to most of a second on a
busy node. They now run side by side under the turn's budget, and a caller
cut short is marked `(u32::MAX, Vec::new())`, a restart count no instance has,
so the next pass rebuilds it.

The `ss` that cuts open connections after a drop or partition lands taught us
something. Cutting is best effort, nothing reads its result, so the first
version sent it to a detached task. The privileged CI run disagreed: the
Kubernetes demo injects a partition and immediately asks the frontend to talk
to Redis, and three requests in a row went through, on pooled connections the
cut hadn't reached yet. "Nobody reads the result" isn't the same as "nobody
depends on it"; the caller who hears "partition installed" depends on the old
connections being gone. So the cuts run side by side under the turn's budget,
before the answer, like the delays, and only one that's cut short is retried
from a task.

Node pressure got the most machinery, because its helper takes up to four
seconds to say it's ready and the controller has to stay the single owner of
the helper. The controller moved behind a `tokio::sync::Mutex` shared with the
tasks that drive it. The loop checks a request with `try_lock`, which returns
straight away (an error if a task holds the lock), so the loop never waits for
a helper; a task does the start or the stop with the lock held; and the
`FollowUp::NodePressure` branch answers `InjectFault`, `ClearFault` or the
leader's fence once the helper runs or is gone. Answering *after* the work is
done is the same rule as the cleanup's: a caller who hears "cleared" may rely
on it. The mutex also settles the race you'd worry about, a clear arriving
while a start is still in flight: the clear's task waits for the lock, so it
stops the helper the start just made.

#### What the tests say now

Every harness scenario runs. The nine stage-three scenarios are un-ignored and
pass, and a new one joins them: a job rerun whose previous run takes 1.5
seconds to die answers status within the second, finishes the rerun, and
starts the job exactly once. `grep -rn 'LOOP-INLINE: stage' src/` finds
nothing.

Two things have no scenario, because they only exist with a loaded eBPF
program: DNS re-resolution and the execution fence. Their off-loop halves
have unit tests instead. One resolves two allowlists, one of them bad, and
checks that the failure stays with its binding; another checks that an answer
applies only to a binding that hasn't changed owner, cgroup or allowlist.
The off-loop work has tests of its own for the incarnation rule: the same
incarnation finds the task it started, and a new one waits for the old work,
then starts its own and never sees the old result.

And the meter finally has teeth. The V02 checker reads each node's
`bun_agent_loop_turn_seconds_bucket` from `/v1/metrics` at every settle and
heavy check. A histogram counts cumulatively, so `+Inf` minus `le="1"` is the
number of turns over the budget since the process started, and the checker
fails the tier on any increase since its last look. A bun that restarted
starts its counters from zero, which the checker notices the way its resource
trends do, by the pid and image changing. The finding says which bucket the
worst turn landed in, `1 deploy_op turn(s) over the 1 s budget, the worst
between 2.5 and 5 s`, and Bun's own log line from the meter says which op it
was.

Looking back over the whole arc, the lesson isn't about `select!` at all. We
spent a dozen PRs arranging the order in which slow things waited, and three
stages making sure nothing slow waits there. A priority order decides who
suffers; a budget decides that nobody does. The single owner survived all of
it, and so did every invariant that leans on it, because in every case above
the loop still makes the decision and only the waiting moved.

### Two commands, two answers

Issue #241 had one more complaint in it. With three replicas of `hello`
running, `relish inspect hello` listed two, while `relish wtf` said all three
were fine. Both were telling the truth. `inspect` asked only the agent it was
connected to, and that node ran two of the three. The third lived on node 2,
and nothing in the output hinted that anyone else had been asked, because
nobody had.

`inspect` now asks the cluster the same way `wtf` does: every node's
`/v1/status` through the entry node's relay, and the desired replica count
from the leader's `/v1/diagnostics/apps`. What each node said is a small enum:

```rust
pub enum NodeAnswer {
    Answered(Vec<InstanceStatus>),
    Silent { reason: String },
    Down { state: String },
}
```

The three variants are the three things that can be true about a node, and
`match` makes the renderer handle each one. A `Silent` node gets a warning
line, and so does a `Down` one (gossip has already declared it dead, so we
don't spend a timeout asking). Either way, the output says which instances it
can't show instead of quietly showing fewer:

```text
App: hello (namespace default)
  Replicas:  3 desired, 2 running
  Placed:    node-1 2, node-3 1
warning: node node-3 did not answer (timed out after 10s); its instances are not listed

Instance: default__hello-0
  Node:      node-1
  ...
```

Why not call the cluster-wide `/v1/status?cluster=true` that the TUI uses?
It fans out server-side and fails the whole answer when any peer is silent,
which is the right contract for a dashboard that refreshes every second and
the wrong one for the command you run when something is already broken. The
per-node relay keeps each node's failure separate.

The collection and the rendering are separate functions, which is what makes
the tests cheap. `every_replica_is_listed_with_its_node_not_just_the_local_ones`
serves a fake entry node whose own `/v1/status` holds two of three replicas
(exactly what the old command saw) and whose relay answers for node 2 and
refuses node 3. The output has to list all three instances with their nodes
and the `3 desired, 3 running` line. Its siblings check the warning for the
silent node, the dead node that isn't asked at all, and a standalone agent,
which answers for itself as `local`.

### Is everyone running the same build?

The same issue started with a harder question: were the three nodes even
running the same `bun`? Chapter 14 ("Which build is this?") taught every
binary to name its commit. `wtf` now asks each
node's `/v1/version` through the relay (the one new path on its allow list)
and compares the answers.

The interesting part is deciding what "the same build" means. The version
string alone isn't enough; that was the whole problem in #241. The binary's
SHA-256 is too much: one commit built for arm64 and for x86_64 produces two
different hashes, and a mixed-architecture cluster isn't skewed. So the
identity is the version plus the commit, falling back to the hash only when
a build doesn't know its commit:

```rust
fn build_identity(build: &BuildObservation) -> (&str, Option<&str>) {
    (
        build.version.as_str(),
        build.commit.as_deref().or(build.binary_sha256.as_deref()),
    )
}
```

`as_deref` turns an `&Option<String>` into an `Option<&str>`, borrowing the
string inside rather than copying it, and `or` picks the first of the two
that's `Some`. The tuple borrows from `build`; Rust's lifetime elision rules
tie the returned references to the one reference argument, so we didn't have
to write a lifetime by hand.

`wtf` groups the nodes by that identity in a `BTreeMap`. One group is an OK
row that names the build. More than one is a warning, and when one build
clearly has the most nodes, the warning names the odd ones out:

```text
WARNING (1)
  [version-skew] node-3 runs a different bun build from the other 2 nodes (cluster)
    node-1: bun v0.1.1 (3fcb1fd), sha256 aaaa1111bbbb
    node-2: bun v0.1.1 (3fcb1fd), sha256 aaaa1111bbbb
    node-3: bun v0.1.1 (9e1d2c3), sha256 5c0ffee12345
    next: bring every node to one build with `relish upgrade`, then re-run `relish wtf`
```

On a tie there's no majority to be wrong against, so the title just says how
many builds there are and the details list every node. A node whose version
can't be read makes the `builds` source UNKNOWN, as every other source does.
The tests pin each of those cases: a uniform cluster, one odd node, a
different version with no commits at all, one commit on two architectures,
two commit-less builds told apart by their hashes, and an unreadable node.

## Walk the path you actually care about

Say `web` can't reach `redis`. Checking Bun's own DNS and TCP access might tell
us that the node works. It says very little about `web`. The application has
its own network namespace, cgroup identity, firewall decision and DNS setup.
The useful question is therefore: can *this workload* make the connection?

The command that answers it is `relish path`. It was called `trace` until
shortly before 0.1.0, and the old name was a trap. To an SRE, "trace" already
means distributed tracing: OpenTelemetry spans, a Jaeger waterfall, one request
followed through a dozen services. Anyone reaching for a command called `trace`
would expect that and get a network probe instead. This command doesn't follow
requests. It walks the network path between two apps, hop by hop, and asks each
layer what it sees. So it's `path`. Nothing had shipped yet, so we renamed it
outright, with no alias left behind for the old name.

`relish path web --to redis` starts by asking every reachable node for status.
It chooses a node with a running `web` instance, then sends a strict request to
that node's authenticated `/v1/path` endpoint. Bun doesn't accept a shell
command. It accepts application names, namespaces, a destination and an
optional port.

The distinction matters. Bun's DNS probe always runs the same script:

```sh
output=$(nslookup "$1" 2>&1)
status=$?
printf '%s\n' "$output"
printf '__RB_TRACE_DNS_STATUS__=%s\n' "$status"
```

The requested name becomes `$1`, a positional argument. It never becomes part
of the script. The TCP probe does the same with `nc -z -w 3 "$1" "$2"`.
Quoting the positional parameters lets the shell pass each value as data even
if it contains punctuation. The API also validates internal names as DNS
labels, but that isn't our only command-injection defence.

Both commands execute through the runtime's `exec()` implementation. On runc
and Apple Container that means they run inside the selected workload. The
image must contain a POSIX shell, `nslookup` and `nc`. If it doesn't, `relish path`
says `Unknown`. A missing debugging tool isn't proof that the network failed.


One more lesson came from qualifying the release. The staged-install script ran the homepage tour exactly as written: `relish apply`, `relish status`, `relish path frontend --to redis`. Two fresh clusters in six failed the last step with "0 of 0 backends healthy". Nothing was broken. Every instance said `running`, but Redis hadn't passed its first health check yet, or the new catalogue hadn't reached the frontend's node. A person typing the tour takes longer than a script does, but not always long enough. So `path` now waits up to 30 seconds when the only problem is a destination with no healthy backend in the source's view, and prints that it's waiting. A name that shows no instances anywhere after six seconds is almost certainly a typo, and fails straight away rather than making you wait the full half-minute.
## Don't stop the control plane to debug it

A DNS query can wait. A TCP connect can wait too. Each workload operation has
an eight-second outer timeout, but awaiting those operations in Bun's command
loop would still delay status, shutdown and every command queued behind it.

The agent therefore builds a `PreparedTrace<G>`. It contains owned copies of
the runtime handle, request, source instance and service state needed by the
probe. The command arm moves that value and the response channel into a new
task:

```rust
tokio::spawn(async move {
    let _ = response.send(trace.run().await);
});
```

We saw `move` closures earlier. An `async move` block applies the same rule to
an asynchronous block: it owns every captured value rather than borrowing the
agent's stack frame. Tokio requires spawned futures to be valid independently
of their caller. Ownership makes that requirement visible in the type system.
It also gives us a useful test. The mock runtime holds `exec()` in flight while
the test asks Bun for status. Status still returns before the probe is
released.

Spawning doesn't make capacity free. Bun owns a semaphore with eight probe
permits. `try_acquire_owned()` moves one permit into `PreparedTrace`; dropping
the probe returns it automatically. If all eight are occupied, the ninth
request gets HTTP 429 immediately. We test that refusal while all eight mock
`exec()` calls are held. A bound that merely queues an unlimited number of
waiting tasks isn't a resource bound.

`PreparedTrace` also owns a clone of Bun's cancellation token. Each probe
selects between runtime execution, its timeout and shutdown. Cancelling Bun
drops an in-flight probe immediately, returns the semaphore permit and reports
Unknown if the caller is still listening. A separate held-`exec()` test proves
that path; graceful shutdown doesn't wait for a diagnostic timeout.

## Five steps, three kinds of evidence

An internal path probe reports five layers:

1. A real `nslookup` from the source workload for
   `<destination>.<namespace>.internal`. The answer must contain the VIP from
   live service state.
2. The live userspace service map and its healthy backends. On Linux with
   Onion attached, Bun also reads the actual eBPF `backend_map`.
3. The live firewall decision. Bun resolves the source PID to its cgroup, reads
   `cgroup_namespace_map` and `firewall_map`, then applies the same rule as the
   eBPF connect hook.
4. The active faults on this path (see "A path that knows about faults"
   below).
5. A real TCP connect from the source workload to the service VIP and port,
   repeated with `--count`.

Portable builds don't have attached kernel maps. They can still observe DNS,
userspace service state and TCP, but they can't claim to have inspected eBPF.
`TraceEvidence` makes that difference part of the response:

```rust
pub enum TraceEvidence {
    Observed,
    Inferred,
    Unavailable,
}

pub enum TraceVerdict {
    Pass,
    Fail { reason: String },
    Unknown { reason: String },
    Degraded { reason: String },
}
```

Evidence answers "how do we know?" A verdict answers "what did we learn?" A
healthy userspace service map without an attached backend map is inferred but
can pass that layer. A firewall map we can't read is unavailable and therefore
unknown. A live map with no allow entry is an observed failure. `Degraded`
means the path works, worse than it should: a delay or partial drop sits on it,
or only some connects succeeded. Overall, Fail wins, then Degraded, then
Unknown, then Pass. Degraded outranks Unknown because it's a positive
observation (usually of a fault somebody injected on purpose), which is more
useful to report than a gap in the evidence. Missing evidence still can't
quietly turn green.

Relish preserves that contract in human, JSON and YAML output. Pass exits 0,
Fail exits 1, and Unknown and Degraded exit 2, matching `wtf`'s warnings. The JSON schema is versioned
and rejects unknown top-level fields so automation doesn't silently interpret
a changed response as the old one.

External probing has a narrower safety boundary. The caller supplies `--port`
and must be an Admin. The server must grant `probe_external_destination`, the
protected-cluster gate must allow it where applicable, and
`external_probe_allowlist` must contain the exact `host:port`. No wildcard or
CIDR matching. When live egress enforcement is active, the TCP result is still
observed, but the current kernel map stores resolved addresses rather than the
requested hostname relationship. `relish path` calls that firewall evidence
Unknown.
Honest again. Slightly annoying again. You can probably see the pattern by
now.

## A path that knows about faults

Chapter 8's network faults made the tour's best moment possible: inject a
300 ms delay between the frontend and redis, then *walk* the path and watch
the tool point at it. The first attempt was a let-down. The probe passed, its
latency figure timed the whole `runc exec` rather than the connect, and
nothing in the output hinted that an experiment was running.

Three changes fixed it. First, a new step lists the faults that act on this
source's calls to this destination. Bun already knows them: it filters its own
registry with the same `applies_to_caller` rule the fault installer uses, so
the probe and the kernel can't disagree about scope. Where it can, the step
adds live evidence (the `fault_connect_map` entry the connect hook would find
for this source's cgroup, and the netem delay on the source's interface), and
labels the listing `inferred` when it can't. A partition, an NXDOMAIN or a
100% drop fails the step; a delay or a partial drop degrades it.

Second, the TCP step measures the connect *inside* the container. The probe
script reads the clock either side of each `nc -z`, so exec overhead isn't in
the figure. Reading the clock turned out to be the tricky part. `date +%s%N`
gives nanoseconds with GNU date, but podinfo's Alpine BusyBox prints whole
seconds, which a naive parser reads as "0 ms". So the script also reads
`/proc/uptime` (good to 10 ms), and the parser only trusts a `date` value that
is plausibly nanoseconds since 1970:

```rust
const PLAUSIBLE_EPOCH_NS: u128 = 1_000_000_000_000_000_000;
let nanos = |text: Option<&str>| {
    text.and_then(|text| text.parse::<u128>().ok())
        .filter(|value| *value >= PLAUSIBLE_EPOCH_NS)
};
```

`u128` is a 128-bit unsigned integer, native in Rust, so nanoseconds since
1970 and their differences fit without a second thought. `Option::filter`
keeps the value only if the closure says yes, turning an implausible reading
into `None` without an `if`. When the coarse clock was used, the output says
"10 ms clock" rather than pretending to precision it doesn't have.

Third, `--count N` repeats the connect (up to ten times) and reports "7/10
connects succeeded" with the minimum and median connect time. One connect
through a 30% drop tells you nothing; ten tell you the path is flaky.

On the podinfo demo the whole story reads the way the tour wants it to. Under
`relish fault partition redis --from frontend` the path fails with `fault 1
(partition from frontend) blocks this path`, the live map entry shows the
partition for the frontend's cgroup, and 0/5 connects succeed. Under `delay
redis 300ms --from frontend` it's `DEGRADED`, the netem qdisc is listed, and
5/5 connects succeed at a median of 300 ms. Clear the fault, and it's a clean
pass again. The same step also names the backend the VIP picks, and the DNS
step shrank from nslookup's full output to one line: the name, its answer and
the resolver.

The remaining acceptance work needs a real three-node environment and the
rootful runc and Apple Container profiles. The implementation sandbox used for
this tranche couldn't launch a live Bun process, so the checked evidence is
the pure contract, API/authentication tests and mock-runtime orchestration. We
don't turn that platform limitation into a production claim.


## Release checks: listening isn't ready

Imagine running setup while an unrelated web server occupies port 9117. It
returns a perfectly valid HTTP 404. Our original client treated any completed
HTTP request as success, so setup congratulated you on your new node. There
wasn't one.

The liveness check now requires a successful HTTP status and Bun's JSON
`{"status":"ok"}` response. It bounds the response body as well: a health check
has no reason to download megabytes. This preserves the existing liveness
protocol, but liveness still doesn't prove that the node can do useful work.

The new `relish::readiness::wait_for_node` checks liveness, a parseable node
version and authenticated subsystem readiness. An empty critical-subsystem
list isn't enough, even if the response claims `ready = true`. Every critical
subsystem must report `Ready`. Setup returns an error, including the log path,
when those checks don't succeed.

All requests and retry delays share one `tokio::time::Instant` deadline.
`timeout_at(deadline, future)` polls the future until that absolute instant;
when time runs out, dropping the future cancels that attempt. Creating a fresh
30-second timeout for every request would let a multi-step operation take
several minutes. One deadline means what it says.

The tests run real HTTP listeners on ephemeral loopback ports. They reject
404/500 responses, unrelated HTML, invalid version evidence and a live node
whose subsystems are still starting. A deliberately hung handler proves the
whole operation respects its deadline. This is a node-startup check; cluster
quorum and a successful workload request still need their own acceptance
checks before we call a laptop cluster ready.

### Text doesn't arrive one character at a time

A deployment error containing `café` can arrive with the first byte of `é` at
the end of one network chunk and its second byte in the next. Decoding each
chunk separately replaces those bytes with invalid-character markers. The
connection succeeded; the diagnostic was still wrong.

Relish now retains a `Vec<u8>` (a growable byte vector) until an entire SSE event
has arrived. Only then does it decode the text. Deployment progress, rollback
and followed logs use this ordering. The regression test serves an error over
HTTP with a deliberate pause inside the UTF-8 character, then checks the exact
message returned by both public deployment methods. The old client failed it;
the corrected client preserves the message.

### A broken client configuration is an error, not a different configuration

If a user supplies an unreadable CA file, continuing with public trust roots
changes what the client trusts. If a bearer contains an invalid HTTP header
character, dropping it turns an authenticated request into an anonymous one.
Neither is the operation the user requested.

The client now retains construction failures as a `Result` and returns them
before making a request. Existing constructors keep their return type, but the
HTTP accessor returns `Result<&Client, RelishError>`, so callers use `?` to
propagate configuration errors. The new explicit-CA constructor validates at
construction time for managed cluster setup. Tests reject invalid CA material
and malformed bearer headers; the existing live TLS tests still verify that a
cluster-pinned client refuses unrelated trust roots.

The first hosted release build also caught two issues a local build hadn't:
RustSec reported a newly patched Rustls advisory, and Ubuntu 22.04's compiler
rejected a C label directly before a declaration in the eBPF source. We updated
the locked dependencies and added the empty statement required by the older C
rules. Release builds need their own gate because local source tests can't prove
that every supported build environment produces a usable artefact.

### A failed query isn't an empty cluster

The test context used to skip nodes whose status request failed and return the
remaining rows. A test waiting for zero instances could pass because the node
holding those instances wasn't answering. Collection now returns the failing
node's name instead of an incomplete success.

The whole collection, including discovering peers, runs under the case's
existing absolute deadline. Giving each node a fresh request timeout could make
a supposedly short test wait many times its budget. Polling sleeps also stop at
the remaining deadline. A timed-out wait reports the last query error alongside
the last observed states.

Two HTTP regressions exercise this: one server immediately returns 503, and one
stalls longer than the case's budget. Neither may satisfy an empty-instance
predicate. Cleanup retains its separate deadline, so an exhausted test budget
doesn't prevent the runner from attempting to remove its own workloads.

The test HTTP server's startup methods now return `std::io::Result` too. Tests
can unwrap an ephemeral bind they expect to succeed, while `bun testapp` and the
standalone executable add the requested port to an ordinary error and exit
non-zero. A port already in use no longer produces a panic. The regression
holds a real listening socket and attempts a second bind to the same port.

A test called `deploy_history_records_each_version` used to accept any completed
entry after two deployments. The first deployment alone could satisfy it. We
now wait for the first command to appear in completed history, deploy the changed
command, then require both distinct commands in the collected history. Collection
visits every node because deployment history is currently local to the node
which performed the work. Every query and poll shares the case deadline.

The regression runs the public catalogue case against a controlled HTTP server.
One server deliberately records only the first version; another records both.
The first must fail and the second must pass. This checks the test's verdict,
not just the implementation it claims to test.

Ingress acceptance needs a known answer. The container fixture now creates its
own response file and uses absolute BusyBox commands; it no longer assumes the
image has a `PATH` or `/etc/hostname`. The test polls until both status and body
match, sharing the case deadline with deployment. Stopping an app removes the
desired route, so that case expects 404 after convergence. A configured route
with an unreachable backend still has its separate 502 integration test.

The probe also needs a different HTTP client from the control plane. `BunClient`
adds the cluster bearer to requests. Reusing it against ingress could send that
credential to a workload. `TestContext::workload_http_client` carries no bearer,
disables redirects and ambient proxies, and bounds requests. Its regression
sends a request to a controlled server and checks that no Authorization header
arrives, even when the context's API client has an administrator credential.

The first live run passed its ingress request, then labelled the next two cases
unknown. Their capability snapshot had expired while they waited in the queue.
We already refreshed before chaos cases; ordinary queued cases now also refresh
when required evidence is unknown or stale. A failed refresh remains unknown,
not a pass or a skipped prerequisite. The capability request consumes the same
case deadline as the lease and workload operations. A controlled-server test
covers both ordinary and chaos cases starting with expired evidence.

Each ingress case also gets a hostname derived from its own namespace. Sharing
one hostname across concurrently deployed test apps lets one case accidentally
route to another case's backend. Namespace isolation must extend to the ingress
name, not just the app record.


## Don't let the internet grade your tests

One release candidate failed on a line we'd never touched:
`registry read deadline exceeded`, while staging BusyBox from ECR. Nothing in
Reliaburger was broken. A CDN edge had a slow minute. Two fixes, one for users
and one for us. Chapter 5 covers the product side: stalled reads now retry
inside a bounded total, and digest-pinned images can come from a mirror.

The harness side uses that same mirror feature. Every public image the Linux
suites run is pinned by digest in `testkit::pinned_images`. A small
standard-library Python script fetches them once, with retries, into a
content-addressed cache and serves it as a read-only registry on loopback.
`make test-linux` runs nextest under it, and the tests hand
`[images] mirrors` to every Bun they start. Why a mirror rather than, say,
copying files into Bun's image store? Because the mirror goes through exactly
the code users run: the same client, the same index resolution, the same digest
checks. Pre-seeding the store would test a path nobody takes. And because the
content is addressed by digest, the mirror can't lie: bad bytes fail
verification and Bun falls back to the real registry.

The proof we wanted was blunt. Warm the cache, cut the VM's uplink, run the
image-pulling suites. They pass.

## Pulling the plug

The exporter's checkpoint is written with the full ritual: a private temporary
file, `sync_all`, an atomic rename, then a sync of the directory. The lease
store does the same. Every unit test agrees. None of them proves anything about
a power cut, because a process that dies still leaves the kernel's page cache
behind, and the page cache is exactly what a power cut throws away.

So the storage fixtures in `tests/power_cut.rs` copy the shape of the reboot
fixture from Chapter 1: two phases, driven by
`scripts/release/qualify-storage-power-cut.sh` against a disposable Lima VM.
The prepare phase doesn't do the work itself. It starts workers, and the
workers are the test binary again:

```rust
let child = Command::new(std::env::current_exe().unwrap())
    .args(["--ignored", "--exact", test, "--nocapture", "--test-threads=1"])
    .env(DIRECTORY, directory)
    .env(ROLE, role)
    .process_group(0)
    .spawn()
    .unwrap();
```

A Rust integration test compiles to an ordinary executable, and libtest accepts
the name of one test to run. Re-executing ourselves with a `ROLE` variable gives
us a worker that links the real library, with no extra binary to build or ship.
The test function checks `ROLE` first and, if it's set, loops forever instead of
running a phase. `process_group(0)` (from Chapter 12) detaches the worker from
the shell session that ran prepare, so it keeps writing after prepare returns.

For the exporter there are four workers. A generator flushes real Parquet
through `LogStore` and publishes each file into the exported directory. Three
exporters share one checkpoint and its cross-process lock: one behaves like
`relish logs-export`, two like Bun's disk-pressure tick, which exports and then
prunes. For leases there are three workers, each creating, renewing and
releasing leases in its own store file, all in one directory.

How does verify know what *should* have survived? Every worker keeps a ledger,
and a ledger line is a promise:

```rust
fn record(&mut self, line: &str) {
    self.0.write_all(format!("{line}\n").as_bytes()).unwrap();
    self.0.sync_data().unwrap();
}
```

`sync_data` is `fdatasync`: it flushes the bytes and the file size, and skips
metadata such as timestamps that nobody will read. The worker records an
operation only after the store said it was done, so every complete line names
an operation the store acknowledged. The last line may be torn by the cut, and
verify ignores anything after the final newline. A lease worker also writes a
`begin` line before each operation, which lets verify accept exactly two
outcomes: the state after every acknowledged operation, or that state plus the
single operation that was in flight.

The driver waits for prepare, sleeps a random 0–20 seconds and runs
`limactl stop --force`. Verify first checks that the boot ID changed (a pass
without a new kernel proves nothing), then works through the ledgers.

The lease store passed. The exporter didn't, on its first run, or on the next
three. Every export the workers had acknowledged was at the destination with
the right name and zero bytes, and the pruners had already deleted most of the
sources: 162 of 171 files gone for good in one run. The checkpoint was durable;
the data it vouched for wasn't. The `object_store` crate's local filesystem
backend doesn't `fsync` what it writes unless you ask
(`LocalFileSystem::with_fsync`), and `object_store::parse_url` doesn't ask. A
checkpoint that promises "this is safely elsewhere" is a licence to prune, so
the promise has to be at least as durable as the licence.

The exporter wasn't the only caller. Metrics, volume-snapshot upload and
council backups all opened their destinations with the same `parse_url`, and
each of them acts on a write once it returns. So the fix isn't four lines in
the exporter. It's one function that everybody goes through:

```rust
pub(crate) fn open(url: &url::Url) -> Result<(Box<dyn ObjectStore>, Path), object_store::Error> {
    let (store, prefix) = object_store::parse_url(url)?;
    if url.scheme() == "file" {
        return Ok((Box::new(LocalFileSystem::new().with_fsync(true)), prefix));
    }
    Ok((store, prefix))
}
```

`Box<dyn ObjectStore>` is Rust's spelling of "some type that implements the
`ObjectStore` trait, decided at run time". `dyn` marks a trait object (roughly
a Go interface value), and the `Box` puts it on the heap, because the compiler
can't know how big an unknown type is. That's why we can hand back either the
`parse_url` result or our own `LocalFileSystem`: both fit behind the same
pointer. Cloud schemes pass through untouched, because S3 and GCS don't
acknowledge a write until they've stored it.

The regression tests are less satisfying than we'd like. Nothing a unit test
can observe changes when an `fsync` is missing, so each caller's test checks
the store's `Debug` output for `fsync: true`, and the power-cut fixtures carry
the real proof.

Council backups got a fixture of their own, because they have the same shape:
upload a new backup, then delete everything but the newest three. Before the
fix, three runs out of three ended the same way. Three backup files survived,
all empty, and retention had deleted every older, intact one. The cluster had
been told, every few milliseconds, that it had a backup. After a power cut it
had nothing to restore from. With the shared helper, the fixture passes. The
[qualification record](../qualification/2026-09-25-v02-power-cut.md) has both
sets of runs.

Could an ordinary test have caught this? Not honestly. Every file was fine
until the kernel went away, and no amount of killing processes reproduces
that. The snapshot uploader still needs its own fixture: it needs Btrfs
volumes. Its export receipts are written atomically and durably now, and an
upload is only receipted once the destination has confirmed the archive.

## Lessons learned: audit the evidence too

Export a log file, replace it with new contents under the same name, then export
again. Both commands report success. Where did the first archive go? The exporter
used a content hash to notice the change, but used only the filename for its
archive key. The second write replaced the first. Detecting a new generation and
preserving that generation are two different requirements.

The [September audit](../qualification/2026-09-17-code-audit.md) reproduced this
with temporary files, without a running cluster. It also found that exporting to
a second destination reused the first destination's checkpoint. A checkpoint
must describe what was acknowledged, by whom and where. “We've seen these bytes”
isn't enough evidence to delete their source.

A passing test can establish the wrong thing. The audit's temporary certificate
probe passed when issuance panicked on an invalid DNS name. That was useful for
confirming a report, but it would be a terrible permanent regression test. The
real regression must require an ordinary error and fail if issuance panics.
The temporary probes were removed; their results and limitations remain in the
audit record so the next implementation starts with a reproducible observation.

The same distinction applies to our catalogue. Unknown means we didn't establish
the result. A deliberately unsupported case needs an explicit profile contract;
a required case without evidence must not become a green skip. Fresh capability
evidence can expire while a case waits, and collecting an HTTP response doesn't
prove that the intended workload answered it. Assert the outcome we actually
care about, including the cleanup outcome.

Fault ownership is part of that result. If cancellation arrives after a server
accepts an injection but before the client records its ID, cleanup can't rely on
an empty client list. Track the pending operation and reconcile it. Healing every
fault on the node would remove someone else's experiment too. Exact ownership
matters most when the happy path has already stopped running.

Finally, a milestone checkbox needs a scope. The cluster upgrade coordinator
already checks gossip rejoin; the replacement process's local boot-marker check
still has a separate gap. Calling all upgrade verification either finished or
missing hides useful information. We now keep completed milestones and explicit
residual tasks side by side in the [roadmap](../roadmap.md).

Portable tests and controlled servers let us force awkward orderings quickly.
They don't establish that three independent Linux nodes survive the complete
catalogue. That still needs a real-node execution record, including profiles,
prerequisites, unknown results and proof that owned workloads and faults were
removed. The laptop smoke run is valuable evidence for setup. It doesn't close
that wider gate.

### Absent is not zero

The audit's findings kept rhyming, so here they are in one place. Each is a case of an *absent* observation being read as a *particular* one.

- **A missing measurement.** A baseline with network throughput compared against a run without it used to print PASS, because only the common metrics were compared. A metric missing from today's run now fails the comparison.
- **A missing exit code.** The job probe accepted `None` as success alongside `Some(0)`. `None` means the API had no exit status to report, which proves nothing. It now requires `exit_code == Some(0)`.
- **A missing voter list.** When the council endpoint failed, `relish wtf` fell back on gossip role flags and could compute "zero of zero members, quorum lost". Unavailable membership is now unknown, and no quorum arithmetic runs on it.
- **A string that happens to match.** A `403` whose body mentioned "no eligible nodes" passed the capacity benchmark. The scheduler's refusal is now a typed `ScheduleError` with a JSON `code`, and the benchmark matches the `NoEligibleNodes` variant for the exact app it submitted. Trace had the same bug in another form: it searched nslookup's output for the VIP as text, so a resolver address containing those digits passed. It now parses `IpAddr` values and compares them.
- **A guessed address.** The harness built node URLs by pairing each gossip IP with the entry node's API port, and found Pickle by assuming port 5050. On a laptop with three nodes on three ports, that contacted the same process three times and called it a healthy cluster. Membership now carries each node's advertised `api_address`, and capabilities publish a `ServiceEndpoints` struct whose fields are `Option<String>`: `None` means "no listener declared", never "try the default". Bun also binds its API socket before it starts clustering, so asking for port zero advertises the port the kernel actually chose rather than a zero.

Several failures were in the harness rather than the product, and they're worth a sentence each. A fixture that released a port and then started Bun on it lost the port to another test, and its readiness probe cheerfully connected to *that* listener; fixtures now wait for Bun's own announcement of the address it bound. The cluster-upgrade job failed because the debug `bun` binary outgrew Pickle's 512 MiB upload limit; the harness now strips debug symbols from its copy. A nextest "leaked handle" warning turned out to be a runner bug on macOS, fixed upstream, so we pinned the fixed release and made leaks fail the gate instead of raising the timeout. And a few async tasks were doing blocking filesystem work, sweeping directories and hashing a stored Bun binary on a Tokio worker thread; that work now runs through `spawn_blocking`, and the one inventory read with no deadline got the same five-second bound as the others.

One gap we closed by adding a test rather than fixing code. An app exits, Bun starts its replacement, then dies before saving the replacement's adoption record. Our physical interruption tests covered first deployments and explicit retries, but not this automatic restart. A test-only Linux interposer now pauses the adoption record's `fsync` at exactly that point and kills the real Bun process. Recovery must retire the unrecorded process before reporting ready. The existing code passed. Now it's a claim someone can check.

### A test has to be able to fail

The testing assessment after 0.1.0 found three pieces of evidence that couldn't say no.

The cgroup v2 detector's test called the detector on Linux and threw the answer away (`let _ = result;`). It proved the function didn't panic. It would have passed if the detector said "v1" on a v2 host, or "v2" on a host with no cgroups at all. The fix is a small filesystem boundary: `check_cgroup_v2` now just passes `/sys/fs/cgroup` to `check_cgroup_v2_at(root: &Path)`, and three tests hand it fake mount trees in a temporary directory, one with `cgroup.controllers` (v2), one with a per-controller directory and no controllers file (v1), and one that doesn't exist. The host test still runs, and now it asserts: every Linux host that runs the suite is provisioned with v2, so a failure there is a real finding. How do we know the new tests can fail? We broke the detector on purpose, dropping the controllers check, and watched the v1 test go red.

The node-pressure acceptance test was `#[ignore]`d (good) but, when selected without `RELIABURGER_NODE_PRESSURE_TESTS=1`, printed "skipped" and returned. Nextest has no idea what "skipped" means in a test's stderr. It saw a function return and reported a pass. Selecting an ignored test is a request to run it, so a missing prerequisite is now an `assert_eq!` failure with a message saying which variable to set. The one pattern we left alone is the subprocess fixture: a test that exists only to be re-executed by its parent (its `#[ignore]` reason says so) still returns when the parent didn't start it, because `make test-linux` selects whole binaries and would otherwise fail on every fixture.

The third was the V02 loop summary, the script that turns hours of stress loops into a verdict. It wrote `Verdict: **FAIL**` and exited 0. It counted a JUnit `<skipped/>` as a run. And its combined mode summarised whichever lanes happened to upload a file, so a lane that never ran simply left the table, and the denominator shrank to fit. Now every row carries a status (pass, fail, skip or incomplete) and the commit it belongs to. A test with fewer runs than its loop's iterations is incomplete. The combined record is checked against `scripts/release/v02-loop-lanes.json`, the list of lanes the workflow runs, and a unit test keeps that list in step with the workflow matrix. A missing lane, a missing test, a stray lane or a record from another commit fails the summary, and the exit status finally says the same thing as the text.
