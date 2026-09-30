# Phase 12b.1 — Pickle reference integrity

Theme: `docs/progress.md` §12b.1 "Pickle reference integrity".
Findings: REG1 (P0, re-validated confirmed), REG3, IMG1, plus the old Low
"manifest check" (the misleading missing-layer test).
Source review: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

Out of scope (later 12b themes): registry auth/TLS + quotas (REG4, 12b.3),
fsync/rename transactions and cache revalidation (REG5, 12b.4), peer redirect
constraints (REG6, 12b.4), redundancy honesty/GC-recheck-at-delete (REG7,
12b.4), multi-segment repository routes (REG8, 12b.4), trust-state distribution
to workers/standalone (IMG2, 12b.3), full chain/EKU/identity validation (IMG3,
12b.3).

## Ground truth

- REG1: `manifest_put` (`src/pickle/api.rs:366`) stores the raw manifest bytes
  as a content-addressed blob via `BlobStore::write_blob`, so the manifest blob
  is in GC's swept set (`list_blobs`). But `ImageManifest::all_digests()`
  (`src/pickle/types.rs:132`) returns only `[config, …layers]`, and
  `gc_candidates` (`src/pickle/gc.rs:74`) protects only `all_digests()` of
  catalogued manifests. A tagged manifest's own blob is orphaned: after the
  grace window, two-phase GC deletes it and the catalogue points at a 404.
  The same blind spot applies to holder tracking (`types.rs:251` records
  holders per `all_digests()` only) and therefore to the replication/heal loop
  (`src/pickle/replication.rs`) and P2P peer pull — the manifest blob itself
  has no recorded holders and is never healed to redundancy.
- REG3: `manifest_put` returns 201 without parsing the body as a valid OCI
  manifest, without checking media types or descriptor digest/size formats,
  and without checking the referenced config/layer blobs exist. There is an
  existing test that *asserts* Created for a manifest with a missing layer —
  it encodes the bug.
- IMG1: the scheduler trust lookup (`verify_image_signature` in
  `src/meat/scheduler.rs`, called from `src/bun/agent.rs:2827`) strips an OCI
  reference to basename/tag. A nested local repository (`team/app`) can miss
  policy; an unrelated external image whose basename matches a signed local
  image can pass a check meant for the local one. The verified identity is
  also not what the runtime later pulls (tag, not digest — mutable between
  verify and pull).

## Implementation steps (tests first for each)

### 1. REG1 — manifest blobs join reachability

Introduce one authoritative "referenced digests" notion per catalogue entry:
the manifest's own digest + config + layers. Suggested shape: a method on the
catalogue entry / commit type (e.g. `referenced_digests()`), rather than
changing `all_digests()` silently — audit each `all_digests()` call site
(gc.rs, types.rs holder commit, replication, p2p, pull) and switch the ones
that mean "everything this tag pins" to the new method. Concretely:

- Holder tracking: the push path records the pusher as a holder of the
  manifest blob too, and the Raft commit carries it.
- GC: `gc_candidates` protects the manifest digest of every catalogued tag.
- Replication/heal (`heal_tick`): the manifest blob is replicated to
  `[images] redundancy` like any layer.
- Peer/P2P pull: a node that needs the image can fetch the manifest blob from
  a peer holder.

Tests: unit — `gc_candidates` never nominates a catalogued manifest's own
blob; holder commit includes the manifest digest. Integration (extend
`tests/pickle_cluster.rs` or add `tests/pickle_integrity.rs`): the acceptance
sequence **push → GC past the grace window → manifest GET still 200 → peer
pull of the full image succeeds**. Also cover the upgrade path: a catalogue
persisted *before* this change (no manifest holders) must not panic and must
get healed rather than collected — write a fixture-driven test.

### 2. REG3 — validate before Created

In `manifest_put`, before storing or committing:

- Body parses as JSON into a known manifest schema; reject unknown/absent
  `mediaType` (accept OCI image manifest + Docker v2 schema 2; an image
  *index* may be explicitly rejected with a clear error for now — document).
- Every descriptor digest is well-formed (`Digest` parse) and its `size`
  matches the stored blob's actual size.
- Config and all layer blobs exist in the local store (OCI push order
  guarantees blobs-before-manifest) — missing → 400/404 with an OCI-style
  error body (`MANIFEST_BLOB_UNKNOWN`), matching what real clients expect.

Flip the misleading missing-layer test from asserting Created to asserting
rejection. Add: invalid JSON → 400; bad mediaType → 400; size mismatch → 400;
digest-malformed descriptor → 400; happy path still 201 and byte-identical
manifest GET (content addressing must see the exact bytes, not a re-serialise).

### 3. IMG1 — canonical, immutable image identity through trust

- Policy lookup key: the canonical repository path as pushed (full
  `namespace/name` or `cache/<host>/<repo>` — no basename stripping), so
  nested repos hit policy and external images cannot alias local ones.
- Resolve tag → manifest digest at verification time; verify the signature
  against that digest; then pass the *digest-pinned* reference through the
  deploy path so the runtime pulls exactly the verified bytes. Keep the
  `src/bun/agent.rs` diff minimal (other in-flight themes edit that file):
  prefer changing `verify_image_signature`'s signature/return to hand back the
  pinned digest and threading it through the existing call at agent.rs:2827.
- `cache/` repos stay exempt from `require_signatures` by construction — keep
  the pinning test that asserts that.

Tests: unit — nested repo `team/app` resolves to policy for `team/app`, not
`app`; an external `docker.io/library/app:tag` does not match a local signed
`app`; verify-then-pull uses the digest returned by verification (mutate the
tag between verify and pull in a test double and assert the pulled digest is
the verified one).

## Acceptance

- All new tests + full `make ci` green (fmt, clippy `-D warnings`, tests).
- The push → GC → peer-pull acceptance test runs in the default suite using
  the in-process registry (no Lima needed); note anything genuinely
  Linux-gated in the final report instead of skipping silently.
- `docs/progress.md`: add nested `- [x]` lines under the theme in the existing
  style and check the theme box (this plan completes the theme).
- Book: chapter 5 (`docs/book/05-where-the-images-live.md`) — the REG1 story
  is a good teaching moment (content-addressed stores make *reachability* the
  whole GC game; forgetting one root loses data), plus the manifest-validation
  contract; touch chapter 12's GC section only if it now reads wrong. British
  English, style guide in CLAUDE.md, explain new Rust syntax on first use.

## Constraints

- Keep changes inside `src/pickle/` + `src/meat/scheduler.rs` where possible;
  minimal, surgical edits to `src/bun/agent.rs` only.
- Wire-format changes to Raft catalogue commits must stay self-describing JSON
  and tolerate old on-disk catalogues (see the fixture test above).
- Error messages lowercase, no trailing full stop; thiserror in library code.
