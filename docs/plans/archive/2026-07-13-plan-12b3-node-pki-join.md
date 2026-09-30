# Phase 12b.3 — Node PKI, join and mTLS (Theme 2)

Theme: `docs/progress.md` §12b.3 "Node PKI, join and mTLS".
Findings: PKI4, PKI5, PKI3, PKI9, CP11, + the constant-time join-token
compare from PKI10.
Source review: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

Already done (do NOT redo): persisted node identity, join bundle with TOFU
fingerprint (PKI1), handshake-true CRL-aware mTLS builders, mTLS wired onto
Raft/reporting/agent-API listeners behind `require_mtls` (Stage 5). Frame
caps already exist (CP11 is deadlines only). `atomic_write_mode` already
gives secure per-file temp writes (PKI9 is group-transactional only).

Every fix is enforcement — each test asserts the boundary **rejects** the
bad case (impersonation, double-issue, stalled connection).

## Ground truth (verified 13 Jul — re-verify line numbers)

- PKI4: the issuer generates the joiner's private key and returns it in
  the bundle (`src/sesame/join.rs:99` `private_key_b64`;
  `validate_and_issue` join.rs:195-272). Joiner side: `request_join`
  (join.rs:147-183). Issuer handler: `handle_join_issue`
  (`src/bun/agent.rs:5062-5115`), Raft write at ~5108.
- PKI5: token consume (in-memory, join.rs:218) + serial alloc
  (`state.next_serial()`, join.rs:252) + Raft write (agent.rs:5108) are
  separate steps → concurrent use of one token can double-issue. The join
  path hard-codes the Node CA issuer DN (join.rs:243-249) rather than
  deriving it from the stored intermediate subject.
- PKI3: `PinnedChainServerVerifier` (`src/sesame/mtls.rs:181-220`) pins
  the Node CA but skips node-id/server-name binding (comment says PKI3).
  Node certs set CN=node_id + DNS SAN (`ca.rs:260-278`) but no explicit
  node-id SAN. Expected node-id is available in the gossip
  `DirectoryExtension` (`src/mustard/message.rs:201`) which carries
  `node_id` + endpoints.
- PKI9: `identity_store::save` (`identity_store.rs:86-114`) writes 5 files
  via `atomic_write`/`atomic_write_mode` (`identity.rs:457-482` —
  temp+chmod+rename, secure per file) but not as a group; a crash
  mid-bundle leaves a partial install; no whole-bundle validation on load.
- CP11: Raft RPC (`src/council/network.rs:258-326`, frame cap 64 MiB at
  :206) and reporting (`src/reporting/transport.rs:226-289`, cap 1 MiB at
  :150) accept + read framed messages with NO per-connection handshake/
  read/write deadline. Outbound reporting dial already has a 5s connect
  timeout (transport.rs:306-309).
- PKI10 (this theme's slice): `verify_join_token` (`src/sesame/ca.rs:389`)
  uses plain `==` (timing-leaky).

## Implementation steps (tests first for each)

### 1. PKI4 — CSR-based join (key stays on the joiner)

The joiner generates its own keypair + a CSR (SPIFFE/node-id as required)
and sends the CSR (not asking for a key); the issuer validates the token,
signs the CSR, and returns the leaf + CA chain (NO private key in the
bundle). Reuse the workload-CSR machinery if it fits (`sesame::identity`
`create_workload_csr` pattern) or add a node CSR path. Keep TOFU
fingerprint pinning. Tests: the returned bundle contains no private key;
the joiner assembles a working identity from its local key + the issued
leaf; an issued leaf chains to the returned CA chain.

### 2. PKI5 — atomic validate + consume + serial, correct issuer DN

Make token validation, consumption, and serial allocation a single
Raft-committed operation (propose one request that atomically checks the
token unused, marks it consumed, and allocates the next serial; the issuer
signs only after that commit succeeds). Concurrent use of one token must
yield at most one issuance. Derive the issuer DN from the stored Node CA
intermediate's subject (so issued certs chain), instead of the hard-coded
DN at join.rs:243-249. Tests: two concurrent `handle_join_issue` calls
with the same token issue exactly one cert (the loser gets a clear
"token already consumed" error); an issued cert's issuer field equals the
stored Node CA subject and path-builds to the root.

### 3. PKI3 — expected-node-id binding on the client verifier

Issue node certs with a node-id SAN (URI or DNS form — pick one and be
consistent with what the verifier checks; a `spiffe://…/node/<id>` URI SAN
is cleanest). Thread the expected node-id from the gossip directory
through the mTLS connector to a per-target verifier, and have
`PinnedChainServerVerifier` assert the peer leaf's node-id SAN == expected
node-id (in addition to the existing Node-CA pin + CRL). A valid cert for
node B presented when dialling node A's expected id is rejected. Tests:
matching node-id passes; mismatched node-id (valid cert, wrong node) is
rejected; missing SAN is rejected under the binding mode.

### 4. PKI9 — transactional bundle install + load validation

Install the 5-file identity bundle atomically: stage all files into a temp
dir (secure modes via the existing helper), then swap into place (atomic
rename of the dir, or a marker/manifest committed last that `load` checks
so a partial install is detected and rejected). `load` validates the
bundle is complete + self-consistent (leaf chains to the CA chain) before
use. Tests: a simulated crash mid-install (only some files present) is
detected on load and refused, not silently used; a complete install loads.

### 5. CP11 — per-connection deadlines

Wrap the accept-side handshake, framed read, and write in
`tokio::time::timeout` on both the Raft RPC handler (network.rs:295-326)
and the reporting handler (transport.rs:262-289) with a bounded deadline
(config or constant, e.g. 10s handshake/read). A peer that connects and
stalls is dropped on the deadline instead of holding the task forever.
Keep the existing frame caps. Tests: a connection that sends a partial
length prefix / never completes the frame is dropped within the deadline
(use a fake stalled stream / a real socket that writes 2 of 4 length
bytes then sleeps).

### 6. PKI10 (this slice) — constant-time join-token compare

Replace the `==` at ca.rs:389 with a constant-time comparison. First check
whether `subtle` or `ring::constant_time::verify_slices_are_equal` is
already reachable in the tree (ring is likely a transitive rustls dep) —
**if a new crate is genuinely needed, STOP and report before adding it.**
Tests: correct token verifies; wrong token fails; (the constant-time
property itself isn't unit-testable, so assert via using the CT primitive).

## Acceptance checklist

- `make ci` green (fmt, clippy `-D warnings`, full default suite); README
  counts from your run (main is 2,431 — measure).
- Gated cluster suite (`RELIABURGER_CLUSTER_TESTS=1`) — the CSR join +
  mTLS node-id binding want a real cluster; run it (and the Lima recipe if
  the rig is up) and quote results, or state clearly what's only unit-
  verified.
- `docs/progress.md`: tick the nested `- [x]` sub-items under "Node PKI,
  join and mTLS" (PKI4/PKI5/PKI3/PKI9/CP11 + the CT token compare); check
  the theme box if all its sub-items are done.
- Book: chapter 4 (`docs/book/04-trust-no-one.md`) — the join hardening
  story (why the key must stay on the joiner; why validate/consume/serial
  must be one atomic op; node-id binding vs "any valid node cert"; why a
  connection needs a deadline). British English, CLAUDE.md style guide,
  explain new Rust syntax on first use.

## Constraints

- **Seam ownership:** `src/sesame/{join,mtls,ca,identity_store,identity,
  bootstrap}.rs`, `src/bun/agent.rs` **only the join-issuer region
  (~5062-5115)**, `src/council/network.rs`, `src/reporting/transport.rs`,
  the mTLS connector wiring. A sibling Theme 1 agent owns `src/sesame/
  {auth,token,types}.rs`, `src/bun/api.rs` and `src/brioche/*` — do NOT
  touch those. Do NOT touch the trust-consult region of `agent.rs`
  (~3113) or `pickle/signing.rs`/`meat/scheduler.rs` — those are Theme 3
  (Wave B).
- If the atomic-join Raft change touches `src/council/{types,state_machine}
  .rs`, keep it additive (new request variant + serde-default fields;
  pre-theme snapshot loads cleanly — fixture test).
- Node cert SAN format must stay compatible with the existing CRL/serial
  handling and the Stage 5 verifier pin.
- Error messages lowercase, no trailing full stop; thiserror; no unwrap in
  production code; `// SAFETY:` on any unsafe.
