# PR #167 review fixes: plan

**Status:** in progress. PR #167 merged on 22 September 2026 (`435efc2`);
all decisions (A–G) are made.
**Reviewed at:** `9d8eb3d` on `codex/codebase-completion-fixes`.
**Written:** 22 September 2026, from a Claude review of the Codex-built branch.

This is the working plan for fixing what the PR #167 review found, now that the
PR itself has merged. It's meant to be picked up across several sessions: each
item has a checkbox, a location, the fix we intend and the test that proves it.
Tick an item only when its commit is on the branch and `make ci` passes.

Each tier is its own PR, stacked on the previous one:

| Tier | Branch | Base |
| --- | --- | --- |
| 1 | `fix/tier-1-merge-blockers` | `main` |
| 2 | `fix/tier-2-release-fixes` | `fix/tier-1-merge-blockers` |
| 3 | `fix/tier-3-docs` | `fix/tier-2-release-fixes` |
| 4 | `fix/tier-4-size` | `fix/tier-3-docs` |

Line numbers below were recorded at `9d8eb3d`; re-find each location before
editing, since earlier fixes move them.

## How to resume

1. Read this file top to bottom. Check out the lowest tier branch with unticked
   items (create it from its base in the table above if it doesn't exist), then
   `git log --oneline <base>..HEAD`.
2. Pick the first unticked item in that tier.
3. Write the failing test first, then the fix, then the book paragraph.
4. `cargo fmt`, `make ci`, plus the gated target that matches what you touched
   (`make test-linux`, `make test-cluster`, `make test-rootless-runc`). Gated
   Linux suites need the Lima VM or hosted CI; macOS can't run them.
5. One commit per item; within this approved plan, commit without re-asking.
   Never amend. Push the tier branch and keep its PR description current.
6. Tick the box here in the same commit, and end the commit subject with the
   item ID, e.g. `(T1.1)`, so `git log --grep 'T1.1'` finds it.

Local tooling note: the repo pins nextest 0.9.145. With an older local nextest,
`make test` fails immediately; update it or pass `--override-version-check`.

## What the review found

Hosted CI passes every job at the PR head. Locally on macOS, `cargo fmt`,
`clippy -D warnings` and the portable suite pass (4,084 of 4,084 tests). One
flake appeared under full-suite load and is item T2.9.

The PR's central safety claims hold. No path signals an unverified PID. Node
identity really comes from the verified TLS peer certificate, and one node can't
submit receipts or releases for another. The Raft state machine is deterministic
and fully snapshotted, and old snapshots fail loudly. Durable writes use
tmp + fsync + rename + directory fsync. Every new `unsafe` block has a
`// SAFETY:` comment.

The problem is the opposite one. Codex made nearly everything fail closed, and in
several places that turned into availability defects: nodes going dark, a
cluster-wide discovery freeze, nodes that refuse to start. CI stays green because
nothing measures request continuity under health probing, or what systemd does to
Bun's children on restart.

Scale, measured against `origin/main` (`933930b`): 343 commits, about 91k lines of
code and docs, plus 90k lines of vendored `parquet` (removed in `2ab28f3`, T2.19). PR #171 (the SREday deck) was
merged into this branch rather than `main`, so `docs/talks/` rides along.

Items marked ✔ were traced by hand during the review. The rest come from
subsystem review agents that traced the code; confirm each one before fixing it.

## Decisions needed before starting

- [x] **A. Dead-consumer policy (T1.2).** Decided 22 September 2026, as
      recommended. For 0.1.0: keep manual
      decommission, and add a loud metric, readiness signal and runbook. The
      alternative is automatic consumer expiry once gossip has confirmed the node
      dead for N minutes. That's friendlier, but a partitioned node that is still
      alive could serve stale routes while its VIPs get reused.
- [x] **B. Where fixes land.** Decided 22 September 2026: one stacked PR per
      tier (see the table above), one commit per item.
- [x] **C. Merge style.** Resolved: PR #167 was merged as it stood on
      22 September 2026, since its size made further review impractical.
- [x] **D. Book condensing target.** Decided 22 September 2026: about
      1–1.5k lines across the affected chapters, down from 8.2k.
- [x] **E. Vendored `parquet`.** Decided 22 September 2026: drop it and
      upgrade DataFusion 45 → 55 (T2.19). See the dedicated section below.
- [x] **F. One Runc lifecycle (T4.2).** Decided 22 September 2026: yes. Owned
      Runc becomes the only Linux Runc path; remove all legacy Runc code and
      docs, after the Tier 1 owner-recovery fixes (T1.3/T1.4) land.
- [x] **G. No legacy code before the first release.** Decided 22 September
      2026. Nothing has been released, so there is nothing to stay compatible
      with: no second "legacy" implementation, no deprecation handling for
      unreleased config keys, no migrations for development-only formats.
      This turns T2.18 into a won't-do, takes T4.1 off hold and adds T4.9.

## Tier 1: merge blockers

All of these were introduced or exposed by PR #167. The PR has merged, so
this tier now blocks the 0.1.0 candidate rather than the merge.

- [x] **T1.1 ✔ Health probes blank DNS and ingress for the whole node (cluster
      mode).** Done. Wider than reviewed: *every* catalogue generation change
      (any deploy anywhere) took the same withdraw-everything path, so each
      deploy made every node abort all proxied requests. The consumer journal
      now replaces views in place, retains earlier views until their removed
      backends drain gracefully (30 s), and marks receipts ready only when no
      retained view intersects them. Local changes mark the view stale and the
      agent loop republishes once. Recovery keeps the full withdrawal.
      `publish_instance_health` (`src/bun/agent.rs:7407`, called from `:7368`
      on *every* probe) has no "nothing changed" check. It goes through
      `publish_backend_snapshot` (`:2218`) into `invalidate_consumer_view`
      (`src/bun/agent/consumer.rs:365`). That sets the consumer to
      `Withdrawing`, fsyncs twice, clears the ingress routing table and sends an
      empty `ServiceMap` to DNS. The next 2 s consumer poll sees
      `phase != Active` and `withdraw_consumer_view` removes every eBPF entry
      and starts a zero-timeout drain on every backend, which cancels in-flight
      requests. If the leader is unreachable the node stays dark. Restart
      republish (`:8010`), replacement publish (`:9544`), backend withdrawal
      (`:9566`) and `retire_discovery_service` do the same whole-node withdrawal.
      *Fix:* return early when the backend's health is unchanged. For real
      changes, trigger an immediate consumer resync rather than waiting for the
      poll, so the dark window is one round trip and not 2 s. Check whether the
      whole-node withdraw on a single-service change is needed at all, or whether
      the merged republish can replace the view in place.
      *Test:* a cluster test that keeps requests flowing through ingress and DNS
      while health probes run for 30 s, asserting zero failures. Also a unit test
      that a repeated identical probe result causes no journal write.

- [x] **T1.2 Discovery freezes cluster-wide after about 1,024 withdrawals that a
      dead or plaintext consumer can never acknowledge.** Done. Only
      TLS-authenticated placement polls register consumers (receipts need that
      identity), so plaintext clusters owe nothing. The leader degrades the
      non-critical `discovery:withdrawal-backlog` readiness subsystem at 75%,
      logs which nodes owe receipts and whether gossip sees them, and stops
      proposing publications the ledger would refuse. Runbook: manual chapter
      02. The metric part of decision A moved to T2.20.
      Every node that polls `/v1/placements` becomes a permanent endpoint
      consumer (`src/bun/api.rs` `placements_handler`, around the
      `register` call). Each publication that removes a backend adds a pending
      generation owed by every consumer. When the bound is hit,
      `plan_publication` → `check_capacity`
      (`src/onion/withdrawal.rs:94-154`) refuses every `PublishEndpoints` and
      `RetireEndpointExecution`. Producers then never get address-reuse
      permission. The leader re-proposes every reconcile interval, so the Raft
      log churns with committed refusals. ✔ In plaintext mode receipts are
      refused outright (`src/cluster/consumer.rs:9`), so dev clusters hit this
      after enough deploys. Two reviewers found this independently.
      *Fix:* (1) don't register consumers when the cluster transport is
      plaintext, and don't create withdrawal obligations there; (2) export
      ledger occupancy as a metric, and degrade readiness at 75% of the bound;
      (3) implement the dead-consumer policy chosen in decision A; (4) document
      the decommission runbook in the user manual.
      *Test:* a state-machine test that a dead consumer plus N withdrawals
      reaches the warning signal before the refusal; an integration test that a
      plaintext cluster survives 2,000 rolling-deploy withdrawals.

- [x] **T1.3 ✔ Owners die on every Bun restart, which wedges their instances.**
      Done, and worse than reviewed: after a cgroup kill Bun could not start
      again at all. The unit now uses `KillMode=process`. A dead owner is
      detected by its free `owner.lock`; once a null-signal probe shows the
      workload's process group is empty, the generation retires with an
      unknown exit code. A live orphaned workload still refuses stop/state. A
      Linux regression kills Bun, owners and the Runc launcher through
      `cgroup.kill`, then requires restart, cleanup and redeploy; it fails
      without the fix (Bun exits at startup) and passes with it.
      The shipped guest unit sets `KillMode=mixed`
      (`src/relish/quickstart/provision.rs:83`), and owner processes stay in
      Bun's cgroup. On stop, or on a crash with `Restart=on-failure`, systemd
      SIGKILLs every detached owner and `runc run` launcher. There's no recovery
      for a dead owner whose record is still `Running`
      (`src/grill/process_control.rs:295` `status`, `:315` `signal`, `:367`
      `finish_retirement`). The records stay `Running`, `seal()` times out and
      `require_terminal_commands` blocks everything after, so the instance is
      stuck in `StateUnavailable` and rootful containers run unmanaged. An owner
      loop that returns `Err` (e.g. `persist` on ENOSPC) produces the same state.
      The PR's "actual Bun SIGKILL" tests kill Bun alone, so they miss this.
      *Fix:* detect a dead same-boot `Running` owner through its released
      `owner.lock` flock, and route it into the existing unknown-outcome
      retirement path. Change the unit to `KillMode=process`, which matches
      the intent of durable ownership (workloads outlive Bun).
      *Test:* kill Bun *and* its owners together (simulating the cgroup kill),
      restart Bun and assert the instance is retired or re-adopted rather than
      stuck. Snapshot test for the unit file.

- [x] **T1.4 ✔ Changing runtime config after any container has run stops Bun
      from starting.** Done. Only live intents must match the current
      configuration; a live mismatch still refuses, now naming the instance
      and what to stop. Publishing a replacement removes the retired
      generation's `generations/<id>/` directory. Retired intents themselves
      stay (they carry job exit codes); one small record per instance name.
      `RuntimeIntentStore::load` (`src/grill/runc_intent.rs:325`) rejects any
      record whose `configuration` differs from the current one (runc path, DNS
      nameserver, image dir, node index). Retired records are never deleted, and
      startup propagates the inventory error with `?`
      (`src/bun/agent.rs:3071`).
      *Fix:* skip the configuration check for `Retired` records; prune retired
      intent records, their `locks/<id>` files and old
      `records/<id>/generations/<gen>/` directories once retirement is confirmed
      (this also fixes an unbounded disk leak,
      `src/grill/runc_intent/commands.rs:63-69`). For live records with a
      config mismatch, refuse only that instance, not the whole inventory.
      *Test:* run a container, retire it, change `dns_nameserver`, restart Bun,
      and assert startup succeeds. Assert retired record directories are gone.

- [x] **T1.5 One journal-write failure fences discovery until Bun restarts.**
      Done. Validation runs before the journal moves into its worker, so
      refusals never fence. After a write failure the agent reopens the
      journal from disk on the next update and on every tick, adopting the
      durable checkpoint, and the critical `discovery:journal` readiness
      subsystem reports the fence meanwhile.
      `update_discovery_inventory` (`src/bun/discovery_ownership.rs:397-425`)
      swaps state to `Uncertain` before persisting, and `persist`
      (`src/bun/discovery_owners.rs:243-251`) takes the journal by value, so an
      error drops it. Nothing leaves `Uncertain` (`recover_discovery` requires
      `Disabled`). Validation refusals that never touch disk trigger it too. The
      state isn't reported, so the node looks healthy while doing nothing.
      *Fix:* validate before swapping state; on a persist error, reload the
      journal from disk and restore `Ready`/`Recovered`; report `Uncertain` in
      readiness.
      *Test:* inject a failing persist (read-only directory), assert the next
      publication succeeds once the directory is writable, and assert readiness
      shows the degraded state meanwhile.

- [x] **T1.6 Retirement network calls stall the agent's main loop.** Done.
      The leader request runs as a task keyed by execution; a fresh request
      waits at most 1 s, retries collect the finished answer without asking
      again, and dropping an entry aborts it. A 3 s leader delay used to hold
      a retirement for 3 s; it now returns within 1 s and completes later.
      The 1 s tick (`src/bun/agent.rs:3375-3387`) runs
      `drive_startup_retirements` → `retire_instance_artifacts` (`:8555`) →
      `confirm_producer_release` inline: up to 5 s + 1 s inventory plus a 10 s
      confirm timeout, per instance, serially. `drive_pending_restarts`
      (`:7753`) does the same. With the leader unreachable, the API, health
      checks and reports stall exactly when the cluster is degraded.
      *Fix:* move producer-release confirmation into a spawned task that
      reports back over an `mpsc` channel; the loop only records the outcome.
      *Test:* with a leader that never answers, assert that `/v1/health` and
      health probing keep their cadence while a retirement is pending.

- [x] **T1.7 ✔ Ingress aborts healthy long-lived streams during rolling
      deploys.**
      Done. When a backend answers, `DrainGuard::keep_only` releases the
      other captured candidates (through the guard's own `Drop`) and the
      proxy keeps only that backend's cancellation token;
      `capture_requests` now returns tokens aligned with the candidates. A
      regression streams from one of two backends and drains the other with
      a short deadline: that drain completes at once, and after its deadline
      the stream still arrives whole. Before the fix, the drain waited and
      the stream ended in an unexpected EOF.
      Non-WebSocket requests capture drain guards and cancellation tokens for
      all `MAX_UPSTREAM_ATTEMPTS` (3) candidates (`src/wrapper/proxy.rs:441`)
      and keep them for the whole response. `wait_for_termination`
      (`src/wrapper/draining.rs:342`) fires on *any* token. An SSE stream or
      download served by healthy A is aborted when unused failover candidate B
      reaches its drain deadline, and B's drain can't finish early either.
      *Fix:* once the upstream response is chosen, release the guards and
      tokens for the other candidates.
      *Test:* start a streaming response on A with B in the candidate set,
      drain B with a short deadline, and assert A's stream completes and B's
      drain finishes immediately.

- [x] **T1.8 Certificates have zero clock-skew tolerance.**
      Done. `set_validity` backdates `not_before` by `CLOCK_SKEW_BACKDATE`
      (5 minutes, now shared with workload identity) while the lifetime still
      counts from the issuing instant; validation stays exact. The issuer
      bound now checks the issuer at the real issuing instant (not the
      backdated start) and clamps the leaf's start to the issuer's. A regression
      checks root, intermediate, self-issued and CSR-signed node certificates
      and an end-entity leaf at `now - 60 s`; it failed before the fix.
      Follow-up: the ingress SNI cache measured its renewal midpoint from the
      backdated `not_before`, so short-lived leaves renewed on every handshake
      (caught by `cached_ingress_leaves_renew_before_expiry_and_after_an_idle_expiry`);
      it now measures from the issuing instant.
      `set_validity` in `src/sesame/ca.rs` now sets `not_before = now` (the old
      code effectively backdated to midnight), and `check_validity_at`
      (`src/sesame/cert.rs:123`) has no leeway. A joiner or renewing node whose
      clock is 1 s behind fails `identity_store::save` with `NotYetValid`;
      renewal loops every 5 s and burns a Raft serial each time.
      *Fix:* backdate `not_before` by 5 minutes.
      *Test:* validate a freshly issued certificate at `now - 60 s`.

- [x] **T1.9 `/v1/status?cluster=true` ignores token scope.**
      Done. `status_handler` now takes the `AuthContext` and filters both the
      local list and the merged cluster list through `authorize_scoped`. A
      regression serves a local agent and one peer, each with a `team-a` and
      a `team-b` instance; a read-only `team-a` token sees only the two
      `team-a` instances. It failed before the fix (saw `team-b`).
      `status_handler` / `cluster_statuses` (`src/bun/api.rs`) takes no
      `AuthContext` and fans out to every member with the node's service token,
      so a namespace-scoped read-only token sees every instance on every node.
      *Fix:* filter by scope the way `desired_apps_handler` does.
      *Test:* a namespace-scoped token only sees its own namespace's instances.

- [x] **T1.10 Parquet 59 aborts on an impossible list count (regression from
      T2.19, merged with PR #167).** Done. The T2.19 spike ran on macOS, which
      grants a 206 GB untouched reservation; Linux CI aborted the process in
      `impossible_schema_count_is_rejected_before_allocation`. Upstream fixed
      it in Parquet 60 (apache/arrow-rs#10979) but DataFusion 55 needs 59 and
      the fix wasn't backported. `[patch.crates-io]` pins all fifteen arrow-rs
      crates to `reliaburger/arrow-rs@34ac186` (59.3.0 + that one commit).
      Remove it when DataFusion depends on a Parquet release with #10979; the
      safety tests must pass on Linux.
- [x] **T1.11 The cgroup-kill regression ran in the host-network `test-linux`
      stage.** Done. Its name matched the `cgroup_` filter, so it ran beside
      the serialised owned-Runc tests and they collided on container
      addresses. `test-linux` now excludes `oci_crash`, whose driver runs it
      serially in private namespaces.

## Tier 2: fix before release

Smaller, mostly one commit each.

CI and release:

- [x] **T2.1 ✔ `promote.yml` runs the tag's own scripts with a write token.**
      `.github/workflows/promote.yml:42-44` checks out `refs/tags/<tag>` and
      runs that tree's `scripts/release/candidate.py` with `contents: write`.
      Anyone who can push a tag and dispatch the workflow bypasses branch
      protection. *Fix:* check out `main`, and read the tag's `Cargo.toml` with
      `git show`.
      *Done.* Promotion checks out `main`, validates the tag's shape, fetches it and reads
      version/commit via `git show`/`git rev-parse`. `scripts/release/test_promote_workflow.py`
      (run by `test-packaging`) fails on a tag checkout or `${{ inputs.* }}` inside `run:`.
- [x] **T2.2 ✔ Reboot "qualification" tests pass vacuously in CI.**
      `tests/owned_runc.rs:680` and `tests/oci_crash.rs:1216` `return` when
      their env var is unset, and `qualify-oci-interruptions.sh` and
      `make test-linux` run them. *Fix:* `panic!` when the var is missing, and
      `--skip` them in the automated drivers. The real runs stay manual
      (Lima), which the docs should say plainly.
      *Done.* Both fixtures now `.expect()` their directory variable; the interruption driver
      `--skip`s them and `make test-linux` excludes them by filter. Linux run of the
      panic path still pending (macOS only compiles these files).
- [x] **T2.3 Quorum assertions were loosened.** `tests/placement.rs:~851` and
      `:1093-1103` now also accept capacity or leader-unknown refusals. Restore
      the quorum-specific assertion (set up capacity so the quorum check is the
      one that fires).
      *Done.* Both tests wait until the first fault's voter has left the API membership
      view, then `assert_quorum_refusal` requires 400 + "quorum risk". The first test now
      isolates a follower fully (the old setup only ever hit the 409 reservation refusal).

Execution (grill):

- [x] **T2.4 The subreaper owner doesn't reap orphans while the workload runs.**
      `waitpid(-1)` only runs in `retire_children`
      (`src/grill/process_owner.rs:~584`), so double-forking workloads pile up
      zombies. Reap in the main loop (`:327-355`).
      Done. Each tick peeks with `waitid(WNOWAIT)` and reaps exited orphans
      by exact PID, leaving the root and exec helpers to their own waiters.
- [x] **T2.5 macOS has no boot identity.** `current_boot_id()`
      (`src/grill/process_owner.rs:132-149`) returns `None` off Linux, so a
      reboot leaves `Running` records failing with NotFound forever. Use the
      `kern.bootsessionuuid` sysctl. Also: macOS `retire_children` checks only
      the process group, so `setsid` descendants survive.
      Done. macOS reads `kern.bootsessionuuid`, both systems store lowercase
      UUIDs, and schema-3 records require one everywhere. The `setsid` gap is
      documented in chapter 8, not fixed.
- [ ] **T2.6 Adoption compares start times exactly.**
      `src/grill/runc/owned.rs:786`. Use the ±2 s slack the rest of the code
      uses (NTP steps move `/proc/stat` btime).
- [x] **T2.7 A failed stop is silently ignored on restart egress refusal.**
      Done. A failed stop of the created replacement now takes the existing
      `record_failed_restart` path (Stopping, retry pending) instead of
      marking the instance Failed with its container abandoned.
      `src/bun/agent.rs:~7942` `let _ = …stop(&id).await`, then `Failed` with
      no retry, leaking the container and its network reference. Keep the
      instance owned and retry the stop.
- [ ] **T2.8 An `io::Error::other` allocation runs inside `pre_exec`.**
      `src/grill/volume/owned.rs:437`. Use `io::Error::last_os_error()`, and
      make the `// SAFETY:` comment true.
- [x] **T2.9 Flaky `job_recovery` test (seen locally).**
      `completed_job_survives_bun_death_with_or_without_adoption_record` failed
      once under full-suite load with
      `StateUnavailable { reason: "Broken pipe (os error 32)" }` at
      `tests/job_recovery.rs:254`, then passed 10 times in isolation.
      `ProcessGrill::state` treats an EPIPE from an exiting owner as a hard
      error. Treat it as transient and re-read the durable record.
      Also watch `bun::api::tests::app_metrics_name_injection_cannot_bypass_predicate`:
      it hit `LEAK-FAIL` once under parallel load (the PR made
      `leak-timeout` fatal), then passed 6 times in isolation.
      Done. The single-threaded owner drops a client slower than its 100 ms
      read timeout, and `pid()` turned that into "no PID" (so jobs also
      skipped their adoption record). `status` and `signal` now retry a live
      owner (the lock proves it) up to ten times on a dropped connection.
      Under a CPU hog, 30 runs each: `completed_job_survives...` 7 to 0
      failures, `killed_exec_caller...` 2 to 0. The api leak test wasn't
      investigated.

Discovery and API:

- [ ] **T2.10 The consumer publication list can hit its cap and wedge.**
      `src/bun/agent/consumer.rs:236-239` pushes and persists before the
      withdrawal that shrinks the list; at `MAX_PUBLICATIONS` (1,024)
      `save_consumer` fails validation before reaching it. Collapse repeated
      unfinished publications, or withdraw before pushing. Related to T1.2.
- [ ] **T2.11 Startup retirement may retry forever after partial success.**
      `src/bun/startup_recovery.rs:75-85` requires the original launch to
      still exist unchanged, but `retire_instance_artifacts` isn't atomic.
      *Investigate first* (reviewer marked it plausible), then make each step
      idempotent against an already-completed earlier step.
- [ ] **T2.12 Execution evidence can be dropped silently.**
      `src/bun/discovery_ownership.rs:67-81`: `launch_inventory` returning
      `Ok(None)` records empty `executions`; `*previous = owner` overwrites the
      historical backend list the drain relies on. *Investigate first.*
- [x] **T2.13 Every mTLS request clones the whole cluster state.** Done. A
      borrowing `read_desired` accessor backs `CouncilNode::is_node_retired`,
      and an unreadable peer identity is now refused instead of passed.
      `refuse_retired_tls_peer` calls `council.security_state()`, which clones
      all of `DesiredState`. Add a borrowing accessor for the security part.
      Also make it fail closed when SAN parsing fails.
- [ ] **T2.14 Blocking filesystem calls on async tasks.**
      `sweep_orphaned_identity_dirs` (`src/bun/agent.rs:8590`), `:5360`,
      `prepare_rollback` in `src/upgrade/manager.rs`. Wrap in
      `spawn_blocking`. Add a timeout to `recover_discovery`'s
      `launch_inventory` call.

Registry and storage:

- [x] **T2.15 Lease expiry trusts the proposing node's clock.**
      `src/pickle/lease.rs:155-164`, `src/pickle/api.rs:367`,
      `src/pickle/copy.rs:74`. A slow clock lets a node write after expiry. Have
      the leader stamp the time, or refuse `observed_at` beyond a skew bound.
      *Done.* The leader stamps: `ClaimWriter`/`LeasedManifest` lost their timestamp and
      `RegistryMutation::request(leader_now)` sets it (and overwrites copies'). Test
      `slow_clock_lease_observations_are_refused_after_leader_expiry` proves the refusal.
- [x] **T2.16 `revalidate_blob` reads multi-GB blobs into memory.**
      `src/pickle/store.rs:214-226`, called on every heal tick and p2p resolve.
      Stream the hash as `copy.rs:118` already does. Treat read errors as
      errors, not "not cached".
      *Done.* `revalidate_blob` streams via a shared `sha256_file` (copy.rs uses it too) and
      returns `Result<bool>`; only NotFound is a miss. Tests: a late-byte mismatch in a
      multi-chunk blob is caught and removed; an unreadable blob path errors and survives.
- [x] **T2.17 An explicit council refusal doesn't roll back the local
      catalogue.** `src/pickle/api.rs:316-378`. A push that loses the GC race
      leaves its digest tagged locally, so its layers never get collected. Roll
      back on `Refused`/`Stale`; keep local state only on timeout.
      *Done.* `record_commit_owned` keeps the pre-commit catalogue and restores + persists it
      on `Refused`, `RegistryPublicationStale` or a forwarded 409; other errors keep it.
      `refused_publication_rolls_back_the_local_tag_but_uncertain_keeps_it` covers all three.

- [ ] **T2.20 Export withdrawal-ledger occupancy as a Mayo metric.** T1.2
      shipped the readiness signal and log. Mayo only records host metrics
      today (`src/mayo/collector.rs`), so a gauge needs a small path for
      Bun-internal metrics first.

Config:

- [x] **T2.18 Removed config keys now fail parsing.** Won't do (decision G):
      those keys were never released, so refusing them is correct.

## Tier 3: docs cleanup before merge

- [ ] **T3.1 Condense the book.** 8.2k added lines across 14 chapters, about 300
      appended fix headings, one code block. Rewrite each chapter's additions as
      one or two narrative sections with real code listings. Explain or drop
      "obligation", "authority", "qualification", "fence". Remove the ~20
      development-only protocol/state version numbers (keep only the current
      compatibility policy in chapter 14). Remove the 7 links into
      `docs/plans/`. Follow the style guide in `CLAUDE.md`.
- [ ] **T3.2 Archive plans.** Move the session handoff (1,710 lines), the
      completion plan (2,338 lines), the C34 closure and the per-feature 09-19 /
      09-20 / 09-22 plans to `docs/plans/archive/`. Keep a one-page V01–V04
      remaining-work doc and this file.
- [ ] **T3.3 Clean `progress.md`.** Strip CI SHAs and per-run evidence from
      ticked items. Fix the self-contradicting ticked item at `:187`, and the
      reference at `:2262` to `legacy_wire_bytes_decode_unchanged`, which this
      PR deleted (812b041). 15 cited test names no longer exist; fix or drop
      them.
- [ ] **T3.4 User-facing READMEs.** Replace the ledger prose in `README.md`
      (e.g. `:43-54`) and `docs/README.md` with a short "0.1.0 scope and limits"
      section: rootless Runc is standalone only, container clusters are rootful
      Linux Runc/eBPF, declarative image workloads need root mode. Fix the
      "opt-in owned Runc" contradiction at `README.md:52` versus `:319`.
- [ ] **T3.5 Decide on `docs/talks/`.** It arrived via PR #171 merged into this
      branch. Keep it, or move it to its own PR against `main`.

## Tier 4: reduce code size

`make loc` counts 133.4k production lines in `src/` against 108.5k on `main`
(+24.9k, +23%). The count is accurate: only `src/bun/job_lifecycle_tests.rs`
(349 lines) is misclassified test code. Most of the growth is owned/durable
machinery that production uses, so the realistic production saving is about
1.7–2.0k lines. Tests can lose another 1.2–1.8k through shared fixtures. The
bigger size win is documentation (T3.1/T3.2, about 13k Markdown lines).

Estimates come from a survey agent (22 September 2026); T4.1's reachability was
spot-checked by hand.

- [ ] **T4.1 Delete the legacy rootless Runc path (~400 prod, ~280 test
      lines; low risk).** Since `b41cdd9`, rootless startup always takes the
      owned path (`src/bin/bun.rs:925-947`), so `Slirp4netnsHandle`,
      `setup_slirp4netns`, `stop_recorded_owner`, `PendingSlirp` and
      `add_slirp4netns_port_forward` (`src/grill/rootless.rs:174-450`),
      `start/restore_rootless_network` and `slirp_handles`
      (`src/grill/runc.rs:74, 364-490`), the rootless arm of legacy `adopt`
      (`:~1290-1320`) and its tests (`:1879-2160`) are dead.
      *Unblocked by decision G:* this is legacy code, so it goes. Rootless
      clusters (deferred past 0.1.0) will build on the owned path; git history
      keeps the slirp4netns code if it's ever wanted.
- [ ] **T4.2 Make every Linux Runc instance owned; drop
      `--experimental-owned-runc` (~600–700 prod, ~375 test lines; medium
      risk).** Removes the legacy branch of each
      `if self.ownership.is_some()` in `src/grill/runc.rs:795-1466` plus
      legacy-only helpers (`:194-363`). Nothing in the owned path needs eBPF,
      but `durable_discovery` currently also gates cluster identity rules and
      the mode-change refusal (`src/bin/bun.rs:940, 1081, 1322`). Split "owned
      runtime" from "durable discovery" first, and qualify rootful Runc without
      eBPF on the owned path. Also removes a less crash-safe mode. Needs a
      scope decision (F below).
- [ ] **T4.3 `ask_agent` helper in `src/bun/api.rs` (~250–350 lines; low
      risk).** The oneshot + `cmd_tx.send` + "agent unavailable" + await
      pattern repeats 36 times (e.g. `:853-858`, `:920-927`, `:988-1000`).
- [ ] **T4.4 Shared private-record reader (~150–250 lines; low risk).** One
      `read_private_json(path, limit)` for the O_NOFOLLOW → regular/private
      file → size → bounded read → parse pattern in `process_owner.rs`,
      `runc_intent.rs`, `discovery_owners.rs`, `egress_owners.rs`,
      `volume/owned.rs`, `network_leases.rs`, `command.rs`, `jobs.rs`,
      `schedules.rs`; dedupe `validate_file`/`validate_directory`. Keep the
      domain-specific `validate()`/`validate_transition()` as they are.
- [ ] **T4.5 Remove dead functions (~130–200 lines; low risk).** No callers
      at all: `query_apps` (`ketchup/log_store.rs:605`),
      `spawn_council_reconciler` (`cluster/runtime.rs:973`),
      `derive_with_evidence` (`bun/capabilities.rs:361`), `query_sql_json`,
      `check_route_role`, `generate_new_join_token`,
      `read_cgroup_memory_current`, `bind_with_node_gate`, `rootless_default`,
      `required_role`. Orphaned by this branch (tests only):
      `pull_manifest_layers` (`pickle/pull.rs:311`), `apply_update_locations`,
      `netns::add_port_mapping`. Re-grep before deleting.
- [ ] **T4.6 Small helpers (~50–70 lines).** One `launch_inventory` with a
      single timeout (7 copies, 1 s/5 s/none; this also fixes the missing
      timeout in T2.14) and one drain-then-check helper (3 copies).
- [ ] **T4.7 Shared test fixtures (~1.2–1.8k test lines; low risk).** The
      eBPF enable-and-load preamble repeats 15 times in `tests/ebpf.rs`;
      `NodeFaultAuth` setup 7 times; `spawn_gitops_sync` config 5 times; the
      `router(cmd_tx, None ×11, …)` + agent spawn 12–20 times in the
      `api.rs` test module. Table-driving saves little: only 5
      near-duplicate test pairs exist.
- [ ] **T4.9 Sweep remaining legacy code (decision G).** Search for
      "legacy", "compat", "deprecated", "migrate", `#[serde(default)]` on
      durable formats and version fallbacks. Delete paths that exist only for
      unreleased formats or superseded implementations, and their docs.
- [ ] **T4.8 Make `make loc` honest.** Count tracked files (`git ls-files`)
      so `node_modules` Markdown under `docs/talks/` stops inflating `.md`, and
      treat `#[cfg(test)] mod x;` files as tests.

Not worth it: marking `ProcessGrill`'s unowned branches test-only (~450
lines, but 40 test sites depend on them, so no lines are saved), and merging
the per-module `failure()`/`refuse()` closures (costs readability).

## Vendored `parquet`

Codex vendored `parquet` 54.3.1 (90k lines under `vendor/parquet/`, wired in
through `[patch.crates-io]`) in commit `f1414bd` to close audit item H12: the
Thrift CVE GHSA-2f9f-gq7v-9h6m / CVE-2026-43868, an excessive-allocation bug in
the `thrift` crate below 0.23. H12 in the approved completion plan asked us to
"upgrade/remove the affected dependency or document a reviewed, evidence-backed
disposition". It didn't ask for vendoring.

The reviewer confirmed the vendored tree matches upstream 54.3.1 plus the
documented patch byte for byte, and that the patch itself is correct. So it's
honest, but it's a fork we'd have to maintain.

Facts gathered:

- `parquet` 55 to 58 all still depend on `thrift ^0.17`. **`parquet` 59.3 drops
  the `thrift` crate entirely.**
- **DataFusion 55 depends on `parquet ^59.2`.** We're on DataFusion 45, so the
  upgrade is ten major versions.
- The patch notes' claim that "unmodified Parquet 59.3 fails the same
  regression" refers to the overlong-varint tests: 59.3 *accepts* a malformed
  but harmless encoding of the same integer. That's lax parsing, not a memory or
  crash bug. The real CVE-class risks are the huge-count allocation and the
  truncated-double panic.
- Trust boundary: Reliaburger only reads Parquet it wrote itself (local metrics
  and log archives) or files in operator-configured object storage
  (`src/ketchup/remote_query.rs`). An attacker needs write access to the node's
  data directory or the bucket.

**Spike (22 September 2026).** In a throwaway worktree, with
`datafusion = "55"` and no `[patch.crates-io]` or `vendor/` directory:

- The codebase compiled with **zero errors** and two deprecation warnings,
  both in `src/ketchup/log_store.rs`: `set_max_row_group_size` →
  `set_max_row_group_row_count`, and `set_column_bloom_filter_ndv` →
  `set_column_bloom_filter_max_ndv`.
- `thrift` disappeared from the dependency graph, and `cargo audit` passed.
- The full portable suite: 4,082 of 4,084 passed. The only failures were the two
  lax-parsing tests, `parquet_metadata_rejects_an_overlong_integer` and
  `parquet_metadata_rejects_a_32_bit_integer_overflow`.
- Upstream 59 passes the tests that matter: the impossible-count allocation
  bound, the truncated double, and both unknown-field tests.
- DataFusion 55's MSRV is 1.94, below our 1.97.

**Recommendation: drop the vendoring and upgrade DataFusion 45 → 55.**

- [x] **T2.19 Replace vendored `parquet` with DataFusion 55.** Done in
      `2ab28f3`. `paste` also left the graph, so its
      RUSTSEC-2024-0436 exception was removed; `make audit` passes.
      - Set `datafusion = "55"`, delete the `[patch.crates-io]` block and
        `vendor/`, then `cargo update -p datafusion`.
      - Rename the two deprecated writer-property methods.
      - Keep `tests/parquet_safety.rs` for the four safety tests. Change the two
        varint tests to assert what actually matters: no panic, and no misread
        value (or drop them). Optionally report the lax varint parsing upstream
        to arrow-rs.
      - Run `make ci`, the metrics/log/rollup restart and query suites, the
        benchmarks (DataFusion 10 versions newer may change memory/latency),
        and `make audit`.
      - Re-check the `paste` exception in `.cargo/audit.toml`: its comment cites
        DataFusion/Parquet.
      - Update the references: book chapters 6 and 15, `docs/progress.md` (H12),
        `docs/qualification/2026-09-17-code-audit.md` and the completion plan.
        Explain the upgrade in chapter 6 instead of the patch.
      - Its own commit, independent of the other fixes.

If the upgrade turns out to regress something the spike didn't cover, the
fallback is to keep upstream `parquet` 54 + `thrift` 0.17 and record a
reviewed disposition for Dependabot alert #13 (only self-written or
operator-controlled files reach the reader), which H12 explicitly allowed. A git
fork outside the repo would also avoid the 90k lines, but it's still a fork to
maintain, so it's the least attractive option.

## Deferred (tracked, not blocking 0.1.0)

- `src/bun/agent.rs` is 20k lines (about 11.8k production); `BunAgent` has about
  62 fields; `handle_command` is 657 lines. Split in a follow-up PR, not this
  one. The `launch_inventory()`-with-timeout pattern repeats 8 times with
  inconsistent timeouts (1 s, 5 s, none); the drain-then-check loop is copied
  three times.
- Renewal never revokes the previous certificate
  (`src/sesame/renewal.rs:41-106`), so a stolen key can keep renewing until
  decommission.
- The receipt path re-checks retirement at apply time but not serial
  revocation.
- Identity replacement writes the PEM exports one at a time before the snapshot
  (`src/sesame/identity_store.rs:199-205`).
- Exact-match compatibility means self-upgrade can't apply a protocol/state bump,
  and rollback to a pre-PR binary is refused (`--compatibility` flag).
- `endpoint_withdrawals`/`endpoint_consumers` use `#[serde(default)]` while
  sibling fields are strict (`src/council/types.rs:451-464`). Not reachable
  today; make them strict.
- The registry publication fence compares one scalar GC generation against every
  holder (`src/council/state_machine.rs:193-203`); latent until multi-holder
  commits exist.
- eBPF `detach` skips the program/map identity checks the load path makes
  (`src/onion/ebpf/ownership.rs:271-300`).
- Gossip records rejoin contacts before checking the sender (bounded at 16).
- Every runc CLI call is a full durable owner launch (about 8 fsyncs, three
  execs); each owner wakes every 10 ms. Measure before optimising.
- The reporting accept loop spins on persistent `accept()` errors
  (`src/reporting/transport.rs:353`).
- An unreachable chaos target wedges node-fault capacity with no operator
  override (`src/smoker/reservation.rs:63-73`).
- Chunked pushes to leased repositories cost one Raft write per chunk.
- A GC-approved orphan re-uploaded by a client surfaces as an intermittent
  `MissingLayer` push failure.
- Two SIGKILL crash tests in `tests/documentation_first_run.rs` bind port 0 and
  drop the listener before use (race), and run in every portable job.
- Third-party actions are pinned by tag, not SHA.
- The OCI interruption driver runs raw libtest binaries outside nextest.
- One interruption test uses `--experimental-owned-runc` rather than the
  production startup path (`tests/oci_crash.rs:41-43`).

## Suggested order

1. Decisions A–E.
2. T1.1, T1.3, T1.4 first: they're the ones most likely to break V01 (three-node
   acceptance) immediately.
3. The rest of Tier 1, then Tier 2 in listed order.
4. Tier 3 docs, then the merge decision (C).
5. V01–V04 from `2026-09-22-v0.1.0-remaining-work.md` on the merged result.
