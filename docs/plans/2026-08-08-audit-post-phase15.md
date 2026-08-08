# Post-Phase-15 audit — what's missing, bugs, doc/code drift

Audit run 8 August 2026 against `main` at merge of PR #151 (`34c44e1`). Five
parallel sweeps: the Phase-15 deep review (already captured in
`2026-08-06-plan-phase15-followup.md`) plus four doc-vs-code sweeps
(whitepaper; design docs split in two; manual + READMEs).

Classification for drift: **(a)** doc stale — code is right; **(b)** code
gap/bug — doc describes intended behaviour the code doesn't deliver;
**(c)** ambiguous / needs a product decision.

The single clearest pattern: **the code is more honest than the docs.** Again
and again the implementation fails *closed* (refuses an unsupported operation)
where the docs claim a working feature. That's the right direction, but it
means the whitepaper, most design docs, and parts of the manual currently
describe a system more capable than the one that exists.

---

## 1. What's missing

### Security controls documented as shipped but absent
- **Raft log encryption is unwired.** `src/sesame/raft_encryption.rs` is
  complete and tested (AES-256-GCM), with **zero call sites**;
  `src/council/durable_log.rs` writes plaintext bincode to redb. Cited as the
  mitigation in security-sesame §7.5/§8.3/§8.4 and whitepaper §11.4.
- **Process-workload isolation barely exists.** No `CLONE_NEW*`/`unshare`,
  no seccomp (zero hits in src/), no `burger` unprivileged user, no capability
  model — all documented as shipped (whitepaper §17, agent-bun §8). What's real:
  a deny-by-default binary allowlist, and an honest *refusal* when a workload
  requests mount isolation (`src/bun/supervisor.rs:265-284`).
- **Egress is allow-all by default**, not deny-all (whitepaper §11.3/§20 both
  claim deny-all). `src/sesame/egress.rs:14`, `ebpf/onion_connect.bpf.c:81`.
  Enforcement is additionally Linux + `ebpf` feature + `[ebpf] enabled` (default
  false).
- **Audit logging covers only fault injection.** No source IP anywhere;
  storage is a bounded 1024-event in-memory buffer, not Ketchup; `relish
  history` is deploy-history only (whitepaper §11.5, security-sesame §5.7).
- **TPM sealing / attestation**: enum variant only, never constructed, no TPM
  crate (whitepaper §11.1/§11.4, security-sesame §5.7).
- **`relish ca` family** (`ca status|rotate|revoke`) does not exist; CA
  `generation` never increments — no CA rotation despite whitepaper §11.2 and
  security-sesame §5.8.
- **API tokens without `--ttl-days` never expire** (`src/bun/api.rs:5766`),
  documented default 90 days; no `token rotate`, no expired-token sweep.
- **Per-namespace age keys and per-app JWT audiences** aren't created —
  cluster-wide key and a single hard-coded audience; the `reliaburger.dev/node`
  JWT claim is literally the string `"local"` (`src/bun/agent.rs:6087`). Breaks
  the confused-deputy mitigation in security-sesame §8.7.

### Scheduling / deploy features documented as working
- **Cron/scheduled jobs never fire.** `schedule` is parsed and read by no
  scheduler; no cron dependency; jobs aren't even cluster-scheduled
  (`src/bun/api.rs:2364`). Whitepaper §5.2/§Q8 present them as working; a
  testkit case (`scheduled_job_fires_on_its_schedule`) depends on it.
- **Blue-green deploy is unwired.** `src/meat/blue_green.rs` +
  `orchestrator.rs` have no production caller; the wired agent path never
  branches on `strategy`, so a blue-green config silently runs rolling
  (deployments §5, scheduler-meat).
- **`run_before` dependency ordering is unenforced.** Parsed and lint-checked,
  modelled in the unwired library, consumed nowhere in production (deployments
  §5.4, whitepaper §13).
- **Deploy state is not Raft-persisted / resumable / supersedable** in the
  wired path — every `DeployUpdate`/`DeployComplete` write is `#[cfg(test)]`;
  the reconciler re-drives on a later tick instead (whitepaper §13). The
  deployments.md status note already discloses this.
- **cgroup requests are inert.** `cpu.weight`/`memory.high` are computed
  (`src/grill/cgroup.rs`) but never written; only quota/period + memory limit
  reach the OCI spec. Resource *requests* don't shape enforcement.

### Observability features that exist only as library code
- **Prometheus scraping is unwired** — `src/mayo/scrape.rs` has no caller;
  `scrape_interval_secs` is dead config; per-app `metrics`/`metrics_interval`
  keys don't exist. (whitepaper §15, metrics-mayo §4).
- **Metrics fan-out is dead code** — `fan_out_app_query`/`fan_out_cluster_query`
  have zero callers; the handler reads only local rollup state and says so
  (`src/bun/api.rs:5142`).
- **Rollup retention never enforced** — `rollup_retention_hours` read by
  nothing; `RollupStore::prune` called only from tests. No 1h/90d tier; actual
  retention is a single flat `retention_days` by file mtime.
- **Ingress metrics never emitted** (`ingress_requests_total` etc. — zero
  hits).
- **No PromQL** — the query surface is DataFusion SQL; alerts are fixed
  threshold rules, not expressions (whitepaper §15, metrics-mayo §5).
- **GitOps coordinator failover, sync history, Brioche GitOps view** — all
  library-only or never-written (`SyncState.history` "never written by the
  runner"); whitepaper §14, gitops-lettuce.
- **Ingress: active L7 health probes, retry/failover, `X-Real-IP`/
  `X-Request-ID`, ALPN/HTTP2, and the drain-termination protocol** (RST / WS
  Close 1001 / mid-stream 503) are all absent (ingress-wrapper §4-§6).

### Franchise (multi-cluster)
- **§21 Franchise is entirely unimplemented and actively refused**
  (`src/bun/supervisor.rs:218`). No `relish franchise` command. Presented in
  present tense in the whitepaper.

### User-facing gaps (not contradictions, just missing)
- **Manual has no coverage of `relish wtf`, `relish trace`, `relish bench`**
  (7 chapters: getting-started, deploy, cluster, networking, observability,
  chaos, under-the-hood). The docs/README command table also omits them plus
  ~15 other real commands.

---

## 2. Bugs (code defects, most from the Phase-15 deep review; a few new)

### From the Phase-15 review — tracked in 2026-08-06-plan-phase15-followup.md
Critical/major, still open (Phase 15 shipped these; the follow-up plan fixes
them):
- **Cluster lease-cleanup snapshot race** (`src/testkit/lease.rs:610`) — a
  resource attached between the state snapshot and `TestLeaseBeginCleanup`
  leaks forever, and `FinishCleanup` then destroys the ownership record.
- **`cluster_apply` treats Raft `Refused` as committed**
  (`src/bun/api.rs:2344`) — a refused leased write streams "committed"; the
  case dies later as `Unknown(TimedOut)`.
- **Clear-all fault authorisation gaps** — `DELETE /v1/fault?service=` with an
  empty string matches every node fault, and blanket `DELETE /v1/fault` clears
  `NodePressure`, both under the Deployer workload grant rather than Admin.
- **NodePressure TTL-expiry wedge** (`src/bun/agent.rs:3958`) — a failed
  cgroup removal re-inserts the handle but drops the rule, permanently
  refusing future pressure faults until restart.
- **Node-kill quorum TOCTOU** (`src/bun/api.rs:3968`) — evidence is SWIM-only,
  so two kills inside the suspicion window can break quorum.
- **`relish wtf` can never exit 0** — two collectors hard-wired `Degraded` →
  unknown → exit 2.
- **Benchmark probes ignore exec exit status** — a broken data plane yields a
  great-looking throughput/latency number.
- **Registry regressions that break routable (non-loopback) clusters**: build
  pipeline hardwired to `localhost`; buildah push presents no creds; the
  self-upgrade binary fetch sends no bearer → 401. (Phase B of the follow-up
  plan.)

### New, found in this audit
- **Ingress default-certificate SNI resolver** mints cluster-Ingress-CA leaves
  for *any* SNI hostname, no routing-table allowlist, unbounded cache keyed by
  attacker-controlled SNI (`src/wrapper/tls.rs:226-255`). Security-relevant.
- **Build namespace scope check is a no-op for unprefixed names**
  (`src/pickle/build.rs:283`, `src/config/build.rs:111` — `validate_build_namespace`
  does no namespace comparison); any Deployer token can push to any repo.
- **Firewall perimeter port range hardcoded 30000-31000**
  (`src/firewall/rules.rs:45`) disagrees with the 10000-60000 network default
  (`src/config/node.rs:509`) — the perimeter can drop legitimately-allocated
  host ports.
- **`live_council_voter()` gate is wired to an always-false field** — `is_council`
  is never set in production, so the guard for `relish council recover` keys on
  a field that's always false (`src/bun/agent.rs:2391`, `src/mustard/membership.rs`).
- **No `deny_unknown_fields` on any config struct** — every mistyped or
  hallucinated key in the docs (and there are dozens) parses silently and does
  nothing. This is why so many doc examples "work" without producing the
  documented behaviour. A single `#[serde(deny_unknown_fields)]` sweep would
  turn a whole class of silent misconfig into loud errors.
- **`auto_rollback` default mismatch + ignored** — default is `true` in code
  (`src/meat/deploy_types.rs:57`), `false` in docs; the wired rolling path
  rolls back unconditionally and never reads the flag, so
  `auto_rollback = false` gets no halt behaviour.
- **Over-limit backends silently dropped** — `TooManyBackends` discarded by
  every caller via `let _ =` (`src/onion/service_map.rs:134`); documented as a
  logged warning.
- **`upgrade/orchestrator.rs` module comment describes the opposite flow**
  (leadership-transfer) from what ships; stale code comment, not a runtime bug.

---

## 3. Doc/code inconsistencies (highest-value; full lists per doc below)

The Phase-15-reworked sections are accurate (test-harness.md wholly clean; the
Phase-15 parts of chaos-smoker and cli-relish match). Drift concentrates in
pre-Phase-15 sections never revisited, and in three docs that carry a "Design"
header but read as current (metrics-mayo, logs-ketchup, ui-brioche).

### Recurring stale patterns worth a single mechanical sweep
- **`/api/v1` prefix** in many docs — the real API is `/v1/*` (no `/api`).
- **Port 9443 as the API/UI port** — that's gossip; the API/UI is `9117`.
- **Dependency tables list crates not in `Cargo.toml`** — git2,
  sequoia-openpgp, keyring, indicatif, chrono, promql-parser, regex, memmap2,
  tonic/gRPC, dashmap, arc-swap, sled, and more. gRPC is claimed in three docs;
  nothing uses it (transports are bincode/TCP and HTTP).
- **eBPF denials are `EPERM`, not `ECONNREFUSED`/`ENETUNREACH`** — ~12
  occurrences across whitepaper/chaos-smoker/onion; the docs even contradict
  themselves (onion §5.1.4 is right, §5.1.2 wrong).
- **`allow_from` syntax** documented three different ways
  (`app.api@production`, `app.api`); code accepts `namespace/app` or bare `app`.

### Most consequential single-doc rewrites
- **Registry push durability (whitepaper §12/§Q7, registry-pickle §1)** — the
  most consequential falsehood in the docs. Push is **async** (commits locally,
  returns `oci-replication: pending`), replication runs in a 60s leader-only
  heal loop, and `redundancy = 2` counts the pusher, so "N=2 peers" is really
  **1 peer**. The sentence "a successful push guarantees the image survives any
  single node failure" is false. `push_sync` doesn't exist. Also: pull-through
  credentials are startup-env, not "age-encrypted in Raft, rotatable without
  restart".
- **Worker-node trust model (security-sesame §3.2/§8.2)** — "worker nodes never
  hold CA/age/OIDC keys" is false; every clustered node loads the master key
  and locally unwraps age + Workload-CA keys. The threat model needs a rewrite
  or the code needs a real key split.
- **§16 CLI table (whitepaper) / cli-relish §2-§9** — ~two dozen commands are
  documented that don't exist (`scale`, `events`, `plan`, `firewall`,
  `identity`, `ca *`, `pickle gc`, `completions`, `login`, `volume *` [it's
  `snapshot`], `token rotate`, `exec --debug`, `import --kubeconfig`, …) or
  have a different shape (`deploy` takes a path not an image; `diff` is
  file-vs-file not cluster-drift; `top` prints no CPU/memory; `status`/`lint`
  exit codes differ). Config-file/env/keychain story is wrong (no config file,
  env is `RELIABURGER_*`, no keyring).
- **metrics-mayo + logs-ketchup wholesale** — the implementation is Parquet +
  DataFusion SQL, not a custom TSDB / binary log format; most storage, query,
  retention, scraping, metric-name, alert-expression, and config claims are
  contradicted. Notable behaviour gaps: stderr is never distinguished from
  stdout (every line tagged Stdout); `--grep` is substring not regex; log
  follow is local-node only; `--since` rejects RFC3339.
- **gossip-mustard topology** — ports are 9443/44/45 not 7946/47; join is a UDP
  ping with no TCP full-state exchange; there's no LEAVING/drain state; the
  reporting tree is a flat star to the leader, not two-level; council
  steady-state is 7 not 5; resource summaries are never actually gossiped so
  council eligibility filters never reject anyone.

### Manual + READMEs (user-facing — copy-paste correctness matters most)
- `relish top` promises live CPU/memory it deliberately doesn't print
  (docs/README:396, manual 01/04).
- `dev create` documented as "rootless runc"; it runs bun as **root via sudo**
  (docs/README:426; `src/relish/dev.rs`; the module's own doc comment is also
  stale).
- Manual `join-token create` snippet omits the **required** `--node-id` — the
  printed command fails to parse (manual 02:29).
- `apply --dry-run` sample output shows a format that no longer exists
  (docs/README:543).
- Top-level README says "Twelve subsystems", omitting Lettuce (13).
- "six starter chapters" → seven embedded.

Full per-doc finding lists (with line numbers and file:line citations) are in
the sweep transcripts; the counts across the seven component design docs alone:
roughly **95 (a) doc-stale, 55 (b) code-gap, 25 (c) ambiguous**.

---

## Suggested triage

1. **Cheap, high-value now:** add `#[serde(deny_unknown_fields)]` across config
   (turns silent misconfig into errors and stops new doc drift); mechanical doc
   sweeps for `/api/v1`→`/v1`, 9443→9117, EPERM, and the dependency tables; fix
   the manual's copy-paste-broken snippets (`join-token --node-id`, `dev
   create` rootless claim, `top` CPU/memory promise).
2. **Correctness follow-ups:** the Phase-15 review items already staged in
   `2026-08-06-plan-phase15-followup.md` (Phases B-H), plus the new bugs above
   (ingress SNI resolver, build-namespace no-op scope, firewall port range,
   `auto_rollback` default+wiring).
3. **Honesty rewrites (docs describe unbuilt features as shipped):** registry
   push durability, worker-node trust model, CLI command tables, mayo/ketchup
   storage+query, Franchise, cron jobs, blue-green, `run_before`. Each should
   either gain a "planned/not-yet-wired" marker or have the code wired.
4. **Time-boxed:** advisory exceptions in `.cargo/audit.toml` expire
   **18 August 2026** — `make audit` fails closed after that (follow-up plan G4).
