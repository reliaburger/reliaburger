//! Test support: prove an async function does its slow work on tokio's
//! blocking pool, not on the runtime thread that polls it.
//!
//! The first version of these tests polled the future once on a
//! single-threaded runtime and asserted `Pending` (#1618, #1636). That
//! raced: `spawn_blocking` hands the closure to another thread at once, and
//! small work (one `create_dir_all`) could finish there before the first
//! poll looked at the `JoinHandle`, so the poll came back `Ready` and the
//! test failed (#1667). Here the runtime gets one blocking thread, and a
//! closure the test controls holds it. Work sent to the pool queues behind
//! it and can't run, so the first poll is `Pending` every time. Work done
//! inline finishes inside that poll, so it's `Ready` every time.

use std::future::Future;

/// Poll `work` once on a single-threaded runtime whose only blocking thread
/// is busy, assert the poll is `Pending`, then free the thread and drive
/// `work` to completion, returning its output.
///
/// Panics if the first poll is `Ready`: the work ran inline, on the runtime.
pub(crate) fn assert_runs_on_blocking_pool<F: Future>(work: F) -> F::Output {
    // `#[tokio::test]` can't cap the blocking pool, so the test builds its
    // own runtime.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let (holding_tx, holding_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let occupant = tokio::task::spawn_blocking(move || {
            holding_tx.send(()).unwrap();
            // Returns when the test sends, or drops the sender on a panic.
            let _ = release_rx.recv();
        });
        // Wait until the occupant holds the pool's only thread.
        holding_rx.await.unwrap();

        let mut work = std::pin::pin!(work);
        let first = futures_util::poll!(work.as_mut());
        assert!(
            first.is_pending(),
            "the work finished inside one poll, on the runtime thread"
        );

        release_tx.send(()).unwrap();
        occupant.await.unwrap();
        work.await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_sent_to_the_blocking_pool_passes() {
        let answer = assert_runs_on_blocking_pool(async {
            tokio::task::spawn_blocking(|| 42).await.unwrap()
        });
        assert_eq!(answer, 42);
    }

    #[test]
    #[should_panic(expected = "inside one poll, on the runtime thread")]
    fn work_done_inline_fails() {
        assert_runs_on_blocking_pool(async {
            std::fs::create_dir_all(std::env::temp_dir()).unwrap();
        });
    }
}
