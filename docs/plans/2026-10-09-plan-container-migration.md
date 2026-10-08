# Plan: moving running workloads between nodes

9 October 2026. Release series: 0.4. Pull request:
[#268](https://github.com/reliaburger/reliaburger/pull/268). Milestone:
[0.4.0](https://github.com/reliaburger/reliaburger/milestone/7).

This is the delivery plan: what we'll build, in what order, and which tests
prove it. The research behind it, the continuity contract and the long-form
design live in
[Research: moving a running container between nodes](2026-09-28-research-container-migration.md),
which we cite as "the research" below. Nothing here is implemented yet.

## Decisions

These are the decisions in force. The research's section 12 keeps the history
of how we got here (28 September, 5 and 6 October).

1. Moves use the runc CLI under the existing runtime owners, not a new CRIU
   client. ProcessGrill and the Apple runtime refuse checkpoint and live moves.
2. A move has a mode: `cold` (restart, carry managed data), `checkpoint`
   (memory, execution and filesystem; TCP may close) or `live` (checkpoint plus
   preserved sessions within an interruption budget). The research's section 1
   defines what each mode must preserve.
3. Checkpoint and live never fall back to a cold restart unless the workload's
   policy allows it explicitly, and then the result is reported as degraded.
4. Target activation is the one-way recovery boundary. After it, no timeout or
   error authorises restoring the source's older checkpoint. Uncertain ownership
   holds the workload as recovery-required (research, section 5.6).
5. Plaintext process dumps stay in bounded no-swap memory and never reach disk.
   Transfers are authenticated and encrypted.
6. Pre-1.0 rules apply: an incompatible format bumps the compatibility
   generation from the then-current main, with a fresh cluster and no migration
   code.
7. Host-path apps block drain. `--force` stops them with their data left in
   place, and says so.
8. GPUs, rootless runtimes, automatic rebalancing and cross-architecture restore
   are out of scope.
9. The book gets a new chapter for this work, written alongside each step.

## Naming

"Migration" already means three things in Reliaburger. A `run_before` job is a
schema migration in the manual (`docs/manual/01_deploy-an-app.md`), the
Kubernetes importer prints a "migration report", and the cluster apply plan for
prerequisite jobs is called cluster migration
([plan](2026-10-04-cluster-migration-prerequisites.md)). Adding a fourth would
make `relish test --filter migration` and every error message ambiguous.

So everything the operator types or reads says **move**:

| Surface | Name |
|---|---|
| Command | `relish move <namespace>/<app> --to <node>`, `relish move status`, `relish move cancel` |
| App and job policy | `[app.<name>.move]` and `[job.<name>.move]`, with `mode = "cold" \| "checkpoint" \| "live"` |
| Built-in tests | `relish test --filter move`, `--profile move`, `--chaos --profile move-recovery` |
| Records and metrics | `MoveRecord`, `move_id`, `reliaburger_moves_total` |

Prose can still call the feature "container migration" or "live migration",
because that's what people search for. The roadmap headline keeps it. Drain,
cordon and uncordon keep their usual names; `relish fault node-drain` stays the
simulated Smoker fault and the manual says how it differs from a real drain.

## Spikes

None of these are results yet. Run them on the then-current main's generated
OCI spec, pinned runc and CRIU packages and real kernels. x86_64 and arm64 need
separate evidence.

| Spike | Question and consequence |
|---|---|
| S1 | Checkpoint/restore with rootful userns, external netns, private overlay, volume backends and resolv.conf. Unsupported combinations block their claimed mode. |
| S2 | Inherited/file/pipe stdio and capture ingestion across restore. Resolve owner changes before checkpoint ships. |
| S3 | Preserve logical ordinal/job execution with new runtime/host identity, time namespaces and declared checkpoint TCP closure. |
| S4 | Enforce cgroup egress before any restored workload executes; reject unsupported ordering. |
| S5 | Restored init parentage, boot/start-tick evidence, Bun adoption, stop/exit receipts and stale-generation rejection. |
| S6 | Cross-host timers/monotonic time and remote timeout/lease behaviour within a declared budget. |
| S7 | Memory/volume/rootfs payload and client-observed pause for realistic sizes/rates; set envelopes from results. |
| S8 | Release-specific resource matrix and refusal fixtures for io_uring, devices, attached tracers, packet pipes, corked UDP and unsupported IPC/socket/exec dependencies; supported-counterpart round trips. Exercise late resource acquisition, actual dump rejection and source continuation/state/session correctness; no implicit cold fallback. |
| S9 | Actual dirty tracking/pre-copy and convergence on qualified x86_64/arm64 hosts; select from capabilities. |
| S10 | Lazy-page TLS/manifest binding, source/provider loss, fault stalls, provider independence and optional redundant-page recovery. |
| S11 | TCP_REPAIR with packet locking, borrowed-address prototype and source-independent address/NAT ownership, ingress and repeated moves. Prototype success alone cannot pass the release gate. |
| S12 | Partition-safe source retirement and target activation with current main's runtime/storage/recovery fences; no timeout-authorised second writer or stale replay. |
| S13 | Consistent frozen filesystem cut, metadata/whiteouts/unlinked files, same-size/mtime modifications, quota and loop/Btrfs provisioning. |
| S14 | Credential mobility or tested reload/rotation, fresh authenticated sessions and source retirement authority. |
| S15 | No-swap reservations, protected transfer-key recovery across Bun/owner/node loss, staged import confinement and interruption cleanup. |
| S16 | Tiny signed/pinned mechanism fixture plus Redis first and PostgreSQL worker-mode second, on both architectures. Prove memory-only/persistent data, original sessions, open SQL transactions and active queries with independent ledgers and no reconnection/retry masking (research, section 9.2.1). |
| S17 | Migration under authenticated test leases; expiry/stop/cleanup at every boundary on both nodes. |
| S18 | Versioned profile completeness, directional pools and recorded evidence; partial/skip/unknown cannot certify conformance. |
| S19 | Actual drain followed by source VM shutdown/restart under live traffic, with independent entry/observer topology and no resurrection. |

**Portable/model tests first:** state transitions and epochs; delayed/duplicate
messages; target activation uncertainty; stale source rollback forbidden;
capability/policy refusal; job execution/retry binding; manifests/encryption/
metadata import; reservation concurrency; lease expiry/cancel; profile completeness
and verdict aggregation. A property test interleaves leader changes, partitions
and crashes and checks exclusive ownership and acknowledged-state monotonicity.
It must allow recovery-required unavailability: universal terminal success by a
deadline is not an invariant.

**Portable integration:** use the existing in-process cluster/public API harness
in `tests/suite/` with mock runtime outcomes, including restore that executes then
returns an error and an isolated target that is still running. These tests protect
orchestration; they do not count as real CRIU or connection-preservation evidence.

**Gated Linux/cluster:** real runc/CRIU with current owners on one host for runtime
integration, then actual cross-node movement and data paths on both architectures.
Match `make test-linux`/`make test-cluster` and add a `test-move` gate only if ownership
and discoverability need it. Once selected, missing prerequisites fail/refuse;
never return early as a passing gated test. CI initially proves which hosted
runners can support CRIU; unavailable hardware gets an explicit owner and release
qualification lane, not a green mock substitute.

**Built-in operator cases:** short demonstration and complete migration profile
run the same Redis/PostgreSQL fixtures/scenarios/assertions with different
coverage/load, alongside mechanism fixtures in conformance. Recovery
cases share observations but use explicit chaos selection and node-state authority.
Wire catalogue, capabilities, lease/authz routes, reports and built-in manual
consistently; verify the documented commands select real required cases.

**Release/soak:** repeat A->B->A under load with the volume writer, memory-only
counter and the required Redis/PostgreSQL fixtures; exercise main's recovery
faults and each database's source-off case. Cover both architectures, every claimed
backend, ingress/egress
paths, jobs and credential continuity. Retain acknowledgement/session ledgers,
per-cutover latencies, generation transitions, fingerprints and cleanup outcomes.
Retries must not erase first failures. No pass on an empty run or skipped-only lane.
Main's release qualification scripts own hardware/soak execution and save records
in `docs/qualification/`; reuse the versioned conformance manifest so release
checks cannot drift into a separate weaker demonstration.

**Packaging:** package pinned qualified CRIU/runc in the appliance/managed Linux
guests, check Ubuntu 26.04 availability before depending on a PPA, and expose
runtime capabilities in `wtf`/dry-run. No automatic PPA installation on an existing
cluster from `relish test`. Verify signed mechanism and Redis/PostgreSQL fixture
image availability before timing; support existing mirrors/local staging rather
than depending on a mutable tag.

## Implementation order

The old **23-29 focused weeks** was for the source-forwarding/automatic-cold-fallback
design. It is historical, not the estimate for this revised contract. Portable
network/NAT/ingress ownership, partition-safe activation, key/credential recovery,
lease integration and conformance add material work. Re-estimate after S1-S19
resolve the architecture; do not mechanically add a few weeks to the old total.

Work in dependency order, behind meaningful tests, while keeping the full 0.4.0
release acceptance scope:

1. Refresh main evidence; implement fixture/observer and ownership/policy/model
   tests; prepare Redis then PostgreSQL fixtures and run their feasibility spikes
   before locking the transport/network design.
2. Cordon/drain ownership and status, held assignments, exclusive source fencing,
   activation and safe recovery. Cold managed-data moves provide the first path.
3. Stable logical/storage/job identities, consistent copy and capacity reservations.
4. Checkpoint runtime/stdio/time/egress, secure no-swap transfer and recoverable
   journals/keys; credential continuity. Establish real Redis and PostgreSQL
   checkpoint/state/session results before extending live guarantees.
5. Qualified memory/filesystem pre-copy and optional post-copy with honest failure
   envelope; interruption measurements.
6. Source-independent addresses, egress mapping and ingress ownership; actual
   source-off acceptance and repeated moves.
7. Demonstration/catalogue/lease ownership, versioned conformance, capability/pool
   evidence, JSON reports and benchmark envelopes throughout the work, not bolted
   on after the mechanisms.
8. Required Redis/PostgreSQL recovery/soak on both architectures/backends,
   documentation and the new 0.4.0 book chapter; final release qualification.

Intermediate mechanisms can land with their actual weaker guarantees, but cannot
be advertised as live-conformant or used to declare a seamless drain complete.
GPU warm starts, non-runc/rootless migration and automatic rebalancing remain later
projects. Source-independent network ownership is no longer deferred if needed
to satisfy this release's continuity contract.
