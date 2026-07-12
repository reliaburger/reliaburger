# Phase 12b.1 — Secret and workload-identity safety (remainder)

Theme: `docs/progress.md` §12b.1 "Secret and workload-identity safety".
Findings: PKI6, PKI7, D9's workload-identity lifecycle slice, and the PKI8
remainder (verify-before-retire, reject concurrent rotations).
Source review: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

Already landed (do not redo): PKI8 core — encryption picks the newest
non-read-only generation, decryption tries every live generation, finalize
refuses to empty a scope, malformed rotate bodies are rejected.

Out of scope: CRL/chain/EKU validation (IMG3/PKI items in 12b.3), node PKI
join hardening (PKI3–PKI5/PKI9, 12b.3), OIDC issuer/audience (12b.3).

## Ground truth

- PKI6 (validity): `system_time_to_date_components` /
  `time_to_year`/`time_to_month` (`src/sesame/identity.rs:295-340`) truncate
  workload-certificate validity to calendar dates — a one-hour certificate
  usually gets equal midnight not-before/not-after. Node certs
  (`build_node_certificate`, `src/sesame/cert.rs:139`) use exact
  `now`/`now + one_year`; workload certs must get the same exactness.
- PKI6 (SANs): `validate_and_sign_csr` (`src/sesame/identity.rs:99`) checks
  the expected SPIFFE URI SAN is *present* but signs the CSR's SAN list as
  submitted — attacker-supplied extra SANs (another workload's SPIFFE URI, a
  DNS name) are signed unchanged. The signer must rebuild the SAN list
  server-side: exactly the expected URI, nothing from the CSR but the public
  key.
- PKI7: the workload identity lands in an app-scoped durable directory (the
  deploy path in `src/bun/agent.rs` chooses the mount source; `write_identity_
  to_tmpfs` at identity.rs:193 writes wherever it is pointed). Replicas of the
  same app overwrite one another's key/JWT; rolling replacement drops the
  rotation tracking entry; adopted workloads come back with `identity: None`
  so the rotation task re-CSRs from scratch or not at all; stop/remove leaves
  key material on disk.
- PKI8 remainder: `RaftRequest::RotateSecretKey`
  (`src/council/state_machine.rs:305`) + the rotate endpoint
  (`src/bun/api.rs:3255`): finalize retires read-only generations as soon as
  an active replacement exists — it never verifies that every stored secret
  was re-encrypted under the new generation, so finalising early bricks any
  secret still sealed under the old key. Nothing rejects a second rotation
  started while one is un-finalised (per scope).

## Implementation steps (tests first for each)

### 1. PKI6 — exact validity windows

Workload certificates carry exact `not_before = now` (with a small clock-skew
backdate, e.g. 5 minutes) and `not_after = now + configured lifetime` as
timestamps, not calendar dates. Delete the date-component helpers once nothing
uses them. Tests: a one-hour cert's window is one hour ±skew (not zero, not a
day); `check_validity` accepts it now and rejects it after expiry (inject the
clock — follow the existing injected-clock pattern from `grill::snapshot`).

### 2. PKI6 — server-side SANs

`validate_and_sign_csr` extracts only the public key from the CSR and issues a
certificate whose SAN list is rebuilt server-side: exactly the expected SPIFFE
URI. Tests: a CSR smuggling a second URI SAN / a DNS SAN yields a certificate
containing only the expected URI (parse the issued DER and assert the SAN set);
the legitimate path still round-trips.

### 3. PKI7 — per-instance identity, full lifecycle

- Identity directory becomes per-instance: keyed by the instance id, not the
  app (`…/identity/<instance-id>/`), mounted into the container at the same
  in-container path as today. On Linux root mode, back it with a real tmpfs
  mount (size-bounded, `0700`, owned by the workload's UID in rootless/userns
  setups); elsewhere a `0700` directory + documented gap (same honesty rule
  the network theme used for non-runc runtimes).
- Ownership/permissions: key file `0600`, owned by the container's runtime
  UID; fix the world-readable workload key while in here (M25 is this theme's
  neighbour — check and close it if it is one line, note it if not).
- Cleanup: stop/remove/rollback deletes the instance's identity dir (and
  unmounts the tmpfs); redeploy creates the new instance's dir before start.
- Rolling replacement: the new instance gets its own identity + tracking
  entry; the old one's entry is dropped with the instance (no orphaned
  rotation timers).
- Adoption/restart: rebuild the rotation-tracking map from the per-instance
  dirs on startup (metadata sidecar with spiffe URI + next_rotation), so an
  adopted workload keeps its identity and rotation schedule instead of
  `identity: None`. Restart-safe rotation = rotation state derived from disk,
  not process memory (D9's lifecycle slice).

Tests: two replicas of one app get distinct keys (the overwrite regression);
stop removes the dir; rolling redeploy leaves exactly the live instances'
dirs; a simulated adoption (write dirs, restart the supervisor path) restores
rotation tracking — assert the next rotation fires without a fresh CSR.

### 4. PKI8 remainder — verify before retire, one rotation at a time

- Record the sealing generation alongside each stored secret at write time
  (age ciphertext does not disclose its recipient, so it must be metadata; a
  fixture test covers legacy entries without it — treat unknown generation as
  "needs re-encryption").
- `finalize` fails (409-style CouncilResponse, surfaced by the endpoint) while
  any secret in the scope is still sealed under a generation older than the
  newest — the error names the offending secrets. The `relish secret rotate`
  re-encrypt step updates the recorded generation; when everything is current,
  finalize proceeds as today.
- Reject a `RotateSecretKey` (non-finalise) for a scope that already has an
  un-finalised rotation in flight (read-only older generation still present)
  — the error tells the operator to finalise or re-encrypt first. Idempotent
  retries of the *same* rotation must not trip this: dedupe on the new
  generation number.

Tests (state-machine level, like the existing `finalize_secret_rotation_*`):
finalize with a stale-sealed secret is refused and names it; after re-encrypt
it succeeds; concurrent second rotation refused; same-generation retry
accepted; legacy secret without generation metadata blocks finalize until
re-encrypted. End-to-end: the api.rs rotate endpoint surfaces the refusal.

## Acceptance

- `make ci` green (fmt, clippy `-D warnings`, full default suite).
- Anything Linux-only (tmpfs mount, ownership under userns) goes behind the
  existing Lima gates; run the Lima suite if the rig is available, otherwise
  name what is unverified.
- `docs/progress.md`: nested `- [x]` items under the theme, theme box checked
  — **this completes all five 12b.1 themes**; update the 12b.1 heading area
  if it carries a summary line, and README/docs/README test counts.
- Book: chapter 10 (`docs/book/10-locking-it-down.md`) — the SAN-rebuild
  lesson (never sign what the requester asked for; sign what the server
  decided), exact validity windows, and the per-instance identity lifecycle;
  chapter 4 only if its secret-rotation prose now reads wrong. Update
  `docs/design/security-sesame.md` where it describes the app-scoped layout.
  British English, style guide in CLAUDE.md, explain new Rust syntax on first
  use.

## Constraints

- Branch from main only after PR #86 (network policy) is merged — both themes
  edit the deploy/stop/adoption seams in `src/bun/agent.rs`; build on its
  `finish_instance_networking`/pre-start structure rather than adding a
  parallel one (a shared "prepare instance environment" step is welcome if it
  falls out naturally, not a rewrite).
- Raft/SecurityState changes stay self-describing JSON and tolerate old
  persisted state (legacy secrets without generation metadata — fixture test).
- Error messages lowercase, no trailing full stop; thiserror in library code;
  no unwrap in production code.
