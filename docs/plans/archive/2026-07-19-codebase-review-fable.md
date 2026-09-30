# Codebase review — correctness, bugs, security, missing features (2026-07-19)

Independent audit of the whole tree **except Phase 15** (testing/diagnostics,
which is still TODO). Conducted by fan-out review across every subsystem, with
every Critical finding re-verified against current `main` by reading the exact
code path. Each finding carries `file:line`, a concrete trigger, and a fix
direction. Findings are grouped Critical / Medium / Optional; a recommended
implementation order (the TODO list) follows at the end, plus an appendix of
things checked and found sound.

Verification legend: **[V]** = I personally re-read the code and confirmed the
mechanism; **[A]** = reported by a subsystem reviewer, high-confidence, code
cited but not independently re-read by me; **[?]** = flagged uncertain.

Severity legend: Critical = exploitable security hole, auth bypass, data loss,
split-brain, or host escape, with a plausible trigger. Medium = a real bug with
a realistic trigger or a meaningful missing safeguard. Optional = hardening /
correctness cleanup.

---

## Critical

### C1 — Relish trusts public CAs while disabling hostname checks → MITM steals the admin bearer token **[V]**
`src/relish/client.rs:249-257`. With `--ca-cert` set, the client does
`add_root_certificate(cluster_ca).danger_accept_invalid_hostnames(true)` but
never calls `tls_built_in_root_certs(false)`. reqwest keeps the system/webpki
roots *in addition* to the cluster CA, and hostname verification is off — so any
certificate chaining to any public CA is accepted for `https://node:9117`. An
on-path attacker presents a valid Let's Encrypt cert for their own domain,
terminates the TLS, and harvests the `Authorization: Bearer` header. An admin
token is cluster takeover. The comment claims "the CA pin is the real guarantee"
but the CA is *added*, not *pinned*.
**Fix:** add `.tls_built_in_root_certs(false)` on the `--ca-cert` path, and
verify the node-id SAN instead of blanket-disabling hostname checks.

### C2 — Auth middleware fails **open** on an empty token store, reachable at runtime by revoking the last token **[V]**
`src/sesame/auth.rs:262-264` (`if tokens.is_empty() { return next.run(...) }`),
`src/council/state_machine.rs:358-362` (`RevokeApiToken` = `retain(|t| t.name != name)`,
no floor), `src/bun/api.rs` `token_revoke_handler` (no last-admin guard). The
empty-store allow-all is documented for first-run, but the loopback-only listener
guard in `bun.rs` is enforced **only at startup**. A node that booted with tokens
and bound `0.0.0.0` and then has its last token revoked (e.g. an admin rotating
credentials, or deleting what looks like a stale duplicate) drops to
*unauthenticated for everyone who can reach the port* — apply, exec, secret
rotate, token create all open.
**Fix:** fail closed when the store empties post-bootstrap (gate the allow-all on
an explicit one-shot bootstrap flag), and refuse a revoke that would remove the
last Admin token.

### C3 — Token namespace/app scope is enforced only on mutations; all per-app reads ignore it → cross-tenant disclosure **[V]**
`src/bun/api.rs`: `logs_handler`, `status_app_handler`, `app_env_handler`,
`metrics_app_handler`, `ws_logs_handler` take no `auth` argument and never call
`authorize_scoped` (contrast `stop`/`exec`/`rollback`, which do). The `/v1/logs/sql`
endpoint (`logs_sql_handler`) compounds this — it exposes the node's **entire** `logs`
table with no app/namespace filter at all (unlike `LogStore::query`, which filters by
tenant). A legitimately issued token scoped to namespace `team-a` can `GET
/v1/logs/web/team-b`, `/ui/app/web/team-b/env`, `/v1/status/web/team-b`,
`/v1/metrics/app/...`, and `GET /v1/logs/sql` for every tenant's logs (which routinely
carry secrets/PII), plaintext env, and status. `status_app_handler` fetches full cluster status and filters only by the
*path* app/namespace, never by the caller's scope. Exploitable by a normal
low-privilege token — no admin mistake required.
**Fix:** thread `auth` into every per-app read handler (and the WS log stream) and
call `authorize_scoped(auth, app, namespace)`.

### C4 — Default disaster recovery restores EMPTY state on a young cluster → silent total data loss **[V]**
`src/council/recovery.rs:98-107` (`load_state_from_data_dir`) opens
`{data_dir}/raft/snapshot.redb` with `redb::Database::create` — which *creates an
empty database* if none exists — and returns its `desired_state()`. Default
`snapshot_threshold` is `10_000` (`src/council/types.rs:415`), so a cluster with
fewer than 10k applied entries has never written a snapshot. `relish council
recover` (default source `NodeDataDir`) then re-bootstraps with **zero apps, no
CAs, no tokens**, and prints "Council recovery complete." The recovery path never
consults `log.redb`.
**Fix:** open the snapshot read-only and error when no snapshot blob exists (or
replay `log.redb` past the snapshot) rather than silently returning `Default`.

### C5 — Disaster recovery has no *protocol-level* fence; a partition defeats the only guard and split-brains a live cluster **[V]**
Recovery does have a pre-flight guard: `relish council recover` refuses when gossip
shows a live council voter (`src/relish/commands.rs:680-684`) unless `--force`. But
that guard is a *soft gossip snapshot* (`recovery.rs:59-64`, `live_council_voter`),
and `recovery_epoch` — bumped on recovery (`state_machine.rs:725/756`) — is **never
read** in `vote`/`append_entries` (grep confirms: only written, displayed in
`commands.rs:715`, asserted in tests), so there is no Raft-level fence. If nodes
A/B/C are healthy but *partitioned* from survivor D, D's gossip view shows them Dead,
`live_council_voter` returns `None`, and the guard passes with no `--force` — D
re-bootstraps as a fresh sole voter while A/B/C keep committing. Two leaders,
divergent terms, lost writes on heal.
**Fix:** carry the monotonic recovery epoch inside the Raft `Vote`/RPCs so any
surviving old voter refuses to serve once a higher epoch appears (a real fence the
gossip check can't provide across a partition).

### C6 — State reports are trusted by self-declared `node_id`; one peer poisons the whole cluster view **[V]**
`src/reporting/aggregator.rs:139` discards the authenticated transport peer
(`Some((_, Report(report)))`) and `store_report:221-230` keys `entries` by
`report.node_id`. mTLS is optional and, even when on, the client-cert identity is
never compared to `report.node_id`. Any worker can send
`Report{ node_id: "victim", running_apps: [] }`, overwriting the victim's entry.
During a leader's learning period the diff then sees the victim as
reporting-but-empty → every desired app there becomes `MissingApp` → the leader
reschedules workloads that are still running (duplicates / split brain); a forged
`running_apps` yields `ExtraApp` → live workloads killed. `AggregatedReport`
injects hundreds of fake nodes at once.
**Fix:** derive reporter identity from the authenticated TLS client cert, reject
any report whose `node_id` ≠ cert identity, and accept `AggregatedReport` only
from authenticated council peers.

### C7 — Cluster transports run plaintext/unauthenticated when identity/master-key is unset **[A]**
`src/cluster/runtime.rs:182-189` (gossip HMAC only when `wrapping_ikm` set),
`:266-285,350-352,462-522` (Raft acceptor / reporting `bind_tls` are `None`
without identity). Nothing refuses to bring up cluster transports in the clear. A
clustered node started without identity exposes Raft RPC and reporting ports
unauthenticated; any host reaching them can drive append-entries/vote to poison
consensus, forge reports (C6), or gossip a poison leader-hint (C8). This is the
substrate that makes C6/C8 remotely reachable rather than insider-only: with
`require_mtls` unset, the C6 report-spoofing needs no valid certificate at all — any
host reaching the reporting port injects reports for arbitrary `node_id`s (and seeds a
whole fake fleet via `AggregatedReport`).
**Fix:** fail closed — refuse to bind Raft/reporting/gossip unauthenticated when
`--cluster` is set (allow plaintext only in explicit single-node mode).

### C8 — Untrusted gossip leader-hint term monotonically ratchets the reporting epoch → permanent cluster-wide reporting wedge **[A]**
`src/cluster/directory.rs:70-73` (a `hint.term > term` beats real Raft metrics)
and `src/cluster/runtime.rs:672-679` (`epoch_tx` only ever grows), with no
plausibility bound on the hint term. A member (or any network peer when the gossip
HMAC key is unset, per C7) gossips `LeaderHint{ term: u64::MAX, reporting_address:
attacker }`. Every node adopts it, bumps `epoch` to `u64::MAX`, and points
`council_rx` at the bogus address. Because the epoch is monotonic, no genuinely
elected leader at a real (lower) term can ever reclaim reporting — the wedge
survives real elections and redirects report/placement traffic.
**Fix:** bound a gossip hint term against the local Raft term window before it can
outrank Raft metrics.

### C9 — Pickle GC deletes a blob re-referenced between Raft approval and physical delete → image data loss **[V]**
`src/pickle/gc.rs:159` (`delete_approved`) physically deletes each approved digest
with **no re-check** of the reference set; the check happens only at approval time
(`types.rs:330`). Standard docker push dedup (`HEAD` the blob → 200 → skip upload →
`PUT` manifest) re-references an about-to-be-deleted orphan; `check_descriptor`
(`api.rs:708`) sees it on disk and the commit lands; the node then executes the
earlier approval and deletes it, leaving the catalogue referencing a blob that
exists nowhere. Deploys 404. The single-node GC task has the same window.
**Fix:** re-check the (local or council) reference set immediately before each
`delete_blob`, and/or refuse a manifest whose descriptor digests sit in an
approved-pending-delete set.

### C10 — GitOps wipes cluster desired state on a parse error or a failed `ls-tree`, reported as success **[V]**
Two paths, same outcome: (a) `src/lettuce/sync.rs:221-227` skips an unparseable or
duplicate-colliding file entirely, so its resources are absent from the merged
config and `compute_diff` emits `Remove` for every one of them; (b)
`src/lettuce/git.rs:208-215` (`list_toml_files`) never checks `ls-tree`'s exit
status, so a renamed watched directory or a typo in `[gitops] path` yields empty
stdout → empty config → **Remove everything**. The runner treats `PartialSuccess`
as applyable and advances `last_applied_commit`, so the wipe is recorded as a
successful sync. A single typo in one repo file deletes that file's apps from the
cluster; a bad path deletes the whole cluster's desired state.
**Fix:** treat any file/parse error and any non-zero `ls-tree` as a hard sync
`Failure` (suppress all `Remove` changes when `file_errors` is non-empty).

### C11 — GitOps signature verification fails open **[V]**
`src/lettuce/verify.rs:21-33` returns `SignatureStatus::NotChecked` when
`trusted_keys` is empty *and* when spawning `git` fails, and `execute_sync` treats
`NotChecked` as a pass. With `require_signed_commits = true` but
`trusted_signing_keys = []` (or `git` missing on the leader), zero verification
happens and unsigned commits are applied — no warning. The existing test encodes
this fail-open as expected behaviour.
**Fix:** when `require_signed_commits` is true, treat `NotChecked` (and empty
trusted keys at config-validation time) as a hard failure.

### C12 — Scheduler namespace quota is bypassed once apps converge **[V]**
`src/cluster/orchestrate.rs:303` (`if placements_ok { continue; }`) short-circuits
a converged app *before* the quota-admission block at `:312`, and the ledger is
rebuilt empty every tick (`:179`). A running app therefore contributes nothing to
the ledger. Namespace `prod` with `cpu = "1000m"`: app A (600m) converges → counts
as 0 → app B (600m) is admitted against an empty ledger → both run at 1200m over a
1000m budget. Same for `max_memory`, `max_replicas`, `max_apps`.
**Fix:** seed the ledger from every desired app's committed footprint before
planning, so admission checks real namespace usage.

### C13 — GitOps skips the config validation that manual apply enforces (divergent write paths) **[V]**
Manual apply runs `config.validate_against(&known_namespaces)`
(`src/bun/api.rs:1119`); the GitOps path parses with bare `Config::parse`
(`src/lettuce/sync.rs:221`) and never validates. A repo with an invalid
`[autoscale]` (min > max), `request > limit` resource range, or unknown-namespace
reference is rejected by `relish apply` but committed straight to desired state via
git — the autoscaler then logs "invalid" and silently never scales. The shared
`config_to_desired_writes` doesn't close this because validation lives around it,
on the apply side only.
**Fix:** call `validate_against` in `sync` before `compute_diff` and surface
failures as sync errors.

### C14 — Process-workload mount isolation is documented and admission-gated but never implemented → host FS access as root **[V]**
`src/config/process_workloads.rs:24-35` and `src/grill/process_workload.rs:1-6`
promise host `exec`/`script` workloads "run in a separate mount namespace and
cannot see `/var/lib/reliaburger` or other workloads' volumes," and
`src/bun/supervisor.rs:267-277` refuses these workloads on non-Linux *because they
require mount isolation* — implying Linux enforces it. It does not:
`grep -r 'unshare|CLONE_NEWNS|pre_exec' src/` is empty, and `ProcessGrill::start`
(`src/grill/process.rs:207-259`) spawns a plain `Command`. bun runs as root, so an
allowlisted host workload has full host FS access — it reads other namespaces'
volumes, the node identity key material, and encrypted secrets, and can write
anywhere.
**Fix:** actually `unshare(CLONE_NEWNS)` + pivot/bind-restrict via `pre_exec` when
isolation is requested, or refuse host workloads that request isolation until it
exists.

### C15 — Managed-volume paths are unvalidated → host path-traversal write primitive as root **[V]**
`src/grill/volume.rs:55-60` builds the host path as
`volumes_dir.join(namespace).join(app).join(mount_path.strip_prefix("/"))`, and the
mount source is built the same way at `src/grill/oci.rs:352-354` with mount options
`["bind", "rw"]` (`oci.rs:361`); `src/bun/agent.rs:3779-3785` passes
`spec.volumes[].path` verbatim. Config validation rejects a *non-absolute* mount
path (`src/config/validate.rs:225`) — but `/../../../../etc/cron.d` **is** absolute,
so it passes, and nothing checks for `Component::ParentDir`. After `strip_prefix("/")`
the `../…` join escapes `volumes_dir`; bun (root) `create_dir_all`s it and bind-mounts
it **read-write** into the container. Reachable by anyone who can deploy an app
(a Deployer-scoped token, or a GitOps repo committer). The traversal exists in both
the create path (`volume.rs`) and the mount path (`oci.rs`), so both must be fixed.
**Fix:** reject a managed-volume `path` that contains any `Component::ParentDir`
(in addition to the existing absolute-path check), in both paths.

### C16 — Self-upgrade retention GC can delete the running binary and brick the node **[V]**
`src/upgrade/manager.rs:467-470` — the post-commit GC protects only
`marker.previous_version`, never the version the symlink now points at.
`garbage_collect` keeps the newest `retain` and deletes the rest. Rollback past the
retention window (5 versions on disk, `retain_versions=3`, roll back to the oldest)
deletes the very version the `bun` symlink now targets → the symlink dangles → the
next exec/restart hits ENOENT with no automatic revert. (`retain_versions=0` is
separately rejected by config validation — `validate.rs:340` — so the first-commit
variant of this can't be configured; the rollback-past-window path above needs no
misconfiguration.)
**Fix:** protect the current symlink target too
(`garbage_collect(retain, &[previous, target])`).

---

## Medium

### M1 — Smoker chaos faults are unreliable and can leave a node damaged **[V]**
A cluster of related defects in `src/bun/agent.rs` make the fault-injection tooling
untrustworthy — serious because its whole purpose is producing *honest* resilience
signals:
- **`fault partition` is a silent no-op** (`:3121-3124`, `Partition => Ok(())`); the
  `InjectFault` path used by `relish fault partition` never writes a blocklist, so
  it reports success and blocks nothing — a chaos experiment "passes" against no
  fault. **[V]**
- **`chaos heal` clears faults without reversing them** (`:2474-2483`): unlike
  `ClearAllFaults` (`:2583-2589`) it never calls `reverse_fault`/
  `delete_fault_bpf_entry`, so a SIGSTOPped workload stays frozen forever and
  cgroup `cpu.max`/`memory.high` caps persist, with no record a fault existed. **[V]**
- **Legacy partition never auto-heals on TTL** (`:3371-3399` omits `clear_partition`):
  a Ctrl-C'd `relish chaos council-partition` leaves the council partitioned
  permanently while `chaos status` reports none. **[A]**
- **Partial cgroup application leaks limits** (`:3193-3212` aborts on first failure,
  caller removes the registry entry): earlier replicas stay throttled with no
  reversal state. **[A]**
- **Safety rails skipped** on the legacy `InjectPartition` path (`:2444-2472`, no
  `evaluate_safety`) and whenever there's no cluster handle (`:2884-2886`), so
  `fault kill --count 0` can kill every replica. **[A]**
**Fix:** implement the Partition arm; make `HealPartition` and TTL-expiry run the
same reversal loop as `ClearAllFaults`; restore saved cgroup values on partial
failure; run `evaluate_safety` on every inject path and build a degraded safety
context when no cluster handle exists.

### M2 — Pickle node-to-node replication never authenticates → images stay at one copy in any cluster with tokens **[A]**
`src/pickle/replication.rs:191,236` POST/PUT to peers with no `Authorization`
header; the cluster HTTP client adds none, and `authorise_write` requires a bearer.
The moment an operator mints the first API token, every peer 401s the replication
uploads; the heal loop only `eprintln`s, and images silently stay single-copy
(durability loss). `bun.rs:1530`'s comment claims the service token is presented —
it isn't.
**Fix:** attach `Authorization: Bearer <service_token>` in
`replicate_layer_to_peer`.

### M3 — Pickle `cache/` namespace is not reserved on push → cache poisoning bypasses `require_signatures` **[A]**
`src/pickle/api.rs:743` (`manifest_put`) accepts any repo name including
`cache/docker.io/library/redis`; the scheduler exempts `cache/*` from signature
checks (`src/meat/scheduler.rs:318`) and `upstream::decide` treats the attacker's
commit as a cache hit. A Deployer pushes a poisoned `cache/...:tag` and every node
deploying that image runs it with no upstream fetch and no signature.
**Fix:** reject client pushes to repositories starting with `cache/` (only the
cache-fill path may write there).

### M4 — Join token is not bound to the requested `node_id` → a token holder can mint a cert impersonating any node **[V]**
`src/sesame/types.rs:366` (`JoinToken` has no node_id field) and
`src/bun/agent.rs` `handle_join_issue` signs a cert for whatever `node_id` the
request carries, rebuilding the SPIFFE SAN from it, with no check that the id is new
or matches the token's intent. A token minted for `node-05` can be used with
`node_id: "node-01"` to obtain a valid Node-CA cert whose SAN is
`spiffe://…/node/node-01` — which the bound Raft verifier accepts as node-01,
defeating the impersonation guarantee.
**Fix:** bind the intended `node_id` into the token at creation and reject a CSR
whose id doesn't match or already exists.

### M5 — Single-node network upgrade silently downgrades dual-signature to embedded-only **[V]**
`src/relish/upgrade.rs:174-181` builds the single-node directive with
`BinarySource::LocalFile` even for bytes just downloaded from the network;
`manager::prepare` derives `network` from the source variant, so `verify_binary`
runs with `network=false` and (when the release metadata has no external signature,
the normal public-release case) hits the `(false, _, _) => Ok(())` arm — skipping
the external signature *even though the node has `external_signing_key`
configured*. The operator-approval half of dual-signing is bypassed on this path.
**Fix:** derive `network` from how the bytes were obtained, or require the external
signature whenever `external_signing_key` is set.

### M6 — Leader self-upgrade bypasses the live-quorum gate **[A]**
`src/upgrade/orchestrator.rs:158` hardcodes `may_direct_new=true` for the
`UpgradingLeader` phase and ignores `context.quorum_ok` (which correctly gates the
council phase at `:124-129`). The reachable exposure: a council voter that upgraded
*Healthy* during the council phase then goes gossip-dead during the leader's own
exec window — `quorum_ok` flips false, but the leader upgrades in place anyway,
dropping live voters below quorum, and indefinitely if the new binary crash-loops
before revert. (A voter that is *already* dead before the leader phase instead pauses
the run at the council phase, so that simpler scenario doesn't reach here — the
code defect, a hardcoded `true` ignoring `quorum_ok`, is the same.)
**Fix:** gate the leader phase on `context.quorum_ok` too.

### M7 — Production rolling deploy caps the health wait at 5s and ignores `max_surge`/`max_unavailable` **[V for the 5s cap]**
`src/bun/agent.rs:6781` clamps the health wait to `min(health_wait, 5s)`, so a
configured `health_timeout = "60s"` is silently 5s and a slow-starting container is
declared unhealthy → rollback. Separately (`:6672-6683`), `rolling_redeploy` reads
only `health_timeout` from `DeployConfig`; `max_surge`/`max_unavailable` parse,
validate, and change nothing (always full surge then retire all old).
**Fix:** honour the full configured timeout (poll off the command loop without
capping the deadline) and actually apply the surge/unavailable knobs.

### M8 — Ingress `tls = "cluster"` silently serves a self-signed `localhost` cert **[A]**
`src/wrapper/tls.rs:57` (`issue_ingress_cert`) is dead code (only tests call it);
`bind_proxy_with_drains` (`src/wrapper/proxy.rs:147-158`, wired at `bun.rs:874`)
only loads an operator disk cert or generates a self-signed `localhost` cert, and
there is no SNI/per-route cert selection. A route marked `TlsMode::Cluster` (which
correctly does the HTTP→HTTPS redirect, so no plaintext downgrade) is served the
self-signed cert, not one from the Sesame Ingress CA — a client trusting the
cluster root gets an untrusted-cert failure; one that ignores validation is exposed
to MITM.
**Fix:** thread the Ingress CA into `bind_proxy_*` and issue per-host certs for
`Cluster` routes, or reject `cluster` until wired.

### M9 — WebSocket upgrade path bypasses `X-Forwarded-*` sanitisation → client spoofs the trusted client IP **[A]**
`src/wrapper/proxy.rs:436-440` returns into the WS upgrade *before* the
header-strip block at `:457-469`, and `build_upgrade_request`
(`src/wrapper/websocket.rs:186-206`) copies every client header verbatim and never
sets the proxy's own `x-forwarded-for`. A client opens a WS to any `websocket:
true` route with a forged `X-Forwarded-For`; a backend that trusts XFF for
rate-limiting or IP allow-listing is lied to (the non-WS path strips this, and a
test proves it).
**Fix:** strip/replace `x-forwarded-*`/`forwarded` in `build_upgrade_request`.

### M10 — Pickle quota is bypassable via chunked/bare-blob uploads **[A]**
`src/pickle/api.rs:576-637` (`blob_upload_complete`) never calls `enforce_quota`,
and `stored_sizes:89` counts only committed manifests. A writer lands arbitrary
bytes on disk (chunked, or monolithic blob without a manifest) regardless of
`max_storage`, bounded only by the 1h orphan grace before GC. `UploadSession.written`
is tracked but consulted by nothing.
**Fix:** enforce quota in `blob_upload_patch`/`_complete` from the session's running
`written` plus stored sizes.

### M11 — Pickle whole-blob buffering enables OOM **[A]**
`MAX_REQUEST_BYTES` = 512 MiB buffered per request (`src/pickle/api.rs:27`) and
`pull_layer_from_peer` buffers up to 2 GiB × `p2p_concurrency` in RAM
(`src/pickle/pull.rs:24,78`), with no concurrency bound. A few concurrent large
pushes/pulls OOM a node.
**Fix:** stream PATCH bodies to the upload temp file and stream peer fetches through
an incremental hasher to disk.

### M12 — Gossip incarnation resets to 1 on restart → restarted node stuck Suspect/Dead **[A]**
`src/mustard/protocol.rs:99-103` sets `incarnation: 1` on every `new()`; `refute()`
bumps by 1 and a lower incarnation loses. A node that reached incarnation 50, crashed
(peers hold Dead@50), and restarts at 1 needs ~49 Dead gossips before its Alive wins
— often longer than the 60s reap, invisible to scheduling/council throughout.
**Fix:** persist incarnation across restarts, or seed from `max(seen-about-self)+1`.

### M13 — SWIM indirect-probe ACK overwrites the probed node's address → false eviction of a healthy node **[A]**
`src/mustard/protocol.rs:483-489` + `membership.rs:152` overwrite a node's address
from a relayed ACK. When A reaches B only via relay C, C's forwarded ACK
(`sender=B`, socket=C) makes A record B's address as C's; A's next direct probe
hits C, whose ACK `sender=C ≠ B` never matches, so A marks the healthy B
Suspect→Dead. A successful rescue manufactures a failure.
**Fix:** never update a node's address from a relayed/forwarded ACK.

### M14 — Placement reconciler wedges forever on a hung deploy (no timeout) **[A]**
`src/cluster/orchestrate.rs:747` → `deploy_succeeded:786-795` awaits `events.recv()`
with no timeout. A stuck image pull / hung runc that holds `event_tx` without
emitting `Complete`/`Error` blocks the tick forever: the node stops polling the
leader, never syncs endpoints, never converges — silently drops out of
reconciliation while still alive.
**Fix:** `tokio::time::timeout(deploy_deadline, …)`; treat elapsed as not-applied
and retry.

### M15 — Reconstruction counts stale-but-unevicted reports as live actual state **[A]**
`src/reconstruction/diff.rs:42-47` builds `actual_placements` from `actual.reports`
ignoring `actual.stale_nodes` (which linger ~90s after silence), and
`controller.rs:120-121` counts them toward coverage. A node that died ~60s before an
election is counted (ends learning early) and its dead apps are treated as running
→ no `MissingApp` → workloads that actually died are never rescheduled.
**Fix:** exclude `stale_nodes` from both the diff's actual set and the coverage
count.

### M16 — Reconstruction coverage shortcut can skip learning entirely **[A]**
`src/reconstruction/controller.rs:70-93,178-185`: `on_leader_elected(0)` jumps
straight to `Active`, and a low alive-count during a gossip lull lets a handful of
(possibly spoofed) reports clear the threshold. A leader elected in a lull resumes
scheduling against an empty view and reschedules workloads still running elsewhere.
There is also no internal learning timer (relies on an external `check_timeout`
caller).
**Fix:** require a settling delay / re-sample before the shortcut and drive
`check_timeout` from an owned interval.

### M17 — Council rollup idempotency is in-memory only → restart double-counts cluster sums **[A]**
`src/mayo/rollup_store.rs:87,116-121` — the `(node_id, window)` dedup set is
process-local and starts empty on restart while re-loading prior Parquet. After a
council member restarts, a reassigned node's backfill of an already-ingested window
is counted twice, doubling `query_cluster_metric` sums for that window.
**Fix:** persist the seen keys, or dedup ingest against the on-disk
`(node_id, timestamp)` set.

### M18 — Alert webhook logs the full destination URL on failure → Slack secret leaks into collected logs **[A]**
`src/mayo/webhook.rs:149,266-281` `eprintln!`s `dest.url` on every retry and final
give-up; Slack webhook URLs carry the secret in the path, so a transient Slack outage
writes the credential to the node's stderr/journal in cleartext. (PagerDuty/generic
secrets live in `dest.secret` and are *not* logged — only URL-embedded creds leak.)
Whether it then reaches object storage depends on how bun's own stderr is captured by
the log pipeline — plausible but not proven here; the cleartext-to-local-logs exposure
is certain regardless.
**Fix:** log scheme+host only (drop the path), never the full URL.

### M19 — "Memory-capped" bounded log SQL materialises the whole archive first **[A]**
`src/ketchup/log_store.rs:317-351` (`session_with_memory_limit`) `.collect()`s every
`logs_*.parquet` row into a `MemTable` before planning, and that base scan is not
charged to the `RuntimeEnv` pool — so the OBS5 memory limit only covers aggregation,
not the dominant cost. `MayoStore`/`RollupStore` `read_disk_batches` do the same.
Retention normally caps archive size (why this is medium).
**Fix:** query the Parquet dir via a streaming `ListingTable`
(as `remote_query.rs` already does) instead of a MemTable.

### M20 — Alert evaluation lacks per-value freshness and collapses metrics by name across labels **[A, harm downgraded on verification]**
`gather_latest_values` (`src/mayo/webhook.rs:334`) returns the newest value seen
anywhere in the last 120s with no per-value freshness check, and collapses to
first-seen *per metric name* across labels (`values.entry(name).or_insert(val)`), so
distinct labelled series merge. **Correction from the original draft:** the claimed
"a stale in-range reading wrongly *resolves* a firing alert" harm does **not** hold —
`alert.rs:145-172` gates both firing and resolution on that same newest value, so a
mid-breach freeze keeps the last (breaching) value and stays firing, and any genuinely
in-range newest reading would already have resolved the alert while fresh. The residual
issue is robustness (freshness + label collapse can attribute the wrong series' value),
not a false-resolve bug — treat this as Optional-tier.
**Fix:** carry each value's timestamp with a freshness threshold, and key by the full
label set rather than metric name.

### M21 — Relish CLI cannot manage non-default namespaces; `history`/`build` bypass auth **[A]**
`src/relish/commands.rs`: `logs`/`exec`/`stop`/`rollback` hardcode namespace
`"default"` (`:194,302,315,895`), so an app in a namespace derived by `compile` from
its directory can't be managed and may hit an unrelated same-name app in `default`;
`history` (`:856-886`) and `build` context upload (`:1263-1272`) use bare
`reqwest::get`/`Client::new()` with no bearer/CA, so they 401 against a secured agent
and `history` exits 0 on failure.
**Fix:** add a `--namespace` flag threaded through those handlers; route `history`
and the build upload through the authenticated `BunClient`.

### M22 — ProcessGrill silently ignores cgroup CPU/memory limits **[A]**
`src/grill/process.rs:194-259` never reads `spec.linux.resources`; admission only
refuses limits on *rootless* nodes (`supervisor.rs:311-321`). A rootful Linux node
with no `runc` installed falls back to ProcessGrill and accepts a `memory`/`cpu`
workload, then runs it unbounded — an OOM-looping workload takes down the host.
**Fix:** refuse limit-bearing workloads when the active runtime is ProcessGrill (or
enforce via cgroup writes).

### M23 — Adopted-process polling has no start-time recheck → pid-reuse mis-tracking and wrong-process kills **[A]**
`src/grill/records.rs:159-175` (`poll_adopted_process`) falls back to `kill(pid, 0)`
after `ECHILD` and never re-checks `pid_started_at`. After a bun restart reparents
the adoptee, if the workload exits and its pid is reused, `state()` reports Running
forever and a later `stop`/`kill` signals the innocent reused pid.
**Fix:** pass `pid_started_at` into `poll_adopted_process` and treat a start-time
mismatch as exited.

### M24 — `deploy_app` leaks ports and orphans instances on mid-loop failure **[A]**
`src/bun/supervisor.rs:355-402` allocates a port and inserts each instance inside the
replica loop but writes `app_instances` only after the loop. If a later replica's
`allocate()` is `Exhausted`, the `?` bails leaving earlier replicas' ports reserved
and instances unreachable by `remove_app` → ports leak until agent restart.
**Fix:** allocate all ports first (release on early return) and write `app_instances`
transactionally.

### M25 — Scheduler pass-cache and daemon-set convergence defects **[A]**
Two related `src/cluster/orchestrate.rs` bugs: a partially-placed fixed-replica app
writes its phantom reservations back into the shared pass cache (`:331`, after
`schedule_fixed` errored), starving later apps in the same pass; and a daemon set
whose `want = alive.len()` (`:353-359`) never reaches `placements_ok` when any alive
node is ineligible, committing a fresh `SchedulingDecision` to Raft every 2s forever.
**Fix:** discard the mutated cache on scheduler error; compute daemon `want` from the
eligible set.

### M26 — Autoscaler metric lookup is a substring, namespace-blind match **[A]**
`src/cluster/orchestrate.rs:493` filters `agg.labels.contains(app)` with a bare app
name, so `web` averages in `webhook`/`web-api` rows and `web` in `prod`/`staging`
share one pool → wrong utilisation drives wrong scaling.
**Fix:** match a structured, namespace-qualified label key exactly.

### M27 — GitOps operational defects: no HEAD advance, replay opt-in, credential exposure **[A]**
`src/lettuce/`: the bare clone sets no fetch refspec so local HEAD never advances
(`git.rs:90-114`) → every 30s poll is a full re-sync writing `GitOpsSyncUpdate` to
Raft (unbounded log churn); webhook replay detection is skipped when `delivery_id` is
absent (`webhook.rs:95-105`); and the repo URL (often carrying a token) is passed as
`git clone` argv and echoed into `GitFailed` errors stored durably in Raft
`last_error` (`git.rs:64-77`, `runner.rs:201-206`).
**Fix:** `update-ref` HEAD to FETCH_HEAD after fetch; require a delivery ID when a
secret is configured; use `GIT_ASKPASS` and redact URLs in errors.

### M28 — k8s import/export silently drop fields **[A]**
`src/relish/k8s_import.rs`: documents starting with `#` are skipped (`:154-156`) so
Helm-rendered manifests (every doc prefixed `# Source:`) import *nothing* with exit
0; splitting on the substring `---` (`:153`) shreds embedded PEM/block scalars; only
`containers.first()`, `resources.limits`, and `readiness_probe` are read, so
sidecars, requests, liveness probes and volumes are lost without warning. Export
(`k8s_export.rs:114-148`) drops `command`, health, resources, volumes, init,
placement, and namespace silently.
**Fix:** parse multi-doc via `serde_yaml::Deserializer`; warn per dropped field;
export the missing spec fields.

### M29 — `max_boot_attempts = 0` is unvalidated → upgrades can never commit **[V]**
`src/config/validate.rs:327-345` validates the upgrade section (external key,
release-key override, `retain_versions > 0`, absolute `binary_dir`) but does **not**
guard `max_boot_attempts`. With `max_boot_attempts = 0`, `decide_startup`
(`src/upgrade/marker.rs:210`) reverts every upgrade on its first boot (`0 + 1 > 0`),
so no upgrade can ever succeed. (Correction to an earlier draft: `retain_versions = 0`
*is* rejected here — only `max_boot_attempts` is the gap.)
**Fix:** reject `max_boot_attempts == 0` in the same validation block, and floor it
defensively in `decide_startup`.

---

## Optional (hardening / cleanup)

- **O1 — Anonymous Pickle reads are cluster-wide on a non-loopback bind**
  (`src/pickle/registry_auth.rs:8`): any network client can pull every image,
  including `cache/` copies of credentialed private upstreams. Consider requiring a
  bearer for reads on non-loopback binds.
- **O2 — Pickle build-context URLs hardcode `http://`** (`src/pickle/build.rs:249,266`)
  and `buildah push --tls-verify=false` (`:230`): delegated builds fail / transfer
  source in plaintext against a TLS registry. Thread the registry scheme.
- **O3 — Upgrade binary blob fetch/push is plaintext `http://`**
  (`src/upgrade/manager.rs:508-509`, `src/relish/upgrade.rs:480-483`): breaks cluster
  upgrades against a TLS-only registry (integrity still sig+sha gated). Route through
  `ClusterHttp`.
- **O4 — `prepare_rollback` execs a stored binary with no signature re-check**
  (`src/upgrade/manager.rs:344-357`): defense-in-depth only (store write already =
  code exec), but re-verify the `.sig` envelope.
- **O5 — Unbounded memory growth** across the cluster plane: reporting aggregator maps
  (`aggregator.rs:221-264`), mustard dissemination heap / membership table
  (`dissemination.rs:70-76`, `membership.rs:105-122`), and never-pruned consumed
  `join_tokens` / expired `crl.entries` (`state_machine.rs:315-328,462-466`). Admit
  only current-membership node_ids; cap/compact; prune on apply.
- **O6 — Raft RPC pre-allocates an attacker-controlled ≤64 MiB buffer per connection
  with no connection cap** (`src/council/network.rs:239`): bounded by mTLS when
  enforced, open on the plaintext path (C7). Semaphore-bound accepts; grow the buffer
  incrementally.
- **O7 — Security-relevant reads are served from local follower state**
  (`src/council/node.rs:148-150,224-231`): a revoked cert can read valid for a window
  during a leadership transition. Gate on `ensure_linearizable()` or route through the
  leader.
- **O8 — Peer HTTP poll and reporting send rely on TCP defaults (no timeout)**
  (`src/cluster/orchestrate.rs:692-702`, `src/reporting/transport.rs:338-385`): a
  wedged peer that completes connect then stalls blocks the reconciler/worker inline.
  Add `tokio::time::timeout`.
- **O9 — SWIM `refute()` and the relay-ACK path don't refute about self**
  (`protocol.rs:607-620,634,752-754`) and `Left` is unrefutable/sticky
  (`:531-535`, `state.rs:78-83`): a false or replayed Suspect/Left about self can
  evict a node for up to 60s. Apply the fresh Alive to the local record; allow
  refuting `Left`-about-self; gate `Left` to authenticated senders.
- **O10 — `fmt` writes non-atomically and deletes comments**
  (`src/relish/commands.rs:986`, `src/relish/fmt.rs:7-11`): temp-file + rename and warn
  about comment loss. `compile` silently drops duplicate app names across files
  (`compile.rs:207-213`) and `_defaults.toml` only honours `image` with parse errors
  swallowed (`:155-179`).
- **O11 — DNS forwards only over IPv4** (`src/onion/dns.rs:628`, `bind 0.0.0.0:0`): an
  IPv6 `upstream` SERVFAILs every non-`.internal` query. Bare `<app>.internal` resolves
  in the node's `default_namespace`, not the caller's (`dns.rs:610-615`) — cross-ns
  steering if eBPF connect enforcement is unavailable.
- **O12 — Retention prunes Parquet by file mtime, not data timestamp**
  (`src/mayo/store.rs:582-603`, `rollup_store.rs:511-532`): a touched/copied file or
  clock skew drops in-range data. Prune on the file's max timestamp.
- **O13 — `relish logs-search` runs raw operator SQL with no read-only/row/memory
  guard** (`src/ketchup/remote_query.rs:29,135`): operator-local, but an accidental
  unbounded query OOMs the CLI host. Reuse the bounded wrapper.
- **O14 — `dev` clusters share VMs regardless of `--name`** (`src/relish/dev.rs:425`),
  and `dev destroy` deletes the shared build VM: embed the cluster name in VM names.
- **O15 — Bootstrap secret-file permission check ignores ownership**
  (`src/sesame/bootstrap.rs:49`): a `0600` file owned by another uid is accepted. Also
  check `st_uid`.
- **O16 — `validate_chain` doesn't assert leaf `!is_ca` / EKU**
  (`src/sesame/cert.rs:132`): CA-pinning covers it today; add the assertions as
  defense-in-depth.
- **O17 — Smoker unit/arithmetic bugs**: `CpuStress --cores` accepted and ignored with
  wrong multi-core maths (`resource.rs:68-72`); `"1mbps"` = 1 MiB/s not megabit
  (`fault.rs:79-84`); no TTL upper bound / unchecked add (`types.rs:305,313`); replica
  rail counts fault *rules* not replicas (`agent.rs:2932-2934`).
- **O18 — Port allocator edge cases** (`src/grill/port.rs:52-86,111-113`): underflow if
  `end < start`; out-of-range reserved ports count against the exhaustion cap. Validate
  `start < end` at config load.
- **O19 — `top` claims "live resource usage" but prints none**
  (`src/relish/commands.rs:1034-1058`); `apply --dry-run` never fetches current state so
  it always shows `+ create` (`:40,61`). Wire real state or retitle.
- **O20 — Stale/misleading docs & dead code**: mustard datagram bincode-decoded before
  HMAC verify (`transport.rs:274` vs `:279`); `AppDelete` leaves `active_deploys`/
  `deploy_history` behind (`state_machine.rs:204-217`); Raft-id djb2 collision risk
  (`cluster/identity.rs:28-33`); lettuce `recursive` flag ignored, sync history/phases
  never written, coordinator role cosmetic (`lettuce/{types,runner}.rs`);
  `smoker/node.rs` dead structs.

---

## Recommended implementation order (TODO)

Ordered by (impact × reachability) ÷ fix cost. The first block is cheap, high-impact
security/data-loss; do it before anything else.

**Phase A — stop the bleeding (small diffs, severe impact)** — DONE (PR #133, merged)
1. C1 — add `tls_built_in_root_certs(false)` on the relish `--ca-cert` path (one line; stops admin-token MITM). ✅
2. C16 + M29 — protect the current symlink target in upgrade GC, and reject `max_boot_attempts == 0` in config validation (stops node-bricking / never-committing upgrades). ✅
3. C2 — fail closed on an empty post-bootstrap token store + refuse revoking the last Admin token. ✅ (last-Admin-revoke floor; the broader bootstrap-flag hardening deferred)
4. C11 — make GitOps signature verification fail closed when `require_signed_commits`. ✅
5. C10 — make any GitOps file/parse error or non-zero `ls-tree` a hard `Failure` (suppress `Remove` when errors exist). ✅
6. C4 — error (don't return empty) when disaster recovery finds no snapshot. ✅

**Phase B — close the data-loss and quota holes** — DONE (PR #134)
7. C9 — re-check the reference set immediately before each Pickle blob delete. ✅
8. C12 + C13 — seed the namespace quota ledger from committed state, and run `validate_against` on the GitOps path. ✅
9. C15 — reject managed-volume paths containing `..` / non-absolute before provisioning. ✅
10. C14 — refuse host workloads that request the (unimplemented) mount isolation; real mount-namespace isolation deferred. ✅

**Phase C — authenticate the cluster plane (the systemic theme)**
11. C7 — refuse to bind Raft/reporting/gossip unauthenticated in `--cluster` mode. ✅ (PR: Phase C, part 1)
14. C8 — bound gossip leader-hint terms against the local Raft term window. ✅ (PR: Phase C, part 1)
12. C6 — bind report identity to the authenticated TLS client cert. ✅ (Phase C part 2)
13. C5 — carry the recovery epoch inside Raft RPCs as a real fence. ✅ (Phase C part 2) — implemented as a transport-layer envelope fence (RaftRpcEnvelope stamped with the sender's recovery epoch; the accept side drops different-epoch RPCs). This fences the two epochs apart at the RPC boundary — the split-brain vector — rather than embedding the epoch in the openraft Vote type itself.

**Phase D — deploy/runtime correctness**
15. M14 — timeout the reconciler's terminal-event wait. ✅ (Phase D)
16. M7 — honour `health_timeout` (drop the 5s cap) ✅ (Phase D); `max_surge`/`max_unavailable` still deferred (deploy-loop rewrite; noted in the M7 commit).
17. M22 + M23 + M24 — ProcessGrill limit admission, adopted-pid start-time recheck, transactional port allocation. ✅ (Phase D)
18. M25 + M26 — scheduler pass-cache discard on error, daemon-set `want` from eligible set, namespace-qualified autoscale match. ✅ (Phase D)

**Phase E — chaos, registry, supply-chain, upgrade**
19. M1 — make Smoker faults real and reversible. ✅ (Phase E) — heal/TTL reversal loop, `FaultReversal::Partition` for the council-partition path, safety context built (and rails run) on every inject path including with no cluster handle, and partial-cgroup rollback. The service-to-service `Partition` apply arm stays an accepted no-op without eBPF, flagged `TODO(Phase 15)`: the quorum-rail acceptance test injects a Partition to exercise the safety context on a no-eBPF cluster, so tightening it needs that test moved onto an eBPF node first.
20. M2 + M3 + M10 + M11 — Pickle replication auth ✅, reserve `cache/` ✅, quota on chunked uploads ✅, streaming peer pulls to bound memory ✅ (Phase E). Push-side request-body streaming (`MAX_REQUEST_BYTES`) deferred `TODO(Phase 15)`.
21. M4 — bind join tokens to a node id. ✅ (Phase E) — `--node-id` mandatory on `join-token create`; `check_join_token` refuses a mismatched id; `init` mints no bootstrap token.
22. M5 + M6 — upgrade dual-sig on single-node network path ✅ (Phase E), live-quorum gate on the leader bounce ✅ (Phase C part 2 branch, M6 commit).

**Phase F — observability, ingress, CLI** — DONE (one PR)
23. M8 + M9 — ingress per-SNI cluster-CA cert resolver ✅, WS `X-Forwarded-*` sanitisation ✅.
24. M17 + M18 + M19 — rollup restart idempotency (seed from disk) ✅, webhook-URL redaction ✅, streaming log SQL via `ListingTable` ✅. M20 (alert freshness/label-collapse) moved to the Optional PR — the review's own correction downgraded it to Optional-tier, and its full-label-keying fix ripples into the alert evaluator's contract.
25. M12 + M13 + M15 + M16 — gossip refute seeds from `max(seen)+1` ✅, relayed-ACK address ignored ✅, reconstruction excludes stale reports + settling delay/alive re-sample ✅.
26. M21 + M27 + M28 — CLI `--namespace` + authenticated history/build ✅, GitOps HEAD advance/delivery-id/URL redaction ✅ (GIT_ASKPASS argv follow-up), k8s multi-doc Deserializer + dropped-field warnings ✅ (broader export fidelity follow-up).
27. Work the Optional (O1–O20) list as capacity allows; O5/O6/O7/O8/O15/O16 are the security-adjacent ones to prioritise within it.

---

## Appendix — checked and found sound (no finding)

Recording these so the audit's negative space is explicit:

- **API authz shape**: constant-time token comparison (`tokens_equal`), Argon2id with
  unique salts, bounded + short-circuited so junk bearers can't pile up; write handlers
  role-gated via `authorize`/`authorize_user`; public endpoints self-authenticate (join
  validates a join token; the GitOps webhook fails closed without a secret and checks
  HMAC + replay + rate limit).
- **Egress enforcement** (`src/sesame/egress.rs`): fails closed
  (`plan_pre_start_egress` when connect4/connect6 hooks aren't attached — no v6 bypass),
  and `merge_cidr_ports` correctly folds enclosing-prefix ports into more-specific LPM
  entries.
- **Log SQL injection**: `query_sql_json_bounded` can't be escaped via
  subquery/UNION/CTE/comment/`;`; the row cap always holds; metrics handlers interpolate
  only escaped literals and typed bounds.
- **Export checkpoint** is keyed on content hash (no skip/dup on retention reuse);
  cross-node log fan-out percent-encodes, reports partial failures, dedups on stable
  identity; rollup epoch alignment is correct.
- **Upgrade core**: `verify_binary` requires the embedded release signature on *every*
  path with the sha256 gate first; `release_keys_override` is debug-only; symlink swap is
  atomic (temp + rename); the *council* rolling phase computes quorum from live voters.
- **Runtime core**: per-instance OverlayFS upper isolation is sound (instance-id-keyed,
  drop-guard rollback on failed create); port double-booking is prevented across restart;
  the supervisor doesn't duplicate workloads on restart; SIGTERM→SIGKILL is exit-aware;
  the host-exec allowlist is deny-by-default exact-match with GPU/rootless gating.
- **Pickle core**: digest/reference path-traversal is blocked at entry points, peer bytes
  are re-hashed, GC sole-copy arbitration and signature-chain validation are correct.
- **No production stubs**: no `todo!`/`unimplemented!` in shipping paths; two justified
  `unreachable!`; two intentional `process::exit` (upgrade-revert restart, test hook).

---

## Coverage & method

Every subsystem except Phase 15 was reviewed: sesame, council, mustard,
reconstruction, reporting, cluster, onion, wrapper, firewall, pickle, grill, bun
(agent/api/supervisor/deploy), meat, mayo, ketchup, upgrade, lettuce, smoker,
relish/config. All 16 Critical findings were re-verified by reading the exact code
path on current `main`; Mediums/Optionals are cited to `file:line` and marked [V]/[A]/[?]
by verification depth. Not exhaustively traced: `finalise_rolling_deploy`/
`rollback_rolling_deploy` per-node replica counting, apple.rs beyond command
construction, and the netns/portmap/rootless internals — a deeper pass there could
surface additional strand/duplicate edge cases.

---

## Comparison with the prior review (`2026-07-17-review-codebase-current-state.md`)

I wrote this audit without reading the prior one; the comparison below was done
only afterward. The prior review was conducted on SHA `8ba727f` — **before** the
H1–H7 hardening PRs (#122–#126, #128) landed — so the two reviews are largely
looking at different code, which is why they mostly don't overlap.

**Its findings that current `main` has already fixed** (verified here): its four
security P1s are resolved. SEC-1 (standalone non-loopback empty-token bind) —
`refuse_open_non_loopback_bind` (`bun.rs:264-285`) now refuses it and rejects
unparseable hostnames. SEC-2 (mTLS opt-in) — `relish init` now writes
`require_mtls=true` (#124). SEC-3 (egress fail-open when eBPF absent) — now
fail-closed via `plan_pre_start_egress` (#122); I re-verified this. NET-1 (DNS
unreachable/non-fatal) — reachable and fail-closed (#123). The "first-run path not
executable" P1 — fixed (#126). Its dependency-advisory P1 may still stand (I did
not scan dependencies — see below).

**Where the two reviews are complementary (little overlap):** the prior review is
a *posture* review — fail-open config **defaults**, documentation drift (a large
command-compatibility appendix), maintainability (the `agent.rs`/`api.rs` god
modules), dependency/build shape, and a genuinely valuable eBPF-DNS feasibility
study with a TC proof-of-concept. This review is a *code-logic* review and found a
different, deeper class of issues that the prior one did not surface: the relish
TLS-hostname MITM (C1), scoped-token read leakage (C3), disaster-recovery
empty-state restore and no-fence split-brain (C4/C5), report-identity spoofing
(C6), the gossip leader-hint wedge and SWIM incarnation/relay-ACK bugs (C8/M12/M13),
the Pickle GC delete race (C9), the GitOps parse-error/ls-tree wipes and
signature fail-open (C10/C11/C13), the namespace-quota bypass (C12), the
managed-volume path traversal (C15), the upgrade-GC brick (C16), and the
Smoker fault no-op / no-reverse cluster (M1). The prior review's C2-analogue is its
SEC-1 (a *startup-bind* fail-open); my C2 is the distinct *runtime revoke-to-empty*
path that its still-startup-only guard doesn't cover.

**One direct disagreement worth adjudicating:** the prior review classifies process
workload isolation as a "substantial implementation" with "mount controls" that
"exist" (§3 table, §1.1). My C14 finds the opposite — `grep -r
'unshare|CLONE_NEWNS|pivot_root|pre_exec' src/` returns nothing, and
`ProcessGrill::start` spawns a plain `Command` — so the documented mount-namespace
boundary is absent while the code signals it is enforced. I'm confident in the grep;
if "mount controls" referred to the config *schema* (the `mount_isolation` field
exists) rather than a runtime mechanism, both statements are true but the security
conclusion is mine: the field is honoured by nothing.

**Its findings I did not independently reproduce but consider valid and additive:**
FUNC-4 (workload SPIFFE trust domain hard-coded to `default`), the 12 open
dependency advisories (I ran no `cargo-audit`/`cargo-deny` — treat this as an
un-covered area on my side), GPU device passthrough missing from the OCI spec, the
macOS `--all-features` lint failure, and the maintainability/god-module split. These
belong in the combined backlog. Conversely, the prior review's "no P0 established"
verdict reflects its SHA and its config-posture lens; on current `main`, this review
establishes multiple stop-ship-class defects (the Critical list above), primarily in
code paths (recovery, GC, GitOps, quota, host isolation) rather than in defaults.

**Net:** treat the two as one backlog. The prior review owns docs/maintainability/
deps/DNS-strategy and the (now-fixed) config-default posture; this review owns the
exploitable code-logic defects. The `Phase A` items at the top of the TODO are the
ones that did not exist in, or were not caught by, the prior pass and carry the
highest impact-to-effort ratio.
