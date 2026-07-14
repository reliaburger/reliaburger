# Phase 12b.4 — Ingress transport and draining (Theme I, Wave B)

Theme: `docs/progress.md` §12b.4 "Ingress transport and draining".
Findings: ING1, ING2, ING3, ING4, ING5, D7, D10.
**Final theme of the 12b.4 tier — checking its box completes "Finish the
data plane".**
Source: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

Already done (do NOT redo): ING4's **HTTP** drain — #97 (T5) fully wired
`SharedDrains`/`DrainGuard` through the rolling-deploy `retire_with_drain`
path; only the **WebSocket** drain remains. The namespaced routing table
(`(namespace, app_name)` keys) landed with Theme S #102 — build on it.

Runs solo (Wave A merged; no live sibling). Each fix asserts the boundary
**rejects/bounds/drains** the bad case.

## Ground truth (verified 14 Jul on main @ #104)

- ING1/D7: `IngressSpec.tls` (`src/config/app.rs:167`) parsed but never
  applied; HTTP + HTTPS share one router (`src/wrapper/proxy.rs`) so
  TLS-configured routes serve plaintext. Only self-signed/disk certs
  (`src/wrapper/tls.rs`); no ACME/cluster-CA.
- ING2: TLS handshake spawned with no semaphore/deadline (`proxy.rs`
  serve_tls loop); `active_connections` (`proxy.rs:264-272`) is an HTTP
  request-level counter; the WebSocket permit is released at the 101 while
  the splice is live (`proxy.rs` do_proxy exit, `src/wrapper/websocket.rs`).
- ING3: request body buffered to 10 MiB (`proxy.rs:376`
  `to_bytes(body, 10*1024*1024)`); backend response collected unboundedly
  (`proxy.rs:408` `resp.bytes()`), so SSE/gRPC/large downloads buffer
  whole.
- ING4/D10: HTTP drain done (#97). WS counters `increment_websocket`/
  `decrement_websocket` (`src/wrapper/draining.rs:171-180`) exist but are
  never called; `check_completions` only checks `active_connections`.
- ING5: X-Forwarded-* forwarded not replaced (`proxy.rs` header copy loop,
  ~368); `/api` matches `/apievil` (`routing.rs:131` `starts_with`); IPv6
  Host parse broken (`routing.rs:128` naive `split(':')`); rate buckets
  per-IP shared across routes + zero/overflow rates unvalidated
  (`proxy.rs` rate limiter, `src/wrapper/rate_limit.rs`).

## Implementation steps (tests first for each)

### 1. Per-route TLS mode + HTTP→HTTPS redirect (ING1)

Carry `IngressSpec.tls` into the `PathRoute` so the router knows a route's
TLS mode. A route marked TLS served over plain HTTP gets a 301/302 to
HTTPS (except ACME challenge paths). For the certificate: implement the
**cluster-CA** path (issue the ingress cert from Sesame's Ingress CA, which
exists) OR honour the **documented explicit-certificate contract** — do
NOT build full ACME unless it falls out cheaply; if a route requests an
unsupported TLS mode (e.g. `acme` when not implemented), **reject the
config up front** rather than silently serving plaintext. Tests: a
TLS-marked route is not served plaintext (redirect or refuse); an
unsupported TLS mode is a config error; a cluster-CA cert is issued/served.

### 2. Connection permits through the full lifetime (ING2)

- TLS handshake: bound with a semaphore (max concurrent handshakes) + a
  handshake deadline (`tokio::time::timeout`) so slow-handshake floods
  can't exhaust tasks.
- WebSocket: hold the connection permit through the splice lifetime — do
  not release at the 101. Transfer the permit/guard to the splice task so
  it's released when the WS actually closes.
Tests: N stalled TLS handshakes don't exhaust capacity / are dropped on
the deadline; a WS connection holds its permit until the splice ends (not
at the 101).

### 3. Streamed bodies with limits + backpressure (ING3)

Stream request and response bodies instead of buffering whole: use
`Body`/`hyper` streaming with `tokio::io::copy` (or `copy_bidirectional`
for upgrades) so SSE/gRPC/large transfers flow with backpressure. Keep a
**configurable** max-body bound (not a silent 10 MiB) that applies where a
limit is wanted; unbounded streaming for the streaming content types.
Tests: a large streamed response is not fully buffered (assert memory /
that bytes flow incrementally); a request over the configured cap is
rejected; an SSE/chunked response streams through.

### 4. WebSocket drain (ING4/D10)

Wire `increment_websocket` at the WS upgrade (101) and `decrement_websocket`
when the splice ends; make `check_completions`/`wait_drained` wait for BOTH
`active_connections` AND `websocket_connections` to reach zero. So a
rolling deploy honours `drain_timeout` for in-flight WebSocket traffic, not
just HTTP. Tests: a rolling retire waits for an active WS splice to finish
(or times out) before killing the old instance — extend the #97
drain tests with a WS case.

### 5. Trusted forwarded headers + correct route/rate keys (ING5)

- X-Forwarded-For/Proto: **replace** (don't forward) for untrusted peers —
  set them to the proxy's view (the real client IP / scheme), stripping
  client-supplied values, so a backend can't be lied to.
- Route matching: boundary-correct — `/api` must NOT match `/apievil`
  (`path == prefix || path.starts_with(&format!("{prefix}/"))`), with
  deterministic ordering for equal-length routes (sort, don't rely on
  HashMap order).
- Rate limiting: key per (route, client) not shared per-IP-across-routes;
  validate `rps > 0 && burst > 0` at rebuild (reject zero/overflow — the
  divide-by-zero risk).
- IPv6 Host: bracket-aware parse (`[::1]:8080` → `[::1]`), not naive
  `split(':')`.
Tests: `/apievil` does not match the `/api` route; X-Forwarded from an
untrusted client is replaced; a zero/overflow rate is rejected at config
time; an IPv6 Host routes correctly; per-route rate isolation holds.

## Acceptance checklist

- `make ci` green (fmt, clippy `-D warnings`, full default suite); README
  counts from your run (main is 2,570 — measure).
- Gated: any real-socket TLS/WS behaviour on the Lima rig if it fits;
  quote results or state what's only unit-verified. The drain-with-WS test
  can run in-process.
- `docs/progress.md`: nested `- [x]` items under "Ingress transport and
  draining"; **check the theme box, and note the 12b.4 tier is complete**
  (all three themes done).
- Book: chapter 3 (`docs/book/03-talking-to-each-other.md`) Wrapper/ingress
  section — per-route TLS, connection-lifetime permits, streaming vs
  buffering, WS draining, trusted-forwarded-headers + route-boundary
  correctness. British English, CLAUDE.md style guide, explain new Rust
  syntax on first use.

## Constraints

- **Seam ownership:** `src/wrapper/*` (proxy, routing, websocket, draining,
  tls, rate_limit), `src/config/app.rs` (`IngressSpec.tls` plumbing), and
  the rolling-deploy/drain sites in `src/bun/agent.rs` (~finalise_rolling_
  deploy / retire_with_drain — the WS-drain wait). Theme S #102 left a
  2-line `#[cfg(test)]` block in `proxy.rs` on the namespaced routing
  signatures — it's yours now, adjust freely. All other 12b.4 themes are
  merged; no live sibling.
- Reuse the Sesame Ingress CA for the cluster-CA cert path; do not invent
  a parallel TLS scheme, and do not build full ACME speculatively.
- `IngressSpec.tls` changes stay serde-compatible with existing configs.
- Error messages lowercase, no trailing full stop; thiserror; no unwrap in
  production code.
