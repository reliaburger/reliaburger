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
