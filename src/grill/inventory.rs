//! Bound original-inventory reads even when their caller stops waiting.

use std::future::Future;
use std::io;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Shared admission for one runtime adapter's original-inventory reads.
#[derive(Debug, Clone)]
pub(crate) struct InventoryReader {
    slot: Arc<Semaphore>,
}

impl Default for InventoryReader {
    fn default() -> Self {
        Self {
            slot: Arc::new(Semaphore::new(1)),
        }
    }
}

impl InventoryReader {
    /// Keep the read operation alive until it reports its actual outcome.
    pub(crate) async fn read<T: Send + 'static>(
        &self,
        operation: impl Future<Output = io::Result<T>> + Send + 'static,
    ) -> io::Result<T> {
        let permit = self
            .slot
            .clone()
            .acquire_owned()
            .await
            .map_err(io::Error::other)?;
        // A cancelled caller must not admit another read while its worker survives.
        tokio::spawn(async move {
            let _permit = permit;
            operation.await
        })
        .await
        .map_err(io::Error::other)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;
    use tokio::sync::Notify;

    #[tokio::test]
    async fn cancelled_inventory_waiter_retains_admission_until_the_original_read_finishes() {
        let reader = InventoryReader::default();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let first_reader = reader.clone();
        let first_entered = entered.clone();
        let first_release = release.clone();
        let first = tokio::spawn(async move {
            first_reader
                .read(async move {
                    first_entered.notify_one();
                    first_release.notified().await;
                    Ok(())
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let second_started = Arc::new(AtomicBool::new(false));
        let observed = second_started.clone();
        let second = tokio::time::timeout(
            Duration::from_millis(30),
            reader.read(async move {
                observed.store(true, Ordering::SeqCst);
                Ok(())
            }),
        )
        .await;
        // Release the surviving operation even if the assertion below fails.
        release.notify_one();
        let final_read = tokio::time::timeout(Duration::from_secs(1), reader.read(async { Ok(7) }))
            .await
            .unwrap()
            .unwrap();
        assert!(
            second.is_err(),
            "the timed-out caller released a still-running reader's slot"
        );
        assert!(
            !second_started.load(Ordering::SeqCst),
            "a cancelled queued read ran later"
        );
        assert_eq!(final_read, 7);
    }

    #[tokio::test]
    async fn inventory_read_failure_releases_admission_for_retry() {
        let reader = InventoryReader::default();
        let result = reader
            .read(async { Err::<(), _>(io::Error::other("fixture read failure")) })
            .await;
        assert!(result.is_err());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), reader.read(async { Ok(11) }))
                .await
                .unwrap()
                .unwrap(),
            11
        );
    }
}
