//! Agent leases on a person's direct writes (#1686): `relish apply`,
//! `deploy`, `stop`, `delete`, `rollback` and `release`.
//!
//! A person never needs a lease to change an app, but someone else's lease
//! on it stops them, by name: the answer is `lease_held`, naming the
//! holder, why and for how long, and saying how to take it over. Repeated
//! with `--break-lease` (the `x-reliaburger-break-lease: acknowledged`
//! header), the leader breaks each lease in the way, audited as
//! `lease.broken`, and makes the write.
//!
//! The check runs on the leader against its own leases, then the write
//! goes to the council wrapped in `RaftRequest::LeaseFenced`, carrying the
//! lease generation the leader checked at. If a lease was taken, released,
//! broken or expired in between, the state machine answers
//! `LeaseGenerationMoved` and the leader checks again. Renewing a lease
//! doesn't move the generation, so an agent renewing every minute never
//! makes a person's write retry. It's the policy generation fence of
//! chapter 18, used a second time.

use axum::http::HeaderMap;

use crate::council::types::RaftRequest;
use crate::council::{CouncilError, CouncilNode};
use crate::meat::AppId;
use crate::proposal::lease::AgentLease;

/// The header a person's write carries to break leases in its way.
pub const BREAK_LEASE_HEADER: &str = "x-reliaburger-break-lease";

/// How many times a fenced write is checked again after the leases moved
/// under it, before it gives up.
pub const LEASE_ATTEMPTS: usize = 5;

/// Whether `headers` ask to break leases: `x-reliaburger-break-lease:
/// acknowledged`. Anything else is a refusal the caller reports.
pub fn break_lease_requested(headers: &HeaderMap) -> Result<bool, String> {
    match headers.get(BREAK_LEASE_HEADER) {
        None => Ok(false),
        Some(value) if value.as_bytes() == b"acknowledged" => Ok(true),
        Some(_) => Err(format!("{BREAK_LEASE_HEADER} must equal acknowledged")),
    }
}

/// The leases that stop `principal_id` from writing `apps` at
/// `now_unix_ms`, one per lease, and the lease generation they were read
/// at.
pub async fn blockers(
    council: &CouncilNode,
    apps: &[AppId],
    principal_id: &str,
    now_unix_ms: u64,
) -> (u64, Vec<AgentLease>) {
    council
        .read_desired(|desired| {
            let leases = &desired.agent_leases;
            let mut found: Vec<AgentLease> = Vec::new();
            for app in apps {
                if let Some(lease) = leases.blocker(app, principal_id, now_unix_ms)
                    && !found.iter().any(|seen| seen.target == lease.target)
                {
                    found.push(lease.clone());
                }
            }
            (leases.generation, found)
        })
        .await
}

/// Break each of `leases` on behalf of `by`, answering the leases actually
/// broken. A lease that lapsed or was released in between is skipped.
pub async fn break_leases(
    council: &CouncilNode,
    leases: &[AgentLease],
    by: crate::proposal::lease::LeaseHolder,
) -> Result<Vec<AgentLease>, CouncilError> {
    let mut broken = Vec::new();
    for lease in leases {
        let response = council
            .write(RaftRequest::LeaseBreak {
                target: lease.target.clone(),
                by: by.clone(),
                observed_at_unix_ms: crate::testkit::lease::now_unix_millis(),
            })
            .await?;
        if let crate::council::CouncilResponse::AgentLease(outcome) = response
            && let crate::proposal::lease::LeaseOutcome::Broken { lease } = *outcome
        {
            broken.push(lease);
        }
    }
    Ok(broken)
}

/// `change` fenced on the leases at `lease_generation`.
pub fn fence(lease_generation: u64, change: RaftRequest) -> RaftRequest {
    RaftRequest::LeaseFenced {
        lease_generation,
        change: Box::new(change),
    }
}

/// The app a write changes, for a write the fence carries.
pub fn fenced_app(write: &RaftRequest) -> Option<&AppId> {
    match write {
        RaftRequest::AppSpec { app_id, .. }
        | RaftRequest::AppStop { app_id }
        | RaftRequest::AppDelete { app_id }
        | RaftRequest::AppRelease { app_id } => Some(app_id),
        RaftRequest::PolicyFenced { change, .. } => match change.as_ref() {
            RaftRequest::AppSpec { app_id, .. } => Some(app_id),
            _ => None,
        },
        _ => None,
    }
}

/// The sentence a refused write answers with.
pub fn held_message(lease: &AgentLease, now_unix_ms: u64) -> String {
    format!(
        "{} is held by {} for {}s more ({}); repeat with --break-lease to take it over",
        lease.target,
        lease.holder.name,
        lease.expires_at_unix_ms.saturating_sub(now_unix_ms) / 1000,
        lease.purpose
    )
}

/// The JSON body of a `lease_held` refusal: the message, the code an agent
/// branches on, and the lease.
pub fn held_body(lease: &AgentLease, now_unix_ms: u64) -> serde_json::Value {
    serde_json::json!({
        "error": held_message(lease, now_unix_ms),
        "code": "lease_held",
        "lease": serde_json::to_value(lease.view(now_unix_ms)).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(holder: &str) -> AgentLease {
        AgentLease {
            target: crate::proposal::lease::LeaseTarget::app("shop", "web"),
            holder: crate::proposal::lease::LeaseHolder {
                name: holder.to_string(),
                principal_id: format!("token:{holder}"),
            },
            purpose: "rolling back".to_string(),
            proposal: None,
            acquired_at_unix_ms: 0,
            expires_at_unix_ms: 90_000,
        }
    }

    #[test]
    fn the_header_must_say_acknowledged() {
        let mut headers = HeaderMap::new();
        assert_eq!(break_lease_requested(&headers), Ok(false));
        headers.insert(BREAK_LEASE_HEADER, "acknowledged".parse().unwrap());
        assert_eq!(break_lease_requested(&headers), Ok(true));
        headers.insert(BREAK_LEASE_HEADER, "yes".parse().unwrap());
        assert!(break_lease_requested(&headers).is_err());
    }

    #[test]
    fn a_refusal_names_the_holder_and_how_to_take_it_over() {
        let body = held_body(&lease("holmes"), 30_000);
        assert_eq!(body["code"], "lease_held");
        assert_eq!(body["lease"]["holder"], "holmes");
        assert_eq!(
            body["error"],
            "app:shop/web is held by holmes for 60s more (rolling back); repeat with \
             --break-lease to take it over"
        );
    }

    #[test]
    fn the_fence_carries_only_app_writes() {
        let web = AppId::new("web", "shop");
        let stop = RaftRequest::AppStop {
            app_id: web.clone(),
        };
        assert_eq!(fenced_app(&stop), Some(&web));
        assert!(fenced_app(&RaftRequest::PolicyDelete { name: "x".into() }).is_none());
        assert!(matches!(
            fence(3, stop),
            RaftRequest::LeaseFenced {
                lease_generation: 3,
                ..
            }
        ));
    }
}
