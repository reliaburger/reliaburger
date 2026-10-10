# A Million Jobs

How many jobs does a cluster run in a day? For most teams the honest answer is
"a few hundred": backups, migrations, a report or two. Then somebody fans out a
render, a test matrix or a data backfill, and the answer becomes a million,
each one a short command that runs for milliseconds. That's where orchestrators
usually tap out, for reasons the first section spells out.

0.2.0 is the release where Reliaburger takes that load on itself. Its promise is
specific: a million tasks submitted as one job, each a real process with its own
exit code, retries and resource limits, accepted and counted by the cluster,
while the apps on the same nodes keep running. A job is a template plus a count;
the control plane records ranges and chunks, not tasks. Short commands run in
warm containers or native host executors instead of a fresh container each.

What 0.2.0 doesn't promise yet is the daily number. The whitepaper's target is
100 million accepted successes a day, and the
[timed-scenario record](../qualification/2026-10-09-timed-job-scenarios/README.md)
projects past that from one-minute windows. A projection isn't a qualification:
the 24-hour run with bounded storage
([#668](https://github.com/reliaburger/reliaburger/issues/668)) is the evidence
that will, and until it passes we don't claim it. The end of the chapter lists
the other gaps still open.

The chapter builds up in two halves. First, the bookkeeping: how a million
tasks become a few hundred bytes of request, a few hundred chunks and one Raft
entry per array per tick, and how singleton jobs, cron and deployment hooks
share that one lifecycle. Second, the execution: why a fresh container per
five-millisecond command is the wrong unit, and what it takes to reuse a
process safely.

## A million jobs without a million records

Everything in the batch path Chapter 12 built ([fifty envelopes](12-squeezing-every-drop.md#a-thousand-jobs-fifty-envelopes) and [durable trackers](12-squeezing-every-drop.md#when-the-leader-forgets)) treats a job as a *thing*: a spec in the request, a record in Raft, a report per transition, an owner helper and two log files on the node. That's fine for a thousand jobs. Now picture a million. The request alone would be a few hundred megabytes, every finished job would be its own Raft write, and a node's job checkpoint (capped at 16 MiB and rewritten on every state change) would stop admitting work somewhere around the fortieth thousand. The whitepaper's answer to "can the leader schedule 100M jobs a day?" says the Raft log records only batch-level decisions. The code, until this section, didn't.

Kubernetes has the same problem in a different shape. An Indexed Job creates one Pod per index, and each Pod is several etcd writes over its life (create, bind, status updates, finalizer removal, deletion). The upstream scalability envelope stops at 150,000 Pods per cluster, so a million tasks run in waves. The usual escape hatch is a work queue: a few long-lived workers draining Redis. That scales beautifully, and the orchestrator no longer knows your tasks exist. Retries, logs and results become your code.

We wanted the other thing: every task its own process, retried and tracked by the orchestrator, at a control-plane cost proportional to chunks rather than individual tasks. This section first builds the pieces as libraries, tested hard and measured in one process, and then wires them into Raft, the API and `relish`. Wiring changes the Raft log and snapshot formats, so it bumps the compatibility generations, and 0.2.0 needs a fresh cluster; before 1.0.0 we don't migrate (the plan in `docs/plans/2026-09-28-plan-million-jobs.md` has the details).

### Template plus count, ranges instead of records

A *task array* is one job template plus a count. Every index from 0 to `count - 1` becomes one task, and `{index}` in the arguments becomes the task's number. A million-task submission serialises to a few hundred bytes; there's a test for that.

The indices are grouped into fixed-size *chunks* (1,024 by default, so a million tasks is 977 chunks). The chunk, not the task, is what the leader hands to a node and what it records. Which leaves one question: how do you remember which of a million things are done without a million entries? You store ranges. `IndexRangeSet` keeps `u32` indices as sorted, disjoint, non-adjacent inclusive ranges. A million tasks finished in order is one pair, `[[0, 999999]]`. Sparse failures cost one pair each.

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<[u32; 2]>", into = "Vec<[u32; 2]>")]
pub struct IndexRangeSet {
    ranges: Vec<(u32, u32)>,
}
```

Those two serde attributes are worth a look. `into` tells serde to convert the value into another type (`Vec<[u32; 2]>`, a list of two-element arrays) and serialise that instead; `try_from` goes the other way on the way in, through a `TryFrom` implementation that can fail. That's where the invariant check lives: reversed pairs, overlaps and touching ranges are refused at the door, so no code path ever sees a malformed set, however it arrived. The private field does the rest. Nobody outside the module can build a `ranges` vector by hand.

Lookups are binary searches with `partition_point`, which takes a predicate that's true for a prefix of the slice and false for the rest, and returns where it flips. Finding the range that might hold `index` is "the first range whose end isn't below it":

```rust
let position = self.ranges.partition_point(|&(_, last)| last < index);
```

Inserting a range finds the run of ranges it overlaps or touches with two of those searches and replaces the run with one merged range using `Vec::splice`. The arithmetic around the ends is done in `u64`, because `last + 1` overflows at `u32::MAX` and the full `u32` domain has 2^32 members, one more than a `u32` can count. The test suite includes a property test (proptest generates random sequences of inserts, removes and takes) that checks the set against a plain `BTreeSet<u32>` model after every step, and re-checks the canonical-form invariant each time.

### The leader's bookkeeping, and a fence

`TaskArrayState` is the leader's view of one array: chunk ids move between a queued set, a held set per node and a done set, and a finished chunk contributes only its counts and its failed indices. It never reads a clock and never holds a record per task, because it's shaped to live inside the Raft state machine, where `apply` has to be deterministic.

The interesting rule is who's allowed to retire a chunk. Suppose node n3 goes quiet, the leader gives up on it and hands its chunks to n1. Then n3 comes back and reports one of those chunks as done. If the leader accepted that, the chunk's tasks would count twice. So every grant carries an *attempt* number, re-granting bumps it, and a completion is accepted only from the node holding the chunk *at the current attempt*. Everything else is a typed error. The distributed-systems name for this is a fencing token, and it's the same idea as the term number on a Raft message: a stale actor can't act, however confident it is.

Granting is a separate pure function, `plan_grants`, which tops each node up to about two rounds of its slots, emptiest node first, lowest chunk ids first. Fast nodes drain their chunks sooner and get more, so there's no up-front split to get wrong. The design doc's original sketch partitioned the count across nodes by capacity once, up front, which guarantees a straggler tail whenever one node turns out slower than predicted.

The test that matters most here is another property test: a random mix of plans, completions, node losses and cancels, after each of which every chunk must be in exactly one of queued, held or done, and every task counted exactly once. Then there's a plain unit test that runs a whole million-task array with 1% of indices failing for good on three nodes. The leader's state at the end is well under the 256 KiB budget, and the number of rounds depends on the number of chunks. At a fixed chunk size this still grows with task count; the gain is amortisation, not constant cost.

### The node: slots, not chunks

On the node, a `TaskPool` runs tasks through a `tokio::sync::Semaphore` whose permits are the node's slots. Every chunk the node holds draws from the same permits, so the tail of one chunk overlaps the head of the next instead of leaving slots idle. Each task is a tokio task in a `JoinSet` holding an *owned* permit, the same RAII trick Chapter 4 used for connections: dropping the permit frees the slot, so there's no release call to forget on an error path.

Running an attempt is behind a trait with two real implementations, which is our rule for when a trait is allowed to exist. Production `OwnedRunner` uses the node's durable runtime ownership and cached images. It holds the resource reservation through cancellation and uncertain retirement; a kill acknowledgement is not proof of exit. `ProcessRunner` remains a direct-process test and benchmark backend with bounded capture. `FakeRunner` computes the outcome from the invocation, and it's what makes a million-task test run in half a minute. It takes the outcome as a closure:

```rust
pub fn new(
    delay: Duration,
    outcome: impl Fn(&TaskInvocation) -> AttemptOutcome + Send + Sync + 'static,
) -> Self
```

`impl Fn(&TaskInvocation) -> AttemptOutcome` means "any function or closure with this signature", and the extra bounds say it can be shared across threads (`Send + Sync`) and doesn't borrow anything short-lived (`'static`). The struct stores it as a `Box<dyn Fn(...)>`, a heap-allocated closure called through a pointer, which is Rust's closest equivalent to a Go `func` value. The fake also records the highest number of attempts it saw running at once with `AtomicU32::fetch_max`, which is how the test proves the semaphore really bounds concurrency.

Waiting for a permit races against cancellation with `tokio::select!`, and here we add `biased;` as its first line. Normally `select!` picks randomly among ready branches, to be fair. With `biased;` it checks them in the order written, so once the array is cancelled, a free permit never wins the race and starts one more task.

### Durable enough, cheaply

A node running thousands of tasks a second can't fsync thousands of times a second. The ledger writes each finished task as a 22-byte record (index, attempts, outcome, exit code, run time and 64-bit grant generation) into an append-only file, in blocks with a CRC32 each, and a background writer *group-commits*: it fsyncs once per 100 ms or 4,096 records, whichever comes first, and only then tells each waiting chunk its records are durable. Only then does the chunk get reported to the leader.

On restart, `replay` reads the file back. Decoding uses `as_chunks`:

```rust
let (raw_records, _) = body.as_chunks::<RECORD_BYTES>();
```

The `::<RECORD_BYTES>` is a *turbofish*, the syntax for passing a generic parameter explicitly. Here the parameter is a number, not a type (a *const generic*), and the result is a slice of fixed-size arrays, `&[[u8; 22]]`, plus whatever bytes were left over. The decoder then takes `&[u8; RECORD_BYTES]`, so indexing into a record can never go out of bounds, and the compiler knows it.

A block cut short by a crash at the very end is truncated before new records are appended: nobody was told those records were durable. Damage anywhere earlier is an error, not a skip, because skipping would silently re-run or lose finished tasks. Anything in a held chunk without a terminal record runs again. That's at-least-once execution, and it's a deliberate trade for tasks this short.

### Putting it together, in one process

The acceptance test in the portable suite wires three simulated nodes (each a real pool with a fake runner and a real ledger on disk) to the real leader state and grant policy. One task in a hundred fails its first attempt and succeeds on retry. A third of the way through, node n3 is lost: its chunks go back to the queue at the next attempt, and its late reports are refused by the fence. At the end every index has exactly one accepted outcome, exactly one retry is counted per failing index, and the three ledgers between them hold a terminal record for all of them. The suite runs 100,000 tasks, which takes about three seconds in a debug build. With the full million it took 30.6 seconds on the laptop, about 32,700 tasks a second, and the leader changed its state in 422 ticks. Real processes will be far slower than a fake; the Criterion suite (`make bench-task-arrays`) measures the machine's fork/exec floor through the real runner, so we'll know by how much before promising anyone a number.

### Wiring it in: one Raft entry per array per tick

Those 422 ticks become Raft entries once the pieces are wired, so the shape of the entry matters. We gave task arrays exactly one new `RaftRequest` variant:

```rust
/// Register, sync, cancel or requeue a task array.
TaskArray(Box<crate::meat::task_array_store::TaskArrayWrite>),
```

`TaskArrayWrite` is its own enum with registration, sync, cancellation and requeue variants, plus atomic mixed-manifest registration and cancellation, and the rules for applying each one live beside the data in `meat::task_array_store`, where they're plain functions with plain unit tests. The state machine's part is six lines. Why the `Box`? A Rust enum is as large as its largest variant, because every value has to fit in the same slot. A `Sync` carries two vectors and a `Register` carries a whole `JobSpec`, and without the box every `RaftRequest` in the log (including the humble `Noop`) would pay for that space. `Box<T>` puts the payload on the heap and leaves a pointer behind. It's the same reason the other big variants in that enum are boxed.

Arrays take their ids from the same counter as ordinary batches, so `relish batch-status 12` names one thing. The apply function borrows the counter as a closure:

```rust
pub fn apply(
    &mut self,
    write: &TaskArrayWrite,
    allocate_id: impl FnMut() -> u64,
) -> Result<TaskArrayApplied, TaskArrayStoreError>
```

`FnMut` permits several calls: a mixed manifest takes one parent ID and one ID per resource profile. Validation happens before any allocation, so an invalid profile rejects the whole submission without consuming IDs. The call site still says `|| batch_state.allocate_id()` without `TaskArrays` knowing about the counter.

The leader runs one loop, on every node, which does nothing unless the node leads. Once a second it reads the replicated arrays and sends each node its share: the chunks it holds, each with its grant attempt. The node's answer is its free slots and the chunks it has finished. For each running array the leader then writes a single `Sync` entry holding both the finished chunks and the next grants. To plan grants that account for the chunks being retired in the same entry, the leader clones the state, applies the results to the clone and plans against that. A node that finished a chunk gets its replacement in the same entry, and `apply` re-checks everything anyway. A holder that hasn't answered for 30 seconds gets a `Requeue`, and the fence from earlier makes its late reports harmless.

The node persists the highest recovery/term/index control version and the highest grant generation per chunk. It reconstructs work from the next valid snapshot: every sync tells it what it holds, and it starts what it isn't running, cancels what it no longer holds, and keeps reporting a finished chunk until the leader stops listing it. That makes a restarted leader and a restarted node the same case as a normal tick. There was one trap. Finished results sat in a map keyed by chunk id, with the attempt stored beside the result. If the leader takes a chunk back and later re-grants it to the same node at the next attempt, the old run (cancelled, but still finishing) could land after the new one and overwrite it, and the chunk would never be reported again. Keying the map by `(chunk, attempt)` makes that impossible, rather than unlikely.

Two smaller Rust points came out of the node. The first is that `TaskRunner` can't be used as a trait object. Its method returns `impl Future`, a type each implementation picks for itself, and `dyn TaskRunner` would need one type known up front. So the node holds an enum, `NodeRunner::Owned`, `NodeRunner::Process` or `NodeRunner::Fake`, whose own `run` matches and forwards. With two implementations that's three lines, and the API state can hold one concrete node type. The second is that Clippy rejected our first version of "open the array if we haven't yet", a `contains_key` followed by `insert`, because it looks the key up twice. The `Entry` API does it once:

```rust
let run = match arrays.entry(assignment.batch_id) {
    Entry::Occupied(entry) => entry.into_mut(),
    Entry::Vacant(entry) => match self.open(assignment).await {
        Ok(run) => entry.insert(run),
        Err(error) => { /* answer with zero slots and the reason */ continue; }
    },
};
```

`entry` returns a handle to the slot, full or empty, and holding it keeps the map borrowed, so nothing else can change the map between the check and the insert.

On the node, a restart replays the ledger and each held chunk runs only the tasks with no terminal record (`TaskPool::resume_chunk`). The binary has to be on the node's `[process_workloads]` allowlist, like any host process, and a node that can't run an array answers with zero slots and the reason, which `relish batch-status` prints. The integration test pushes 100,000 fake tasks through a real single-node council: the whole array cost 51 Raft entries.

What we didn't do is as telling. No per-task Raft entries, obviously. No bitmap for the done set: a million-bit bitmap is 122 KiB whatever it holds, while ranges are eight bytes in the common case. No progress over the reporting tree: it's bincode, so a new field there would drag a binary format along, and a pull over HTTP puts the leader in charge of the cadence. Image tasks now use rootful Linux OCI isolation and resource limits. Host tasks still refuse mount isolation and explicit resource limits, and require the owned process runtime and an allowlist. And no speculative duplicates of slow chunks. At-least-once would allow them, but we'd rather measure a tail before we optimise one.

### Packing profiles and querying outcomes

A shared `ExecutionBudget` accounts for app commitments and actual running attempts in CPU and memory. App creation, rolling replacements and adoption use the same ledger as batch work. Recovery restores ownership even if the node's capacity has shrunk; it stops new admission rather than erasing a surviving app. Waiting batch attempts enter a FIFO resource queue. This prevents continual overtaking by smaller requests, but can leave free capacity idle behind a large request. There is no tenant DRF or app pre-emption; new deployments still need rollout headroom.

A manifest groups up to sixteen homogeneous resource profiles. Stable parent/profile/index identities avoid serialising a million specs. Workers reserve each attempt, not its queued chunk, release requests during retry backoff, and reuse a bounded pool of owned runtime identities per namespace. Each image attempt has a fresh runtime generation, a read-only root and temporary scratch space. Cached images reduce transfer and unpacking cost; runc still launches a container per attempt, so startup cost remains part of the throughput budget.

Terminal outcomes stream into the ledger while a chunk runs. The acknowledgement waits for the checksummed block, fsync and derived redb index transaction. A failed write stops local work and reports a refusal. The leader's accepted owner/generation ranges select results, including failures beyond the capped summary list. The index keeps the newest grant for each task even when an older execution finishes later. Output is keyed by grant too. New worker and array directories are synced in
their parent before a grant can execute, because syncing a file alone does not
make its directory entry durable. Streaming recovery retains one checksummed
block and only records from held chunks; it does not build a sparse set of every
historical index. The explicit `replay` API still collects records for tests and
small callers.

A disaster-recovery epoch can rewind the council snapshot while worker grant fences remain newer. Comparing just term and index would either hang that work or mix histories. The node instead persists a recovery refusal before cancelling old attempts, preserves their directories and requires fresh worker data after re-enrolment. Ordinary leader elections keep the same epoch and still resume durable outcomes. An empty leader snapshot still syncs once with each worker; it cannot assume no work exists locally. The recovery test finishes generation five, rolls control back to generation one in a new epoch, restarts the worker and verifies that neither execution nor deletion can cross that refusal. External effects still need a business key that survives a cluster rebuild.

The derived index uses a 1 MiB cache per profile, rather than redb's 1 GiB default. A node can retain many profiles, so leaving the database default would make job-history queries compete with application memory. The million-record ledger case also exercises index rebuild and lookup with that small cache.

The default view is a summary, not a task list. Watch, JSON status and the dashboard show counts and rates. Histograms merge before p50/p95/p99 are read; those are bucket upper bounds for final attempts, not end-to-end latency. Detail pages inspect at most 4,096 indexes, return at most 1,000 rows and contact at most eight workers, with a cursor even for an empty failure page. Retention starts at terminal acceptance and groups a parent with its profiles. Worker loss can lose detail without changing replicated accepted counts.

The manual's burger manifest runs 1,000 small and 64 larger hashing jobs beside the web service; the executable homepage tour checks both the app and all accepted outcomes. This demonstrates the path, not the whitepaper's daily target. Qualifying 100 million unique successes needs a real sustained run with resource profiles, concurrent apps, retries, failures and storage measurements. The [implementation plan](../plans/2026-10-04-plan-delegated-jobs.md) records that gate.


Control reports carry exact failed counts and a capped preview of 256 index
ranges per chunk. The full result index remains authoritative beyond that
preview. Streaming capture moves to the writer when a task finishes, rather
than keeping another copy until the chunk ends. Otherwise sparse failures in
a large chunk would make the executor retain hundreds of megabytes of output
that it had already persisted.

Owned runtime reuse also has to preserve the source's namespace. A runtime
process alone does not carry the agent's firewall binding. Delegated executors
now live under `/reliaburger/<namespace>/<executor>/<slot>`. The node publishes
the namespace ancestor before starting a descendant, and the eBPF connect hook
uses it when there is no exact app binding. Exact app bindings keep precedence.
The executor caches at most 256 ancestors and evicts only one with no surviving
attempts. It journals cache ownership before publication, and startup retires
old runtime owners before clearing those recorded, same-boot bindings. A live
source check refuses start or stops the original owner when enforcement is lost.
The real-runc namespace regression connects successfully inside its namespace,
refuses a cross-namespace service, removes the live binding during execution,
and checks that the process tree has retired before recovery clears the journal.

Rebasing this path onto the newer security work exposed a second admission
boundary: arrays must bind tags and apply upstream trust rules before committing
any definition, just as ordinary jobs do. Both array and mixed-profile submissions
now use the API's shared image binder, verify required cosign signatures over the
bound digest, and accept every still-trusted cluster root during CA rotation.
Followers preserve caller authentication when forwarding admission and cancellation.
The shared batch counter also checks space for the manifest parent and every
profile before allocating anything. A refusal cannot leave half a manifest or
panic a Raft replica when the counter is exhausted. Regression tests cover both
submission forms, unavailable registries, refused upstream images, missing cosign
verification, and exhausted IDs.

The image-backed acceptance tests have their own `owned_task_arrays` binary.
The regular Linux gate keeps the warmed image mirror alive while they run;
the OCI interruption driver isolates networking and cannot serve that role.
Main's finite evidence registry now names both runtime cases and the cluster
worker-loss case explicitly. A repository regression check catches missing
bindings and stale reviewed OCI source fingerprints before aggregation.

### Definitions, runs and durable trigger identities

A template and a count describe work, but not why it runs. A manually submitted
cleanup, the same cleanup at 03:00 UTC, and a deployment migration need stable
run identities. Replaying the admission transaction after a lost response must
return the original run, rather than launch the command again.

#638 adds `JobDefinition` and `JobCatalog` to the existing
`TaskArrays` state machine. A definition fixes the template, task count and
trigger policies. Each admitted `RunRecord` captures a revision, the complete
definition's digest and its trigger. The execution snapshot remains in the
ordinary task-array record. Updating the reusable definition cannot rewrite
that snapshot or change an existing run's unknown-outcome policy. Count defaults
to one when the task policy is omitted.

`JobWrite` is an enum: its `Put` variant records a definition and optionally
starts a manual or deployment-hook run; `Fire` claims a scheduled occurrence.
Matching the enum forces each transaction to handle its own inputs. The store
prepares metadata on a candidate clone, validates execution capacity, and only
then allocates an ID and publishes both pieces. A refused transaction cannot
advance the definition revision without creating its promised run. A duplicate
manual or hook identity returns the existing run; changed work under the same
identity is refused. This deduplication lasts while the run is retained.

Cron identity uses the definition revision and UTC minute. The same transaction
advances the occurrence cursor and creates the run. With overlap forbidden,
it advances the cursor even when it deliberately skips a firing. The cursor
survives result pruning and definition updates, so neither a new leader nor a
backwards clock step can revive that occurrence. The initial missed-run policy
is explicitly `skip`; there is no catch-up queue. An `allow` overlap policy
still obeys the shared active-run bound.

The catalogue caps reusable definitions and retained run provenance, validates
its shape when deserialising, and bounds complete definition bytes, including
schedule text. Before checking the shared counter, the store computes whether
this exact write needs an ID. Replaying an accepted run or skipping an occurrence
still works when the counter has no IDs left.

The public paths now use those transactions. TOML apply persists a deployment
intent, admits prerequisite hooks, and waits for accepted successful results.
A leader commits app publication and ordinary run admission together. The
standalone controller serialises app publication and cancellation through an
owned mutex guard, held by an independent worker even if the client drops its
event stream. Tokio's `OwnedMutexGuard` keeps an `Arc` reference to the mutex,
so a spawned task can hold it without borrowing a departed stack frame.

Standalone writes clone the checkpoint, preflight identities and progress
capacity, then publish private JSON with file and directory fsync. Only durable
publication replaces the in-memory value. An I/O error fences later writes and
dispatch. Compact initial ranges can expand into sparse progress; admission
reserves that representation before accepting work. Reopen validates the chunk
partition, counts and exact hook identities. Omitted metadata must never open
an app gate. Expired results are pruned before capacity preflight; otherwise the
last receipt could block the submission that would prune it.

Worker admission preserves encrypted templates. Execution decrypts with live
namespace keys in a blocking task, then injects indexed environment without
discarding the decrypted values. Decrypted configuration stays execution-local;
retained output follows the normal scoped capture contract.
A conservative launch marker is durable before start. On reopen an unfinished
marker becomes unknown. Known non-zero exits can retry; uncertain execution
requires a user decision tied to the exact grant fingerprint. Losing namespace
enforcement after launch is unknown too: retiring the process tree cannot prove
that external effects never occurred. The real-container test establishes a
running owner before removing its binding, then checks retirement and that
honest outcome. Bulk runs keep at-least-once replay on the same engine.

Singleton attempts retain successful head/tail output and forward stdout/stderr
under the logical name, namespace and stable `run-ID`. Physical executor slots
can't serve as log selectors because later jobs reuse them. A per-run reader
guard holds that generation while its follower drains. Cleanup closes new
readers, gives existing readers a bounded grace period, then cancels stalled
followers before reusing the slot. Rust's `Drop` releases the guard even when a
client disconnects. Reused process slots replace old capture files before
launch, giving checkpoint readers a new file identity; old bytes cannot become
another job's output. A tail snapshots complete-line offsets and file identity;
following resumes there instead of replaying those lines twice. Bounded CLI and
dashboard summaries show runs, schedules, accepted counts and rates. Detail is
an indexed worker query. Followers preserve the caller's credential for reads
and writes, so forwarding cannot expand a scoped user's authority.

The regressions exercise replay, immutable snapshots, cron rollback and overlap,
hooks across leadership changes, storage failure, malformed reopen, worker
restart, output identity and public endpoints. The release bumped the protocol
and state formats, so it needs matching binaries and a fresh cluster
([cluster compatibility](../releasing.md#cluster-compatibility)). Executor reuse, throughput
qualification and resident model workers remain separate issues; these semantics
don't establish 100m accepted successes/day.

## Short jobs without fresh containers

What does a job that runs for five milliseconds cost? Until now, a whole
container. Bun pulled or found the image, created namespaces, wrote an OCI
bundle, started runc, waited for the payload and tore everything down again.
Caching image layers takes care of the pull. It doesn't touch the rest. For a
nightly backup that runs for an hour, nobody notices. For an array of a million
tiny commands, the setup *is* the job.

How big is the gap? On one four-vCPU VM, in the same sixty seconds, fresh
containers finished 213 BusyBox `true` jobs. Native host executors finished
294,000. That's the one throughput
comparison this section quotes; the rest live in the
[timed-scenario record](../qualification/2026-10-09-timed-job-scenarios/README.md),
next to what they do and don't prove.

The fix isn't one trick. It's two cheaper runtimes, an explicit way to choose
between them, and a lot of care about when a reused process is really safe to
reuse. The [plan](../plans/2026-10-07-plan-reusable-executors-and-throughput.md)
sets out the qualification we still owe before claiming 100 million successes
a day.

### Choosing the runtime per job

A job now names its runtime:

```toml
[job.prepare-record]
image = "registry.example.com/dataset-tools@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
runtime = "shared-runc"
command = ["/usr/local/bin/prepare-record", "{index}"]
cpu = "100m-500m"
memory = "64Mi-256Mi"
```

There are three choices. `runc` (the default) gives every attempt a fresh
container. `shared-runc` runs each command as a new process inside a container
that's kept warm for compatible commands. `process` runs an allowlisted host
binary or script with no image at all. In Rust they're the three variants of a
`JobRuntime` enum, and `#[serde(rename_all = "kebab-case")]` turns `SharedRunc`
into the `shared-runc` you write in TOML.

Our first version guessed the runtime from the fields: an image meant a
container, `exec` or `script` meant a host process. That's convenient right up
until a job has both, or a node quietly interprets an image name as a host path.
Now the runtime is chosen first, and it decides which fields are legal:

```rust
pub fn is_host(&self) -> bool {
    self.runtime == JobRuntime::Process || self.exec.is_some() || self.script.is_some()
}

pub fn validate_runtime(&self) -> Result<(), &'static str> {
    match self.runtime {
        JobRuntime::Process if self.image.is_some() => {
            Err("runtime=process refuses image; use exec or script")
        }
        JobRuntime::Process if self.exec.is_some() == self.script.is_some() => {
            Err("runtime=process requires exactly one of exec or script")
        }
        JobRuntime::Runc | JobRuntime::SharedRunc if self.image.is_none() => Err(
            "runtime=runc/shared-runc requires an image; host exec/script requires runtime=process",
        ),
        // ... a container runtime also refuses exec and script
        _ => Ok(()),
    }
}
```

Two bits of syntax are new here. The `if` after a pattern is a *match guard*:
the arm only matches when the pattern fits *and* the condition holds, so one
variant can have several arms, tried top to bottom. `A | B` matches either
variant. The error type, `&'static str`, is a borrowed string that lives for
the whole program, which a string literal always does. It's fine for a fixed
message; callers wrap it in their own error type.

`exec.is_some() == script.is_some()` is a compact "exactly one of": it's true
when both are set or neither is. And `is_host` is deliberately broader than the
validated rule. An unvalidated spec that names a host command anywhere still
counts as host, so routing and the `host-exec` permission check can never treat
a host command as a container, whichever order the checks run in.

Worker admission and the owned runner check the same contract, so a control
message can't bypass the API. The job schema and stored definitions changed,
so the protocol and state generations in `src/compatibility.rs` advanced
together. Before 1.0, an old cluster starts fresh rather than reading an old
definition differently.

**One node, two backends.** A node that runs both containers and host commands
uses `bun --runtime mixed`, which builds a `MixedGrill<C, H>`. The type is
generic over its container backend `C` and host backend `H`. Rust
*monomorphises* generics: it compiles a separate copy of the type for each
concrete pair, so production gets a `MixedGrill<RuncGrill, ProcessGrill>` with
no dynamic dispatch, and the portable tests get one built from two mocks.
`--runtime runc` stays container-only and refuses host jobs. The adapter never
falls back to the other backend after a failed launch.

The adapter keeps a small route journal recording which backend owns each
instance, written before creation. Status and recovery read that journal rather
than guessing from a PID. Switching an instance to the other backend needs
positive proof that the original owner retired. A missing route isn't
permission to recreate: both backends' inventories must prove the identity
absent first.

A file lock serialises route changes, and the adapter moves that lock into a
spawned task. If the caller's future is dropped mid-create, the lock stays held
until the runtime operation really finishes. That's right for create and stop.
It was wrong for `exec`. `relish exec app -- sleep 3600` held the lock for an
hour: the agent's 300-second timeout dropped the caller's future, the spawned
task kept going, and `stop` waited behind it. `exec` doesn't change a route, so
it now reads the route without the lock and awaits the backend in the caller's
own future. Dropping that future drops the backend call.

Inventory snapshots had the same disease. We had routed them through the
exclusive lifecycle claim used by create, start and retirement, so a read could
queue behind a mutation. A public run of a thousand fresh containers never
finished, because every snapshot timed out behind live container work. Later,
after a slot switched from containers to host jobs, inventory waited on the
claim now held by the running host replacement. Reads now take no claim. They
read the atomically published route files, or ask the original backend for its
retirement receipt, and change nothing. Anything that changes execution
authority keeps the stronger fence. A snapshot is observation.

Lock-free reading has its own trap. Route files are replaced the safe way:
write a temporary file, then `rename` it over the old one, so a reader sees the
old route or the new one, never half of each. The reader opens the file, then
checks it's a private file with exactly one link, the guard against someone
planting a hard link to a file they control. But a reader that opens the old
file a moment before the rename is left holding a file that has just lost its
only name, and its link count reads zero. Our check called that tampering, and
the orchestrator logged "state unavailable" for an executor about once a run.
A planted hard link has *two or more* links; zero means "replaced while you
were looking". `read_bounded` now opens the path again in that case, a bounded
number of times, and the test reproduces the exact interleaving: open, rename,
validate.

A mixed node also has to describe itself honestly. It reported its runtime as
`runc+process`, which the capability classifier didn't recognise, so the
secrets catalogue skipped all its container tests. Now the classifier sees the
container backend on a mixed node, and reports the host backend only when the
executable allowlist is configured. Host commands run with Bun's own authority.
A container capability on the same node must never be read as isolation for a
host workload.

### Keeping a container warm

`shared-runc` keeps the expensive part of a container (its namespaces, network,
mounts and runtime owner) and throws away the cheap part (the process). Which
commands may share one container? Ones that would have got an identical
container anyway. That's the *compatibility key*: a SHA-256 hash of the job
template with the per-command fields removed.

`ExecutorKey::new` clones the template, clears `command`, `exec`, `script`,
`schedule` and `run_before`, fills in the default namespace and resource
ranges, serialises the result to JSON and hashes it. What's left is the pinned
image digest, the namespace, the environment and the CPU and memory ranges. Two
different commands with the same image and limits share a key. A different
memory limit or a rotated credential gets a different container. The key also
refuses an image that isn't pinned by digest: `myimage:latest` can move under a
warm container, and then "compatible" would be a lie.

A pool holds at most 32 of these containers per node, each running one command
at a time. A chunk of a thousand indexes is still a queue, not a thousand
processes. Here's the checkout:

```rust
async fn slot(
    &self,
    key: ExecutorKey,
    reservation: crate::meat::Resources,
    holder: Option<u64>,
    cancel: &CancellationToken,
) -> Option<(usize, Option<Context>, Option<ResourceLease>)> {
    loop {
        let changed = self.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        // ... return None if already cancelled
        let mut slots = self.slots.lock().await;
        let warm = (!self.budget.has_waiters())
            .then(|| {
                slots.iter().position(|slot| {
                    !slot.busy
                        && slot
                            .context
                            .as_ref()
                            .is_some_and(|context| context.key == key)
                })
            })
            .flatten();
        let selected = warm.or_else(|| slots.iter().position(|slot| !slot.busy));
        if let Some(index) = selected {
            // ... charge the budget for an empty slot before any image I/O
            slots[index].busy = true;
            if lease.is_some() || slots[index].context.is_some() {
                slots[index].holder = holder;
            }
            slots[index].key = Some(key);
            return Some((index, slots[index].context.take(), lease));
        }
        drop(slots);
        tokio::select! { biased; () = cancel.cancelled() => return None, () = changed => {} }
    }
}
```

The function prefers a free slot whose container already has our key. Failing
that, it takes any free slot, which may hold an incompatible container to
retire. If nothing's free, it waits for a change and tries again.

`holder` records which run has the slot, but only once there are resources
behind it: a reservation charged right here, or a container that already holds
one. A node reports its busy slots plus the ones that still fit as its
capacity. So a caller that got a slot but is still queued for resources must
not count. If it did, the node would advertise a slot it can't run anything on,
and the leader would send work there instead of to a free peer. Such a caller
becomes the holder only when `admit` charges its reservation. A caller that
retires another profile's container drops back out until its own reservation
is charged. Our first version recorded every caller at checkout, and #654's
author found the phantom slot in review.

A few Rust details carry weight. `bool::then` turns `true` into `Some(value)`
and `false` into `None`, and `.flatten()` collapses the resulting
`Option<Option<usize>>`. So the whole `warm` expression reads: "only if nobody
is queued for resources, find a compatible idle slot". That condition matters.
Without it, a steady stream of tiny jobs could keep reusing warm containers
forever while a large job waited for capacity that never came free.

`Option::take` moves the container context out of the slot and leaves `None`
behind. The caller now owns it. If the caller's future is dropped halfway
through a command, the slot is still `busy` with no context, and stays
quarantined rather than being handed to someone else.

The waiting is the subtle part. `Notify::notified()` creates a future, and
`enable()` registers it *before* we look at the slots. Without that, a slot
could be released between our check and our wait, and we'd sleep through the
notification. `tokio::pin!` fixes the future in place on the stack, which
`enable` needs. `tokio::select!` waits for whichever finishes first, and
`biased;` makes it check cancellation first rather than picking at random.

**The helper.** Inside each warm container, PID 1 is a small static C program.
Rust keeps admission, credentials, outcomes and retirement; the helper only
forks commands and reports on them. Static linking means the image doesn't need
a particular libc or a worker framework of its own. Bun ships the helper inside
its own binary, and that's the first time we've compiled C into the Rust build.

A *build script* is a `build.rs` file at the crate root. Cargo compiles and
runs it before compiling the crate, and anything it prints as `cargo:...` is an
instruction back to Cargo. Ours compiles `helper.c` twice:

```rust
fn compile_executor() {
    println!("cargo:rerun-if-changed=src/bun/reusable_executor/helper.c");
    // ... choose the compiler from CC_<target>, CC or plain `cc`
    for (name, host) in [
        ("rb-executor-helper", false),
        ("rb-host-executor-helper", true),
    ] {
        let output =
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output directory"))
                .join(name);
        let mut command = Command::new(&compiler);
        command.args(["-O2", "-static", "-std=c11", "-Wall", "-Wextra", "-Werror"]);
        if host {
            command.arg("-DRB_EXECUTOR_HOST");
        }
        let result = command
            .arg("src/bun/reusable_executor/helper.c")
            .arg("-o")
            .arg(output)
            .status()
            .expect("failed to execute static Linux C compiler");
        assert!(
            result.success(),
            "static executor helper compilation failed"
        );
    }
}
```

`OUT_DIR` is a scratch directory Cargo gives each build script, so generated
files never land in the source tree. `for (name, host) in [...]` destructures
each tuple in the array as it loops, like Python's `for name, host in ...`.
`main` only calls this when `CARGO_CFG_TARGET_OS` is `linux`, the target being
built for rather than the machine doing the building. And yes, that's
`expect` and `assert!`. Panicking is how a build script says "this build
failed", so the no-panics rule for production code doesn't apply here.

The pool then embeds both executables:

```rust
const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rb-executor-helper"));
const HOST_HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rb-host-executor-helper"));
```

All three macros run at compile time. `env!` reads an environment variable
while compiling (Cargo sets `OUT_DIR`), `concat!` glues string literals, and
`include_bytes!` copies the file into the binary as a fixed-size byte array,
which the `&[u8]` constant borrows for the life of the program. At run
time Bun writes those bytes into the container's private bootstrap directory.
There's no install step and no chance of a helper from a different build.

That bootstrap directory came from a bug. Our first version bind-mounted the
helper as a file into the shared unpacked image, which made runc create a file
mountpoint there. Two containers starting at once raced to create it, and the
first jobs failed with `file exists`. The helper now runs from a private
directory bind, and the image needs no mountpoint at all.

**What a command gets.** The helper starts each command in a sibling task
cgroup, as a separate uid, without the helper's capabilities or descriptors,
with private mount and IPC namespaces, fresh scratch filesystems and an
explicit environment. The image root stays read-only. The PID and network
namespaces are shared between commands in the same container. That's the
isolation trade-off you opt into with `shared-runc`, and why it's not the
default.

Bun talks to the helper over a private Unix socket. It authenticates the peer
against the container's real init process, rather than trusting a PID written
in a message. Every command carries a sequence number. Each message back
(started, output, exited, cleaned up) repeats it, and Bun refuses any message
whose sequence doesn't match the command it's waiting for, so a late message
from one command can never be read as news about the next.

**Positive retirement.** When is a slot safe to reuse? Not when the command
exits. It may have left a background child running, and that child would share
the next command's cgroup and limits. Bun asks for cleanup; the helper kills and
reaps every remaining descendant and replies with a cleanup receipt for the same
sequence; Bun then checks the task cgroup's `cgroup.events` says
`populated 0`. Only then is the slot free. We call this *positive* retirement:
we act on proof that something is gone, never on the absence of news. A
timeout, a cancellation or a missing receipt retires the whole container
instead. Recovery after a Bun restart carries the same obligation, including
the sibling task cgroup, before any work is replayed.

Retirement can't wait forever either. A process stuck in an uninterruptible
kernel wait, say on a hung NFS mount, ignores `SIGKILL` until the kernel lets
go. `RETIREMENT_DEADLINE` gives up after ten seconds: the caller gets its
timeout back, while the slot and its resource reservation stay quarantined. The
eviction loop keeps retrying and frees both once the cgroup finally empties.

Our first version of that deadline was a loop that checked the clock between
attempts. #654's author pointed out in review that this bounds nothing if one
attempt never comes back. Each attempt awaits the runtime's `state` and `kill`,
and runc's lifecycle lock can make either wait indefinitely. So the deadline
now wraps the whole operation, every attempt and the final file removal
included:

```rust
let retired = tokio::time::timeout(budget, async {
    while !self.retirement_step(context).await {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    self.remove_files(context).await
})
.await
.unwrap_or(false);
```

In Rust, a timeout cancels a future by dropping it, at whichever `.await` it
happens to be paused. Nothing inside gets a chance to clean up. Go has no
equivalent: a goroutine only stops if it checks its `context`. Python's
`asyncio.wait_for` is closer, but it at least raises `CancelledError` inside
the task. So everything under the timeout has to be *cancel-safe*: dropping it
halfway must leave nothing inconsistent. Three pieces weren't.

The first was the runtime calls themselves. If an abandoned `kill` were simply
started again on the next attempt, a runtime that hangs would collect one stuck
call per retry. The `state`-then-`kill` probe now runs as its own spawned task,
and the context keeps its `JoinHandle`. Awaiting a `&mut JoinHandle` doesn't
consume it, so a cancelled attempt leaves the handle in place. The next attempt
waits on that same probe instead of starting another.

Filesystem cleanup needs the same ownership. Tokio's `fs` functions hand work
to a blocking thread; dropping the future that awaits a removal doesn't stop
that thread. Our first fix kept the caller's wait bounded, but a retry could
start another removal while the old one was still outstanding. Executor paths
come from the slot number. Once reused, the same path names the next executor,
so a delayed removal could delete its files. The context now also retains one
cleanup `JoinHandle`, awaited by mutable reference. A timeout leaves that handle,
the slot and its reservation together. Only completion permits reuse. The
regression test holds cleanup at a gate across repeated timeouts, retires another
executor alongside it, then opens the gate and checks that capacity comes back.
It doesn't need a hung disk to exercise that ownership rule.

The third was releasing the executor's namespace binding. That takes a lock
and then decrements a counter, and cutting it off in between would leak the
binding. It now runs in its own task, once retirement is proven.

The eviction loop had the same flaw one level up: it retired executors one
after another, so a single stuck one held up all the rest. Each executor now
gets one second per eviction tick, and if it hasn't retired by then, it goes
back into quarantine and the loop moves on. The tests use a fake runtime whose
`kill` never returns. With the old loop, both tests hang until their guard
fires.

Reading the mixed-runtime route journal exposed a different race. The writer
publishes a complete record by renaming it over the previous one. A reader that
already opened the old inode then sees a link count of zero. That is a normal
replacement, while two links still mean an unsafe hard-linked record. Checking
for zero links and *then* inspecting metadata again for privacy leaves a gap:
the rename can happen between the two inspections. We now decide whether to
retry and whether to accept the record from one metadata snapshot. An
`Exclusive` reader, which requires a single link, reopens at most eight times
and checks ownership and permissions on every accepted record. All readers
still refuse symlinks. A deterministic test renames the journal at the validation
boundary; the unsafe-replacement tests keep the security checks honest.

A local upgrade qualification exposed another distinction: `Regular` and
`OwnerOnly` readers do not require a live link. Reopening on every rename could
exhaust their budget even though the already-open record was complete and met
its privacy policy. Those readers now validate and read that snapshot directly.
The complete old record is a valid concurrent observation, just as it would be
if the rename happened after validation. Tests replace the path at every
inspection and require one bounded snapshot read; a separate test refuses an
unsafe original snapshot even when its replacement has valid permissions.

Environment filtering also exposed a test dependency. The OCI crash fixture's
runc wrapper read `OCI_CRASH_ROOT` from Bun's environment, which belongs to the
admission injector, not runtime commands. The rootless gate correctly lost it.
The wrapper now finds its fixture beside its own executable. Bun keeps the
private variable for the injector, and descendants keep their filtered
environment. Fixing the test's dependency preserves the boundary we're testing.
Another cancellation fixture put a fake `ip` on `PATH` inside the owner
wrapper, after recording the command's environment. That override no longer
reaches the command. The test now starts an isolated caller with the fake tool
already on its `PATH`, so the recorded environment includes it without changing
the test runner's environment.
The reviewed OCI test registry pins the source file's hash as well as each
test's identity. Changing the fixture also requires refreshing that pin after
review and runtime validation; otherwise CI correctly refuses the old approval.

**What the real Linux tests found.** Most of the bugs in this path were
invisible to mocks:

- A command allocated 128 MiB under a 32 MiB `memory.max` and still succeeded,
  because the VM had swap. `memory.max` limits resident memory, not memory plus
  swap. Memory-limited image jobs now also set `memory.swap.max = 0`, and the
  test demands a real OOM kill in `memory.events`.
- Eight 300 ms commands with one-second timeouts shared one slot. Three
  succeeded, because the timer started while they were still queueing for the
  slot. The timeout now starts once a command is admitted.
- A warm container's 10 millicores and 8 MiB of helper overhead stay reserved
  while it's idle. Ignoring that let a node advertise three slots where two
  fitted, while ignoring idle compatible helpers made a busy node advertise
  none. Offers now count both. Idle containers retire after a second, and
  sooner if someone is queued for resources.
- A queued command's encrypted environment was decrypted *before* it waited
  for a slot. When the namespace's key was retired during the wait, the
  plaintext still ran. Encrypted values are now resolved against live keys
  again after the wait, and must still match the container's compatibility
  key. The pool receives this as a closure, a small function that borrows the
  runner and template and returns a future. Its `Fn` bound means it can be
  called repeatedly without consuming what it borrows. No plaintext ever
  reaches the replicated template or the worker's ledger.
- A Bun crash beside a running application left the replacement refusing to
  start: `kernel source entries have no original ownership`. Delegated jobs
  publish their namespace into the same kernel map as applications, but
  startup only consulted the application journal. Startup now also validates
  the delegated journal (names, boot identity, cgroup inodes). Recognising an
  entry still isn't permission to clear it: that waits until the old runtime
  owners have positively retired.

### When cleanup killed the next command

The real Linux regression ran two commands through one warm container. The
first succeeded. The second died with `SIGKILL` before it did any work.

Our first cleanup wrote `1` to the task cgroup's `cgroup.kill` file, which
kills everything in the group in one go, then waited for the group to empty and
reused it. An empty cgroup looked safe. The kernel remembered something we
couldn't see. We saw this on Ubuntu's 6.8 kernel with runc 1.4; that's what we
tested, not the full list of affected versions.

To follow it, you need two pieces of Linux. The first is how the helper starts
a command. The classic way is `fork()`, then write the child's PID into the
target cgroup's `cgroup.procs`. For a moment, the child runs outside its
limits. `clone3` with `CLONE_INTO_CGROUP` closes that gap: you pass an open
descriptor for the destination cgroup directory, and the child is born there.
It needs no capability, only write access to that cgroup's `cgroup.procs`.
That's cgroup v2 *delegation*: Bun creates the task cgroup and `chown`s its
`cgroup.procs` to the helper's uid, so the helper can place children there and
nowhere else.

The second piece is how `cgroup.kill` catches a child that's being forked
while the kill sweeps through. Each cgroup has an internal counter, `kill_seq`,
which goes up on every kill. A fork compares the counter before and after; if
it moved, the new child is killed too. On affected kernels,
`CLONE_INTO_CGROUP` read the *parent's* counter (the helper's cgroup) before
the fork and the *destination's* counter after it. Our kill bumped only the
destination's. Every later child into that cgroup saw a mismatch and was
killed at birth, even with no kill anywhere near it. Emptying the group didn't
reset the counter.

There's an [upstream fix](https://kernel.googlesource.com/pub/scm/linux/kernel/git/tip/tip/+/8e359920216689b3b79e0fe8961a77fe312a511f)
that reads the destination's counter both times. We can't assume every node
has it, or a distribution backport, and a version string can't tell us. So
ordinary cleanup no longer touches `cgroup.kill`:

```c
static pid_t launch(int directory) {
    struct clone_args arguments = {.flags = CLONE_INTO_CGROUP, .exit_signal = SIGCHLD,
                                   .cgroup = (uint64_t)directory};
    return (pid_t)syscall(SYS_clone3, &arguments, sizeof(arguments));
}

static int cleanup(int fd, uint64_t sequence) {
    unsigned char receipt[9];
    if (all(fd, receipt, sizeof(receipt), 0) || receipt[0] != 'C' ||
        decode64(receipt + 1) != sequence) return -1;
#ifdef RB_EXECUTOR_HOST
    if (retire_owned_children()) return -1;
#else
    if (kill(-1, SIGKILL) && errno != ESRCH) return -1;
#endif
    int status;
    while (waitpid(-1, &status, 0) > 0 || errno == EINTR) errno = 0;
    if (errno != ECHILD) return -1;
    return event(fd, 4, sequence);
}
```

(We've trimmed a test-fixture branch that uses plain `fork()` off Linux.)
`launch` calls `clone3` through `syscall`, because older C libraries have no
wrapper for it. `cleanup` first checks that Bun's request carries the sequence
of the command that just ran. In a container, `kill(-1, SIGKILL)` signals every
process in the helper's PID namespace except PID 1 itself, which covers a
background child that changed its process group or its uid. The `waitpid` loop
reaps them all until the kernel says `ECHILD`, no children left. Only then does
the helper send event 4, the cleanup receipt, and Bun checks the cgroup is
empty.

`cgroup.kill` still has a job: retiring a whole executor after a timeout,
cancellation or uncertain cleanup. Then the killed task cgroup must never run
another command. Our first version intended that but didn't enforce it. It
ignored a failed `rmdir`, and the next executor's `create_dir_all` quietly
adopted the surviving directory, poisoned counter and all. Now retirement
doesn't count until the task cgroup is really gone, and setup creates it with
`create_dir`, which fails rather than adopting an existing one. A root-gated
test plants a killed cgroup at the next executor's path and checks the command
runs in a fresh directory. A mocked launch would never have found any of this.

### Native host executors

Shared containers fixed the container setup. Host jobs still paid for their
own: each attempt launched a new durable owner, published its ownership record
and polled for the outcome. On rootful Linux, host `exec` and `script` jobs now
borrow the same bounded pool, with a host build of the same helper
(`-DRB_EXECUTOR_HOST`). The compatibility key drops the image and keeps the
namespace, environment and resource profile, so different allowlisted binaries
can share a slot.

The resource story is the same. Each command is born into its limited task
cgroup with `CLONE_INTO_CGROUP`, so CPU, memory, swap and PID limits apply
before user code runs. The helper lives in its own charged cgroup and doesn't
eat the command's allowance. A durable owner holds the helper for the life of
the slot, and the existing group-commit task ledger records each command's
outcome, so there's no new owner record per command. Other platforms keep the
original one-owner-per-attempt backend and refuse explicit CPU or memory
limits, since they can't enforce them.

What's different is that this is *trusted* host execution with resource
controls, not a sandbox. Commands run as Bun's user, with the host filesystem.
That made us look hard at what the helper itself is allowed to do, and the
answer was "too much" in three places:

- **Capabilities.** The container helper keeps `CAP_SYS_ADMIN` and
  `CAP_SETPCAP` to give each command private mounts. Inside a user namespace
  those are harmless. Our first host helper kept them too, and on the host
  they're real: `CAP_SYS_ADMIN` alone lets you mount filesystems. The host
  helper now keeps only `CAP_SETUID`, `CAP_SETGID` (to switch to Bun's user)
  and `CAP_KILL` (to clean up). The gated test reads `CapEff` and `CapPrm` from
  `/proc/<pid>/status` and expects exactly those three bits.
- **Environment.** The helper `exec`s with exactly the environment it's sent.
  Our first version sent only the job's own variables, so `/usr/bin/env`
  printed nothing on Linux, while the same job on macOS inherited everything
  Bun had, cloud credentials included. Every host backend now calls one
  function, `host_environment`, which keeps a short allowlist of Bun's
  variables (`PATH`, `HOME`, the locale and a few more) and lays the job's
  `env` over it. The in-process backend calls `Command::env_clear()` first,
  because Rust's `std::process::Command`, like `subprocess` in Python,
  otherwise inherits the parent's whole environment. Clearing has a cost if
  you miss a caller, and we did: the owner's exec gate now cleared its
  environment too, but `relish exec` built its owner record with an empty map,
  so an exec'd `/usr/bin/env` printed nothing. #654's author caught it in
  review. Exec now copies the workload's own recorded environment, the
  in-memory backend's exec applies the same function, and tests run a command
  found only on the workload's custom `PATH`.
  Recovery exposed a second mistake: validating that saved environment by
  rebuilding it from the recovering Bun's defaults. A restart with a different
  `PATH` rejected a valid owner before it could retire. Inherited values are a
  snapshot of the preparing Bun. Recovery now checks every explicit workload
  override against that snapshot and rejects unrequested variables outside the
  allowlist; it does not compare inherited values with the new caller. Tests
  preserve a historical `PATH` and missing `HOME`, while rejecting changed or
  missing workload values and an injected private variable.
- **The socket.** The helper's socket used to live in `/tmp` under a
  predictable name. Authentication stopped impersonation, but not another user
  creating that name first, which made Bun refuse the slot forever: a cheap
  denial of service. Sockets now live in `/run/reliaburger/host-executors`
  (mode 0711), where only root can create names. The test plants a file at
  the old `/tmp` name, owned by `nobody`, and checks the next command starts.

Cleanup is harder without a PID namespace. `kill(-1, SIGKILL)` on the host
would signal every process Bun's user owns. So the host helper makes itself a
*subreaper* with `prctl(PR_SET_CHILD_SUBREAPER)`: orphaned descendants are
re-parented to it rather than to init, even after a `setsid`. Its
`retire_owned_children` reads its own children from
`/proc/self/task/<pid>/children`, opens a *pidfd* (a file descriptor that
refers to one specific process) for each and signals through it, then reaps
and repeats until none are left. A pidfd can't be fooled by PID reuse, and
because the helper is single-threaded and hasn't reaped the child yet, the PID
it read is still that child. Then the same cleanup receipt and empty-cgroup
check apply. Losing Bun's connection makes the helper retire its children
before it exits.

One more host bug was about output. The helper relays stdout and stderr with
`poll`, at most sixteen 4 KiB reads per stream per pass, so a flooding command
can't starve the exit check. After the command exited, our first version made
just one more pass. A pipe holds 64 KiB by default, so that looked like plenty,
but a command can grow its pipe to a megabyte with `fcntl(F_SETPIPE_SZ)`. One
that wrote 500,000 bytes and exited lost most of them, and still reported
success. Now the helper reads each stream to end-of-file after exit. If a
background descendant still holds the pipe open, it stops once that's been
quiet for 10 ms or after 16 MiB, and appends a visible
`[reliaburger: output written after exit truncated]` line. The test makes the
race deterministic: it sends `SIGSTOP` to the helper while the command fills
its pipe and exits.

That fix had a bug of its own, and only a benchmark found it. Each loop pass
reads the pipes and then checks whether the command has exited. For a quiet
command both pipes are already at end-of-file in the pass that sees the exit,
but the "both streams closed" check ran only after the next `poll`. With the
command gone, that `poll` waits its full 10 ms. Ten milliseconds sounds
harmless, but a `busybox true` takes well under one, so every command now cost
about 11 ms. Host jobs fell from the roughly 4,000 a second we'd measured
before to 1,233 a second, the same in every round. The check now runs before
polling. A gated test times 200 quiet commands on one warm executor: 2.35
seconds with the bug, 0.17 seconds without, against a two-second limit.

None of this keeps a *model* loaded. Each command is still a new process that
loads whatever it loads. Keeping a model resident between requests is a
separate piece of work, #641.

### Feeding fast workers

With commands this cheap, the bottleneck moved. Workers finished their durable
completions fast, but public jobs crawled. The cause was the leader's grant
loop. Recall that a node holds a couple of granted chunks at a time and asks
for more as it finishes. Each one-second control tick, the leader accepted the
node's receipts, and only on the *next* tick did it deliver the grants that
replaced them. A fast worker emptied its chunks and spent most of each
two-second cycle waiting.

The fix is *grant lookahead*: give fast nodes enough queued work to cover the
gap. How much is enough? That depends on how long commands take, so the leader
learns it from the final-attempt duration histogram the nodes already report.
Bucket `i` counts commands that took up to `2^i` ms. Using the upper bound of
each bucket gives a conservative estimate of throughput: slots × two seconds ÷
average duration. Learned depth is capped at sixteen chunks.

That's the idea. The edges took four more fixes:

- **A slow start mustn't stick.** The last bucket has no upper bound, and our
  first version fell back to the small window if it held a single sample. One
  cold image pull switched lookahead off for the rest of the array, and since
  the counts only grew, it was never forgotten. The planner now uses a second,
  *decaying* histogram, and falls back only when more than one in sixteen
  recent samples overflowed.
- **The tail must be shared, by capacity.** `plan_grants` tops up the
  emptiest node first. Near the end of an array, the first fast node could
  take sixteen of the last twenty chunks while another sat idle. Our first
  cap split what was left evenly, and #654's author showed in review that this
  is wrong too. With 27 slots on one node and 8 on the other, the last twenty
  chunks went 10/10, so the bigger node finished early and the smaller one
  owned half the tail. The cap is now each node's share of every outstanding
  chunk, queued or already held, in proportion to its slots: 15/5 in that
  example. Counting held chunks means a node still working through a big
  grant doesn't get more on top. Rounding each share up would hand out more
  chunks than exist, and rounding down would strand some. So the whole parts
  go out first, and the leftover chunks go to the largest fractions (the
  *largest remainder* method that some countries use to share out
  parliamentary seats). The baseline window still applies, so even a node
  with a thousandth of the capacity gets two chunks.
- **Lookahead needs automatic replay.** If a node dies, every chunk it held
  has an unknown outcome. Arrays that replay automatically don't mind. Arrays
  that need an operator to acknowledge and replay would turn sixteen chunks
  into manual work. Those keep the baseline window, and the leader passes the
  run's policy to `plan_grants` as a plain `bool`.
- **The durations must be honest.** The executor used to time the whole
  `runner.run` call, which for a pool includes waiting for a slot and, on a
  cold start, an image pull. A 5 ms command queued behind 256 callers reported
  50 to 200 ms. `Attempt` now carries `ran: Option<Duration>`, filled from the
  helper's start receipt to its exit receipt. `None` says "this runner can't
  tell", which is not the same thing as zero and doesn't look like a very fast
  command. Fresh containers keep the old clock: starting the container is part
  of their cost.

The decay is the part with a distributed-systems twist:

```rust
while self.recent_duration_counts.iter().sum::<u64>() > RECENT_DURATION_SAMPLES {
    for recent in &mut self.recent_duration_counts {
        *recent /= 2;
    }
}
```

Once the recent histogram holds more than 4,096 samples, every bucket is
halved. That's exponential decay, the same thing an exponentially weighted
average does with a factor like 0.9. Why not use a float? Because this state
lives in Raft. Every replica applies the same receipts and must end up with
byte-identical state, and integer division gives the same answer on every CPU
and compiler. Floating-point rounding mostly would, but "mostly" isn't a word
you want near a replicated state machine. `&mut self.recent_duration_counts`
iterates over mutable references to the array's elements, and `*recent`
dereferences each to update it in place. The planner's arithmetic then runs in
`u128`, so multiplying sixteen `u64` counts by durations can't overflow. This
is pure planning from committed state. The decaying histogram is the one new
durable field, so it rode on this release's existing state-format bump.

Lookahead only works if nodes advertise honest capacity, since queued grants
still wait for the node's concurrency and CPU/memory admission. Our first
version counted every caller inside `runner.run` as running, including callers
still waiting for a pool slot. A node whose budget fitted two executors
advertised 32 slots.

The first fix swung too far the other way. It counted commands between the
helper's start and exit receipts. A `busybox true` lives for about a
millisecond, so a node running 27 executors flat out would sample only the few
commands caught mid-flight, and advertise far less than it was doing. (We first
blamed this for a benchmark plateau; the drain bug above was the real cause, but
the under-count is real too.) The right count sits between the two. Each
pool slot records which run's caller holds it, from the moment resources are
charged to it until release, setup and cleanup included. A caller waiting for
a slot or for admission doesn't count, and a millisecond command does. One regression fills a two-executor budget with 64
two-second commands and checks the node advertises two. Another stops the
helper with `SIGSTOP` so a submitted command holds its slot without starting,
and checks the slot counts as busy while no command counts as started.

The started-command count still has a job: it feeds `relish batch watch`,
which shows verified commands beside other attempts. A backend without start
receipts, including fresh containers, reports it as unknown rather than
guessing from a launcher PID.

The price of lookahead is ownership. More granted work may need reconciliation
or replay after a worker is lost. That window is bounded, and stale attempts
keep their existing fences.

### What the measurements do and don't show

Measuring this turned out to be as instructive as building it, mostly because
of the measurements that lied. The numbers live in two records: the
[earlier matched and one-hour runs](../qualification/2026-10-09-host-job-executors/README.md)
and the [current equal-minute scenarios and concurrency sweep](../qualification/2026-10-09-timed-job-scenarios/README.md).
Here's what we learned reading them.

**Compare like with like.** The first landing-page experiment made host jobs
look ten times slower than shared containers. It also reserved a whole CPU for
each host job and a tenth of one for each shared command, so on a four-CPU node
the two paths ran different numbers of commands at once. Every path now uses
the same CPU request, limit, memory and concurrency.

**Equal counts aren't equal work.** A million raw processes and a thousand
fresh containers take wildly different times, so the totals can't be lined up.
The demonstration now gives every path the same sixty seconds and credits only
outcomes the leader accepted before the cutoff. A minute's count times 1,440 is
a daily *projection*, and we label it as one.

**Receipts have a granularity.** The first equal-minute run reported zero
fresh-container successes despite real progress. Fresh containers couldn't
finish a thousand-job receipt chunk inside a minute, and an unfinished chunk
earns nothing. Fresh runs now use one-job chunks; fast paths keep a thousand.
That changes reporting, not resources. An empty accepted window now fails the
harness instead of quietly printing zero.

**Cold and warm are different questions.** Each concurrency point starts with
cold executors and a warm image, then measures a cold minute and a warm minute
in the same submission, subtracting the counters at the boundary so no job is
counted twice.

**The raw baseline isn't a floor.** Raw `fork` and `exec` in the VM have no
limits, durability or outcomes. Adding the same cgroup limits to the raw path
made it *slower* than our native executor, because the raw runner moves each
child into its cgroup from a large Rust parent, while the helper clones
straight into it from a tiny one. So you can't subtract one rate from another
and call the difference "scheduler overhead". Isolation and durability have
real work to do.

**A no-op measures overhead.** Every run used BusyBox `true`. That isolates
per-job cost, which is exactly what this work attacked, but it says nothing
about a command that does real work. Size a real workload with real commands
on your own hardware.

**More concurrency isn't always faster.** Twenty-seven slots came from
admission arithmetic: what fits in the VM's job budget. The sweep from 1 to 64
showed host jobs plateau from 8 and fall off past 32, while shared containers
liked 27 once warm. We keep 27 as a common comparison point because it makes
the comparison fair, not because it's the best setting for each runtime. The
host plateau also says the next limit is work supply through grants and
receipts, not process creation.

**A minute isn't an hour.** Each path then ran for an hour at admitted speed,
one after another, beside the same live application. No path had a terminal
failure and the application answered every probe. The hours also showed how
much a cold minute understates a warm pool: shared containers averaged more
than three times their first-minute rate over the hour. And the VM's CPU was
only a little over half busy for the public paths, against more than 90% for
raw processes, so the next limit is feeding work and accepting receipts, not
starting processes.

**An hour is not a day.** Storage is the unfinished part. Free disk in the VM
fell to about 200 MiB by the end of the host hour, and every bounded storage
scan was incomplete, so none of this proves storage stays bounded. A fixed pool
of 32 live slots doesn't bound history either: slot identities include the
namespace, so rotating through new namespaces leaves retired ownership and
routing journals behind. The fresh-container hour also recorded four retries
that later succeeded, and we couldn't say why: a bulk success record keeps the
final outcome and the attempt count, not the earlier failure's reason. At this
volume you want bounded summaries of failure causes, not millions of log lines
for successful jobs. A real daily run, faults and collection of that history
remain in #668.

**Benchmark your own fixes.** The review that hardened this code was checked
by rerunning the benchmarks on the fixed build, and the reruns found three
regressions in the fixes themselves. One was the 10 ms drain wait above. The
second was a single line. Bounding retirement had made the "is the task group
empty?" check async, and the read moved to `tokio::fs::read_to_string`. Tokio's
file functions hand each call to a pool of blocking threads, because ordinary
file I/O can stall a thread. That's the right default for a disk. But
`cgroup.events` lives in cgroupfs, which, like `/proc`, is answered from kernel
memory and never waits. The read happens once per command, and the thread hop
cost about 3% of host-job throughput. It's a plain synchronous read again,
with a comment saying why.

The third was memory. Bun's resident memory after five minutes of host jobs
moved by up to 170 MiB between builds, with changes that had nothing to do
with memory. Reverting them one at a time never brought it back to #654's
figure. The cause was glibc's allocator. To avoid lock contention, glibc gives
busy threads their own *arenas*, up to eight per core, and keeps freed memory
in each arena for reuse rather than returning it to the kernel. Tokio's worker
and blocking threads all count. So resident memory tracked how many threads
had ever been busy, not how much data Bun held. Go and Python manage their own
heaps, so you meet this mostly in C, and in Rust, which uses the system
allocator by default. Setting `MALLOC_ARENA_MAX=2` brought the same run down to
about 190 MiB. Bun now does it for itself, before the runtime starts any
threads:

```rust
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn limit_malloc_arenas() {
    if std::env::var_os("MALLOC_ARENA_MAX").is_some() {
        return;
    }
    // SAFETY: mallopt only tunes the allocator, and runs here before Bun
    // starts any other thread. A value glibc rejects is reported by the return
    // value, which we can ignore: the default arenas simply stay in place.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
}
```

`#[cfg(...)]` compiles the function only on Linux with glibc. musl and macOS
have different allocators and no such knob. Calling a C function is `unsafe`
in Rust because the compiler can't check what C does. The `// SAFETY:` comment
records why this call is fine, and an operator's own `MALLOC_ARENA_MAX` still
wins. Bun now ends the same run at 165 MiB, against 587 MiB for #654, and
accepts at least as many jobs: its hot paths wait on I/O, not on the allocator.

**Keep the failures.** One rerun of the direct matrix forgot its private
hostname wrapper, collided with the live node and produced a thousand startup
failures. The record keeps it beside the successful rerun. Runs use the normal
three attempts and report accepted retries separately, because a retry that
succeeds hides its cause rather than fixing it.

**Put the experiment in the tool.** Reproducing a benchmark shouldn't mean
remembering a dozen paths from someone's temporary directory, so the four
scenarios are now part of Relish. `relish bench --scenario
jobs-shared-containers` runs with sensible defaults; there are scenarios for
fresh containers, host processes and the raw baseline too. The runner counts
through an `AcceptedCounts` value that refuses a counter going backwards or a
changed total, so polling twice can't turn one success into two. The active
submission is an `Option`: `None` means we may submit, `Some` means we must
keep watching the work we own. Ctrl-C triggers a `CancellationToken` that stops
submission and observation without skipping cleanup. The raw baseline keeps its
child waiters in a Tokio `JoinSet`, a set of spawned tasks you can await as
they finish, and drains exits after the deadline without crediting them.

Adding the benchmark's flags taught a Rust lesson of its own. Clap's derive
macro generates the parser from the command enum, and with every benchmark
option inline the generated code overflowed the test threads' default stack,
in tests for unrelated commands too. A Rust enum is as large as its largest
variant, and that size lands on the stack wherever the value is built. The
options moved into their own `#[derive(clap::Args)]` struct, held as
`Bench(Box<BenchOptions>)`. `Box<T>` puts the value on the heap and stores only
a pointer, so every variant shrank back to a few words. When you add a
specialised command, keep running the ordinary command tests: generated code
can change their stack use too.

Inside each pool, admission, cold setup, command execution and cleanup each
record into a fixed sixteen-bucket histogram of `AtomicU64` counters, so
concurrent slots record without taking a lock. A small guard records the
elapsed time in its `Drop` implementation, Rust's destructor, which runs on
every exit path, error returns included. Its lifetime spans exactly one phase,
so waiting for a slot can't leak into execution time.

### Soak jobs while the apps keep running

A fast command completing a million times tells us little about what happens
when Bun dies halfway through owning it. The release soak already kills agents,
powers off nodes and loses quorum while data-bearing apps run. It now drives
all three job runtimes through the same faults, within the existing 90-minute
fast and eight-hour final tiers.

The [extension plan](../plans/2026-10-10-plan-release-job-soak.md) describes a
bounded controller with two resource profiles per runtime. It persists each
submission intent before sending it, then reuses that exact request ID after
a lost reply or controller restart. Accepted counters must conserve indexes
and never regress. A small authenticated verifier on each VM independently
records the audited cohort's logical effects; repeated attempts are counted,
not claimed as exactly-once execution. Cron, publication-triggered singletons
and deploy gates use the same common job path. Fresh containers do not expose live
command activity through the summary API, so their fault-overlap proof joins an
independent boot/start-time receipt to the current private generation and a
populated cgroup. Counting queued callers as active would fabricate coverage.

The controller deliberately tests non-zero exits, deadlines with descendants,
memory limits and cancellation. Long jobs opt out of automatic replay. If a
fault leaves an unknown outcome, the test operator acknowledges only a known
replay-safe fixture, after observing its exact grant fingerprint twice.
That explicit decision is part of the evidence, not a production recovery
policy.

Reusable executors outlive their commands, so the app-only leak checker needed
to change. It now recognises an exact executor only when its private journal,
current boot, generation, pool slot and cgroup identity agree. A prefix is no
proof. Complete disk inventories and actual cgroup limits accompany the owner
proof; they do not substitute for the scheduler's commitment ledger. Missing
job evidence, stale heartbeats, orphaned owners or incomplete drain fail the
release verdict. Drain begins inside the last two minutes, with no new settle
allowance.

This machinery still needs the normal staged fast and final qualification on
the integrated candidate. It records reliability under app load and faults;
the hourly saturation measurements and #668's retained-storage/24-hour work
answer different questions. No extrapolated daily rate determines a soak pass.

## What 0.2.0 doesn't do yet

An audit of the job path before the release
([#674](https://github.com/reliaburger/reliaburger/issues/674)) found gaps
that matter at a million tasks and barely show at a thousand. Each one is being
closed in 0.2.0, and each fix rewrites its paragraph here, so this section
should shrink to nothing before the release ships.

- **Bounded storage per task**
  ([#678](https://github.com/reliaburger/reliaburger/issues/678)). Every
  accepted task leaves a little behind on the node: ledger entries, captured
  output, executor metadata. At 0.2.0 rates that adds up to a full disk within
  hours, and nothing reclaims it between runs.
- **Fairness across namespaces**
  ([#679](https://github.com/reliaburger/reliaburger/issues/679)). Job limits
  are cluster-wide and namespace quotas ignore jobs, so one tenant's arrays can
  starve everybody else's cron jobs and deployment hooks.
- **App placement that sees jobs**
  ([#681](https://github.com/reliaburger/reliaburger/issues/681)). Job attempts
  reserve CPU and memory in each node's own budget, which the leader's app
  scheduler can't see. It can place a replica on a node that jobs have already
  filled.
- **Job history and events**
  ([#684](https://github.com/reliaburger/reliaburger/issues/684)). Finished
  runs are pruned within the hour, there are no job metrics or events, and
  array tasks don't show in `relish top`. You can't yet answer "did last
  night's cron job succeed?" the next morning.

## Lessons

**Store the shape, not the instances.** A million tasks fit in a few hundred
bytes of request and one range pair of progress because nothing ever writes
down a task individually unless it did something unusual. Ranges, chunks and
one Raft entry per array per tick all follow from the same refusal: the
control plane's cost should grow with decisions, not with work.


**Reuse needs proof, not hope.** Every reuse bug in this chapter had the same
shape: a slot looked empty, so we reused it. An exit code isn't proof the
command's children are gone. An empty cgroup isn't proof the kernel has
forgotten it was killed. A missing directory record isn't proof nothing was
published. Positive retirement, acting only on a receipt or an observed empty
state, is slower to write and much faster to debug.

**Mocks can't see the kernel.** The `kill_seq` bug, the swap that let a
memory-limited command survive, the racing image mountpoint and the oversized
pipe all needed a real Linux node. Mocks were still the right tool for the
state machines around them. They just can't fail the way a kernel does.

**Reads shouldn't wait behind writes.** Twice, an inventory snapshot queued
behind a mutation's lock and starved a control loop. A lock that deliberately
outlives an abandoned caller is the right fence for changing authority, and the
wrong one for looking.

**Distinguish "pending" from "broken".** A recovery test failed because it
read a logical run as active while its retry was still waiting for a runtime
binding, then treated the `pending` instance with no PID as a running process
missing one. The waiter now waits through non-running states but still fails
at once if a known running process has no PID. We kept the cases separate, so
fixing the test couldn't hide the real missing-PID bug it was written to catch.

**Measure the thing you claim.** The most misleading numbers here weren't
wrong. They measured something else: unequal reservations, unequal counts,
receipt chunks bigger than the window, a no-op instead of real work. Writing
down what a number *doesn't* show turned out to be most of the work.

**Durable identity before durable work.** Replaying a lost admission must
return the original run, and a new leader mustn't revive a cron occurrence the
old one skipped. Both work because the identity (a request ID, a revision and
UTC minute) is committed in the same transaction as the run, before anything
executes. Get the identity right and retries become boring.
