# Failed Ketchup flush discards rows and advances replay checkpoints past lost data

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Failed flush, replay refusal and restart loss reproduced.

### Problem

Ketchup clears its buffered rows before a flush's filesystem operations succeed. A temporary write failure permanently removes those rows from the live store. The in-memory capture offsets already cover the lost rows, so the capture reader cannot replay them. A subsequent successful flush persists a newer checkpoint, making the loss survive restart.

### Evidence

- [src/ketchup/log_store.rs:600–613](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L600-L613): `take_flush_batch` builds a batch, clears the buffer at line 606 and increments the counter before IO.
- [src/ketchup/log_store.rs:385–398](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L385-L398): the production `flush_shared` hands the batch to `write_log_pending` and returns an error without restoring it.
- [src/ketchup/log_store.rs:622–626](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L622-L626): direct `flush` has the same failure.
- [src/ketchup/log_store.rs:503–511](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L503-L511): ingest advances capture offsets and rejects records at or below the stored offset.
- [src/ketchup/log_store.rs:333–375](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L333-L375): a successful subsequent flush persists its capture checkpoint.

### Reproduction

The executable public API reproduction is in `evidence/state.rs`.

1. Create a capture file containing `lost\nkept\n`.
2. Make the configured Parquet directory a regular file, forcing a transient `create_dir_all` failure.
3. Ingest `lost` with capture end offset 5 and flush.
4. Repair the Parquet directory and replay `lost` at offset 5.
5. Ingest `kept` at offset 10, flush successfully, reopen the store and query `SELECT line FROM logs`.

Actual verified output:

```text
failed flush result=Err(Io(Os { code: 17, kind: AlreadyExists, message: "File exists" })) buffered=0
replay lost row after failed flush accepted=false
reopened log checkpoint offset=Some(10)
rows after recovery=[Object {"line": String("kept")}]
```
Expected: both records remain retryable and are eventually persisted exactly once. No committed checkpoint may advance past a missing batch.

### Suggested fix / acceptance

Retain ownership of pending batches until successful persistence, or restore failed batches and corresponding offset state without breaking ordering with concurrent ingestion/flushes. Cover both shared and direct flush callers and cancellation. Add failures at directory creation, Parquet write and checkpoint publication followed by recovery; confirm no missing/duplicate rows and correct restart replay. This is distinct from #308, which added successful-flush replay checkpoints, and #510's physical disk pressure handling.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/ketchup/log_store.rs:600–615](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L600-L615)

```rust
    pub fn take_flush_batch(&mut self) -> Result<Option<LogPendingFlush>, KetchupError> {
        let Some(batch) = self.buffer_to_batch()? else {
            return Ok(None);
        };
        let filename = format!("logs_{:06}.parquet", self.flush_counter);
        let path = self.data_dir.join(filename);
        self.buffer.clear();
        self.flush_counter += 1;
        Ok(Some(LogPendingFlush {
            data_dir: self.data_dir.clone(),
            path,
            batch,
            checkpoint: self.ingested.clone(),
        }))
    }

```

[src/ketchup/log_store.rs:385–399](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L385-L399)

```rust
pub async fn flush_shared(
    store: &std::sync::Arc<tokio::sync::RwLock<LogStore>>,
) -> Result<bool, KetchupError> {
    let pending = {
        let mut guard = store.write().await;
        guard.take_flush_batch()?
    };
    match pending {
        Some(p) => {
            write_log_pending(p).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}
```

[src/ketchup/log_store.rs:503–512](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L503-L512)

```rust
    fn ingest_at_nanos(&mut self, nanos: u64, record: &super::types::LogRecord) -> bool {
        if let Some(position) = &record.position {
            let seen = self.ingested.offsets.get(&position.file).copied();
            if seen.is_some_and(|offset| position.end_offset <= offset) {
                return false;
            }
            self.ingested
                .offsets
                .insert(position.file.clone(), position.end_offset);
        }
```
