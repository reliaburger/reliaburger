//! Bounded reads of the runtime's launch inventory.

use std::time::Duration;

use super::{BunAgent, BunError, Grill};
use crate::grill::RuntimeLaunch;

/// How long an inventory read may take before the caller gives up on a
/// wedged runtime.
pub(super) const RUNTIME_INVENTORY_TIMEOUT: Duration = Duration::from_secs(5);

/// A shorter bound for reads that run inside the agent loop's own turn, so
/// a slow runtime cannot stall reports or command handling.
pub(super) const LOOP_RUNTIME_INVENTORY_TIMEOUT: Duration = Duration::from_secs(1);

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Read the runtime's launch inventory within `deadline`.
    ///
    /// `Ok(None)` means the runtime cannot enumerate its launches. A timeout
    /// returns the error `refuse` builds from a short description.
    pub(super) async fn runtime_inventory(
        &self,
        deadline: Duration,
        refuse: impl FnOnce(String) -> BunError,
    ) -> Result<Option<Vec<RuntimeLaunch>>, BunError> {
        match tokio::time::timeout(deadline, self.supervisor.grill().launch_inventory()).await {
            Ok(launches) => Ok(launches?),
            Err(_) => Err(refuse("runtime inventory timed out".into())),
        }
    }

    /// Like [`Self::runtime_inventory`], but an incomplete inventory is an
    /// error too, built by `refuse`.
    pub(super) async fn complete_runtime_inventory(
        &self,
        deadline: Duration,
        refuse: impl Fn(String) -> BunError,
    ) -> Result<Vec<RuntimeLaunch>, BunError> {
        self.runtime_inventory(deadline, &refuse)
            .await?
            .ok_or_else(|| refuse("runtime inventory is incomplete".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grill::mock::MockGrill;
    use crate::grill::port::PortAllocator;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn agent(grill: MockGrill) -> BunAgent<MockGrill> {
        let (_, receiver) = mpsc::channel(1);
        BunAgent::new(
            grill,
            PortAllocator::new(30000, 31000),
            receiver,
            CancellationToken::new(),
        )
    }

    fn refuse(reason: String) -> BunError {
        BunError::AdoptionState(format!("test {reason}"))
    }

    #[tokio::test(start_paused = true)]
    async fn wedged_runtime_inventory_is_refused_at_the_deadline() {
        let grill = MockGrill::new();
        grill.set_inventory_delay(Some(Duration::from_secs(300)));
        let result = agent(grill)
            .runtime_inventory(RUNTIME_INVENTORY_TIMEOUT, refuse)
            .await;
        assert!(
            matches!(result, Err(BunError::AdoptionState(ref reason)) if reason == "test runtime inventory timed out")
        );
    }

    #[tokio::test]
    async fn runtime_without_inventory_is_incomplete_only_when_completeness_is_required() {
        let agent = agent(MockGrill::new());
        assert!(matches!(
            agent
                .runtime_inventory(RUNTIME_INVENTORY_TIMEOUT, refuse)
                .await,
            Ok(None)
        ));
        assert!(matches!(
            agent.complete_runtime_inventory(RUNTIME_INVENTORY_TIMEOUT, refuse).await,
            Err(BunError::AdoptionState(ref reason)) if reason == "test runtime inventory is incomplete"
        ));
    }

    #[tokio::test]
    async fn complete_runtime_inventory_returns_the_launches() {
        let grill = MockGrill::new();
        grill.set_launch_inventory(Vec::new()).await;
        let launches = agent(grill)
            .complete_runtime_inventory(RUNTIME_INVENTORY_TIMEOUT, refuse)
            .await
            .unwrap();
        assert!(launches.is_empty());
    }
}
