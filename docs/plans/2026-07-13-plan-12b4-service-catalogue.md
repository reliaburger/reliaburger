# Phase 12b.4 — Global namespaced service catalogue and DNS (Theme S)

Theme: `docs/progress.md` §12b.4 "Global namespaced service catalogue and
DNS". Findings: D3/codex-M1 (P0), D5, D6, D7-routes, old M5/M6.
Source: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md),
[2026-07-09-review-design-discrepancies.md](2026-07-09-review-design-discrepancies.md).

**Scope = FULL global replicated catalogue (user decision).** Namespace the
service identity to fix the collision AND replicate healthy endpoints
cluster-wide so any node resolves any namespace's services. **Two PRs.**

Already correct (do NOT redo): eBPF firewall namespace isolation — NET5
(#86) put `namespace_id` in `BackendValue`, so cross-namespace *isolation*
is enforced. The gaps are VIP/route *collision* and bare-name DNS/routes.

## Ground truth (verified 13 Jul on main @ #101)

- `src/onion/service_map.rs:21` — `entries: HashMap<String, ServiceEntry>`
  keyed by BARE app name; `register_app` (:37), `resolve` (:159),
  `add_backend` (:68), ~12 methods; `default/api` and `payments/api`
  collide. VIP derived from app name only (`src/onion/vip.rs`, SipHash into
  `127.128.0.0/16`, no collision detect, no release on stop).
- eBPF `BackendKey` = `(vip, port)` (`src/onion/types.rs:67`) — no
  namespace; `BackendValue` already carries `namespace_id`/`app_id`.
  `sync_from_service_map` (`src/onion/ebpf/maps.rs:63`). Changing the key
  struct forces an eBPF recompile.
- `src/bun/agent.rs` — ~64 service-map call sites across deploy/health/stop
  (register_app ~3587, add_backend ~3391/3602, set_backend_health
  ~4361/4371, unregister ~3539, remove_backend_ebpf, rebuild_routing_table
  ~4834).
- DNS (`src/onion/dns.rs:75-177`) — resolves BARE app name, UDP-only, binds
  `127.0.0.53:53`, no TCP, no source ACL, non-fatal on bind failure.
- Wrapper `src/wrapper/routing.rs:69-108` — routes keyed host+path;
  `ingress_configs` keyed by app name → cross-namespace collision.
- No non-eBPF VIP fallback (`agent.rs` gates egress on eBPF, not discovery).
- Council: `DesiredState` (`src/council/types.rs:271`) has NO service/
  endpoint catalogue field today (PR 2 adds one).

## PR 1 — Namespaced local service identity (`12b4/service-catalogue`)

The `ServiceId{namespace,name}` refactor + VIP + DNS + routes + eBPF key.

1. **ServiceId key.** Introduce `ServiceId{namespace,name}` (align with T5's
   `InstanceIdentity` — `src/grill/mod.rs`). Re-key the service map
   (`HashMap<ServiceId, ServiceEntry>`); update all ~12 methods + ~64
   agent.rs call sites. A newtype with a canonical string form keeps grep
   reliable.
2. **VIP from (namespace,name).** Derive the VIP from the namespaced
   identity so two namespaces get distinct VIPs. Add collision detection
   (a hash collision must be detected + resolved, not silently shared) and
   VIP release on `unregister`/stop (today VIPs linger). Keep the
   `127.128.0.0/16` range; document exhaustion behaviour.
3. **DNS namespace-qualified.** Resolve `<app>.<namespace>.internal` (and
   decide same-namespace bare-name convenience: a query from a container
   in namespace `ns` for `<app>.internal` resolves within `ns` — the
   caller's namespace comes from the cgroup→namespace mapping the firewall
   already has). Add DNS **TCP** listener (large answers) and a **source
   ACL** (only container-reachable clients). Bind on a container-reachable
   address; **fail the deploy/startup closed** if the responder is
   required but unavailable (today non-fatal).
4. **Wrapper routes namespaced.** Key `ingress_configs`/routes by
   `(namespace, host, path)` so same-named apps in different namespaces
   route independently. (The ingress theme, Wave B, builds on this — do
   NOT do its TLS/draining/matching work here, only the namespace key.)
5. **eBPF backend key.** Namespace the VIP (step 2) so the existing
   `(vip, port)` key stays collision-free WITHOUT a struct change if
   possible — prefer that over growing `BackendKey` (which forces a
   recompile). If the key genuinely must carry namespace, update the
   `#[repr(C)]` struct + the C program and note the recompile.
6. **Non-eBPF fallback = reject up front.** When service discovery needs
   eBPF and it isn't available (load failed / unsupported kernel),
   fail-fast at config-validation/startup with a clear error, rather than
   deploying an app whose VIP silently never routes.
7. **D6 doc-drift.** Correct the whitepaper/design prose that claims
   in-kernel eBPF DNS — the implementation is a userspace responder
   (already documented in design-onion.md; fix the whitepaper §10).

Tests (default + Lima-gated): two same-named apps in different namespaces
get distinct VIPs and resolve independently (the D3 regression); VIP
released on stop; DNS resolves namespace-qualified + refuses a
non-container source (ACL); startup fails closed when the responder can't
bind; a no-eBPF config is rejected up front. Lima (`RELIABURGER_CLUSTER_
TESTS`/`relish dev test`): eBPF backend map routes the namespaced VIP.

**Report + STOP after PR 1** for the orchestrator to merge, then continue
with PR 2.

## PR 2 — Replicated global endpoint catalogue

Make healthy endpoints cluster-wide so any node resolves any namespace's
services (not just node-local).

1. **Raft/reporting endpoint catalogue.** Add a `service_catalog` (or
   `endpoint_catalog`) field to `DesiredState` (`council/types.rs`) —
   **append-only, serde-default, self-describing JSON; a pre-theme
   snapshot loads cleanly (fixture test)** — plus the RaftRequest
   variant(s) + `state_machine.rs` apply. Healthy backends are published
   (leader-side, via the reporting path from 12b.2 #89 or a Raft write —
   pick the one that fits the existing endpoint flow) so every node's DNS/
   routing can resolve services on other nodes.
2. **Cross-node resolve.** DNS + Wrapper resolve from the replicated
   catalogue (namespaced), so a container on node A reaches a service
   whose backends live on node B.
3. **Collision/exhaustion/release at cluster scope.** VIP allocation is
   cluster-wide deterministic + collision-checked across the replicated
   catalogue; release propagates.

Tests (gated, `RELIABURGER_CLUSTER_TESTS=1`): a service deployed on node B
resolves + routes from node A; the replicated catalogue survives a leader
change (reuse the failover harness); a namespaced collision is impossible
cluster-wide.

## Acceptance (whole theme)

- Each PR: failing-first tests, `make ci` green (fmt, clippy `-D
  warnings`, full suite); README counts from your run (main is 2,499 —
  measure). Gated eBPF/DNS on Lima where the kernel matters; cluster
  suite for cross-node resolve.
- `docs/progress.md`: nested `- [x]` per PR under "Global namespaced
  service catalogue and DNS"; **check the theme box on PR 2.**
- Book: chapter 3 (`docs/book/03-talking-to-each-other.md`) — the
  namespaced-service-identity + replicated-catalogue story (why one global
  string key was a collision bug; namespace-qualified DNS; the userspace-
  DNS reality vs the old in-kernel claim). British English, CLAUDE.md
  style guide, explain new Rust syntax on first use.

## Constraints

- **Seam ownership:** `src/onion/*`, `src/wrapper/routing.rs` (namespace
  key ONLY — not proxy/tls/draining), the service-map call sites in
  `src/bun/agent.rs`, config validation, and (PR 2) `src/council/
  {types,state_machine}.rs` for the endpoint catalogue field. A sibling
  Theme P (Pickle) agent runs concurrently and also touches `council/
  {types,state_machine}.rs` — it changes the *manifest_catalog READ path*
  and does not add state; you ADD a distinct `service/endpoint_catalog`
  field. Keep your council changes additive and separate; do NOT touch
  `manifest_catalog` or `pickle/*`. Do NOT touch `wrapper/{proxy,tls,
  websocket,draining,rate_limit}.rs` (Wave B ingress).
- Align `ServiceId` with T5's `InstanceIdentity` naming/format.
- Error messages lowercase, no trailing full stop; thiserror; no unwrap in
  production code; `#[repr(C)]` + padding for any eBPF struct change with
  `// SAFETY:` on casts.
