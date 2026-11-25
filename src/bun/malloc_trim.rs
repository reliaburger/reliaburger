//! Hand the memory glibc malloc keeps back to the kernel, on a slow timer
//! (#1936).
//!
//! glibc's malloc doesn't return freed memory to the kernel as it goes. A
//! freed block goes back on the arena's free lists for the next allocation,
//! and only the free space at the very top of the main heap is ever trimmed
//! by itself. Under a day of load, Bun's free-but-kept memory grew from 28
//! to 95 MiB while the memory it actually held fell (#1417): resident memory
//! that looks like a leak in `relish nodes`, `relish top` and `top`, on the
//! small servers and 2 GB appliances where people watch it most closely.
//!
//! glibc's `malloc_trim(0)` walks every arena and gives each whole free page
//! back with `madvise`. Bun calls it every [`MALLOC_TRIM_INTERVAL`] on
//! Tokio's blocking pool, because it locks each arena while it works and
//! must not stall a runtime worker. On any other C library (macOS, musl)
//! there is nothing to call and the loop returns at once.

use std::time::{Duration, Instant};

use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

/// How often Bun trims. Kept memory builds up over hours, so ten minutes
/// keeps resident memory within a few MiB of the live heap at a cost of six
/// short calls an hour.
pub const MALLOC_TRIM_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// A trim that takes at least this long is reported on stderr: every thread
/// that allocates waits for it.
pub const SLOW_TRIM: Duration = Duration::from_millis(100);

/// Whether this build's C library has `malloc_trim`: glibc on Linux only.
pub const SUPPORTED: bool = cfg!(all(target_os = "linux", target_env = "gnu"));

/// Give malloc's free pages back to the kernel now. Returns whether glibc
/// released any memory; always `false` where [`SUPPORTED`] is `false`.
pub fn trim_now() -> bool {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: malloc_trim takes no pointers and touches only malloc's
        // own state, under the same per-arena locks malloc and free take, so
        // it is safe to call from any thread at any time. The argument is
        // the padding to leave at the top of the heap; 0 keeps none.
        let released = unsafe { libc::malloc_trim(0) };
        released != 0
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        false
    }
}

/// The stderr line for a trim that took `took`, or `None` if it was quick.
pub fn slow_trim_warning(took: Duration) -> Option<String> {
    (took >= SLOW_TRIM).then(|| format!("bun: malloc_trim took {} ms", took.as_millis()))
}

/// Trim every [`MALLOC_TRIM_INTERVAL`] until `shutdown`, where glibc can.
pub async fn run_malloc_trim_loop(shutdown: CancellationToken) {
    if !SUPPORTED {
        return;
    }
    run_trim_loop(MALLOC_TRIM_INTERVAL, shutdown, trim_now).await;
}

/// Call `trim` on the blocking pool every `interval` until `shutdown`.
///
/// The first call comes a full interval after the start: a node that just
/// started has freed little yet. A trim that runs late delays the next one
/// rather than bunching them up.
pub async fn run_trim_loop<F>(interval: Duration, shutdown: CancellationToken, trim: F)
where
    F: Fn() -> bool + Clone + Send + 'static,
{
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // The first tick completes at once; skip it.
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = ticker.tick() => {}
        }
        let trim = trim.clone();
        // Timed on the blocking thread, so a wait for a free one in a busy
        // pool doesn't count as a slow trim (#2323).
        let timed = move || {
            let started = Instant::now();
            trim();
            started.elapsed()
        };
        // A join error means the runtime is shutting down.
        let Ok(took) = tokio::task::spawn_blocking(timed).await else {
            return;
        };
        if let Some(warning) = slow_trim_warning(took) {
            eprintln!("{warning}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    /// A fake trim that reports each call on a channel.
    fn reporting_trim() -> (
        impl Fn() -> bool + Clone + Send + 'static,
        mpsc::UnboundedReceiver<()>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        (move || tx.send(()).is_ok(), rx)
    }

    #[tokio::test(start_paused = true)]
    async fn trims_once_per_interval_starting_one_interval_in() {
        let interval = Duration::from_secs(600);
        let shutdown = CancellationToken::new();
        let (trim, mut calls) = reporting_trim();
        let start = tokio::time::Instant::now();
        let watcher = async {
            let mut at = Vec::new();
            for _ in 0..3 {
                calls.recv().await.unwrap();
                at.push(start.elapsed());
            }
            shutdown.cancel();
            at
        };
        let ((), at) = tokio::join!(run_trim_loop(interval, shutdown.clone(), trim), watcher);
        assert_eq!(at, vec![interval, interval * 2, interval * 3]);
    }

    #[tokio::test(start_paused = true)]
    async fn returns_without_trimming_when_already_cancelled() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        let trim = move || {
            counter.fetch_add(1, Ordering::SeqCst);
            true
        };
        run_trim_loop(Duration::from_secs(600), shutdown, trim).await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn stops_between_trims_when_cancelled() {
        let interval = Duration::from_secs(600);
        let shutdown = CancellationToken::new();
        let (trim, mut calls) = reporting_trim();
        let start = tokio::time::Instant::now();
        let watcher = async {
            calls.recv().await.unwrap();
            // Halfway to the second trim.
            tokio::time::sleep(interval / 2).await;
            shutdown.cancel();
        };
        tokio::join!(run_trim_loop(interval, shutdown.clone(), trim), watcher);
        // The loop returned on the cancel, not at the next tick.
        assert_eq!(start.elapsed(), interval + interval / 2);
        assert!(calls.try_recv().is_err(), "no second trim after the cancel");
    }

    #[tokio::test(start_paused = true)]
    async fn the_loop_waits_only_where_glibc_can_trim() {
        let shutdown = CancellationToken::new();
        let finished = tokio::time::timeout(
            Duration::from_secs(1),
            run_malloc_trim_loop(shutdown.clone()),
        )
        .await
        .is_ok();
        // On glibc the loop runs until shutdown; elsewhere it's a no-op.
        assert_eq!(finished, !SUPPORTED);
    }

    #[test]
    fn supported_only_on_linux_glibc() {
        assert_eq!(
            SUPPORTED,
            cfg!(target_os = "linux") && cfg!(target_env = "gnu")
        );
    }

    #[test]
    fn trim_now_is_safe_to_call_and_a_no_op_off_glibc() {
        // Free something first so glibc has pages it could hand back.
        drop(vec![0u8; 4 << 20]);
        let released = trim_now();
        if !SUPPORTED {
            assert!(!released);
        }
    }

    #[test]
    fn only_a_slow_trim_is_reported() {
        assert_eq!(slow_trim_warning(Duration::from_millis(99)), None);
        assert_eq!(
            slow_trim_warning(Duration::from_millis(250)).as_deref(),
            Some("bun: malloc_trim took 250 ms")
        );
    }
}
