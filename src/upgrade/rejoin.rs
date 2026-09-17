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

    #[tokio::test]
    async fn standalone_has_no_gossip_requirement() {
        assert!(wait_for_rejoin(None, Duration::ZERO).await.is_ok());
    }
}
