# Phase 12b.3 — Image trust policy (Theme 3, Wave B)

Theme: `docs/progress.md` §12b.3 "Image trust policy".
Findings: IMG2, IMG3, the OIDC half of PKI10, and the persistent
build-signing key follow-up (T7/#90). **This is the FINAL theme of the
12b.3 tier — checking its box completes "Secure every boundary".**
Source review: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

Already done (do NOT redo): IMG1 (canonical repo policy + digest-pinning,
#84); PKI6/PKI7 (exact validity, server-side SANs, per-instance tmpfs
identity, #87); the constant-time join-token compare + CSR join landed in
Theme 2 (#100). The worst T7 build-signing bug (an *external untrusted*
ephemeral key) is already fixed — signing is workload-CA keyless; only the
per-build-ephemeral vs persistent-identity issue remains.

Runs solo in Wave B after #99/#100 merged, so the `bun/agent.rs` join
region (Theme 2) is settled. Every fix is enforcement — each test asserts
the boundary **rejects** the bad case.

## Ground truth (verified 13 Jul on main @ #100 — re-verify)

- IMG2: `trust_policy` is per-node config
  (`src/config/node.rs` `TrustPolicySection`, ~566). The deploy-time check
  lives in `src/bun/agent.rs` (~1110-1113 field; the verify call is where
  `require_signatures` is consulted) and `src/meat/scheduler.rs::
  verify_image_signature`. The skip: when there's no local council handle
  (non-voter workers, standalone) the check returns "allowed" instead of
  verifying — a fail-OPEN. Trust state is not distributed
  (`DesiredState` has no trust field).
- IMG3: `verify_keyless` (`src/pickle/signing.rs:151`) checks leaf→first
  and last→root signatures + CRL-on-all (Stage 5 L17), but NOT every
  intermediate→intermediate link, and omits validity, EKU, issuer and
  SPIFFE/OIDC identity binding. **Reuse `src/sesame/cert.rs`:
  `validate_chain` (:128, signatures+validity over a chain),
  `check_validity_at` (:104), `check_crl` (:168).**
- PKI10 (OIDC): `verify_jwt` (`src/sesame/oidc.rs:108`) checks signature +
  `exp` only; omits issuer, audience, algorithm/kid, and `iat` bounds. No
  production caller today — hardening a not-yet-wired path (say so).
- Build signing: `sign_pushed_image` (`src/bun/build_runner.rs:664`) mints
  a per-build EPHEMERAL workload CSR (`create_workload_csr`, :677); the
  `require_signatures` build-failure gate already exists (:885-893).

## Implementation steps (tests first for each)

### 1. IMG2 — fail closed on workers/standalone

When `require_signatures` is set and the node cannot authoritatively
verify the image signature, REFUSE the deploy — never skip. Concretely:
the verify path must not return "allowed" just because there's no council
handle. Two parts:
- The trust decision resolves on every node: the policy
  (`require_signatures` + trusted roots) is available from per-node config
  already; the *verification material* (the image's signature + the
  cluster root CA to check it against) must be reachable by a worker. If a
  worker genuinely cannot obtain what it needs to verify, that is a
  fail-CLOSED refusal, not a skip.
- If distributing an authoritative trust projection is warranted (so
  workers verify against the cluster's roots rather than only local
  config), the minimal correct move is: keep config authoritative for
  the *policy*, ensure the *root CA / trust anchors* are on every node
  (they are, via the identity bundle / bootstrap), and fail closed when a
  required verification can't complete.

Tests: a node WITHOUT a council handle, `require_signatures = true`, an
unsigned (or unverifiable) image → deploy refused (not allowed); a signed
image with the trust anchor present → allowed. A standalone node behaves
the same. Assert the refusal reason names the missing verification.

### 2. IMG3 — complete keyless chain validation

In `verify_keyless`, replace the partial leaf→first / last→root check
with full chain validation:
- Every adjacent link (leaf→i, i→i+1, …, last→root) verified — reuse
  `cert::validate_chain` where the chain shape matches, or extend it /
  loop `verify_signature` over all adjacent pairs.
- Validity (`check_validity_at`) on every cert in the chain at "now"
  (inject the clock for tests, following #87's pattern).
- EKU: the leaf must carry code-signing EKU (add an EKU-check helper in
  `cert.rs` if none exists — parse the extension via the existing x509
  parser).
- Issuer binding: each cert's issuer DN matches its parent's subject
  (path-building, not just signature).
- Identity binding: the leaf's SPIFFE/OIDC identity SAN matches the
  expected signer identity for the image (tie to the workload identity
  the build signer uses — see step 4).
- Keep the existing CRL check.

Tests (default suite, pure): a chain with a broken intermediate link is
rejected; an expired cert in the chain is rejected; a leaf missing
code-signing EKU is rejected; a wrong-issuer chain is rejected; a
well-formed chain with the right identity passes. Extend the existing
`verify_keyless_*` tests.

### 3. PKI10 — OIDC issuer/audience/alg/kid/iat

In `verify_jwt`, after signature + exp, enforce: `iss` == expected issuer;
`aud` contains the expected audience; the JOSE header `alg` is the
expected algorithm and `kid` (if the config pins one) matches, validated
BEFORE trusting the signature; `iat` not in the future (small skew) and
not older than a max age. Config carries the expected iss/aud/alg/kid.
Tests: wrong issuer rejected; wrong/absent audience rejected; unexpected
alg rejected; future/stale iat rejected; a fully-correct JWT passes.
State honestly in the book that this path has no production caller yet
(defence-in-depth).

### 4. Persistent build-signing key

Load a persistent signing identity once at agent startup (a configured
build-signer identity, or a durable workload identity provisioned for the
build role) and reuse it across builds instead of minting a per-build
ephemeral CSR. The signer's identity must be one the trust policy accepts
(ties to IMG3 step 2's identity binding). Preserve the existing
`require_signatures` build-failure gate (build_runner.rs:885-893) — a
build that needs a signature but can't produce a policy-trusted one FAILS,
it doesn't report success. Config: a `build_signer` identity/key path in
`src/config/node.rs` (document; off/optional with a clear default).
Tests: two builds reuse the same signing identity (not a fresh key each);
a build with `require_signatures` and no available trusted signer fails
with a clear reason; the signed manifest verifies under the deploy-time
`verify_image_signature` (round-trip: build-sign → deploy-verify passes).

## Acceptance checklist

- `make ci` green (fmt, clippy `-D warnings`, full default suite); README
  counts from your run (main is 2,473 — measure).
- Gated: the buildah-gated build-sign→deploy-verify round-trip
  (`RELIABURGER_BUILDAH_TESTS`) and the cluster suite
  (`RELIABURGER_CLUSTER_TESTS=1`) if the trust distribution touches
  cluster state; run what you can (Lima `reliaburger-test` if up) and
  quote results, or state clearly what's only unit-verified.
- `docs/progress.md`: tick the nested items under "Image trust policy",
  **check the theme box, and note the 12b.3 "Secure every boundary" tier
  is complete** (all three themes done).
- Book: chapter 10 (`docs/book/10-locking-it-down.md`) — the image-trust
  story (fail-closed on workers; why a partial chain check is no check;
  OIDC constraint hardening; a persistent signer vs a decorative ephemeral
  key). Chapter 5 only if its signing prose now reads wrong. British
  English, CLAUDE.md style guide, explain new Rust syntax on first use.

## Constraints

- **Seam ownership:** `src/pickle/signing.rs`, `src/meat/scheduler.rs`
  (verify_image_signature), `src/sesame/{cert,oidc}.rs`, `src/bun/
  build_runner.rs`, the trust-consult region of `src/bun/agent.rs` (~1110
  + the verify call site), `src/config/node.rs` (trust policy + build
  signer). All other 12b.3 themes are merged, so no live sibling — but do
  not gratuitously rewrite Theme 2's join/mtls or Theme 1's auth code.
- Reuse the `cert.rs` helpers rather than re-implementing chain/validity
  logic.
- If a trust-state projection touches `council/{types,state_machine}.rs`,
  keep it additive (serde-default; pre-theme snapshot loads cleanly —
  fixture test).
- Error messages lowercase, no trailing full stop; thiserror; no unwrap in
  production code.
