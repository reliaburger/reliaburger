//! Post-boot gossip evidence for local upgrade verification.

use std::time::Duration;
use tokio::sync::watch;

/// Require a peer acknowledgement from this process within the configured deadline.
/// Standalone nodes have no gossip receiver and keep their workload-only check.
pub async fn wait_for_rejoin(
    receiver: Option<watch::Receiver<bool>>,
    timeout: Duration,
) -> Result<(), String> {
    let Some(mut receiver) = receiver else {
        return Ok(());
    };
    tokio::time::timeout(timeout, receiver.wait_for(|joined| *joined))
        .await
        .map_err(|_| format!("gossip did not rejoin within {} seconds", timeout.as_secs()))?
        .map_err(|_| "gossip stopped before rejoining".to_owned())?;
    Ok(())
}

/// Decide whether a freshly swapped-in replacement may commit: wait out the
/// boot grace and the rejoin deadline, both measured from the moment the
/// replacement can answer for itself.
///
/// `answerable` resolves once the node can report what it runs (Bun passes
/// its running-binary hash, which `/v1/version` reports). Starting the clock
/// at boot instead let a slow hash of a large binary eat the window: under
/// load a debug Bun spent the whole five-second rejoin deadline unable to
/// answer, so nobody could see it before it reverted (#771). A replacement
/// that never becomes answerable starts the clock after one `rejoin_timeout`
/// anyway, so a stuck hash still ends in a verdict rather than a marker
/// left in flight.
///
/// A peer acknowledgement that arrives before the clock starts still counts.
/// The grace and the rejoin deadline run at the same time, as in
/// [`wait_for_rejoin`].
pub async fn verify_after_answerable<F>(
    answerable: F,
    grace: Duration,
    receiver: Option<watch::Receiver<bool>>,
    rejoin_timeout: Duration,
) -> Result<(), String>
where
    F: std::future::Future,
{
    // Only the delay matters here, so a timeout is as good as the answer.
    let _ = tokio::time::timeout(rejoin_timeout, answerable).await;
    let (_, rejoin) = tokio::join!(
        tokio::time::sleep(grace),
        wait_for_rejoin(receiver, rejoin_timeout),
    );
    rejoin
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn local_health_without_a_peer_expires_at_the_configured_deadline() {
        let (_sender, receiver) = watch::channel(false);
        let started = tokio::time::Instant::now();
        assert!(
            wait_for_rejoin(Some(receiver), Duration::from_secs(7))
                .await
                .is_err()
        );
        assert_eq!(started.elapsed(), Duration::from_secs(7));
    }

    #[tokio::test]
    async fn peer_acknowledgement_allows_verification() {
        let (sender, receiver) = watch::channel(false);
        let pending = tokio::spawn(wait_for_rejoin(Some(receiver), Duration::from_secs(7)));
        sender.send(true).unwrap();
        assert!(pending.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn stopped_gossip_cannot_commit() {
        let (sender, receiver) = watch::channel(false);
        drop(sender);
        assert!(
            wait_for_rejoin(Some(receiver), Duration::from_secs(7))
                .await
                .is_err()
        );
    }

    // #771: the verification clock measured the replacement's first
    // seconds of hashing its own binary, not its time to rejoin.
    #[tokio::test(start_paused = true)]
    async fn the_clock_starts_once_the_replacement_can_answer() {
        let (_sender, receiver) = watch::channel(false);
        let started = tokio::time::Instant::now();
        let verdict = verify_after_answerable(
            tokio::time::sleep(Duration::from_secs(3)),
            Duration::from_secs(2),
            Some(receiver),
            Duration::from_secs(5),
        )
        .await;
        assert!(verdict.is_err());
        assert_eq!(started.elapsed(), Duration::from_secs(8));
    }

    #[tokio::test(start_paused = true)]
    async fn a_replacement_that_never_becomes_answerable_starts_the_clock_after_one_rejoin_period()
    {
        let (_sender, receiver) = watch::channel(false);
        let started = tokio::time::Instant::now();
        let verdict = verify_after_answerable(
            std::future::pending::<()>(),
            Duration::from_secs(2),
            Some(receiver),
            Duration::from_secs(5),
        )
        .await;
        assert!(verdict.is_err());
        assert_eq!(started.elapsed(), Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn a_peer_acknowledgement_before_the_clock_starts_still_counts() {
        let (sender, receiver) = watch::channel(false);
        sender.send(true).unwrap();
        let started = tokio::time::Instant::now();
        let verdict = verify_after_answerable(
            tokio::time::sleep(Duration::from_secs(3)),
            Duration::from_secs(2),
            Some(receiver),
            Duration::from_secs(5),
        )
        .await;
        assert!(verdict.is_ok());
        // The boot grace still runs in full after the node can answer.
        assert_eq!(started.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn standalone_waits_out_the_grace_after_it_can_answer() {
        let started = tokio::time::Instant::now();
        let verdict = verify_after_answerable(
            tokio::time::sleep(Duration::from_secs(1)),
            Duration::from_secs(2),
            None,
            Duration::from_secs(5),
        )
        .await;
        assert!(verdict.is_ok());
        assert_eq!(started.elapsed(), Duration::from_secs(3));
    }

    #[tokio::test]
    async fn standalone_has_no_gossip_requirement() {
        assert!(wait_for_rejoin(None, Duration::ZERO).await.is_ok());
    }
}
