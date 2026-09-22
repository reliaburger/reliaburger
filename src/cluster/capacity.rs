//! Bounded requests to the live leader's scheduling loop.

use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::config::app::AppSpec;
use crate::meat::scheduler::ScheduleError;
use crate::meat::types::AppId;

/// Capacity refusal returned before a benchmark app enters desired state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulingRefusal {
    /// Stable scheduler error, independent of its human-readable wording.
    pub error: ScheduleError,
}

/// Failure to obtain admission from the current leader's scheduler.
#[derive(Debug, thiserror::Error)]
pub enum CapacityAdmissionError {
    /// Missing, stale or interrupted scheduler evidence is not saturation.
    #[error("capacity admission unavailable: {0}")]
    Unavailable(String),
    /// A live planning pass explicitly rejected the requested app.
    #[error("{0}")]
    Rejected(ScheduleError),
}

/// A bounded request handle owned by the API while the scheduler is running.
#[derive(Clone)]
pub struct CapacityAdmission {
    sender: mpsc::Sender<CapacityRequest>,
}

/// A single observation request consumed by the leader loop.
pub(crate) struct CapacityRequest {
    pub app_id: AppId,
    pub spec: AppSpec,
    pub response: oneshot::Sender<Result<(), CapacityAdmissionError>>,
}

/// Build a bounded API-to-scheduler channel.
pub(crate) fn admission_channel() -> (CapacityAdmission, mpsc::Receiver<CapacityRequest>) {
    let (sender, receiver) = mpsc::channel(8);
    (CapacityAdmission { sender }, receiver)
}

impl CapacityAdmission {
    /// Ask the live scheduling loop to admit one new app, within five seconds.
    ///
    /// This is a planning observation, not a resource reservation or proof that
    /// the runtime launched the app. Callers must observe the eventual workload.
    pub async fn check(
        &self,
        app_id: &AppId,
        spec: &AppSpec,
    ) -> Result<(), CapacityAdmissionError> {
        if spec.replicas != crate::config::Replicas::Fixed(1) {
            return Err(CapacityAdmissionError::Rejected(
                ScheduleError::InvalidSpec {
                    reason: "capacity admission requires one replica".into(),
                },
            ));
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            let (response, receiver) = oneshot::channel();
            self.sender
                .send(CapacityRequest {
                    app_id: app_id.clone(),
                    spec: spec.clone(),
                    response,
                })
                .await
                .map_err(|_| CapacityAdmissionError::Unavailable("scheduler stopped".into()))?;
            receiver.await.map_err(|_| {
                CapacityAdmissionError::Unavailable(
                    "leader, reconstruction or capacity evidence is not ready".into(),
                )
            })?
        })
        .await
        .map_err(|_| CapacityAdmissionError::Unavailable("scheduler response timed out".into()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stopped_or_interrupted_scheduler_never_reports_saturation() {
        let config = crate::config::Config::parse("[app.probe]\nimage = \"busybox\"\n").unwrap();
        let app = AppId::new("probe", "default");
        let spec = &config.app["probe"];
        let (admission, receiver) = admission_channel();
        drop(receiver);
        assert!(matches!(
            admission.check(&app, spec).await,
            Err(CapacityAdmissionError::Unavailable(_))
        ));

        let (admission, mut receiver) = admission_channel();
        let server = tokio::spawn(async move {
            drop(receiver.recv().await.unwrap());
        });
        assert!(matches!(
            admission.check(&app, spec).await,
            Err(CapacityAdmissionError::Unavailable(_))
        ));
        server.await.unwrap();
    }
}
