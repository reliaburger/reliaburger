# Phase 12b.3 — API authorisation and Brioche (Theme 1)

Theme: `docs/progress.md` §12b.3 "API authorisation and Brioche".
Findings: AUTH1, AUTH2, AUTH3, AUTH4, AUTH5, AUTH7, AUTH8, H4/D8.
Source review: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

Already done (do NOT redo): AUTH6 (browser session auth + `/`+`/ui/*`
lockdown + env masking, Stage 5 #77); the `ROUTE_MATRIX` audit table
(`src/bun/authz.rs`) and the `require_system` internal-route guards
(12b.1). H4 route-matrix coverage test passes already.

Every fix here is enforcement — each test must assert the boundary
**rejects** the bad request, driven through the binary/agent API.

## Ground truth (verified 13 Jul — re-verify line numbers)

- AUTH1: `AuthContext` (`src/sesame/auth.rs:57-69`) carries `role`,
  `scoped_apps`, `scoped_namespaces`; `authorize()` (`auth.rs:245-253`)
  checks role only. **`TokenScope::allows(app, ns)` already exists
  (`src/sesame/types.rs:269-280`) and is never called.**
- AUTH2: `ROUTE_MATRIX` (`authz.rs:89-184`) is documentation; enforcement
  is per-handler. Under-protected: snapshot mutations (`AnyToken`,
  authz.rs:141-144; handlers take no role gate), app rollback
  (`api.rs:3147`, no auth param), upgrade rollback (`api.rs:490,768`, no
  auth param). Batch/build submit+run already enforce Deployer/System.
- AUTH3: bootstrap window (`auth.rs:180-182`) passes everything while the
  token store is empty; listener defaults loopback (`bun.rs:39`, CLI
  `--listen`).
- AUTH4: service token derived by HKDF (`token.rs:142-159`); `__system`
  maps to full Admin via `system_context()` (`auth.rs:124-131`) on every
  route.
- AUTH5: `find_valid_token` (`token.rs:130-140`) linear-scans + runs
  Argon2 synchronously (`token.rs:98-100`) while holding the store read
  lock (`auth.rs:178`). No index.
- AUTH7: dashboard hardcodes cluster_name empty / node_count 1 / nodes []
  (`api.rs:2666-2671`, `2805`); `state.membership` (RwLock<Vec<...>>)
  exists but is unread by `gather_dashboard_data` (`api.rs:2644-2674`).
- AUTH8: `escape_html` (`src/brioche/dashboard.rs:148-153`) escapes
  `& < > "` but not `'`; chart config JSON sits in a single-quoted
  `data-chart-config='...'` attribute (`app_detail.rs:129-132`,
  `node_detail.rs:39-42`).

## Implementation steps (tests first for each)

### 1. AUTH1 — scope enforcement

Call the existing `TokenScope::allows(app, namespace)` after the role
check in every handler that mutates a specific app/namespace: apply, stop,
exec, deploy, rollback (app), snapshot mutate, batch/build submit,
autoscale/secret where scoped. Prefer a small reusable helper /
extractor (e.g. `authorize_scoped(ctx, app, namespace)`) so it can't be
forgotten — the plan calls for "reusable extractors". A scoped principal
whose token doesn't allow the target app/namespace gets 403.
Tests: a Deployer scoped to ns `a` is refused on ns `b` (403) and allowed
on `a`; an unscoped (wildcard) token is allowed everywhere.

### 2. AUTH2 — cover the under-protected mutations

Snapshot mutations (`POST /v1/snapshots/{ns}/{app}`, `/restore`, DELETE) →
require Deployer (+ scope from step 1). App rollback (`POST /v1/rollback/
{app}/{namespace}`) → Deployer + scope. Upgrade rollback endpoints →
Admin, actually enforced in the handler. Update `ROUTE_MATRIX` to match
and keep its coverage test green. Tests: an `AnyToken`/ReadOnly caller is
refused (403) on each; a correctly-roled+scoped caller succeeds.

### 3. AUTH3 — fail-closed bootstrap

While the token store is empty (bootstrap window), refuse to serve
non-loopback requests: either refuse to bind a non-loopback address in the
empty-token state (startup error naming the risk), or reject non-loopback
requests in the middleware until a real token exists. Loopback bootstrap
stays working (dev). Document the operator path (create the first token
over loopback / via the seeded bootstrap). Tests: middleware refuses an
anonymous request with a non-loopback client addr while tokens are empty;
allows loopback; once a token exists, normal auth applies.

### 4. AUTH4 — restrict the `__system` principal

`system_context()` should authorise `__system` only on the System-tagged
internal routes it needs (the `require_system` routes: join issue,
batch/run, batch/report, build/run, build/track, reporting) — NOT on
user-Admin routes (token create/list/revoke, secret rotate/pubkey,
identity sign, chaos/fault). Drive this off the existing `ROUTE_MATRIX`
`RoutePrincipal::System` classification so it stays consistent. Preserve
all node-to-node fan-out (it uses System routes). Tests: `__system` token
accepted on a System route, rejected (403) on `POST /v1/token` and secret
rotate.

### 5. AUTH5 — bounded, off-lock Argon2

Move token verification off the async worker and off the store lock: clone
the candidate hashes out from under the read lock, then verify in
`spawn_blocking` under a concurrency bound (a `Semaphore`, small N). Add a
cheap short-circuit index — e.g. match on a non-secret token id/prefix
before Argon2 so a bad token doesn't hash against every stored token.
Keep the constant-time properties of Argon2's own verify. Tests: a valid
token still authenticates; a burst of N invalid bearer requests does not
hold the store lock / does not run unbounded concurrent Argon2 (assert via
a concurrency counter or that a concurrent legit read isn't starved).

### 6. AUTH7 — real dashboard data

`gather_dashboard_data` reads `state.membership` for node count + node
list and the node's own name/cluster name (from config/state) instead of
hardcoded values; the nodes fragment lists real members with real state.
Tests: with a seeded membership, the dashboard data reflects the member
count and names (snapshot or field assertions).

### 7. AUTH8 — attribute-context escaping

Escape `'` (→ `&#39;`) in `escape_html`, or switch chart attributes to
double-quoted with full JSON/attribute escaping — whichever is the smaller
correct change. Ensure app/label values flowing into `title`/`y_label`/
`endpoint` are escaped for the single-quoted (or double-quoted) attribute
context. Tests: an app or label value `x' onload='alert(1)` renders
escaped (no attribute break); round-trip of a normal value is unchanged.

## Acceptance checklist

- `make ci` green (fmt, clippy `-D warnings`, full default suite); README
  counts from your run (main is 2,431 — measure).
- The gated cluster suite (`RELIABURGER_CLUSTER_TESTS=1`) stays green if
  your service-token change (AUTH4) touches node fan-out — run it and
  quote results.
- `docs/progress.md`: tick the nested `- [x]` sub-items under "API
  authorisation and Brioche"; check the theme box only if ALL its
  sub-items are done (they are, after this).
- Book: chapter 10 (`docs/book/10-locking-it-down.md`) — the enforcement
  story (scopes not just roles; the bootstrap fail-open; why a shared
  Admin service token is a lateral-movement risk; off-lock Argon2); a
  short chapter 11 note if the dashboard-data change belongs there.
  British English, CLAUDE.md style guide, explain new Rust syntax on first
  use.

## Constraints

- **Seam ownership:** `src/sesame/{auth,token,types}.rs`, `src/bun/api.rs`
  (middleware + handler enforcement + dashboard data), `src/bun/authz.rs`
  (ROUTE_MATRIX), `src/brioche/*`, `src/bin/bun.rs` (bootstrap bind
  check). A sibling Theme 2 agent owns `src/sesame/{join,mtls,ca,
  identity*,bootstrap}.rs` and the join issuer in `src/bun/agent.rs` — do
  NOT touch those. If you must touch `sesame/types.rs`, keep it additive
  and minimal (the sibling shouldn't need it, but flag if you both do).
- Do NOT weaken the `__system` access that node fan-out relies on (System
  routes must keep working) — AUTH4 only removes its user-Admin access.
- Error messages lowercase, no trailing full stop; thiserror; no unwrap in
  production code.
