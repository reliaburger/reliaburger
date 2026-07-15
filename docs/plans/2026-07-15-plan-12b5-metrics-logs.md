# Phase 12b.5 — Metrics, logs and object storage (Theme O)

Theme: `docs/progress.md` §12b.5 "Metrics, logs and object storage".
Findings: OBS2-OBS8, codex-M4/D13, old M3/M4/M19/M20/M24/X8. OBS1 already
done (#82). Source:
[2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

Large theme — **2 sequential PRs**: O1 metrics (`src/mayo/*`), O2 logs +
object storage (`src/ketchup/*`).

## Harness contract (PR #106 is merged — this is now the law)

- Green gate = **`make ci`** (`fmt-check + lint + test + test-doc +
  test-no-default`). `make test` = `cargo nextest run --profile default
  --no-tests=fail`. **NOT `cargo test`.**
- nextest default: **retries=0, 60s kill-timeout**, single-threaded
  `cluster-heavy`/`host-network` groups → tests must be **deterministic**.
- **No fixed-wait sleeps** — observable synchronisation; the harness owns
  tasks/ports/temp.
- Env/cloud tests are **honest**: `#[ignore]`d + named + wired into the
  right `make test-*` filter (+ a `.config/nextest.toml` test-group if
  heavy). **No silently-passing gates.** Real S3/GCS → a local
  `object_store` fixture in the portable suite (mirror #106's local-OCI-
  fixture pattern); real cloud behind a named manual/ignored suite.
- **Do NOT update a headline test count** — the README reports a suite
  taxonomy now. Update suite/design-doc prose if anything, not a number.

## PR O1 — Metrics (`src/mayo/*`, api.rs metrics handlers, bun.rs collection)

Seams (verified post-#106):
- OBS1-remainder: `metrics_app_handler` (`src/bun/api.rs:3172`) interpolates
  `metric_name='{name}'` unescaped. Parameterise via the existing
  `escape_sql_literal` (mayo, #82). Test: `?name=x' OR '1'='1` cannot bypass
  the tenant/time predicate.
- OBS2 rollups (`src/mayo/rollup_store.rs`): `buffer: Vec<BufferedRollup>`
  (:56) unbounded; `flush_counter: 0` (:62/72) hardcoded — no
  `next_flush_counter()` so restart overwrites `rollup_NNNNNN.parquet`;
  windows not epoch-aligned (`rollup_generator.rs:44`) and not idempotent
  per (node,window) → reassignment double-counts. Fix: bound + prune,
  epoch-align windows, idempotent ingest keyed by (node,window), recover the
  flush counter on start. Tests: a re-sent window doesn't double-count; the
  buffer is bounded; restart continues the counter (no overwrite).
- OBS3 per-app metrics: production collects node metrics only
  (`src/bin/bun.rs` collection task ~884). Wire the existing-but-uncalled
  `collector::collect_process_metrics` (`src/mayo/collector.rs:118`) and/or
  `scrape::scrape_endpoint` (`src/mayo/scrape.rs:61`) into the collection
  loop so per-app (app-labelled) metrics exist and autoscaling has a signal.
  Test: after a deploy, an app-labelled metric is collected + queryable.
- OBS4 alerts (`src/mayo/alert.rs:142`, `src/mayo/webhook.rs:112`):
  **distinguish stale telemetry** — a firing alert must NOT silently
  resolve because telemetry stopped (`unwrap_or(false)` → Inactive today);
  add a NoData/stale state so an app that died-and-stopped-emitting doesn't
  clear its own alert. Provider payloads: `build_payload` sends one generic
  shape; Slack + PagerDuty need their **provider contracts** (Slack
  attachments/blocks; PagerDuty Events API v2). Guard `interval > 0` at
  config. Tests: stale telemetry does not resolve a firing alert; Slack and
  PagerDuty payloads match their provider shape; zero interval is rejected.
- OBS5/M3 async flush: flush holds the store `RwLock` across synchronous
  Parquet I/O (`bun.rs` ~893, `src/mayo/store.rs:160`). Move the write to
  `spawn_blocking` / release the lock during I/O; a corrupt Parquet file
  must not break every query (skip/quarantine on read). Tests: a query can
  proceed during a flush (no lock starvation — assert via ordering/timing
  without a sleep); a corrupt file in the dir doesn't fail an unrelated
  query.

Book: chapter 6 "Watching Everything". `docs/progress.md`: nested `- [x]`
for O1. **Report + STOP after O1** for merge, then continue with O2.

## PR O2 — Logs + object storage (`src/ketchup/*`, api.rs logs handlers, bun.rs export/shutdown)

- OBS5 raw-log SQL: `logs_sql_handler` (`src/bun/api.rs:2993`) passes
  arbitrary `?q=` SQL straight to `query_sql_json` — no table/row/time/
  memory bound. Add typed/bounded access (constrain to the log table, cap
  rows/time, bound memory). Test: an unbounded/oversized/cross-table query
  is rejected or bounded.
- OBS6 log fan-out (`src/ketchup/query.rs`): `fan_out_query` (:27)
  concatenates unencoded values into the URL and turns node HTTP/JSON/task
  failures into empty success (hides failures). URL-encode; return partial
  failures (which nodes failed). Dedup is adjacent `(timestamp,line)` only
  (`merge_log_entries` :13, M4) — dedup by stable (node,instance,event)
  identity. Tests: an unreachable node is reported as a partial failure not
  silent empty; a `grep` value with `&`/`?` is transmitted intact; separated
  duplicates from two replicas dedup, distinct events survive.
- OBS7/M20/X8 object-store export (`src/ketchup/export.rs:59`,
  `std::fs::copy` at :100): implement real `object_store` export for
  `s3://`/`gs://` (aws+gcp features are enabled); ONE Bun-owned checkpoint
  (kill Relish's competing checkpoint, X8); durable object ids so a reused
  filename after retention isn't skipped forever; **shutdown flush** of
  metrics+logs (`src/bin/bun.rs` shutdown path drops unflushed buffers
  today). Tests (portable, local `object_store` fixture — `LocalFileSystem`/
  a temp dir): export writes to the object store and the checkpoint
  advances; a reused filename is not skipped; shutdown flushes the buffer.
  Real S3/GCS goes in a named `#[ignore]` manual suite, not a silent gate.
- OBS8/M24 legacy store: remove/consolidate the dead `KetchupStore`
  (`src/ketchup/store.rs`, constructed-then-unused; its calendar/index +
  `logs.max_file_size_mb` configure neither live path). LogStore is the live
  path. Test: the build/config still parse; no live behaviour lost.

Book: chapter 6 (+ chapter 15 "Ready for Production" only if the harness/
export prose belongs there). `docs/progress.md`: nested `- [x]` for O2 and
**check the "Metrics, logs and object storage" theme box** (O2 completes it).

## Constraints

- **Seam ownership:** `src/mayo/*`, `src/ketchup/*`, the metrics/logs
  handlers in `src/bun/api.rs` (NOT the gitops webhook route ~3376 — a
  sibling Theme G agent owns that + the router split), the collection/flush/
  export/shutdown tasks in `src/bin/bun.rs` (NOT the gitops runner spawn
  ~1124). Do NOT touch `src/lettuce/*` or `src/council/*`.
- Reuse `escape_sql_literal` and the `object_store` crate already in the
  tree; no new deps expected.
- Error messages lowercase, no trailing full stop; thiserror; no unwrap in
  production code.
