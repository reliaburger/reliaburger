# images.max_storage does not bound bare blobs or temporary uploads on disk

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Quota accounting and all normal upload paths verified.

### Problem / impact


The configured registry storage ceiling is checked against the sum of published manifest logical sizes. Completed blob uploads without manifests never increase that sum, and active chunked upload bytes are not charged before writing to disk. Consequently a series of individually below-limit uploads can exceed `images.max_storage` by any amount. Normal `relish build` context uploads use the blob-only `_buildcontext` repository, so even legitimate repeated builds create uncharged storage until periodic GC. Chunked requests can also grow temporary files above the total ceiling and are only checked when completed.

Published-manifest accounting has the inverse mismatch: duplicated physical content in different repositories counts more than once, while the configured cap is per-node physical storage. Concurrent admission has no reservation, so more than one accepted upload/publication can observe the same remaining budget.

### Verified evidence


- [src/bin/bun.rs:2677](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bin/bun.rs#L2677): production `QuotaConfig.total_bytes` is wired from `[images] max_storage`.
- [src/pickle/api.rs:179](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L179): quota usage queries the catalogue, not storage and outstanding writers.
- [src/pickle/types.rs:456](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/types.rs#L456): `stored_sizes` iterates only `self.manifests` and sums `manifest.total_size`.
- [src/pickle/api.rs:1283](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L1283): completed upload checks quota once against incoming size then `complete_upload_guarded` installs the blob; it does not publish manifest metadata or increase usage.
- [src/pickle/api.rs:1175](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L1175): `stream_upload` appends bytes to disk with only a 512 MiB request ceiling, not a session aggregate ceiling or quota reservation.
- [src/pickle/build.rs:24](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/build.rs#L24): `_buildcontext` is expressly a blob-only scratch repository.
- [src/bin/bun.rs:2941](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bin/bun.rs#L2941): normal GC interval is at least one hour, so cleanup is not an admission bound.
- [src/pickle/p2p.rs:661](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/p2p.rs#L661)–`:729`: pull-through cache fills write upstream manifest/config/layers and publish through `record_commit` without any quota check; peer download path at [src/pickle/pull.rs:153](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/pull.rs#L153) likewise commits bytes without quota admission.
- Existing `push_over_repository_quota_is_refused` and `chunked_upload_over_repository_quota_is_refused` exercise one single assembled blob larger than the limit, not cumulative accepted storage.

### Proposed fix / acceptance


Enforce max_storage against per-node physical CAS and temporary upload usage, reserving capacity atomically before accepting/writing bytes; release reservations only when replacement/deduplication or confirmed cleanup justifies it. Keep logical repository accounting as a distinct policy if desired. Define whether downloaded/replicated images share the same cap and apply the policy consistently.

Ordinary regression cases: sequential unique blob-only uploads whose cumulative bytes exceed a small configured total must refuse further bytes; simultaneous uploads must not oversubscribe remaining capacity; PATCH growth must be limited before the temporary file crosses the cap; same-digest retries must not double-charge physical storage; restart must reconstruct accounting from durable files and pending uploads. Check build contexts as well as manifests.

### Verification limit


Parent verified the production quota and upload paths. No disk-filling workload or live overload was run. Existing quota tests cover a single oversized assembled blob and pass; cumulative storage and reservations are not covered by those tests.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/pickle/types.rs:455–466](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/types.rs#L455-L466)

```rust
    /// Logical stored image sizes used by repository and aggregate quota admission.
    pub fn stored_sizes(&self, repository: &str) -> (u64, u64) {
        let mut repository_bytes = 0u64;
        let mut total_bytes = 0u64;
        for (_, manifest) in &self.manifests {
            total_bytes = total_bytes.saturating_add(manifest.total_size);
            if manifest.repository == repository {
                repository_bytes = repository_bytes.saturating_add(manifest.total_size);
            }
        }
        (repository_bytes, total_bytes)
    }
```

[src/pickle/api.rs:1280–1304](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L1280-L1304)

```rust

    // Enforce the storage quota against the fully-assembled blob before it is
    // committed (M10). Every upload path only knows its total size here — the on-disk
    // temp file is authoritative. Over quota: drop the temp and the session so
    // nothing lands in the blob store.
    match state.store.upload_size(upload_id).await {
        Ok(incoming) => {
            if let Err(response) = state.enforce_quota(name, incoming).await {
                discard_upload(state, upload_id).await;
                return response;
            }
        }
        Err(super::types::PickleError::InvalidUploadId(_)) => {
            return StatusCode::BAD_REQUEST.into_response();
        }
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    }

    state.sessions.retire(upload_id).await;
    let result = state
        .store
        .complete_upload_guarded(upload_id, &digest, Some(writer), access.guard.clone())
        .await;
    if result.is_ok() {
        state.sessions.complete(upload_id).await;
```

[src/pickle/api.rs:1180–1194](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L1180-L1194)

```rust
    let write = async {
        let mut stream = body.into_data_stream();
        let mut received = 0usize;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
            received = received.saturating_add(chunk.len());
            if received > MAX_REQUEST_BYTES {
                return Err(StatusCode::PAYLOAD_TOO_LARGE.into_response());
            }
            state
                .store
                .write_upload_chunk(upload_id, &chunk)
                .await
                .map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
        }
```

[src/pickle/build.rs:20–30](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/build.rs#L20-L30)

```rust
/// entry-bomb archive cannot exhaust inodes or directory entries.
pub const MAX_CONTEXT_ENTRIES: usize = 65_536;

/// The scratch repository `relish build` uploads its context tarball to.
/// It only ever holds bare blobs: nothing tags or publishes a manifest here.
pub const BUILD_CONTEXT_REPOSITORY: &str = "_buildcontext";

/// Default Pickle registry port.
///
/// X1 regression note: this used to be 9117 — the *Bun API* port,
/// which has no `/v2` routes — so context uploads always 404ed.
```
