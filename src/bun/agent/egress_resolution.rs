//! DNS re-resolution of egress allowlists, off the agent loop (#351,
//! stage 3).
//!
//! Every five minutes the tick re-resolves each owned binding's allowlist
//! and reprograms the eBPF egress maps when a hostname's addresses changed
//! (L16). The lookups used to run on the loop, one binding and one host at
//! a time: milliseconds with a healthy resolver, five to ten seconds a host
//! against one that doesn't answer. Now the tick takes a snapshot of the
//! bindings and a task resolves them all at once, each under
//! [`RESOLUTION_TIMEOUT`]. The loop applies what came back through a
//! follow-up, but only to a binding that is still the one the task resolved:
//! an instance that retired, restarted into a new cgroup or changed its
//! allowlist meanwhile keeps whatever the loop has done to it since.
#![cfg_attr(not(all(feature = "ebpf", target_os = "linux")), allow(dead_code))]

use std::time::Duration;

use super::InstanceId;
use crate::bun::egress_owners::{EgressBinding, PolicyPhase};
use crate::sesame::egress::EgressDestination;

/// How long one binding's lookups may take before the task gives up on it
/// until the next re-resolution.
pub(super) const RESOLUTION_TIMEOUT: Duration = Duration::from_secs(10);

/// One owned binding's allowlist, as the tick saw it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ResolutionRequest {
    pub(super) instance_id: InstanceId,
    pub(super) cgroup_id: u64,
    pub(super) allow: Vec<String>,
}

/// What resolving one allowlist produced.
#[derive(Debug)]
pub(super) struct Resolution {
    pub(super) request: ResolutionRequest,
    pub(super) resolved: Result<Vec<EgressDestination>, String>,
}

/// The owned bindings whose allowlists a re-resolution covers.
pub(super) fn requests<'a>(
    bindings: impl IntoIterator<Item = (&'a InstanceId, &'a EgressBinding)>,
) -> Vec<ResolutionRequest> {
    bindings
        .into_iter()
        .filter(|(_, binding)| binding.phase == PolicyPhase::Owned)
        .map(|(instance_id, binding)| ResolutionRequest {
            instance_id: instance_id.clone(),
            cgroup_id: binding.cgroup_id,
            allow: binding.allow.clone(),
        })
        .collect()
}

/// Resolve every allowlist at once, each under [`RESOLUTION_TIMEOUT`].
pub(super) async fn resolve(requests: Vec<ResolutionRequest>) -> Vec<Resolution> {
    let lookups = requests.into_iter().map(|request| async move {
        let resolved = match tokio::time::timeout(
            RESOLUTION_TIMEOUT,
            crate::sesame::egress::re_resolve_egress_async(&request.allow),
        )
        .await
        {
            Ok(Ok(resolved)) => Ok(resolved),
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err(format!(
                "lookups did not finish within {}s",
                RESOLUTION_TIMEOUT.as_secs()
            )),
        };
        Resolution { request, resolved }
    });
    futures_util::future::join_all(lookups).await
}

/// Whether `binding` is still the one `request` resolved: owned, in the
/// same cgroup, with the same allowlist.
pub(super) fn still_current(binding: Option<&EgressBinding>, request: &ResolutionRequest) -> bool {
    binding.is_some_and(|binding| {
        binding.phase == PolicyPhase::Owned
            && binding.cgroup_id == request.cgroup_id
            && binding.allow == request.allow
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(allow: &[&str]) -> ResolutionRequest {
        ResolutionRequest {
            instance_id: InstanceId("default__web-0".into()),
            cgroup_id: 42,
            allow: allow.iter().map(|entry| entry.to_string()).collect(),
        }
    }

    #[tokio::test]
    async fn every_allowlist_is_resolved_and_a_failure_stays_with_its_binding() {
        let resolutions = resolve(vec![
            request(&["203.0.113.1:443"]),
            request(&["not a destination"]),
        ])
        .await;
        assert_eq!(resolutions.len(), 2);
        assert!(
            resolutions[0]
                .resolved
                .as_ref()
                .is_ok_and(|r| !r.is_empty())
        );
        assert!(resolutions[1].resolved.is_err());
        assert_eq!(resolutions[1].request, request(&["not a destination"]));
    }

    #[test]
    fn a_result_applies_only_to_the_binding_it_was_resolved_for() {
        use crate::bun::egress_owners::owned_test_binding;

        let resolved = request(&["example.com:443"]);
        let binding = owned_test_binding(42, vec!["example.com:443".into()]);
        assert!(still_current(Some(&binding), &resolved));
        assert!(!still_current(None, &resolved), "the instance retired");

        let restarted = owned_test_binding(43, vec!["example.com:443".into()]);
        assert!(
            !still_current(Some(&restarted), &resolved),
            "the instance restarted into another cgroup"
        );
        let redeployed = owned_test_binding(42, vec!["example.org:443".into()]);
        assert!(
            !still_current(Some(&redeployed), &resolved),
            "the allowlist changed"
        );
        let mut retired = binding.clone();
        retired.phase = PolicyPhase::Retired;
        assert!(
            !still_current(Some(&retired), &resolved),
            "the owner retired"
        );
    }

    #[test]
    fn only_owned_bindings_are_re_resolved() {
        use crate::bun::egress_owners::owned_test_binding;

        let owned = owned_test_binding(42, vec!["example.com:443".into()]);
        let mut retired = owned_test_binding(43, vec!["example.org:443".into()]);
        retired.phase = PolicyPhase::Retired;
        let (web, api) = (
            InstanceId("default__web-0".into()),
            InstanceId("default__api-0".into()),
        );
        let planned = requests([(&web, &owned), (&api, &retired)]);
        assert_eq!(planned, vec![request(&["example.com:443"])]);
    }
}
