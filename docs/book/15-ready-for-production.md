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
Both expiries did their job: ratatui 0.30 later brought a fixed `lru`, and moving to
DataFusion 55 removed Thrift from the graph altogether (Chapter 6 tells that story).

A later `rkyv` advisory showed why the compiled graph matters too. Cargo locked
`rust_decimal`'s optional `rkyv` 0.7 dependency, but `byte-unit` disables the defaults that
would enable it. `cargo tree --target all -i rkyv` found no active path, and Reliaburger reads
no rkyv archives. The advertised fix starts at the incompatible rkyv 0.8 API while
`rust_decimal` 1.x deliberately pins its optional integration to 0.7. We recorded a temporary,
expiring exception instead of pretending that changing an unused lockfile version was a fix.

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

Production runs acquire server-owned leases for their apps and namespaces.
The server checks exact ownership before cleanup; a familiar name is not proof
of ownership. Names still use `rbtest-{run}-{seq}` for recognition. The legacy
lease-free path used by focused tests also checks that prefix:

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
cleanup code first. Teardown reverses exact fault receipts, releases the
server-owned lease and checks runtime absence. Timeout or unreachable peers
mean unknown cleanup; they do not prove the resources are gone. The server's
expiry reaper can still finish later. Jobs, images, tokens and mounts need the
additional ownership work tracked as C34. We record timeout, API failure and a
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
    Partition {
        source_app: Option<String>,
        source_cgroup_id: u64,
    },
    CouncilPartition { peers: Vec<String> },
    // ...
}
```

Why leave `source_cgroup_id` on the wire at all? Compatibility. Why refuse a
non-zero value from a client? Authority. The kernel compares the key against
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

The memory `oom` form follows the same rule from the other direction. It
doesn't pretend to be reversible: Bun refuses it and points the experiment at a
Kill fault when the goal is to test restart after abrupt termination.

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
one marker file and no executable, so it couldn't prove deployment. That third
case remained unknown until the runnable fixture described below.

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
    Namespace { name: String },
}
```

Apps sort before namespaces, so cleanup removes every app before its quota
record. That's deliberately honest. Jobs, faults, tokens, images, mounts and
node state need different cleanup operations. Pretending a namespace owns them
would produce green reports and leaked state, which is quite an achievement for
a test system.

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

That closes the app/namespace and portable-workload part of the milestone.
Jobs, faults, tokens, registry images, mounts and node state still need explicit
ownership before the chaos catalogue can rely on them.

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
though. If peers have already
removed the failed node from their live directory, the operator must point
Relish at that node's still-open API to clear it; expiry needs no route and
will restore it automatically. Durable lease ownership for node state remains
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

This helper runs before Bun constructs Tokio's runtime. We replaced
`#[tokio::main]` with an ordinary `main` which handles the hidden synchronous
subcommand first and explicitly builds the normal multi-thread runtime for the
agent path. Otherwise Tokio's worker threads would exist before the helper
joined its cgroup, muddying process ownership and wasting memory in a programme
which only needs to park.

Linux gives us one more safety net: `prctl(PR_SET_PDEATHSIG, SIGKILL)` asks the
kernel to kill the helper when the thread which created it exits. That thread
can be a Tokio worker; Linux doesn't wait for the whole Bun process to die.
The [Linux interface documentation](https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html)
also explains that an earlier death doesn't produce a retroactive signal.
`prctl` is a C system call, so Rust requires an `unsafe` block. The block's
safety comment explains the invariant: the call changes process metadata and
doesn't dereference Rust memory. `getppid()` detects process reparenting, but
cannot prove a creator thread still exists inside a live parent process.
Bun therefore records the kernel thread ID immediately before spawning, with
no `.await` between those operations. An await could resume the Rust task on a
different Tokio worker, making the recorded ID describe the wrong creator.
After installing the signal, the helper requires `/proc/PID/task/TID` to exist
inside that same parent process. Missing or unreadable evidence stops startup
before the helper joins a pressure cgroup.

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

The privileged acceptance fixture launches the real Bun helper from a Python
thread, waits for readiness and then lets that thread finish while its process
stays alive. The helper must exit through SIGKILL. A second case kills the
whole parent. A third delays exec until the creator thread has already exited,
while the parent process stays alive. Before the identity check this helper
stays alive indefinitely; afterwards it refuses before applying pressure.
All cases require an empty kernel cgroup membership list, followed by
successful startup reclamation of the old directory. Early helper death never
releases the capacity reservation by itself: cleanup must still inspect the
owned cgroups.

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
The report names the cases that actually ran; a subset is not full qualification. ProcessGrill stays separate for the
same reason: fixed host ports can't restore three replicas onto two surviving
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

## Trace the connection you actually care about

Say `web` can't reach `redis`. Checking Bun's own DNS and TCP access might tell
us that the node works. It says very little about `web`. The application has
its own network namespace, cgroup identity, firewall decision and DNS setup.
The useful question is therefore: can *this workload* make the connection?

`relish trace web --to redis` starts by asking every reachable node for status.
It chooses a node with a running `web` instance, then sends a strict request to
that node's authenticated `/v1/trace` endpoint. Bun doesn't accept a shell
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
image must contain a POSIX shell, `nslookup` and `nc`. If it doesn't, trace says
`Unknown`. A missing debugging tool isn't proof that the network failed.

## Don't stop the control plane to debug it

A DNS query can wait. A TCP connect can wait too. Each workload operation has
an eight-second outer timeout, but awaiting those operations in Bun's command
loop would still delay status, shutdown and every command queued behind it.

The agent therefore builds a `PreparedTrace<G>`. It contains owned copies of
the runtime handle, request, source instance and service state needed by the
trace. The command arm moves that value and the response channel into a new
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

Spawning doesn't make capacity free. Bun owns a semaphore with eight trace
permits. `try_acquire_owned()` moves one permit into `PreparedTrace`; dropping
the trace returns it automatically. If all eight are occupied, the ninth
request gets HTTP 429 immediately. We test that refusal while all eight mock
`exec()` calls are held. A bound that merely queues an unlimited number of
waiting tasks isn't a resource bound.

`PreparedTrace` also owns a clone of Bun's cancellation token. Each probe
selects between runtime execution, its timeout and shutdown. Cancelling Bun
drops an in-flight probe immediately, returns the semaphore permit and reports
Unknown if the caller is still listening. A separate held-`exec()` test proves
that path; graceful shutdown doesn't wait for a diagnostic timeout.

## Four steps, three kinds of evidence

An internal trace reports four layers:

1. A real `nslookup` from the source workload for
   `<destination>.<namespace>.internal`. The answer must contain the VIP from
   live service state.
2. The live userspace service map and its healthy backends. On Linux with
   Onion attached, Bun also reads the actual eBPF `backend_map`.
3. The live firewall decision. Bun resolves the source PID to its cgroup, reads
   `cgroup_namespace_map` and `firewall_map`, then applies the same rule as the
   eBPF connect hook.
4. A real TCP connect from the source workload to the service VIP and port.

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
}
```

Evidence answers "how do we know?" A verdict answers "what did we learn?" A
healthy userspace service map without an attached backend map is inferred but
can pass that layer. A firewall map we can't read is unavailable and therefore
unknown. A live map with no allow entry is an observed failure. Overall, Fail
wins, then Unknown, then Pass. Missing evidence can't quietly turn green.

Relish preserves that contract in human, JSON and YAML output. Pass exits 0,
Fail exits 1 and Unknown exits 2, matching `wtf`. The JSON schema is versioned
and rejects unknown top-level fields so automation doesn't silently interpret
a changed response as the old one.

External probing has a narrower safety boundary. The caller supplies `--port`
and must be an Admin. The server must grant `probe_external_destination`, the
protected-cluster gate must allow it where applicable, and
`external_probe_allowlist` must contain the exact `host:port`. No wildcard or
CIDR matching. When live egress enforcement is active, the TCP result is still
observed, but the current kernel map stores resolved addresses rather than the
requested hostname relationship. Trace calls that firewall evidence Unknown.
Honest again. Slightly annoying again. You can probably see the pattern by
now.

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
residual tasks side by side in [progress](../progress.md), with completion tests
in the [new plan](../plans/2026-09-17-codebase-completion-plan.md).

Portable tests and controlled servers let us force awkward orderings quickly.
They don't establish that three independent Linux nodes survive the complete
catalogue. The [acceptance runbook](../plans/2026-07-06-plan-chaos.md) still needs
its real-node execution record, including profiles, prerequisites, unknown results
and proof that owned workloads and faults were removed. The laptop smoke run is
valuable evidence for setup. It doesn't close that wider gate.

### Readiness belongs to the resource owner

Spawning a future doesn't mean its listener has bound or its initial state has
loaded. A wrapper used to mark every owner ready before polling that future.
We now pass each owner a `ReadySignal`. It consumes that signal only after it has
acquired its resources. Until then the subsystem remains `Starting`.

The signal contains a one-shot sender. Consuming it transfers the acknowledgement
to the supervisor, which still watches the running task. A task that exits or
panics loses readiness. A reconstructible owner gets a new channel on every
attempt, so a delayed signal from the previous attempt cannot mark its successor
ready. This is an ownership boundary, not a sleep long enough to hope startup
has finished.

Bun's listeners acknowledge their already-bound sockets from inside their owner
futures. The agent waits until initial capability collection completes, and the
security refresh worker loads its state before signalling. Regression tests hold
an owner before readiness, fail a real bind, retain a stale attempt's signal and
panic during startup. Each checks the externally visible readiness snapshot.

### One busy lease must not stop the reaper

Suppose a deployment still owns lease A when leases A and B expire. Cleanup
must wait for A's deployment to finish, but B has no reason to wait. Previously
the reaper awaited A's operation mutex and never reached B.

Cleanup now calls `try_lock_owned()`. This returns immediately with either an
owned guard or an error; unlike `lock_owned().await`, it never joins a waiting
queue. We translate contention into `LeaseError::Busy` (HTTP 409 for an explicit
cleanup request). The reaper leaves A's record intact, visits B, then retries A
on its next tick. The operation guard still prevents cleanup from overtaking
a deployment. A regression holds A's guard while the real reaper removes B,
then checks that A remains active and can be cleaned once the guard is dropped.

### Record intent before sending a fault

The server may accept a fault even if the caller never receives its response.
Previously the guard added its receipt after the HTTP request, with another
`.await` to acquire the receipt mutex. Cancellation at either point could leave
an active fault while cleanup reported `NotRequired`.

We now add a pending receipt before sending the request. The receipt keeps the
exact owning client and node, then gains the server's fault ID after the response.
An `Arc<()>` gives each operation a distinct allocation without inventing an ID
for the server: `Arc::ptr_eq` compares allocation identity, even though both
values contain only `()`. An unresolved receipt makes cleanup `Unknown`. It
cannot safely infer that a timeout meant refusal, or clear unrelated faults to
make the result green.

Cleanup also snapshots receipts instead of taking them out of the shared ledger.
It removes each receipt only after confirmed reversal. Cancellation during a
clear therefore leaves a retryable receipt. Local HTTP fixtures exercise both
lost injection responses and interrupted cleanup; only the exact recorded IDs
are ever cleared. This is in-process ownership, not a durable replacement for
server-side fault expiry after the runner itself dies.

### A lease acknowledgement includes the directory entry

Writing and syncing a file does not make its subsequent rename durable. The
lease store now uses the same private atomic writer as identity files: a unique
0600 temporary file, file sync, rename, then parent-directory sync. A pre-existing
`leases.json.tmp` is irrelevant. The regression plants that name as a symlink
and verifies that another writer's file remains untouched.

We move an `OwnedMutexGuard` into `spawn_blocking` along with the proposed lease
map. The blocking transaction keeps the lock until it has persisted and published
the new map and operation locks. Aborting the async caller cannot release that
ownership halfway through a write. A controlled pause in the test demonstrates
that a second writer waits and then preserves both leases after restart.

A persistence failure might occur after rename, so the last acknowledged memory
view might differ from disk. We retain that view for inspection but refuse further
mutations until the store is reopened. Returning an error and then overwriting
possibly newer ownership would defeat the safety mechanism. Physical crash and
filesystem sync-failure qualification remain part of the recovery acceptance gate.

### Expiry outranks a rotation label

A certificate whose encoded expiry has passed is critical, even if its last
rotation observation still says `valid`. Diagnosis now checks that timestamp
first. A healthy automatically rotating 90-second leaf is fine, but the OK text
says it is currently valid with healthy rotation; it no longer promises fourteen
days of remaining validity. Near-expiry leaves without positive healthy rotation
evidence produce a warning. Consumers that cannot hot-rotate remain separately
unknown. Tests cover the exact expiry boundary with both automatic-rotation
settings and the short-lived healthy case.

### Unknown membership has no quorum denominator

When the council endpoint fails, gossip can still report role flags. Those flags
may describe an older Raft configuration. Using them as the configured voter set
could turn an empty observation into “zero of zero members, quorum lost”, or
report a false healthy majority. Council collection now records unavailable or
empty membership as unknown and retains the source failure. Diagnosis also refuses
quorum arithmetic for explicitly degraded or empty membership. A public HTTP
collector regression covers missing membership, stale role flags and an empty
council response; the existing observed failed-majority case remains critical.

### Filesystem identity is not its capacity

Two filesystems can both report 50 bytes used out of 100. That does not make them
one filesystem. Diagnostics now attach a node-local device identity from Unix
filesystem metadata, without exposing host paths. The collector groups storage
domains only when that identity matches. Older reporters or unsupported systems
omit it; those observations remain separate rather than being guessed together.

If readings for one device change during collection, we retain the busiest
complete reading and combine the domain names. A public HTTP collector regression
covers identical-size distinct devices, missing identities and two changing
readings of one device. A real filesystem test also confirms that sibling storage
paths report the same device identity. The optional wire field is backward-readable
by the new client; older strict diagnostic clients may report unknown until updated.

### Match observations as identities, not substrings

A counter-reset error for `api-10` must not hide the disappearance of `api-1`.
The CPU observation window now tracks the exact instances seen in its second
sample, independently of human-readable errors. Reset and missing-instance
messages can therefore coexist without one suppressing the other.

Trace had the same category of mistake: it searched every output line (including
its own target description) for the expected VIP text. A resolver address or a
hostname containing that text could pass. We now parse addresses from nslookup's
answer section and compare `IpAddr` values. This rejects longer textual matches
and recognises equivalent IPv6 spellings. The agent regression supplies successful
probe output with misleading resolver/name text and verifies that DNS still fails;
the existing correct-answer trace remains successful.

### Watch mode keeps watching when collection fails

An unavailable entry node is an observation worth displaying. Watch mode now
renders that collection as unknown and retries after its normal 30-second
interval. A pinned Ctrl-C future is polled both during collection and between
reports, so an interruption can stop an in-flight collection too. `tokio::pin!`
keeps that future at a stable address while multiple `select!` calls borrow it.

Ctrl-C returns the last diagnostic outcome. Missing evidence cannot downgrade
previously observed critical problems to clean; before any successful report,
the outcome is unknown/warning. A real Relish subprocess test makes the first
health request fail, waits for recovery on the next collection, then sends SIGINT
and checks warning exit code 2. The test owns and reaps its child on failure too.

### A missing measurement is a regression

A baseline might contain network throughput while today's run contains only
latency. Comparing their common metrics alone can print PASS without ever
measuring throughput. `BenchComparison::failed` now includes missing baseline
metrics as well as measured regressions. The command uses that same verdict for
its exit outcome and human summary, and names missing and newly added metrics
on separate lines. JSON and YAML retain their existing missing-metric lists.

Hosted comparisons remain informational, but still display missing observations.
The regression test replaces a baseline metric with a new one: it must fail and
name both. Adding a new metric while retaining the baseline measurements stays
clean. We don't invent a numeric value for absent evidence.

### Give each invocation its own namespace

Two test commands launched in the same second used to share a run ID. A
1,024-invocation concurrent regression produced exactly one distinct ID. The
command now draws 128 random bits and formats all 32 hexadecimal digits, leaving
room for the `rbtest-` prefix and case suffix within a DNS label. It no longer
depends on clock resolution, PID reuse or a process-local counter.

Randomness makes accidental collisions negligible; it isn't ownership proof.
The server still refuses a namespace with an active lease, including explicitly
chosen fixed namespaces. Cleanup remains tied to the granted lease. Tests check
concurrent IDs, DNS-label validity and the existing collision refusal.

### A passing test can still leave a process behind

Nextest can observe a test process exit successfully while a child still holds
its output pipe open. That is a passing-but-leaky result, not proof of cleanup.
Our `slow` status filter printed the aggregate leak count without the responsible
case's name. Both default and CI profiles now use `status-level = "leak"`, so
future occurrences retain that identity in the run log.

The no-default suite reproduced one leak during this audit. Three reruns with
detailed reporting did not reproduce it, so H08 remains open: we still need the
case identity, an ownership fix and a focused regression. We haven't increased
the leak timeout to make the warning disappear.


### Case outcomes are data, not an error-message convention

Suppose a failed workload prints `__unknown__:connection refused`. The runner
used to interpret that prefix as a request to downgrade a failure to missing
evidence. A workload's text had acquired authority over the test verdict.

Case bodies now return `Result<(), CaseError>`. The enum distinguishes
`Failed(String)` from `Unknown(String)`, and `unknown(...)` constructs the
second variant explicitly. Ordinary string errors convert only to `Failed`.
The catalogue macro boxes either kind of future and converts its error at the
boundary; it never examines the message. The regression keeps the old prefix
in a failure string and verifies that the report still counts a failure.

The library entry point also validates its own input before creating tasks,
leases or requests. Zero or excessive timeouts, invalid parallelism and unsafe
namespace prefixes return `RunError`. A CLI check cannot protect a library from
its other callers. Deadline construction uses checked clock arithmetic, so even
`Duration::MAX` returns an error rather than panicking; a child deadline remains
bounded by its parent.


### Readiness reporting must not suspend its owner

A subsystem sends its ready signal, then queues a capability update behind a
reader of the readiness tracker. Its supervisor receives the signal and queues
its own write. Tokio's read-write lock admits writers in order, so the owner is
first in line. Who polls it now? In the original loop, nobody. The supervisor
had entered a selected branch and was awaiting its own write there.

This can deadlock without a thread holding a mutex forever. An async future
only progresses when its caller polls it. `tokio::select!` polls the competing
futures until a branch becomes ready, then runs that branch's body. An `await`
in that body does not keep polling the other branches.

The repair keeps readiness publication as a separate future inside the select.
While that future waits for the tracker, the owner remains polled and can finish
its earlier write. We retain the owner in the same supervision task, preserving
cancellation and panic handling. Each restart still owns its own acknowledgement
channel and publication future, so an old attempt cannot mark a new one ready.

The regression deliberately holds a tracker reader while both writes queue.
Releasing it must let the owner publish. We exercise both supervision loops;
both stalled before the repair. The existing tests still cover startup panics,
missing acknowledgements, bounded restarts and stale signals. This establishes
a reproducible deadlock fix; the full upgrade runs remain separate evidence.


### A leak reported by the runner

The portable suite passed while nextest reported one leaked output handle. Once
we retained the per-test report, it named a metrics backfill test. That case
writes and queries Parquet files; it does not launch child processes.

The local runner was nextest 0.9.140. Upstream's 0.9.145 release describes a
matching macOS race: one concurrently launched test could inherit another
test's capture pipe before its close-on-exec flag was set. The completed test
then appeared to leak a handle held by its sibling. This explains why the
reported case need not be the process that kept the pipe open.

We require the repaired runner and pin it in CI. The leak deadline stays at
100 ms, and a future report now fails the gate. We verify both feature
configurations with the new runner; increasing a timeout would not repair
ownership. The runtime's child-process cleanup tests remain necessary too.

See the [nextest 0.9.145 release notes](https://github.com/nextest-rs/nextest/releases/tag/cargo-nextest-0.9.145)
for the capture-pipe fix.

### A safety refusal can arrive during an election

The three-node chaos acceptance test kills one voter, then tries to kill a
second. It originally demanded an error containing "quorum". CI caught another
valid outcome: the surviving node temporarily lacked a live mapping for its
Raft leader and refused with HTTP 503 before it could evaluate quorum.

The test now accepts those two explicit missing-leader-evidence refusals as well
as the quorum refusal. It still rejects unrelated failures and verifies that
neither the second target nor the routing node acquired a fault. Then it reverses
the first fault and observes recovery. The invariant is refusal without an
effect, including while leadership evidence is incomplete.


### Disabled authentication does not grant every chaos operation

In the explicit disabled-auth development mode, a workload fault is attributed
to `local-bootstrap` with the local administrator role. Server policy must still
allow workload fault injection and any required acknowledgement must be present.
When authentication is enabled, the caller's role and namespace scope apply.
Node-level faults always require an authenticated principal, including in
disabled-auth mode. They also pass the separate node-state or capacity policy
and cluster safety checks. A successful workload-fault test therefore says
nothing about permission to kill, drain or pressure a node. C06 still tracks
the cluster-wide reservation needed to make concurrent node faults safe.

## Keep debug symbols out of upgrade fixtures

The cluster-upgrade CI job failed before its first upgrade. Pickle returned HTTP
413 when the harness uploaded Bun. The registry's 512 MiB request limit was doing
its job; the debug executable had grown past the fixture's upload budget.

The harness now copies Cargo's executable into its temporary directory and runs
`strip -S` on that copy. This removes debug information before we make versioned
copies, hash them or sign them. Cargo's original stays available for backtraces.
An explicit size assertion catches an oversized fixture before starting four
nodes, and upload failures include the server's response body. The same real
executable still has to boot, pass compatibility preflight, replace its running
predecessor and survive rollback. Removing symbols doesn't relax those checks.

This is acceptance-test preparation, not release qualification. We still need to
exercise the exact signed release artefacts under V03.


### Testing two faults that race each other

The node-chaos acceptance test sends two kill requests through different API
nodes in a live three-voter cluster. Exactly one may succeed. It clears that
fault and waits for both gossip recovery and confirmed reservation release,
then fails the current leader. The successor must inherit the outstanding slot,
refuse another kill, and release capacity only after the old leader's fault
expires and its target-side fence is acknowledged.

This test found a recovery bug that the individual fault tests missed. The
bootstrap node reopened its transports after expiry but had no configured seed
addresses and had forgotten every peer. We added bounded gossip rediscovery and
a separate regression for that case. The reservation remained held throughout
the failure, which was the safe outcome, but the experiment still hadn't recovered.
A refusal is not a substitute for testing the recovery path.


### Finding the service we actually started

Suppose the API is at `https://[::1]:19117` and Pickle is at
`https://[::1]:15051`. Splitting the API string at its first colon doesn't find a
host. It finds `https`. Even after removing the scheme, it finds `[`. Assuming
port 5050 then produces an invalid URL and tests a listener we never started.

Bun now publishes a `ServiceEndpoints` value with its capabilities. Each field is
an `Option<String>`: `Some(origin)` names a bound listener; `None` says no endpoint
was declared. A capability boolean alone doesn't tell a client how to reach the
service. We bind Pickle before constructing that report, so a configured port of
zero becomes the actual port assigned by the kernel. Ingress uses its bound HTTP
and HTTPS addresses too.

A wildcard bind such as `0.0.0.0` means all local interfaces. The probe substitutes
the API host for that wildcard, while retaining the declared service's scheme and
port. A node-local loopback address on a remote API is refused. It isn't the
caller's loopback. Origins with credentials, paths, queries or unsupported schemes
are refused rather than repaired by string manipulation.

Managed VMs need another translation. Their listeners live in a guest network,
while the laptop connects through explicit forwards. The saved context therefore
replaces the report's endpoints with the forwards the setup operation owns. An
absent forward stays absent. Setup adds one authenticated HTTPS Pickle forward on
localhost port 15050, alongside HTTP ingress on 18080; both are configurable and
checked for collisions. Older state has no registry forward, so lifecycle commands
can still manage it without claiming that an uncreated forward exists.

TLS adds one more name. The socket might be localhost port 18443, but the route
and certificate belong to `workload.example`. We resolve the declared socket,
then keep the workload name in the request URL and override DNS resolution for
that name in a separate HTTP client. That preserves both the HTTP Host header and
TLS SNI (the server name sent during the handshake). The client trusts the cluster
CA alongside ordinary roots and performs normal hostname verification. It never
inherits the API bearer or the control-plane hostname exception. Pickle's client
is separate: it authenticates control-plane operations and refuses remote HTTP
before sending credentials.

The regression first reproduces the malformed IPv6 URL. A real Bun process then
binds all three service ports to zero and proves the published listeners answer.
A separate TLS server, reached through a TCP forward, records the workload SNI
and headers so a successful response cannot hide a leaked administrator token.
A negative handshake checks that API-specific hostname leniency hasn't escaped
into workload requests. Managed-context tests verify that unforwarded guest
endpoints never sneak back in.


A partition test must also wait for the authority that admits the fault. Gossip
can report three live nodes before Raft has elected a leader or finished adding
the third voter. The legacy partition acceptance now waits for every node to see
a known leader and a stable, non-joint three-voter membership as well as live
gossip. The server continues to refuse an earlier request. Waiting only for the
network view made the test race bootstrap on hosted runners.

### A test token belongs to the server lease

Suppose Relish creates a token, sends its probe and then loses power. A cleanup
call at the end of the function never runs. The token must still disappear.

Token issuance now commits the credential and its lease ownership in one Raft
entry. Only the lease's exact authenticated owner may request it, with an
unscoped Admin credential and permission to provision test resources. The
token's name begins with the lease namespace, its namespace scope contains
exactly that namespace, and its role cannot be Admin. Its expiry is capped at
the lease's current expiry. Renewing the lease doesn't extend a token already
issued; a longer test must mint another credential.

`LeasedResource::ApiToken { name, fingerprint }` is another variant of the
resource enum. Matching on the enum makes Rust ask us to handle this new kind
in both cleanup paths. The cluster reaper first commits the Cleaning state,
then revokes the exact fingerprint. A changed credential with the same name
is an error, never permission to delete the replacement. The ownership record
remains until absence is confirmed. A new leader resumes the same work.
Standalone nodes refuse token cleanup because they have no token council.

Names beginning with `rbtest-` are reserved for leased token issuance. Existing
names cannot be overwritten or reclaimed by another lease, even after an
operator revokes the original token before cleanup finishes. This also means
a lost issuance response cannot be retried into a second live credential.
The caller may obtain a new name under the same lease.

The catalogue now uses the accepted lowercase `deployer` role and preserves
the original connection's CA and managed forwards when switching credentials.
It probes a read-only log endpoint outside its namespace. If scope enforcement
breaks, the probe fails without creating a workload that it cannot own. A role
refusal, authentication propagation delay, transport failure or arbitrary 403
isn't evidence that the scope check worked.

API tests exercise anonymous and wrong-owner refusal, bounded expiry and
client-free reclamation. Raft tests cover invalid authority and scope, existing
names, snapshot recovery and replacement fencing. Live acceptance runs the
actual Relish catalogue against HTTPS Bun, and kills a three-node cluster's
leader before the token expires to check successor cleanup. These new persisted
variants require protocol 5, state 4 and lease schema 2: create fresh development
clusters instead of reading them with an older binary.

### An unsupported manifest cannot shed its lease

A manifest with both an app and a job was refused by the runner. A manifest
with only a job took the ordinary, unleased apply path. The same fallback
accepted permission-only and build-only manifests. Removing the supported
resource from a request must not remove its ownership requirement.

The `match` now handles every `Some(lease_id)` path explicitly: supported
resources use leased apply, while unsupported or empty manifests fail before
HTTP. Only `None`, used by focused runner fixtures, selects ordinary apply.
At the API boundary, the reserved test-namespace check covers jobs as well as
apps and namespace declarations. The runner regression observes three outgoing
mutations before the repair and zero afterwards; the API regression changes
from accepting an unleased test job to returning a conflict. Durable job
ownership remains required before job catalogue cases can run under a lease.

### An advisory exception is a condition we must keep checking

Our release review removed the unused PEM wrapper rather than extending its
exception. Four explicit exceptions remain: three maintenance notices and one
vulnerable archive crate that Cargo does not currently build. The
[dated review](../qualification/2026-09-18-dependency-exceptions.md) records the
paths, migration options and the unchanged November review deadline.

The archive exception has an extra condition. Before auditing the lockfile,
`make audit` asks Cargo for the active rkyv dependency graph across all root
features and targets. An active path refuses the build. A failed inspection
also refuses it; an unavailable observation cannot prove absence. Only an
empty, successfully inspected graph allows that exception to remain in force.

A command-level test supplies a controlled Cargo executable and date. It
checks the inactive, active and failed-inspection outcomes without downloading
an advisory database or waiting for the review deadline. The real audit then
fetches the current database and checks the actual lockfile. These are
separate pieces of evidence: one proves the gate's behaviour, the other checks
what we intend to ship today.


### Let the server reserve its own port

The portable Linux run caught a bootstrap test losing its chosen port between
closing a temporary listener and starting Bun. Another process bound that port
first. A longer startup timeout could not repair the race.

Bun now reports the API listener's actual bound address, including the kernel's
choice when you request port zero. The bootstrap and service-endpoint tests
start Bun on `127.0.0.1:0`, read that address from its output and probe that exact
listener. No test releases a guessed API port for Bun to reclaim. The bootstrap
regression first failed because the old startup line printed the requested
port zero; it now requires a non-zero address and a successful health response.


### Secret tests need a workload observation

The secret catalogue used to return Unknown immediately because it could not
obtain the cluster encryption recipient. It now fetches the public recipient
over the authenticated API, encrypts fixture values and applies an app inside
its server-owned test lease. It never reads a cluster private key.

After placement, the probe finds the node that actually runs the app and uses
that node's scoped exec API to read the container environment. The entry API
node need not own the workload. Two independently randomised ciphertexts must
recover the expected bytes, while an adjacent plaintext variable stays intact.
The config-file case uses the same node selection to read its mounted file.

The rootful acceptance test starts the real Bun and Relish binaries with a
generated cluster CA, closes the unauthenticated bootstrap window and runs all
three secrets/config cases against runc. Both the case verdict and server-owned
cleanup must succeed. Missing capability, Unknown, timeout and cleanup failure
all fail this qualification.


### Reserve the API port before advertising it

A listener on `127.0.0.1:0` asks the kernel to choose a free port. Printing that
chosen address fixed standalone discovery, but the clustered first-run test
found another dependency: cluster startup had already copied zero into its
internal API endpoints. The API accepted an app while the scheduler reported
no eligible node.

Bun now binds a `TcpSocket` before starting the cluster and uses its actual
local port everywhere it advertises the API. Binding reserves the address;
listening is a separate operation. We delay that second step until replicated
credentials are ready and the existing bootstrap checks pass. This keeps a
joining node closed to connections while it waits for authentication.

The black-box first-run test asks for port zero, reads the selected endpoint,
creates the first administrator and deploys a process workload. It requires
an observed running instance. Merely accepting the manifest wouldn't prove
that the scheduler can use the advertised endpoint.


### Observe workload identity without trusting its own bundle

The identity catalogue used to return Unknown because it couldn't observe a
workload certificate. A running container already exposes its public bundle at
`/run/reliaburger/identity/bundle.pem`. We can inspect that through the same
namespace-scoped exec path used by the secret/config cases. The helper selects
the node that actually owns the running instance and keeps the case deadline.

The new probe deploys a leased BusyBox workload and waits for identity issuance.
It reads the public bundle, never the private key or bearer token. Rustls checks
the leaf's signature chain, validity and client-auth usage against the CA
configured by the caller. `CertificateDer` represents an encoded certificate;
it doesn't make those bytes trusted. Only the configured anchors do that.
The probe also requires exactly the SPIFFE URI for the expected cluster,
namespace and app. This proves certificate delivery and validation, not a
workload-to-workload TLS handshake or private-key possession.

There is a useful negative test here. A Node CA can authenticate the Bun API,
but it cannot validate a leaf signed by the separate Workload CA. The privileged
fixture runs the identity case with that restricted trust anchor and requires
a chain-verification failure plus confirmed resource cleanup. Trusting whatever
root appears inside the container's bundle would make this test incorrectly
pass. With no explicit CA configured, the case remains Unknown.

### Give jobs an owner on the node that runs them

Consider a catalogue case that starts a batch job, then loses its connection.
The job may still be running. A cron registration can be even less visible:
there may be no instance yet, but the next minute can start one. Cleaning up
only the client's list of running instances misses both situations.

Jobs in 0.1 run on the receiving node. Their test leases now do too. A request
with `scope = "node_jobs"` reserves a server-generated namespace and records the
job names before handing the manifest to the agent. Cluster nodes persist these
records in `node-test-leases.json` and run a local reaper alongside the leader's
application-lease reaper. A client disconnect doesn't drop the deployment guard:
the server keeps draining the agent's event channel until deployment finishes.
Only then may cleanup retire the job and its schedule.

`LeaseScope` is an enum with two variants, `Applications` and `NodeJobs`.
Matching on it makes the admission rule explicit: a job lease accepts job-only
manifests, while an application lease cannot acquire jobs. The resource enum
also distinguishes `Job { job_id }` from `App { app_id }`. Both contain an
`AppId`, our existing namespace/name identity, but the enum variant preserves
which kind of workload the record owns. Rust's exhaustive `match` requires us
to consider the new kind in cleanup as well as validation.

Routing needs a boundary too. Job lease IDs start with `node-jobs-`, followed by
128 random bits encoded as lowercase hexadecimal. Their namespaces use the
same complete suffix after `rbtest-node-`. Application leases cannot reserve
that prefix, and callers cannot select or reuse a job lease's namespace.
Creation requires an unscoped user credential and the ordinary test-operation
grant. Renew, release and apply require the exact owner; an unscoped
administrator may inspect or reclaim a lease. Raft refuses node-job records.
Even an invalid or unknown ID with that prefix stays local: sending it to
another node returns a missing lease, rather than forwarding to a leader.

Cleanup removes ownership only after the agent confirms retirement. The test
reopens the durable store, drops the first retirement reply and starts the
reaper. The record must remain in `Cleaning` with its job still attached until
a later reply confirms success. A separate running-agent test covers a batch
job and a cron registration before its first firing. These exercise different
failure modes; parsing a valid ownership record alone proves neither.

The persisted lease schema is now 3, with protocol and durable-state generation
6. This branch still requires fresh development clusters. Keeping old nodes
from joining prevents a node without job-scope admission from accepting a new
node's reserved namespace as an ordinary application lease.

The actual-process fixture adds a stronger check. It starts a leased sleep job,
records its PID and start time, kills Bun and verifies that the job survives.
After expiry, a replacement Bun must adopt and retire that same process before
removing the lease and instance record. The real TLS/Relish catalogue separately
requires all three job cases to run and report confirmed cleanup. Its first run
correctly skipped them because the fixture lacked an executable allowlist; the
fixture now declares its exact binaries. A skipped case isn't a passing case.
This tests process-crash recovery after the adoption record exists. It doesn't
establish recovery from power loss or the earlier runtime-creation window.

### A stopped job doesn't imply a successful job

The completion probe used to accept both `Some(0)` and `None` as a successful
exit. Those values mean different things: `Some(0)` contains an observed zero
exit status; `None` means the API has no exit status to report. A stopped process
with missing evidence cannot prove that its command succeeded.

The catalogue now requires `exit_code == Some(0)`. Its HTTP regression serves a
stopped job without an exit code and reproduces the old false pass. The same
fixture rejects exit 7 and accepts exit 0. We run the actual job case as well,
so changing the assertion cannot hide a runtime that never supplies the evidence.

### Let the log collector catch up

Job exit and log ingestion happen on different tasks. A successful exit can
reach the status API before the collector publishes the job's stdout. Reading
logs once at that instant used to fail an otherwise healthy catalogue case.

The log probe now polls every 100 ms under the case's original `Deadline`.
The same deadline wraps the HTTP read as well as the delay, so a stalled request
or output that never arrives cannot turn this into an unlimited wait. An API
error remains an error. The success condition is still the expected output.

The regression serves an empty indexed result and an empty local fallback on
the first read, then publishes the expected line. That reproduces the race
without depending on task scheduling. A second case never publishes the line
and must fail at the deadline. Neither test treats empty output as success.


### Follow the caller before calling code unfinished

The release audit listed unused public helpers. A symbol search was the start,
not the conclusion. The old autoscaler loop and proxy wrapper had no callers;
we removed them. A memory-allocation sizing helper also had no callers or tests,
and didn't describe the cgroup-limit path Bun actually uses, so it went too.

Other helpers have narrower jobs. `image_available_locally` is a tested,
read-only verified-blob query; it doesn't pull an image or prove a runtime can
launch it. `renew_test_lease` now has an actual node-job recovery caller. The
catalogue runner requests enough lease lifetime for its bounded case and cleanup
rather than maintaining a hidden renewal task. Its wait helpers share the case's
absolute deadline. Expiry and per-node count queries in the fault registry are
library observations, not scheduling or cluster admission.

We record those callers and tests in the completion plan. The age unsealing
primitive still doesn't constitute a CA recovery workflow, and changing an
identity model's grace period cannot extend a signed certificate. Those operator
features remain explicitly planned. Passing a helper's unit test must never
silently promote the feature around it to supported.

A later hosted minimum-Rust run passed 3,658 tests and failed the cross-node
log partial-result fixture: its supposedly healthy peer contributed no rows.
That fixture put a cold DataFusion query behind the same two-second deadline
used to observe an unreachable peer. It mixed storage setup cost with transport
failure, and its test server even converted storage errors into empty results.

The partial-failure case now serves three fixed log entries over real HTTP.
Its two-second deadline and exact failed-peer assertions stay unchanged. The
other four cross-node cases still query real log stores, and those fixture
handlers now surface storage errors instead of returning empty arrays. We also
print node failures when the row count is wrong. A future CI failure should
say which assumption failed, not send us guessing from a zero.

### Make the public examples executable

`cargo test --doc` used to pass without running any examples. It now executes
small examples on `Config::parse` and `validate_endpoint`. The first parses and
validates a workload, then checks that a misspelt key is refused. The second
accepts remote HTTPS and local development HTTP, but checks the typed refusal
for remote plaintext. Neither example contacts a cluster.

Rustdoc compiles ordinary fenced Rust blocks in `///` comments as tests. We use
`expect` in these examples to make a failed assumption fail the test visibly;
application code can propagate the same `Result` with `?`. The configuration
uses a raw string, `r#"..."#`, so TOML quotes need no escaping. Its image is an
`Option<String>` because some workload forms have no image. `as_deref()` borrows
that optional owned string as `Option<&str>` for the assertion; it doesn't move
the image out of the configuration. `Some(...)` is the present-value variant.
The endpoint example uses `matches!` to check an enum variant rather than error
message wording. These examples teach the public contract and fail when it
drifts.

### Give the current revision the runners

Several successive fixes left older PR builds competing with the current
revision. Source CI and native builds now cancel their superseded PR runs.
Each workflow has its own fixed concurrency prefix and uses the pull-request
number as its group. Main and tag runs use their unique run ID instead, so a
later source push cannot interrupt release validation or publication.

The prefixes also matter when the release workflow calls source CI. GitHub
associates a reusable workflow's context with its caller; using the same group
for both can make a release cancel or wait on itself. The
[workflow concurrency reference](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#concurrency)
and [reusable workflow reference](https://docs.github.com/en/actions/reference/workflows-and-actions/reusing-workflow-configurations)
spell out those rules. Cancelling an obsolete run is queue management, not a
passing test result. We still require the complete gate set for the final
candidate revision.

### Capacity is an observation, not a failed string match

An HTTP 403 containing the words "no eligible nodes" used to pass the capacity
benchmark with a value of zero. The error was a permissions failure, yet its
text looked like the end of a successful experiment. There was a second gap:
cluster apply acknowledges desired-state writes before any worker starts the
container. Counting those acknowledgements measures how many configurations
we submitted, not how many workloads ran.

Capacity applies now ask the actual leader scheduling loop for admission.
A bounded `mpsc` channel carries requests; each request owns a `oneshot` sender
for exactly one reply. Dropping that sender before replying is an unavailable
observation. It cannot become a scheduling refusal. Queueing and response share
a five-second deadline. The normal scheduling pass supplies current placement
reservations and namespace quota usage, and incomplete or unready membership
prevents a saturation claim. The check does not commit a placement. Other
work can change capacity before deployment, which is why runtime observation
is still required.

The refusal schema contains `ScheduleError`, tagged with a stable JSON `code`.
`#[serde(tag = "code", rename_all = "snake_case")]` tells Serde to encode the
enum's variant name as that field. Its other fields carry structured context,
including the app's namespace and name. `deny_unknown_fields` rejects an
unexpected shape. Rust's `match` then selects the specific
`NoEligibleNodes { app_id }` variant, and a guard requires that ID to equal the
app we just submitted. Human-readable error wording can change independently.
Authentication failures, unknown codes and missing IDs stay errors.

The API requires a live lease and authorised capacity operation, forwards the
request to the leader, and rechecks leadership and lease activity after the
scheduler reply. The benchmark counts an app only after the complete cluster
status endpoint observes its one running instance. Before returning a result,
it rechecks all counted apps. A malformed status body, a disappeared workload,
an exhausted deadline or the hard safety limit fails the measurement. Cleanup
still has its own required outcome.

The HTTP regressions cover both original errors and the distinction between
pending, running and missing workloads. A real three-node test sends the
request through a follower: an oversized app must return the typed refusal
without entering desired state; a small app must actually run. That fixture
also supplies the reconciler's internal service identity, just as authenticated
production nodes do.


### One address per node

Run three nodes on your laptop, each with a different API port. A test that
combines every gossip IP with the entry node's API port contacts the same
process three times. Its supposed cluster inventory can look perfectly healthy.

The membership response now carries each node's resolved `api_address` from
the peer directory. `Option<SocketAddr>` means the address can be absent; it
never means “guess 9117”. The shared client constructor returns a `Result`,
so a caller must handle missing or unusable evidence. Formatting `SocketAddr`
also supplies the brackets IPv6 URLs require. Retargeting the existing client
preserves its bearer identity and configured trust roots.

Testkit, diagnostics and trace collection use this constructor. An absent
address fails inventory collection instead of silently dropping that peer.
Standalone inventory still uses the explicit entry connection. Upgrade plans
use the same advertised addresses, with explicit per-node overrides retained,
and validate the target list before uploading the binary. Unit HTTP fixtures
cover different ports, IPv6, absent addresses, invalid addresses and unresolved
members. The authenticated three-node placement fixture checks the actual
advertised ports and contacts each endpoint with the original credentials.

### Accepted cleanup still needs evidence

A DELETE can return HTTP 202 while a worker is unavailable. That's a promise to
keep trying. It isn't evidence that the worker has stopped. Relish now polls the
lease endpoint until the ownership record disappears, with a single 30-second
limit covering the request and every poll. The test runner's shorter remaining
teardown deadline can cancel that wait. Either timeout reports unknown cleanup.
It still checks runtime absence independently after the server confirms release.

The regression serves 202 and an empty runtime inventory while keeping the lease
present. Previously that passed. Now it stays unknown; a second case removes the
lease after two polls and confirms cleanup. This distinguishes accepting work
from finishing it without making an unavailable worker block the CLI forever.

### Retirement includes its durable files

Suppose the process has stopped but Bun cannot unlink its adoption record. A
warning followed by successful lease cleanup loses the live owner that could
retry the unlink. Identity directories have the same problem: clearing an
in-memory path doesn't remove its key material or unmount its backing storage.

Normal Stop and Retire now wait for identity-directory cleanup, then adoption
record removal, before forgetting ownership. Both removals sync their parent
directories, including retries after an uncertain sync. Filesystem errors retain
the tracked instance and its port so the next attempt can finish. The blocking
filesystem work runs through `spawn_blocking`; Bun awaits its result before
clearing the instance's identity fields. Ordinary application data volumes are
outside this cleanup.

The command-channel regression replaces an adoption record with a directory,
then separately replaces the identity directory with a file. Each fault must
refuse retirement and retain the owner. Removing the fault lets the next command
finish. Rolling deployment cleanup has separate call paths and still needs its
own error-propagation audit.

### Integration agents own their filesystem paths

A ProcessGrill fixture still prepares workload identity directories. Leaving its
volume root at the production default makes tests depend on the host's existing
permissions and lets otherwise independent fixtures share paths. Stricter Stop
first exposed this in three Linux unit tests. The integration harnesses now give
each agent a temporary volume root, kept alive until that agent exits; cluster
fixtures use their existing private node directory. This keeps a test's cleanup
inside the files that test owns.

The affected agent, batch, build, ingress and discovery suites pass on macOS and
Linux, as do all ten live Linux placement cases. Two build tests require Buildah
to be absent; on the tool-equipped VM we execute those binaries in child
processes with an empty PATH. Both negative scenarios still run and pass.

### Every worker that might still own the workload

Move a test application from worker A to worker B while A is disconnected.
Deleting the application from Raft removes desired state. It tells us nothing
about the process still running on A. If we also delete its lease, we've lost
our reminder to go back and check.

The lease now keeps every `(application, node)` pair ever assigned to it.
Scheduling records that pair in the same Raft entry that publishes placement.
A `BTreeSet<LeasedPlacement>` stores each distinct pair once, in deterministic
order. `LeasedPlacement` is a struct containing our application and node identity
types. Its `Ord` derive supplies the ordering a tree set needs; `Eq` makes the
meaning of equality explicit. Rescheduling adds an owner rather than replacing
history. A hard limit refuses further scheduling before history can grow
without bound.

Beginning cleanup fences new work. The current leader deletes desired state
but retains those owners, then includes exact lease retirement instructions in
each worker's placement response. A worker runs Retire even if its own journal
is empty. Only observed runtime exit, artifact cleanup and a saved local
checkpoint permit its acknowledgement. Lost responses and disk errors leave
the owner recorded for retry. An acknowledgement includes the immutable lease
ID, so an old request cannot release a replacement lease's resources.

Reads need care too. An isolated former leader can still serve an empty local
snapshot. We require a quorum-confirmed read before publishing placements or
answering lease inspection. Followers forward inspection with the caller's
credentials and a one-hop marker; a loop or unavailable quorum returns an
error. HTTP 202 means cleanup is pending. HTTP 204 means every recorded owner
has confirmed retirement or been explicitly decommissioned, and the remaining
resource records are gone.

The tests move a workload between owners, restore a Raft snapshot, replay
acknowledgements and reject late scheduling. A worker test drops a response,
returns a runtime error and breaks checkpoint persistence before allowing a
successful acknowledgement. A real three-node fixture pauses the owner,
changes leader, then resumes it with an empty journal. Cleanup must remain
pending through the outage and finish only after that worker retires its real
process. Operator-attested decommissioning is a separate path: it records
who retired an identity after stopping or fencing that machine's workloads.

### When a worker will never come back

Worker A has been powered off for maintenance, but its lease still awaits a
retirement acknowledgement. Waiting forever keeps the ownership record honest,
but gives the operator no way to finish. We need a second kind of evidence:
an explicit statement that the operator has stopped or fenced those workloads.

`relish decommission-node worker-a --workloads-stopped --reason "powered off"`
records that decision. An unscoped administrator must submit it; an ordinary
node's shared service credential cannot. Raft stores the authenticated operator,
reason, timestamp and the number of placement obligations released per lease.
It clears those obligations and removes desired placements and service endpoints
in the same state transition. Repeated requests return the original record.
They don't manufacture a second decision or change its author.

The record also retires the node identity permanently. Otherwise, an old disk
could come back with an old certificate and resume work that the cluster has
already forgotten. The existing live certificate revocation handle now checks
node identities as well as serials, so obtaining another serial for the same
identity cannot undo retirement. Enrolment, renewal, membership admission and
scheduling all consult the durable fence. Existing API connections check their
TLS peer identity again for each request. A returning machine needs fresh state,
fresh credentials and a new node name.

That per-request check originally cloned the whole replicated state (apps,
catalogue, withdrawal ledger, leases) just to look up one name, on every request
that arrived over mutual TLS. Now the state machine lends it out instead:
`read_desired` takes a closure, runs it on a borrowed reference while holding the
read lock, and returns only what the closure produces. The lookup costs a map
probe. The same check also used to let an unreadable certificate identity
through, because `is_ok_and` treats a parse error as "not retired". It now
refuses: an identity we can't read might be one we've retired.

Membership changes need their own fence. The retirement request carries the
membership log position the leader observed; Raft refuses it if that position
has changed or a joint membership change or another node's fault is in progress.
An outstanding fault on the retired node is resolved by the same operator
attestation, with its sequence recorded in the retirement audit; the allocation
counter remains intact, so late activation cannot reuse it. Raft also
refuses a decision that would leave fewer than the current quorum outside the
retired set. The council reconciler removes retired voters together, then removes
retired learners. This avoids accidentally promoting a retired peer from gossip
or getting stuck while two retired voters wait for one another's removal.

None of this stops a disconnected computer. The operator must do that first.
The durable record says *operator-attested retirement*, which is different from
Bun observing runtime exit. The tests preserve that distinction: authenticating
with an admin token alone is insufficient without the explicit attestation,
while runtime acknowledgements still require the system identity.

### Run the cleanup owner in the fixture

A placement test asked the real API to delete its lease, waited for HTTP 204,
and timed out. The workers had acknowledged retirement. What was missing?
Bun starts a cluster lease reaper on every node, but this fixture did not. The
leader's reaper advances the persisted cleanup state after acknowledgements
arrive. Wiring only the API and reconciler left nobody to finish that work.

The fixture now starts the production reaper and gives its task to the existing
shutdown guard. This matters for asynchronous acceptance tests: reproducing the
request handler alone does not reproduce the full lifecycle. The regression
keeps its bounded wait for durable lease absence; increasing that timeout would
never create the missing owner.

The broader run caught another asynchronous boundary. Gossip reported all nodes
alive after reversing a fault, but the API’s separate membership table had not
caught up. A second injection therefore met a valid safety refusal. The fixture
now waits for the views the API actually consumes, matching voter membership
and leader identity across peers and confirming quorum before submitting a new
fault. Waiting for raw gossip alone was not enough. We keep the refusal intact
and never retry an uncertain destructive request just to make a test pass.

### Storage belongs in the cleanup acknowledgement

A worker can stop its container and still leave a mounted test volume behind.
Its retirement acknowledgement must cover both. The standalone lease reaper now
asks the agent to retire test resources explicitly. A cluster worker uses the
same command only for a committed cleaning lease; ordinary placement withdrawal
keeps the data. It acknowledges the former placement after runtime retirement,
storage retirement and the applied-placement checkpoint have all succeeded.

The first regression creates identical application names in a test namespace
and an ordinary namespace. Stop preserves the test's marker. Lease cleanup
removes only the test directory. A second regression makes the runtime ignore
termination and checks that both the marker and lease remain. After repairing
the runtime, retrying the same cleanup removes them.

The volume tests reopen checkpoints, interrupt provisioning before the first
filesystem mutation, reject corrupt or duplicate claims, and preserve external
files behind unsafe symlinks. Linux qualification creates real loop-backed ext4
and Btrfs volumes. A process holds its working directory inside the loop mount to
make unmount fail; retirement must remain pending. An unexpected nested mount
also refuses. Another test kills the provisioning owner process, reopens its
completed journal and retires the surviving mount. These tests exercise the
storage boundary; they don't substitute for Bun's separate pre-adoption-record
crash qualification.

The three-node placement fixture adds managed storage to a leased application,
scales it down, then injects an unexpected snapshot directory on one owner.
The lease expires without a client cleanup request.
Former placements must retain their data until lease cleanup. Unblocked owners
can finish, but the lease must stay until the last worker's storage error is
repaired. A neighbouring ordinary application directory remains intact.

### Observing the child before killing its parent

A job can be Running before its first shell command has executed. Spawning it
and scheduling it are separate events. Our crash fixture used to read a counter
file immediately after Running appeared. Hosted macOS caught that assumption:
the retry had started, but the file still counted only its predecessor.

The fixture now gates the retry before its first write, observes Running,
releases that gate and waits for the workload's counter before killing Bun.
This deliberately exercises the scheduling gap while keeping the crash at the
intended point. The deadline is bounded; an extra or missing execution still
fails. It does not relax recovery or treat an unknown launch as absent.

### Waiting for registry writers after workloads

A leased test image can outlive its app if we forget which node accepted the
upload. Each application lease now has a repository-to-writer inventory beside
its placement history. The reaper first removes desired resources and waits
for the former runtime owners. It then commits a workload-retired barrier.
Registry cleanup acknowledgements before that barrier refuse, and the lease
cannot disappear while any writer remains. Decommissioning clears only the
externally fenced identity's receipts and preserves the count in its audit.

The standalone store persists the same inventory and barrier. Its reaper marks
workloads retired only after the agent confirms their cleanup. The storage
worker must subsequently retire uploads and local metadata before acknowledging
its receipt. Chapter 5 describes the catalogue side. These state transitions
are prerequisites; the progress ledger separately tracks connecting every
upload and replication caller and proving physical cleanup.

### Registry cleanup needs storage evidence

An HTTP response disappearing does not mean its upload disappeared. The registry
lease tests pause registration after creating a temporary file, cancel the caller
and require cleanup to wait for the task still holding ownership. The test then
releases registration and checks both the empty upload directory and the confirmed
receipt. Another test replaces the temporary file with a directory, proving a
failed deletion retains the lease. Repairing that obstruction exposes the next
barrier: failed catalogue persistence must also retain the receipt until retry.

These are controlled interleavings around real filesystem operations, not proof
of complete physical-crash recovery. The live multi-node and process-death gates
remain separate. We also keep ordinary content in the registry while deleting a
leased repository with identical blobs, and check that GC still refuses to remove
the shared bytes. The runnable catalogue below exercises leased upload and actual deployment.
Complete multi-node registry crash qualification remains a separate gate.

### Run what we pushed

The registry catalogue used to report two passes and one unknown. Worse, its
synthetic repositories had fixed names outside the lease namespace, so confirmed
app cleanup said nothing about those images. The live runc regression preserves
that original failure before replacing the fixture.

Each case now uses its server-issued namespace as the repository prefix and
attaches the exact lease header to every upload operation and manifest commit.
The list assertion matches both repository and complete digest. Partial uploads
and committed metadata therefore join the same storage receipt inventory used
by normal test cleanup.

For deployment we fetch the already-pinned BusyBox index through the shared
verified upstream reader. The client selects Linux and the target node's reported
architecture, then stages the verified child manifest, configuration and layers
in Pickle. It verifies each blob's digest and length before upload, caps total
fixture content at 64 MiB and refuses mutable upstream tags. Source registry
access and authenticated cluster uploads use separate clients.

The test deploys `repository@sha256:...` using the returned full child digest. Its
command starts BusyBox's foreground HTTP server with known response content.
After cluster-wide readiness, the harness finds the actual owner, executes a
request inside that container and checks the response. A successful upload alone
cannot pass this test. The physical acceptance also requires all three cleanup
results to be confirmed and the final public image catalogue to be empty.

The staging helper accepts `&dyn UpstreamRegistry`, a borrowed trait object. The
borrow lets callers retain their registry client, while `dyn` dispatches through
the implementation chosen at runtime. The same path uses a real upstream reader
in production and controlled fixtures in tests. Refused or interrupted staging
leaves its registered lease ownership available for the server's reaper.


### A listening port can belong to the wrong process

A first-run fixture reserved an API address, closed the reservation and launched
Bun. Another test took that port in the gap. The readiness probe connected to
that other listener and announced success while its own Bun was failing a
reporting-port bind. The retry existed, but the false readiness result bypassed
it.

The fixture now requires the launched child's own API-listener announcement,
parses its `SocketAddr`, checks that it matches the requested address (unless the
OS chose an ephemeral port), and only then probes TCP. An occupied-port test
fails before the change: the first attempt incorrectly succeeds. After the fix,
Bun's bind failure triggers a fresh reservation and the second attempt proves
readiness. This keeps port-allocation races separate from product startup errors;
an unrelated error still fails the test.

### Unknown is not stopped

Hosted macOS CI caught a cancelled-start fixture unwrapping a transient owner
socket error. The fixture was waiting for the independent owner to reach
Running, but treated one broken connection as a final verdict. Under load, the
owner's bounded request deadline can expire while the client is descheduled.

The bounded readiness and retirement loops now accept only the state they are
waiting for. A connection error keeps waiting; it never counts as Running or
Stopped. If the deadline expires, the test still fails. This preserves the
contract that matters: positive evidence establishes readiness and absence,
and uncertain observations establish neither.

### Cleanup through a leader election

The live capacity test rejected an oversized application correctly, then ran its
smaller fixture. Teardown failed anyway. A follower returned HTTP 503 because its
lease leader was temporarily unavailable, and Relish abandoned cleanup at once.

Lease release now retries HTTP 503 during both the initial DELETE and the later
confirmation polls, within the original 30-second deadline. DELETE is idempotent:
repeating it addresses the same durable lease, rather than creating another
cleanup operation. HTTP 202 means accepted, so the client continues polling.
Only HTTP 204 from DELETE, or HTTP 404 after acceptance, confirms completion.
Authentication and conflict errors still return immediately.

The deadline wraps the entire asynchronous operation. Moving it inside the retry
loop would give every attempt another 30 seconds and could wait forever. Our
HTTP fixture reproduces temporary failure before and after acceptance, checks
permanent refusals, and advances Tokio's test clock to prove the overall limit.
The original three-node case still runs against real nodes.

### Retry only a confirmed admission refusal

The concurrent node-fault acceptance test can observe a healthy council and still
reach the fault API after its membership view changes. Hosted CI returned the
explicit refusal that the leader could not be mapped to live membership. That
check runs before reserving or applying a fault. The fixture now refreshes the
views and retries only that exact 503 within a bounded deadline. Lost responses,
other errors and unknown outcomes still fail the test; blindly retrying those
could inject a second fault. Production safety checks are unchanged. The repaired
live scenario passes on macOS (27.087s) and Linux (26.890s).


### Keeping the runtime threads free

Tokio runs async tasks on a handful of worker threads. A task that blocks one,
by reading a large file or walking a directory with `std::fs`, stalls every
other task scheduled there. Three places still did that. Startup swept stale
workload identity directories with `std::fs::read_dir` on the agent's task;
it now decides which names to keep while it can see the agent's state, then
hands the directory walk and deletions to `spawn_blocking`. Rollback read the
stored Bun binary twice, once to verify its signature and once to check its
compatibility, hashing hundreds of megabytes on an async thread; it now reads
it once in a blocking worker and passes the bytes on. And discovery recovery
waited on the runtime inventory with no deadline at all, so a wedged runtime
could hold startup forever. It gets the same five-second bound as the other
inventory reads, and a test with an inventory that never answers checks it
gives up.

### Closing an evidence gap

An application exits. Bun starts its replacement, then dies before saving the
replacement's adoption record. A clean restart test doesn't cover that interval.
Neither does killing Bun during the application's first deployment.

The C34 audit found that our physical interruption matrix covered initial
creation and explicit retries, but left this automatic-restart boundary implicit.
We added a test-only Linux interposer that pauses the adoption record's `fsync`
(the step that asks the filesystem to persist its bytes), before the atomic
rename publishes it. Policy checkpoint writes continue normally. The test proves
that the predecessor's adoption record is gone and a different execution
generation is alive, then kills the actual Bun process.

Recovery must retire that unrecorded execution before reporting readiness. It
must preserve the original kernel owner, publish no stale application and permit
an explicit fresh deployment. The final stop must clear both service and address
obligations. The existing implementation passes; the new regression makes that
claim reviewable and runs in the privileged OCI CI matrix.

We keep implementation closure separate from release acceptance. C34 proves
these ownership and cleanup contracts for supported modes. The independent-host
catalogue, sustained recovery run, exact signed candidate and cold installations
still need their own evidence under V01–V04.
