//! Seed whole-pass admission entries the way a planner commits them.
//!
//! `BatchRegister` and `SchedulingDecisions` carry the Raft log id their plan
//! was made against, and the state machine refuses the entry with "admission
//! revision changed" if any other entry was applied in between. That is the
//! designed contract: the ordinary scheduler replans from committed state on
//! its next tick. Tests that seed these entries beside a running API share the
//! leader with its task-array loop, whose first tick commits a cron
//! observation (#735), so the fixture replans the same way instead of
//! assuming nobody else writes.

use reliaburger::council::{CouncilError, CouncilNode, CouncilResponse, RaftRequest};

/// The state machine's refusal when another entry landed after the plan.
const REVISION_CHANGED: &str = "admission revision changed";

/// How many plans the fixture makes before it returns the refusal. A test
/// that sees the refusal has a writer that never pauses, not a slow one.
const MAX_PLANS: usize = 8;

/// Commit `request` against the revision read immediately before it,
/// replanning when another entry was applied in between. Every other
/// response, a refusal included, is returned unchanged.
pub(crate) async fn write_admission_fixture(
    council: &CouncilNode,
    request: RaftRequest,
) -> Result<CouncilResponse, CouncilError> {
    let mut plans = 0;
    loop {
        plans += 1;
        let previous = council.desired_state().await.last_applied_log;
        let planned = match request.clone() {
            RaftRequest::BatchRegister { batch, .. } => RaftRequest::BatchRegister {
                expected_log_id: previous,
                batch,
            },
            RaftRequest::SchedulingDecision(decision) => RaftRequest::SchedulingDecisions {
                expected_log_id: previous,
                decisions: vec![decision],
            },
            other => other,
        };
        let response = council.write(planned).await?;
        match &response {
            CouncilResponse::Refused { reason }
                if reason == REVISION_CHANGED && plans < MAX_PLANS =>
            {
                continue;
            }
            _ => return Ok(response),
        }
    }
}
