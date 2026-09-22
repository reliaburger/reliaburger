# PR #167 review fixes: plan

**Status:** in progress. Decision E and T2.19 are done; decisions A–D are open.
**Branch:** `codex/codebase-completion-fixes` (PR #167), reviewed at `9d8eb3d`.
**Written:** 22 September 2026, from a Claude review of the Codex-built branch.

This is the working plan for getting PR #167 into a mergeable state. It's meant
to be picked up across several sessions: each item has a checkbox, a location, the
fix we intend and the test that proves it. Tick an item only when its commit is on
the branch and `make ci` passes.

## How to resume

1. Read this file top to bottom, then `git log --oneline origin/main..HEAD | head`.
2. Pick the first unticked item in the lowest open tier.
3. Write the failing test first, then the fix, then the book paragraph.
4. `cargo fmt`, `make ci`, plus the gated target that matches what you touched
   (`make test-linux`, `make test-cluster`, `make test-rootless-runc`). Gated
   Linux suites need the Lima VM or hosted CI; macOS can't run them.
5. One commit per item. Show the commit to Miko before committing. Never amend.
6. Tick the box here, with the commit hash, in the same commit.

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

- [ ] **A. Dead-consumer policy (T1.2).** Recommended for 0.1.0: keep manual
      decommission, and add a loud metric, readiness signal and runbook. The
      alternative is automatic consumer expiry once gossip has confirmed the node
      dead for N minutes. That's friendlier, but a partitioned node that is still
      alive could serve stale routes while its VIPs get reused.
- [ ] **B. Where fixes land.** Recommended: Tiers 1 and 2 as separate commits on
      this branch, then Tier 3 docs, then merge.
- [ ] **C. Merge style.** 343 commits, about 37 of them docs-only "Record
      evidence" bookkeeping. Squash-merge, or keep the history and drop the
      bookkeeping commits?
- [ ] **D. Book condensing target.** Recommended: about 1–1.5k lines across the
      affected chapters, down from 8.2k.
- [x] **E. Vendored `parquet`.** Decided 22 September 2026: drop it and
      upgrade DataFusion 45 → 55 (T2.19). See the dedicated section below.

## Tier 1: fix before merge

All of these are introduced or exposed by this PR.

- [ ] **T1.1 ✔ Health probes blank DNS and ingress for the whole node (cluster
      mode).**
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

- [ ] **T1.2 Discovery freezes cluster-wide after about 1,024 withdrawals that a
      dead or plaintext consumer can never acknowledge.**
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

- [ ] **T1.3 ✔ Owners die on every Bun restart, which wedges their instances.**
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

- [ ] **T1.4 ✔ Changing runtime config after any container has run stops Bun
      from starting.**
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

- [ ] **T1.5 One journal-write failure fences discovery until Bun restarts.**
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

- [ ] **T1.6 Retirement network calls stall the agent's main loop.**
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

- [ ] **T1.7 ✔ Ingress aborts healthy long-lived streams during rolling
      deploys.**
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

- [ ] **T1.8 Certificates have zero clock-skew tolerance.**
      `set_validity` in `src/sesame/ca.rs` now sets `not_before = now` (the old
      code effectively backdated to midnight), and `check_validity_at`
      (`src/sesame/cert.rs:123`) has no leeway. A joiner or renewing node whose
      clock is 1 s behind fails `identity_store::save` with `NotYetValid`;
      renewal loops every 5 s and burns a Raft serial each time.
      *Fix:* backdate `not_before` by 5 minutes.
      *Test:* validate a freshly issued certificate at `now - 60 s`.

- [ ] **T1.9 `/v1/status?cluster=true` ignores token scope.**
      `status_handler` / `cluster_statuses` (`src/bun/api.rs`) takes no
      `AuthContext` and fans out to every member with the node's service token,
      so a namespace-scoped read-only token sees every instance on every node.
      *Fix:* filter by scope the way `desired_apps_handler` does.
      *Test:* a namespace-scoped token only sees its own namespace's instances.

## Tier 2: fix before release

Smaller, mostly one commit each.

CI and release:

- [ ] **T2.1 ✔ `promote.yml` runs the tag's own scripts with a write token.**
      `.github/workflows/promote.yml:42-44` checks out `refs/tags/<tag>` and
      runs that tree's `scripts/release/candidate.py` with `contents: write`.
      Anyone who can push a tag and dispatch the workflow bypasses branch
      protection. *Fix:* check out `main`, and read the tag's `Cargo.toml` with
      `git show`.
- [ ] **T2.2 ✔ Reboot "qualification" tests pass vacuously in CI.**
      `tests/owned_runc.rs:680` and `tests/oci_crash.rs:1216` `return` when
      their env var is unset, and `qualify-oci-interruptions.sh` and
      `make test-linux` run them. *Fix:* `panic!` when the var is missing, and
      `--skip` them in the automated drivers. The real runs stay manual
      (Lima), which the docs should say plainly.
- [ ] **T2.3 Quorum assertions were loosened.** `tests/placement.rs:~851` and
      `:1093-1103` now also accept capacity or leader-unknown refusals. Restore
      the quorum-specific assertion (set up capacity so the quorum check is the
      one that fires).

Execution (grill):

- [ ] **T2.4 The subreaper owner doesn't reap orphans while the workload runs.**
      `waitpid(-1)` only runs in `retire_children`
      (`src/grill/process_owner.rs:~584`), so double-forking workloads pile up
      zombies. Reap in the main loop (`:327-355`).
- [ ] **T2.5 macOS has no boot identity.** `current_boot_id()`
      (`src/grill/process_owner.rs:132-149`) returns `None` off Linux, so a
      reboot leaves `Running` records failing with NotFound forever. Use the
      `kern.bootsessionuuid` sysctl. Also: macOS `retire_children` checks only
      the process group, so `setsid` descendants survive.
- [ ] **T2.6 Adoption compares start times exactly.**
      `src/grill/runc/owned.rs:786`. Use the ±2 s slack the rest of the code
      uses (NTP steps move `/proc/stat` btime).
- [ ] **T2.7 A failed stop is silently ignored on restart egress refusal.**
      `src/bun/agent.rs:~7942` `let _ = …stop(&id).await`, then `Failed` with
      no retry, leaking the container and its network reference. Keep the
      instance owned and retry the stop.
- [ ] **T2.8 An `io::Error::other` allocation runs inside `pre_exec`.**
      `src/grill/volume/owned.rs:437`. Use `io::Error::last_os_error()`, and
      make the `// SAFETY:` comment true.
- [ ] **T2.9 Flaky `job_recovery` test (seen locally).**
      `completed_job_survives_bun_death_with_or_without_adoption_record` failed
      once under full-suite load with
      `StateUnavailable { reason: "Broken pipe (os error 32)" }` at
      `tests/job_recovery.rs:254`, then passed 10 times in isolation.
      `ProcessGrill::state` treats an EPIPE from an exiting owner as a hard
      error. Treat it as transient and re-read the durable record.
      Also watch `bun::api::tests::app_metrics_name_injection_cannot_bypass_predicate`:
      it hit `LEAK-FAIL` once under parallel load (the PR made
      `leak-timeout` fatal), then passed 6 times in isolation.

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
- [ ] **T2.13 Every mTLS request clones the whole cluster state.**
      `refuse_retired_tls_peer` calls `council.security_state()`, which clones
      all of `DesiredState`. Add a borrowing accessor for the security part.
      Also make it fail closed when SAN parsing fails.
- [ ] **T2.14 Blocking filesystem calls on async tasks.**
      `sweep_orphaned_identity_dirs` (`src/bun/agent.rs:8590`), `:5360`,
      `prepare_rollback` in `src/upgrade/manager.rs`. Wrap in
      `spawn_blocking`. Add a timeout to `recover_discovery`'s
      `launch_inventory` call.

Registry and storage:

- [ ] **T2.15 Lease expiry trusts the proposing node's clock.**
      `src/pickle/lease.rs:155-164`, `src/pickle/api.rs:367`,
      `src/pickle/copy.rs:74`. A slow clock lets a node write after expiry. Have
      the leader stamp the time, or refuse `observed_at` beyond a skew bound.
- [ ] **T2.16 `revalidate_blob` reads multi-GB blobs into memory.**
      `src/pickle/store.rs:214-226`, called on every heal tick and p2p resolve.
      Stream the hash as `copy.rs:118` already does. Treat read errors as
      errors, not "not cached".
- [ ] **T2.17 An explicit council refusal doesn't roll back the local
      catalogue.** `src/pickle/api.rs:316-378`. A push that loses the GC race
      leaves its digest tagged locally, so its layers never get collected. Roll
      back on `Refused`/`Stale`; keep local state only on timeout.

Config:

- [ ] **T2.18 Removed config keys now fail parsing.** `[upgrades] release_url`
      and `[dns] default_namespace` in `src/config/node.rs`. Accept them with a
      deprecation warning so an upgraded node with an old config still starts.

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
