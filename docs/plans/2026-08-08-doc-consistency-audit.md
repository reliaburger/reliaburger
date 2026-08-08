# Doc-vs-code consistency audit — 8 August 2026

Audited on main after PR #151 merged (34c44e1). Scope: docs/whitepaper.md, all 14
docs/design/*.md, docs/manual/*.md, docs/README.md, top-level README.md, each
verified claim-by-claim against src/. Classification: **(a)** doc stale — code
is right or deliberately different; **(b)** code gap/bug — doc describes
intended behaviour the code doesn't deliver; **(c)** ambiguous — needs a
decision. Companion: docs/plans/2026-08-06-plan-phase15-followup.md (bugs and
missing work from the pre-merge review; still current except Phase A, which is
done).

Rough totals: ~150 (a) doc-stale, ~70 (b) code gaps, ~35 (c) decisions needed.

## Cross-cutting patterns (fix once, sweep everywhere)

- **No `deny_unknown_fields` anywhere in src/config/** — every stale doc key
  (dozens below) parses silently and does nothing. This converts doc drift into
  silent misconfiguration. Single highest-leverage code change in this audit.
- **`/api/v1` prefix** appears across ui-brioche, gitops-lettuce, metrics docs —
  the real API is `/v1/*`, UI fragments are `/ui/*`.
- **Port 9443 as "the API"** in cli-relish/ui-brioche — API is 9117; 9443 is gossip.
- **gRPC claimed in 4+ docs** (reporting tree, pickle replication, containerd) —
  actual: bincode/TCP reporting, HTTP OCI replication, no tonic in Cargo.toml.
- **Dependency tables list phantom crates** in 7 docs: git2, sequoia-openpgp,
  keyring, indicatif, chrono, promql-parser, regex, memmap2, tonic, dashmap,
  arc-swap, cron, ed25519-dalek, libbpf-rs (code ships aya), etc.
- **`allow_from` syntax documented three different ways, all wrong** — code
  accepts `"namespace/app"` or bare `"app"` (src/sesame/firewall.rs:90-96).
- **Errno for denied connections**: docs say ECONNREFUSED/ENETUNREACH; cgroup
  connect4 denials surface as **EPERM** (tests/ebpf.rs:360-365). ~14 occurrences
  across whitepaper, chaos-smoker, discovery-onion; some code comments repeat it.
- **Deferred-markers that are themselves stale** (feature shipped): wrapper
  WebSocket §5.6, sesame `secret rotate` §5.5, onion IPv6/sendmsg hooks §13,
  meat image-locality/stability scores ("placeholder" — implemented).

## Whitepaper (docs/whitepaper.md)

Critical (a) unless noted:
- **WP:595/940 egress "deny-all by default" — it is allow-all by default**
  (src/sesame/egress.rs:14-15, ebpf/onion_connect.bpf.c:81-82); enforcement is
  Linux+ebpf-feature+`[ebpf] enabled=false` default.
- **WP:616/621/1021 Pickle push "synchronous, N=2 peers, survives node failure
  on success"** — push returns immediately with `oci-replication: pending`
  (src/pickle/api.rs:1013-1026); replication is a leader 60s heal loop
  (src/bin/bun.rs:2215-2265); `redundancy` default 2 counts the pusher (1 peer);
  `push_sync` key doesn't exist. The durability sentence is the most
  consequential falsehood in the doc.
- **WP:829-872 §21 Franchise presented in present tense** — unimplemented;
  `allow_franchise` is refused at runtime (src/bun/supervisor.rs:218-222).
- **WP:755-757/1017 process-workload isolation** — no namespaces/seccomp/
  `burger` user/host-exec check; mount isolation refused not implemented
  (src/bun/supervisor.rs:265-283); only binary allowlist + Lettuce signed gate real.
- **WP:892-916 §16 CLI table**: `plan`, `events`, `scale`, `firewall`, `drain`,
  `ca status|rotate|revoke`, `volume snapshot` don't exist; `deploy`, `exec`,
  `diff`, `top`, `import`, `export` have different shapes/behaviour.
- **WP:694 §15 Mayo**: 3-tier retention false (raw 7d local + 1min/24h council;
  no 1h/90d tier); Prometheus scraping unwired (src/mayo/scrape.rs no callers);
  "PromQL subset" is DataFusion SQL; no GPU metrics. Five default alerts confirmed.
- **WP:606-608 §11.5 audit logging**: only fault endpoints audit; no source IP;
  in-memory 1024-event ring, not Ketchup; `relish history` is deploy history.
- **WP:641-643 §13**: Raft deploy persistence/resume write-sites are
  `#[cfg(test)]`-only (reconciler re-drive approximates it — (c)); supersede
  absent; `run_before` unenforced in production (lint warning only).
- **WP:250/1025 scheduled (cron) jobs never execute** — `schedule` parsed, no
  cron impl, jobs not cluster-scheduled (src/bun/api.rs:2364-2369).

Significant: WP:206 `tls` "defaults to auto" — `auto` is rejected, unset =
plain HTTP; WP:574 `relish ca` family absent (PKI hierarchy itself confirmed);
WP:505 council disk step-down never fires with defaults (no statvfs, no 1GB
constant, thresholds default unlimited); WP:555 no implicit DNS carve-out in
egress allowlists (**(c)** likely code bug — app with egress block can't resolve
external DNS unless nameserver listed); WP:604 TPM sealing absent, CRL is
Raft-replicated not reporting-tree (type doc says "gossip" — third answer);
WP:616 `docker push` — Bearer-only, no WWW-Authenticate challenge so
`docker login` can't negotiate ((c) code gap); WP:624 `build_push_to` absent;
build scope check skips unprefixed names, `validate_build_namespace` is a no-op
despite its doc comment (**(b)**); §5.6 Permissions schema-only (nothing reads
`actions`); §14 GitOps: `poll_interval` string silently ignored (real:
`poll_interval_secs`), webhook-TLS unenforced (HMAC is the auth), unsigned-commit
alert absent, no Brioche GitOps view, `SyncState.history` never written,
coordinator informational (leader drives); §7 diagram implies Onion/DNS/Wrapper
always-on — all default off; read replicas don't exist; WP:1037 kernel check
only runs with eBPF enabled; WP:366/404/410 bootstrap snippets fail as printed
(`--node-id` required; `--agent` flag is `--endpoint`; remote plaintext http
rejected; chaos example missing `--acknowledge`); WP:292 volume inline-table
syntax deploys with **no volume, no error**; WP:295 wtf volume/snapshot warning
absent; WP:637 `max_unavailable` default is 0 not 1.

Minor: §18 fault-cpu throttles quota rather than consuming CPU; group name
`secrets-config` not "secrets"; bench encodes no §2 numeric targets (fixed 10%
regression check); HTTPS-on-9117 only with mTLS configured; no systemd unit
ships; firewall perimeter hardcodes port range 30000-31000 vs 10000-60000
config default (**(b)** code inconsistency, src/firewall/rules.rs:45-51 vs
src/config/node.rs:509-527); nftables chain policy accept-with-drops, not
default-deny; quota rejection surfaces on leader stderr, not to the user;
upgrade orchestrator module comment describes the opposite flow ((b)-comment).

## Design docs A (bun, mustard, meat, onion, wrapper, sesame, pickle)

### registry-pickle.md
(a): sync-replication model + `push_sync` (see whitepaper); redund