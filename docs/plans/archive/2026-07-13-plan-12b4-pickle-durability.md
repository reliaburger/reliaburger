# Phase 12b.4 — Pickle storage and replication durability (Theme P)

Theme: `docs/progress.md` §12b.4 "Pickle storage and replication
durability". Findings: D11, REG2, REG4, REG5, REG6, REG7, REG8, old cache/
upload Lows. Includes the **registry auth/TLS deferred from 12b.3 Theme 2**.
Source: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

Already done (do NOT redo): REG1/REG3/IMG1 (#84 — manifest-blob
reachability, manifest validation before Created, digest-pinning); T7 (#90
— durable batch/build, the PATCH `Location` header, context transfer). GC
sole-copy recheck exists in `apply_gc_report`.

Every fix asserts the boundary **rejects/persists safely** — attacker
redirect refused, over-quota/expired upload refused, torn write recovered.

## Ground truth (verified 13 Jul on main @ #101)

- REG5: `write_blob` (`src/pickle/store.rs:89-104`) writes direct to final,
  no fsync/temp; catalogue `persist_to` (`src/pickle/types.rs:347-359`)
  temp+rename but no fsync; cache hit skips digest revalidation
  (`src/pickle/pull.rs:27-29`); tag rootfs generations race
  (`src/grill/image.rs:242-248`). (`complete_upload` store.rs:156 already
  does atomic rename after digest verify.)
- REG2: two catalogues — Raft `DesiredState::manifest_catalog`
  (`src/council/types.rs:288`) vs node-local `PickleState::catalog`
  (`src/pickle/api.rs:24`). `manifest_get` (`api.rs:604`) + `tags_list`
  (`api.rs:666`) read LOCAL, so a remote/non-council commit is invisible
  until a heal tick.
- REG4: registry routes unauthenticated/plaintext (the 12b.3 handoff);
  512 MiB body limit (`api.rs:107`); synchronous I/O + hashing on the async
  runtime; no principal/repository quota; no aggregate upload limit; no
  upload-session expiry (REG8).
- REG6: `replicate_layer_to_peer` (`src/pickle/replication.rs:149-166`)
  follows an absolute redirect `location` as-is — a compromised peer can
  redirect a PUT to an attacker URL; timeout enforced (30s) but no
  same-origin constraint; peer body reads unbounded beyond blob size.
- REG7/D11: push returns Created before replication (heal-loop only,
  `api.rs:587,597`); zero-peer push "succeeds"; GC recheck at deletion is
  partial (`gc.rs:321-345` re-checks sole-copy but not full catalogue
  reference).
- REG8: single-segment routes (`api.rs:96+`); multi-segment `team/app`
  404s (peer transfers flatten `/`→`-`).

## Implementation steps (tests first for each)

### 1. REG5 — durable persistence

- `write_blob` and catalogue `persist_to`: write to a UNIQUE temp file,
  fsync the file, rename, fsync the parent dir. No direct-to-final, no
  predictable temp name.
- Cache hit (`pull.rs`): revalidate the digest of an existing blob before
  trusting it (or at least on the deploy/verify path) — a truncated/
  corrupt cached blob must not be served as valid.
- Rootfs generations (`grill/image.rs`): isolate immutable per-digest
  rootfs so a tag move / concurrent push can't delete/re-extract another's
  live rootfs (content-addressed generation dir; the running container
  holds its generation).

Tests: a simulated crash between temp-write and rename leaves the old
blob/catalogue intact (no torn final file); a corrupt cached blob is
rejected on revalidation; two tag operations on the same repo don't clobber
each other's rootfs.

### 2. REG2 — one authoritative catalogue

Make tag/manifest GET + list read the **Raft/authoritative** catalogue
(the same `catalog_snapshot()` the P2P path uses), so a remote or
non-council commit is visible everywhere; a non-council push must reach the
authoritative catalogue (propose to Raft, or be rejected as un-committable
rather than silently local). Reconcile the local cache as a projection of
the authoritative catalogue. Tests: a manifest committed via Raft (as by a
peer) is returned by `manifest_get`/`tags_list` on a node that didn't
receive the original PUT; a non-council push either commits authoritatively
or is refused (not silently local).

### 3. REG4 — auth, TLS, quotas, limits (the 12b.3 handoff)

- **Auth + TLS** on the registry listener: require a valid principal
  (reuse `sesame::auth` / the token store — the same auth the agent API
  uses) and serve over TLS (reuse the mTLS/identity material). Anonymous/
  plaintext push is refused.
- **Quotas + limits:** per-principal / per-repository quota, an aggregate
  upload limit, and **upload-session expiry** (REG8 — sessions currently
  never expire; add a TTL + sweep). Bound per-request buffering; stream/
  hash **off the async runtime** (`spawn_blocking` for whole-blob hashing)
  so a large push doesn't stall the executor.

Tests: an unauthenticated push is refused (401/403); a push over the repo
quota is refused; an expired upload session is rejected + swept; hashing a
large blob doesn't block other requests (concurrency assertion).

### 4. REG6 — constrain peer redirects + bound reads

Constrain the replication redirect to a **same-origin relative path**
(reject absolute/cross-host `location`); bound peer body reads to the
expected blob size within the timeout. Tests: a peer returning an absolute
attacker URL is refused (the PUT never leaves for the attacker host); an
oversized/slow peer body is bounded/timed out.

### 5. REG7/D11 — honest redundancy + acknowledged push semantics

- Push must not report success with zero peers when redundancy is
  required; define the **acknowledged push contract** (either wait for the
  Raft commit + the configured replica count before Created, or return a
  status that honestly reflects "committed locally, replication pending"
  — pick what matches the design and document it). A failed Raft proposal
  must not be silently tolerated as a durable push.
- GC: revalidate against the full catalogue reference set immediately
  before deletion (not only sole-copy).

Tests: a push requiring redundancy with no reachable peer does NOT report a
durable success; GC does not delete a blob a catalogue entry still
references (recheck-at-delete).

### 6. REG8 — multi-segment repositories

Support `team/app` repository names in the registry routes (an axum
wildcard/`*rest` capture parsing the full repo path), so normal team/app
names work end-to-end (config + trust-policy examples already use them);
keep the content-addressed peer transfer working. Tests: push/pull a
`team/app:tag` image through the full route path.

## Acceptance checklist

- `make ci` green (fmt, clippy `-D warnings`, full default suite); README
  counts from your run (main is 2,499 — measure).
- Gated: buildah/registry round-trips (`RELIABURGER_BUILDAH_TESTS`) and
  cross-node push→peer-pull (`RELIABURGER_CLUSTER_TESTS=1`) where they
  fit; quote results or state what's only unit-verified.
- `docs/progress.md`: nested `- [x]` items under "Pickle storage and
  replication durability"; check the theme box (this completes it).
- Book: chapter 5 (`docs/book/05-where-the-images-live.md`) — the
  durability story (fsync/rename transactions; one authoritative
  catalogue; the peer-redirect SSRF; honest push semantics; registry auth/
  TLS). British English, CLAUDE.md style guide, explain new Rust syntax on
  first use.

## Constraints

- **Seam ownership:** `src/pickle/*`, `src/grill/image.rs` (rootfs
  isolation), and the `manifest_catalog` READ path — you may touch
  `src/council/{types,state_machine}.rs` ONLY around the existing
  `manifest_catalog` (REG2), and prefer NOT adding new council state. A
  sibling Theme S (service catalogue) agent runs concurrently and ADDS a
  distinct `service/endpoint_catalog` field to `DesiredState` — do NOT
  touch that field or `src/onion/*`/`src/wrapper/*`. Keep any council
  change additive.
- Registry auth/TLS reuses the existing `sesame` auth + identity material
  (don't invent a parallel scheme).
- Raft/state changes stay self-describing JSON; a pre-theme snapshot loads
  cleanly (fixture test).
- Error messages lowercase, no trailing full stop; thiserror; no unwrap in
  production code.
