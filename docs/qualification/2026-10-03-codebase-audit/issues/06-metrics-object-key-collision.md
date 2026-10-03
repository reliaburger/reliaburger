# Metrics writers sharing an object-store prefix overwrite each other’s Parquet chunks

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Two production stores using file:// reproduced.

### Problem and impact


Two Buns configured with the same `[metrics] object_store_url` independently choose the same numeric Parquet object keys. Opening both before either flushes seeds both counters to zero. Their first successful flushes both PUT `metrics_000000.parquet`; the second silently replaces the first node’s durable metrics. Later chunks continue colliding when the counters remain aligned. This is a storage identity defect, independent of the planned metrics query/reporting architecture.

The production Bun passes the configured URL straight to `MayoStore::open`, without adding its node identity. The backend uses unconditional object-store PUT, so an operator’s natural shared-bucket configuration can lose data despite both writers reporting success. A restart-only numeric counter scan prevents a single sequential writer from overwriting itself, but does not coordinate independent live writers.

### Reproduction and actual result


The retained `evidence/mayo_collision.rs` opens two production `MayoStore`s against the same empty `file://` prefix, inserts one labelled CPU sample per node, then flushes A followed by B. Parent independently reran it:

```text
after node a flush [(100, "cpu", "{\"node\":\"a\"}", 11.0)]
after node b flush [(100, "cpu", "{\"node\":\"b\"}", 22.0)]
remote files=["metrics_000000.parquet"]
```

Expected: both samples remain durably queryable after either order of flush and after reopen. The file backend demonstrates the common object-key/PUT path; no S3/GCS service was contacted.

### Evidence and fix direction


[src/bin/bun.rs:945](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bin/bun.rs#L945) passes the raw configured URL to the store. [src/mayo/store.rs:315](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/mayo/store.rs#L315) scans the prefix once on open; [src/mayo/store.rs:430](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/mayo/store.rs#L430) generates names from the process-local counter; [src/mayo/store.rs:244](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/mayo/store.rs#L244) overwrites that key with an unconditional PUT.

Use globally unique immutable chunk identities, or a durable node/writer namespace plus collision-safe writes. Define query ownership too: if every node reads the whole shared prefix, cluster fan-out must avoid counting the same durable samples once per node. An undocumented requirement for manually unique URLs is insufficient while shared prefixes are accepted without a warning or refusal.

### Acceptance criteria


- Open two stores on one prefix before either writes; concurrent/sequential flushes preserve both labelled samples and produce distinct immutable chunks.
- Reopen writers and repeat with counter histories that overlap; no prior chunk is replaced.
- Verify shared-prefix local and cluster queries do not duplicate samples; exercise file:// and an object-store test double.

### Existing issue comparison


#364/F06 covers PromQL, remote read, extra tiers, reporting/event production, chunking and live-metrics contracts. It does not describe object-key collisions and acknowledged durable data replacement in the existing object-store backend. No matching issue or discussion was found in the 112-issue inventory.


### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/bin/bun.rs:944–953](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bin/bun.rs#L944-L953)

```rust
    // it must exist before the runtime starts.
    let metrics_dir = storage_directory(&config.storage.metrics, "metrics").await?;
    // With `[metrics] object_store_url` set, metrics are persisted to and
    // queried from an object store (s3://, gs://, file://) so they survive node
    // loss (H8); otherwise Parquet stays in the local metrics dir.
    let mayo_store = Arc::new(RwLock::new(
        MayoStore::open(metrics_dir, Some(config.metrics.object_store_url.as_str()))
            .await
            .map_err(|e| anyhow::anyhow!("failed to open metrics store: {e}"))?,
    ));
```

[src/mayo/store.rs:315–326](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/mayo/store.rs#L315-L326)

```rust
        let Some(url) = object_store_url.filter(|u| !u.is_empty()) else {
            return Ok(Self::new(data_dir));
        };
        let (store, prefix) = parse_object_store(url)?;
        let flush_counter = next_remote_flush_counter(&store, &prefix).await?;
        Ok(Self {
            buffer: Vec::new(),
            data_dir,
            backend: Backend::Remote { store, prefix },
            flush_counter,
        })
    }
```

[src/mayo/store.rs:427–448](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/mayo/store.rs#L427-L448)

```rust
        let Some(batch) = self.buffer_to_batch()? else {
            return Ok(None);
        };
        let filename = format!("metrics_{:06}.parquet", self.flush_counter);
        self.buffer.clear();
        self.flush_counter += 1;
        let target = match &self.backend {
            Backend::Local => FlushTarget::Local {
                data_dir: self.data_dir.clone(),
                path: self.data_dir.join(&filename),
            },
            Backend::Remote { store, prefix, .. } => FlushTarget::Remote {
                store: Arc::clone(store),
                key: prefix.clone().join(filename.as_str()),
            },
        };
        Ok(Some(PendingFlush { batch, target }))
    }

    /// Build a DataFusion session exposing a `metrics` table over all data:
    /// the Parquet files unioned with the unflushed buffer.
    async fn session(&self) -> Result<SessionContext, MayoError> {
```

[src/mayo/store.rs:240–249](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/mayo/store.rs#L240-L249)

```rust
            let bytes = tokio::task::spawn_blocking(move || batch_to_parquet_bytes(&batch))
                .await
                .map_err(|e| MayoError::Io(std::io::Error::other(e.to_string())))??;
            store
                .put(&key, object_store::PutPayload::from(bytes))
                .await
                .map_err(|e| MayoError::ObjectStore(e.to_string()))?;
            Ok(())
        }
    }
```
