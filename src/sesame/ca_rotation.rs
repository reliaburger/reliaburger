//! Rotating an intermediate CA (F04 R1).
//!
//! A rotation has two Raft steps, the same shape as secret key rotation:
//!
//! 1. **Begin** (`RaftRequest::CaRotationBegin`) adds the new CA as `Active`
//!    and marks the old one `Retiring`. Verifiers trust both; only the new one
//!    signs.
//! 2. **Finalise** (`RaftRequest::CaRotationFinalize`) removes the retiring CA,
//!    and refuses while anything could still depend on it.
//!
//! The functions here are the state machine's rules. They take and return
//! plain values so every replica applies them identically: no clock reads,
//! no randomness, only what the log entry carries.

use std::time::{Duration, SystemTime};

use super::types::{CaRole, CaState, CertificateAuthority, NodeLeafRecord, SecurityState};

/// Why the council refused a rotation step. The state is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaRotationError {
    /// Rotating the root is R5's job, not this one's.
    #[error("rotating the root CA is not supported yet; rotate an intermediate")]
    RootRotationUnsupported,
    /// The request names one role and carries a CA of another.
    #[error("the new CA is a {found} CA, but the rotation is for the {expected} CA")]
    RoleMismatch { expected: CaRole, found: CaRole },
    /// The state has no active CA of this role to rotate away from.
    #[error("there is no active {0} CA to rotate")]
    NoActiveCa(CaRole),
    /// An earlier rotation of the role hasn't been finalised.
    #[error("a {0} CA rotation is already in progress: finalise it before starting another")]
    AlreadyInProgress(CaRole),
    /// Generations go up by one per rotation, so a gap or a step back means
    /// the proposer built the CA against a state that has moved on.
    #[error("the new {role} CA must be generation {expected}, not {found}")]
    WrongGeneration {
        role: CaRole,
        expected: u64,
        found: u64,
    },
    /// The council can't sign leaves with a CA whose key it doesn't hold.
    #[error("the new {0} CA carries no wrapped private key")]
    MissingKey(CaRole),
    /// The new CA doesn't chain to the cluster's root, so nothing would
    /// trust what it signs.
    #[error("the new {role} CA is not signed by the active root: {reason}")]
    NotSignedByRoot { role: CaRole, reason: String },
    /// Finalise with nothing to finalise.
    #[error("no {0} CA rotation is in progress")]
    NotInProgress(CaRole),
    /// Nodes still hold leaves the retiring Node CA signed.
    #[error(
        "cannot finalise the Node CA rotation: these nodes still hold leaves from the retiring CA \
         (renew them first): {}",
        .0.join(", ")
    )]
    LiveNodeLeaves(Vec<String>),
    /// The role's leaves aren't tracked one by one, so finalise waits until
    /// every leaf the retiring CA could have signed has expired.
    #[error(
        "cannot finalise the {role} CA rotation yet: leaves from the retiring CA may be valid \
         until {until_unix_secs} (unix seconds)"
    )]
    LeavesMayStillBeValid { role: CaRole, until_unix_secs: u64 },
}

/// What applying a begin did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeginOutcome {
    /// The new CA is active and the old one is retiring.
    Started,
    /// The same generation was already applied: a retried proposal. Nothing
    /// changed.
    AlreadyApplied,
}

/// The longest a leaf signed by a CA of `role` lives. A retiring CA has to
/// stay trusted at least this long after the rotation begins, unless finalise
/// can prove its leaves moved.
pub fn longest_leaf_lifetime(role: CaRole) -> Duration {
    match role {
        CaRole::Node => super::ca::NODE_LEAF_LIFETIME,
        CaRole::Ingress => super::ca::INGRESS_LEAF_LIFETIME,
        CaRole::Workload => super::identity::WORKLOAD_CERT_LIFETIME,
        // A root signs intermediates, which live five years. Root rotation
        // is refused for now, so this only keeps the match exhaustive.
        CaRole::Root => Duration::from_secs(5 * 365 * 24 * 3600),
    }
}

/// Start rotating `role` to `new_ca`.
///
/// The old active CA becomes `Retiring` until the longest leaf it could have
/// signed expires, counted from the new CA's `not_before` (the moment it was
/// made, less the clock-skew backdate, which we add back). Using a time from
/// the certificate rather than the clock keeps every replica in step.
pub fn begin(
    state: &mut SecurityState,
    role: CaRole,
    new_ca: &CertificateAuthority,
) -> Result<BeginOutcome, CaRotationError> {
    if role == CaRole::Root {
        return Err(CaRotationError::RootRotationUnsupported);
    }
    if new_ca.role != role {
        return Err(CaRotationError::RoleMismatch {
            expected: role,
            found: new_ca.role,
        });
    }
    // A retry of the same rotation is deduplicated on its generation, like
    // `RotateSecretKey`: the first-applied CA stays.
    if state
        .certificate_authorities
        .iter()
        .any(|ca| ca.role == role && ca.generation == new_ca.generation)
    {
        return Ok(BeginOutcome::AlreadyApplied);
    }
    if state.retiring_ca(role).is_some() {
        return Err(CaRotationError::AlreadyInProgress(role));
    }
    let active = state
        .active_ca(role)
        .ok_or(CaRotationError::NoActiveCa(role))?;
    let expected = active.generation + 1;
    if new_ca.generation != expected {
        return Err(CaRotationError::WrongGeneration {
            role,
            expected,
            found: new_ca.generation,
        });
    }
    if new_ca.private_key_wrapped.is_none() {
        return Err(CaRotationError::MissingKey(role));
    }
    let root = state
        .active_ca(CaRole::Root)
        .ok_or(CaRotationError::NoActiveCa(CaRole::Root))?;
    super::cert::verify_signature(&new_ca.certificate_der, &root.certificate_der)
        .and_then(|()| {
            super::cert::check_issuer_binding(&new_ca.certificate_der, &root.certificate_der)
        })
        .map_err(|error| CaRotationError::NotSignedByRoot {
            role,
            reason: error.to_string(),
        })?;

    let until = new_ca.not_before + super::ca::CLOCK_SKEW_BACKDATE + longest_leaf_lifetime(role);
    let active_generation = active.generation;
    for ca in &mut state.certificate_authorities {
        if ca.role == role && ca.generation == active_generation {
            ca.state = CaState::Retiring { until };
        }
    }
    state.certificate_authorities.push(CertificateAuthority {
        state: CaState::Active,
        ..new_ca.clone()
    });
    Ok(BeginOutcome::Started)
}

/// Finish rotating `role`: drop the retiring CA.
///
/// Refused while a leaf could still depend on it. For the Node CA the council
/// knows every node's latest leaf ([`SecurityState::node_leaves`]), so it can
/// prove the nodes moved before the window ends. Workload and ingress leaves
/// aren't tracked one by one, so for those finalise waits out the window.
/// `now` comes from the log entry, never the clock.
pub fn finalize(
    state: &mut SecurityState,
    role: CaRole,
    now: SystemTime,
) -> Result<(), CaRotationError> {
    let retiring = state
        .retiring_ca(role)
        .ok_or(CaRotationError::NotInProgress(role))?;
    let CaState::Retiring { until } = retiring.state else {
        return Err(CaRotationError::NotInProgress(role));
    };
    if now < until {
        if role != CaRole::Node {
            return Err(CaRotationError::LeavesMayStillBeValid {
                role,
                until_unix_secs: until
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            });
        }
        let live = nodes_on_retiring_ca(state, retiring.generation);
        if !live.is_empty() {
            return Err(CaRotationError::LiveNodeLeaves(live));
        }
    }
    state
        .certificate_authorities
        .retain(|ca| ca.role != role || ca.state == CaState::Active);
    Ok(())
}

/// The nodes whose latest leaf a Node CA of `generation` (or older) signed.
/// A decommissioned node is fenced by the CRL whatever its leaf says, so it
/// doesn't hold a rotation up.
fn nodes_on_retiring_ca(state: &SecurityState, generation: u64) -> Vec<String> {
    state
        .node_leaves
        .iter()
        .filter(|(node_id, leaf)| {
            leaf.ca_generation <= generation && !state.crl.retired_nodes.contains_key(*node_id)
        })
        .map(|(node_id, _)| node_id.clone())
        .collect()
}

/// Record the serial the council just allocated for a node's leaf, under the
/// Node CA that is active now. The signer reads the state after this commit,
/// so it signs with this CA or, if a rotation begins in between, a newer one.
/// Either way the record never claims a newer CA than the leaf really has.
pub fn record_node_leaf(state: &mut SecurityState, node_id: &str, serial: u64) {
    let ca_generation = state
        .active_ca(CaRole::Node)
        .map(|ca| ca.generation)
        .unwrap_or_default();
    state.node_leaves.insert(
        node_id.to_string(),
        NodeLeafRecord {
            serial: super::types::SerialNumber(serial),
            ca_generation,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sesame::ca::{self, CaHierarchy, generate_ca_hierarchy, generate_intermediate_ca};
    use crate::sesame::types::SerialNumber;

    const IKM: &[u8] = b"rotation-test-ikm";

    fn hierarchy() -> CaHierarchy {
        generate_ca_hierarchy("rotation", IKM).unwrap()
    }

    fn state_from(hierarchy: &CaHierarchy) -> SecurityState {
        SecurityState {
            certificate_authorities: vec![
                CertificateAuthority {
                    private_key_wrapped: None,
                    ..hierarchy.root.ca.clone()
                },
                hierarchy.node.ca.clone(),
                hierarchy.workload.ca.clone(),
                hierarchy.ingress.ca.clone(),
            ],
            next_serial: 10,
            ..SecurityState::default()
        }
    }

    /// A fresh intermediate for `role`, signed by the hierarchy's root.
    fn successor(hierarchy: &CaHierarchy, role: CaRole, generation: u64) -> CertificateAuthority {
        let generated = generate_intermediate_ca(
            role,
            "rotation",
            SerialNumber(100 + generation),
            hierarchy.root.ca.serial,
            &hierarchy.root.signing_keypair,
            &hierarchy.root.certificate_params,
            IKM,
        )
        .unwrap();
        CertificateAuthority {
            generation,
            ..generated.ca
        }
    }

    #[test]
    fn begin_makes_the_new_ca_active_and_the_old_one_retiring() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        let new_ca = successor(&hierarchy, CaRole::Node, 1);

        assert_eq!(
            begin(&mut state, CaRole::Node, &new_ca),
            Ok(BeginOutcome::Started)
        );

        let active = state.active_ca(CaRole::Node).unwrap();
        assert_eq!(active.generation, 1, "the generation increments");
        assert_eq!(active.certificate_der, new_ca.certificate_der);
        let retiring = state.retiring_ca(CaRole::Node).unwrap();
        assert_eq!(retiring.generation, 0);
        let CaState::Retiring { until } = retiring.state else {
            panic!("the old CA must be retiring");
        };
        assert_eq!(
            until,
            new_ca.not_before + crate::sesame::ca::CLOCK_SKEW_BACKDATE + ca::NODE_LEAF_LIFETIME
        );
        let trusted: Vec<u64> = state
            .trusted_cas(CaRole::Node)
            .iter()
            .map(|ca| ca.generation)
            .collect();
        assert_eq!(trusted, [1, 0], "both are trusted, the active one first");
        // Other roles are untouched.
        assert_eq!(state.trusted_cas(CaRole::Workload).len(), 1);
    }

    #[test]
    fn a_retried_begin_of_the_same_generation_changes_nothing() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        let new_ca = successor(&hierarchy, CaRole::Workload, 1);
        begin(&mut state, CaRole::Workload, &new_ca).unwrap();
        let after_first = state.clone();

        // A different CA with the same generation is the same proposal
        // retried: the first-applied one stays.
        let retry = successor(&hierarchy, CaRole::Workload, 1);
        assert_eq!(
            begin(&mut state, CaRole::Workload, &retry),
            Ok(BeginOutcome::AlreadyApplied)
        );
        assert_eq!(state, after_first);
    }

    #[test]
    fn stacked_rotations_are_refused() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        begin(
            &mut state,
            CaRole::Node,
            &successor(&hierarchy, CaRole::Node, 1),
        )
        .unwrap();
        let before = state.clone();

        let third = successor(&hierarchy, CaRole::Node, 2);
        assert_eq!(
            begin(&mut state, CaRole::Node, &third),
            Err(CaRotationError::AlreadyInProgress(CaRole::Node))
        );
        assert_eq!(state, before, "a refusal changes nothing");

        // Another role rotates independently.
        assert_eq!(
            begin(
                &mut state,
                CaRole::Ingress,
                &successor(&hierarchy, CaRole::Ingress, 1)
            ),
            Ok(BeginOutcome::Started)
        );
    }

    #[test]
    fn begin_refuses_a_skipped_generation() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        assert_eq!(
            begin(
                &mut state,
                CaRole::Node,
                &successor(&hierarchy, CaRole::Node, 2)
            ),
            Err(CaRotationError::WrongGeneration {
                role: CaRole::Node,
                expected: 1,
                found: 2
            })
        );
    }

    #[test]
    fn begin_refuses_a_ca_another_root_signed() {
        let hierarchy = hierarchy();
        let foreign = generate_ca_hierarchy("someone-else", IKM).unwrap();
        let mut state = state_from(&hierarchy);
        let impostor = CertificateAuthority {
            generation: 1,
            ..foreign.node.ca.clone()
        };
        assert!(matches!(
            begin(&mut state, CaRole::Node, &impostor),
            Err(CaRotationError::NotSignedByRoot { .. })
        ));
    }

    #[test]
    fn begin_refuses_the_root_a_mismatched_role_and_a_keyless_ca() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        let node = successor(&hierarchy, CaRole::Node, 1);
        assert_eq!(
            begin(&mut state, CaRole::Root, &node),
            Err(CaRotationError::RootRotationUnsupported)
        );
        assert_eq!(
            begin(&mut state, CaRole::Workload, &node),
            Err(CaRotationError::RoleMismatch {
                expected: CaRole::Workload,
                found: CaRole::Node
            })
        );
        let keyless = CertificateAuthority {
            private_key_wrapped: None,
            ..node
        };
        assert_eq!(
            begin(&mut state, CaRole::Node, &keyless),
            Err(CaRotationError::MissingKey(CaRole::Node))
        );
    }

    #[test]
    fn finalise_is_refused_while_a_live_leaf_chains_to_the_retiring_ca() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        record_node_leaf(&mut state, "node-a", 5);
        record_node_leaf(&mut state, "node-b", 6);
        let new_ca = successor(&hierarchy, CaRole::Node, 1);
        begin(&mut state, CaRole::Node, &new_ca).unwrap();
        let during = new_ca.not_before + Duration::from_secs(60);

        // node-a renews under the new CA; node-b still holds its old leaf.
        record_node_leaf(&mut state, "node-a", 11);
        assert_eq!(
            finalize(&mut state, CaRole::Node, during),
            Err(CaRotationError::LiveNodeLeaves(vec!["node-b".into()]))
        );
        assert!(state.retiring_ca(CaRole::Node).is_some());

        // Once node-b renews, nothing depends on the retiring CA.
        record_node_leaf(&mut state, "node-b", 12);
        assert_eq!(finalize(&mut state, CaRole::Node, during), Ok(()));
        let trusted = state.trusted_cas(CaRole::Node);
        assert_eq!(trusted.len(), 1);
        assert_eq!(trusted[0].generation, 1);
    }

    #[test]
    fn a_decommissioned_node_does_not_hold_up_finalise() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        record_node_leaf(&mut state, "gone", 5);
        let new_ca = successor(&hierarchy, CaRole::Node, 1);
        begin(&mut state, CaRole::Node, &new_ca).unwrap();
        state.crl.retired_nodes.insert(
            "gone".into(),
            crate::cluster::retirement::NodeRetirement {
                node_id: "gone".into(),
                retired_by: "test".into(),
                reason: "test".into(),
                retired_at_unix_ms: 0,
                released_placements: Default::default(),
                released_registry_writers: Default::default(),
                released_node_fault: None,
                released_endpoint_consumer: false,
            },
        );
        assert_eq!(
            finalize(&mut state, CaRole::Node, new_ca.not_before),
            Ok(())
        );
    }

    #[test]
    fn finalise_waits_out_the_window_for_untracked_leaves() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        let new_ca = successor(&hierarchy, CaRole::Workload, 1);
        begin(&mut state, CaRole::Workload, &new_ca).unwrap();
        let until = new_ca.not_before
            + crate::sesame::ca::CLOCK_SKEW_BACKDATE
            + crate::sesame::identity::WORKLOAD_CERT_LIFETIME;

        assert!(matches!(
            finalize(&mut state, CaRole::Workload, until - Duration::from_secs(1)),
            Err(CaRotationError::LeavesMayStillBeValid {
                role: CaRole::Workload,
                ..
            })
        ));
        assert_eq!(finalize(&mut state, CaRole::Workload, until), Ok(()));
        assert!(state.retiring_ca(CaRole::Workload).is_none());
    }

    #[test]
    fn finalise_without_a_rotation_is_refused() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        assert_eq!(
            finalize(&mut state, CaRole::Node, SystemTime::now()),
            Err(CaRotationError::NotInProgress(CaRole::Node))
        );
    }

    #[test]
    fn a_leaf_allocated_before_the_rotation_is_recorded_under_the_old_generation() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        record_node_leaf(&mut state, "node-a", 7);
        assert_eq!(
            state.node_leaves["node-a"],
            NodeLeafRecord {
                serial: SerialNumber(7),
                ca_generation: 0
            }
        );
        begin(
            &mut state,
            CaRole::Node,
            &successor(&hierarchy, CaRole::Node, 1),
        )
        .unwrap();
        record_node_leaf(&mut state, "node-a", 8);
        assert_eq!(state.node_leaves["node-a"].ca_generation, 1);
    }
}
