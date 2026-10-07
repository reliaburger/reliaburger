//! Rotating an intermediate CA (F04 R1 and R4).
//!
//! A rotation has three Raft steps; the last two have the same shape as
//! secret key rotation:
//!
//! 1. **Prepare** (`RaftRequest::CaRotationPrepare`, R4) holds a key the
//!    council made and its CSR, which `relish ca rotate` signs with the root
//!    key the operator keeps. The root key never reaches the cluster.
//! 2. **Begin** (`RaftRequest::CaRotationBegin`) adds the new CA as `Active`
//!    and marks the old one `Retiring`. Verifiers trust both; only the new one
//!    signs.
//! 3. **Finalise** (`RaftRequest::CaRotationFinalize`) removes the retiring CA,
//!    and refuses while anything could still depend on it.
//!
//! Between begin and finalise, each node acknowledges the new trust set
//! (`RaftRequest::AcknowledgeNodeTrust`), and once all have, the nodes renew
//! onto a Node CA one at a time ([`early_renewal_due`]).
//!
//! The state machine's rules here take and return plain values so every
//! replica applies them identically: no clock reads, no randomness, only
//! what the log entry carries.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::trust::TrustSet;
use super::types::{
    CaRole, CaState, CertificateAuthority, NodeLeafRecord, PendingIntermediate, SecurityState,
    SerialNumber, WrappedKey,
};

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
    /// Nodes that haven't said they trust the new Node CA. Dropping the old
    /// one wouldn't hurt them, but a node that refuses new leaves means the
    /// rotation hasn't worked, so finalise waits for them.
    #[error(
        "cannot finalise the Node CA rotation: these nodes have not acknowledged the new trust \
         set (decommission a node that is gone for good): {}",
        .0.join(", ")
    )]
    UnacknowledgedTrust(Vec<String>),
    /// A signed certificate arrived with no CSR waiting for it.
    #[error("no {0} CA certificate request is waiting: run `relish ca rotate` to start one")]
    NoPendingRequest(CaRole),
    /// The certificate certifies a key other than the one the council made.
    #[error("the signed {0} CA certificate is not for the key the council generated")]
    KeyMismatch(CaRole),
    /// The certificate carries a serial other than the one the council
    /// allocated, which could collide with a revoked one.
    #[error("the signed {role} CA certificate has serial {found}, not the allocated {expected}")]
    SerialMismatch {
        role: CaRole,
        expected: u64,
        found: u64,
    },
    /// The certificate couldn't be read, or isn't a CA certificate.
    #[error("the signed {role} CA certificate is unusable: {reason}")]
    InvalidCertificate { role: CaRole, reason: String },
    /// A trust acknowledgement from a node the council has no leaf for.
    #[error("node {0} has no leaf on record, so it cannot acknowledge a trust set")]
    UnknownNode(String),
    /// A trust acknowledgement for a Node CA newer than the active one.
    #[error("the acknowledged Node CA generation {acknowledged} is newer than the active {active}")]
    UnknownGeneration { acknowledged: u64, active: u64 },
}

/// The body of `POST /v1/ca/rotation/prepare` and `/finalize`: which
/// intermediate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RotationRole {
    /// The intermediate to rotate.
    pub role: CaRole,
}

/// What `POST /v1/ca/rotation/prepare` answers: the CSR for the operator to
/// sign, and what the certificate must say.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedRotation {
    /// The intermediate being rotated.
    pub role: CaRole,
    /// The generation the new CA will have.
    pub generation: u64,
    /// The serial the signed certificate must carry.
    pub serial: u64,
    /// The CSR, base64 DER.
    pub csr_b64: String,
    /// The `sha256:HEX` fingerprint of the root that must sign it, so the
    /// operator's backup can be checked against it before it's opened.
    pub root_fingerprint: String,
}

/// The body of `POST /v1/ca/rotation/begin`: the operator's certificate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedIntermediate {
    /// The intermediate being rotated.
    pub role: CaRole,
    /// The certificate the root signed, base64 DER.
    pub certificate_b64: String,
}

/// How long each node waits for the one before it, by node id, before
/// renewing onto a new Node CA anyway. Normally a node renews as soon as the
/// nodes before it have; this only keeps one dead node from holding up the
/// rest.
pub const EARLY_RENEWAL_STAGGER: Duration = Duration::from_secs(60);

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
    // The CSR has been answered: its key is the new CA's now.
    state
        .pending_intermediates
        .retain(|pending| pending.role != role);
    Ok(BeginOutcome::Started)
}

/// Finish rotating `role`: drop the retiring CA.
///
/// Refused while a leaf could still depend on it. For the Node CA the council
/// knows every node's latest leaf ([`SecurityState::node_leaves`]), so it can
/// prove the nodes moved before the window ends. It also waits until every
/// node has acknowledged trusting the new Node CA. Workload and ingress
/// leaves aren't tracked one by one, so for those finalise waits out the
/// window. `now` comes from the log entry, never the clock.
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
    if role == CaRole::Node {
        let unacknowledged = nodes_not_trusting_active_ca(state);
        if !unacknowledged.is_empty() {
            return Err(CaRotationError::UnacknowledgedTrust(unacknowledged));
        }
    }
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
///
/// A renewal keeps the node's trust acknowledgement. A node seen for the
/// first time (a join) starts out trusting the active CA, because its join
/// bundle carried the whole trust set.
pub fn record_node_leaf(state: &mut SecurityState, node_id: &str, serial: u64) {
    let ca_generation = active_node_generation(state);
    let trust_generation = state
        .node_leaves
        .get(node_id)
        .map_or(ca_generation, |record| record.trust_generation);
    state.node_leaves.insert(
        node_id.to_string(),
        NodeLeafRecord {
            serial: SerialNumber(serial),
            ca_generation,
            trust_generation,
        },
    );
}

fn active_node_generation(state: &SecurityState) -> u64 {
    state
        .active_ca(CaRole::Node)
        .map(|ca| ca.generation)
        .unwrap_or_default()
}

/// Whether a node is fenced by the CRL. A decommissioned node can't hold a
/// rotation up, whatever its records say.
fn is_retired(state: &SecurityState, node_id: &str) -> bool {
    state.crl.retired_nodes.contains_key(node_id)
}

/// The live nodes that haven't acknowledged trusting the active Node CA.
fn nodes_not_trusting_active_ca(state: &SecurityState) -> Vec<String> {
    let active = active_node_generation(state);
    state
        .node_leaves
        .iter()
        .filter(|(node_id, leaf)| leaf.trust_generation < active && !is_retired(state, node_id))
        .map(|(node_id, _)| node_id.clone())
        .collect()
}

/// Hold a new key and its CSR for `role` while the operator signs it (F04
/// R4, `RaftRequest::CaRotationPrepare`). Allocates the serial the signed
/// certificate must carry and returns it.
///
/// Refused for the root, while a rotation of the role is in progress, and
/// for any generation but the active one's plus one. A second prepare for the
/// same role replaces the first: the operator may have lost the CSR.
pub fn prepare(
    state: &mut SecurityState,
    role: CaRole,
    generation: u64,
    csr_der: Vec<u8>,
    private_key_wrapped: WrappedKey,
) -> Result<SerialNumber, CaRotationError> {
    if role == CaRole::Root {
        return Err(CaRotationError::RootRotationUnsupported);
    }
    if state.retiring_ca(role).is_some() {
        return Err(CaRotationError::AlreadyInProgress(role));
    }
    let active = state
        .active_ca(role)
        .ok_or(CaRotationError::NoActiveCa(role))?;
    let expected = active.generation + 1;
    if generation != expected {
        return Err(CaRotationError::WrongGeneration {
            role,
            expected,
            found: generation,
        });
    }
    let serial = SerialNumber(state.next_serial);
    state.next_serial += 1;
    state
        .pending_intermediates
        .retain(|pending| pending.role != role);
    state.pending_intermediates.push(PendingIntermediate {
        role,
        generation,
        serial,
        csr_der,
        private_key_wrapped,
    });
    Ok(serial)
}

/// Turn the certificate the operator signed into the role's next CA, ready
/// for [`begin`]. The leader checks it before proposing anything.
///
/// The certificate must answer the role's pending CSR: same public key, the
/// allocated serial, a CA certificate, signed by the active root. The CA
/// takes the pending wrapped key, so the council can sign with it.
pub fn intermediate_from_signed(
    state: &SecurityState,
    role: CaRole,
    certificate_der: &[u8],
) -> Result<CertificateAuthority, CaRotationError> {
    // `from_der` comes from the `FromDer` trait, which has to be in scope
    // for the method to resolve.
    use x509_parser::certification_request::X509CertificationRequest;
    use x509_parser::prelude::FromDer as _;

    let pending = state
        .pending_intermediates
        .iter()
        .find(|pending| pending.role == role)
        .ok_or(CaRotationError::NoPendingRequest(role))?;
    let invalid = |reason: String| CaRotationError::InvalidCertificate { role, reason };

    let (_, certificate) =
        x509_parser::parse_x509_certificate(certificate_der).map_err(|e| invalid(e.to_string()))?;
    let (_, request) = X509CertificationRequest::from_der(&pending.csr_der)
        .map_err(|e| invalid(format!("the pending CSR: {e}")))?;
    if certificate.public_key().subject_public_key.data
        != request
            .certification_request_info
            .subject_pki
            .subject_public_key
            .data
    {
        return Err(CaRotationError::KeyMismatch(role));
    }
    if !certificate.is_ca() {
        return Err(invalid("it is not a CA certificate".into()));
    }
    let serial =
        super::cert::serial_from_der(certificate_der).map_err(|e| invalid(e.to_string()))?;
    if serial != pending.serial {
        return Err(CaRotationError::SerialMismatch {
            role,
            expected: pending.serial.0,
            found: serial.0,
        });
    }
    let root = state
        .active_ca(CaRole::Root)
        .ok_or(CaRotationError::NoActiveCa(CaRole::Root))?;
    super::cert::verify_signature(certificate_der, &root.certificate_der)
        .and_then(|()| super::cert::check_issuer_binding(certificate_der, &root.certificate_der))
        .map_err(|error| CaRotationError::NotSignedByRoot {
            role,
            reason: error.to_string(),
        })?;

    let validity = certificate.validity();
    let at = |seconds: i64| {
        u64::try_from(seconds)
            .map(|seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
            .map_err(|_| invalid("its validity starts before 1970".into()))
    };
    Ok(CertificateAuthority {
        role,
        certificate_der: certificate_der.to_vec(),
        private_key_wrapped: Some(pending.private_key_wrapped.clone()),
        serial,
        not_before: at(validity.not_before.timestamp())?,
        not_after: at(validity.not_after.timestamp())?,
        issuer_serial: Some(root.serial),
        generation: pending.generation,
        state: CaState::Active,
    })
}

/// Record that `node_id` trusts every Node CA up to `generation` (F04 R4,
/// `RaftRequest::AcknowledgeNodeTrust`). An older acknowledgement arriving
/// late changes nothing.
pub fn acknowledge_trust(
    state: &mut SecurityState,
    node_id: &str,
    generation: u64,
) -> Result<(), CaRotationError> {
    let active = active_node_generation(state);
    if generation > active {
        return Err(CaRotationError::UnknownGeneration {
            acknowledged: generation,
            active,
        });
    }
    let record = state
        .node_leaves
        .get_mut(node_id)
        .ok_or_else(|| CaRotationError::UnknownNode(node_id.to_string()))?;
    record.trust_generation = record.trust_generation.max(generation);
    Ok(())
}

/// The Node CA generation a node should acknowledge now, if any: the active
/// one, once the node has installed exactly the trust set the council's
/// state describes and the council doesn't already know.
pub fn trust_acknowledgement_due(
    state: &SecurityState,
    node_id: &str,
    installed: &TrustSet,
) -> Option<u64> {
    let active = active_node_generation(state);
    let record = state.node_leaves.get(node_id)?;
    if record.trust_generation >= active {
        return None;
    }
    (TrustSet::from_state(state).as_ref() == Some(installed)).then_some(active)
}

/// Whether a node holding a leaf from Node CA `leaf_generation` should renew
/// now, ahead of its usual midpoint, to move onto the active Node CA.
///
/// Nobody moves until every live node has acknowledged the new trust set,
/// or a new leaf could be refused by a peer that doesn't trust its issuer
/// yet. Then the nodes go one at a time in node-id order: each waits for the
/// ones before it, or for its slot ([`EARLY_RENEWAL_STAGGER`] per place,
/// counted from when the new CA was made) if one of them is stuck.
pub fn early_renewal_due(
    state: &SecurityState,
    node_id: &str,
    leaf_generation: u64,
    now: SystemTime,
) -> bool {
    let Some(active) = state.active_ca(CaRole::Node) else {
        return false;
    };
    if leaf_generation >= active.generation || !nodes_not_trusting_active_ca(state).is_empty() {
        return false;
    }
    let earlier: Vec<&NodeLeafRecord> = state
        .node_leaves
        .iter()
        .filter(|(other, _)| other.as_str() < node_id && !is_retired(state, other))
        .map(|(_, record)| record)
        .collect();
    if earlier
        .iter()
        .all(|record| record.ca_generation >= active.generation)
    {
        return true;
    }
    let started = active.not_before + super::ca::CLOCK_SKEW_BACKDATE;
    let place = u32::try_from(earlier.len()).unwrap_or(u32::MAX);
    now >= started + EARLY_RENEWAL_STAGGER.saturating_mul(place)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sesame::ca::{self, CaHierarchy, generate_ca_hierarchy, generate_intermediate_ca};

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
        acknowledge_trust(&mut state, "node-a", 1).unwrap();
        acknowledge_trust(&mut state, "node-b", 1).unwrap();

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
                ca_generation: 0,
                trust_generation: 0
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

    // -----------------------------------------------------------------------
    // F04 R4: the CSR round trip, trust acknowledgements and ordered renewal
    // -----------------------------------------------------------------------

    /// Prepare a rotation of `role` the way the council does: a fresh key,
    /// wrapped, and its CSR. Returns the serial the certificate must carry.
    fn prepare_role(state: &mut SecurityState, role: CaRole) -> (SerialNumber, Vec<u8>) {
        let (csr, wrapped) = ca::create_intermediate_csr(role, IKM).unwrap();
        let generation = state.active_ca(role).unwrap().generation + 1;
        let serial = prepare(state, role, generation, csr.clone(), wrapped).unwrap();
        (serial, csr)
    }

    /// Sign a CSR with the hierarchy's root, as `relish ca rotate` does.
    fn operator_signs(
        hierarchy: &CaHierarchy,
        csr: &[u8],
        role: CaRole,
        serial: SerialNumber,
    ) -> Vec<u8> {
        ca::sign_intermediate_csr(
            csr,
            role,
            "rotation",
            serial,
            &hierarchy.root.private_key_der,
            &hierarchy.root.ca.certificate_der,
        )
        .unwrap()
    }

    #[test]
    fn prepare_allocates_a_serial_and_holds_one_pending_csr_per_role() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        let (first, _) = prepare_role(&mut state, CaRole::Node);
        assert_eq!(first, SerialNumber(10));
        assert_eq!(state.next_serial, 11);

        // Asking again (the operator lost the first CSR) replaces it.
        let (second, csr) = prepare_role(&mut state, CaRole::Node);
        assert_eq!(second, SerialNumber(11));
        let pending: Vec<_> = state
            .pending_intermediates
            .iter()
            .filter(|pending| pending.role == CaRole::Node)
            .collect();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].serial, second);
        assert_eq!(pending[0].csr_der, csr);
        assert_eq!(pending[0].generation, 1);
    }

    #[test]
    fn prepare_refuses_the_root_a_rotation_in_progress_and_a_wrong_generation() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        let (csr, wrapped) = ca::create_intermediate_csr(CaRole::Node, IKM).unwrap();
        assert_eq!(
            prepare(&mut state, CaRole::Root, 1, csr.clone(), wrapped.clone()),
            Err(CaRotationError::RootRotationUnsupported)
        );
        assert_eq!(
            prepare(&mut state, CaRole::Node, 2, csr.clone(), wrapped.clone()),
            Err(CaRotationError::WrongGeneration {
                role: CaRole::Node,
                expected: 1,
                found: 2
            })
        );
        begin(
            &mut state,
            CaRole::Node,
            &successor(&hierarchy, CaRole::Node, 1),
        )
        .unwrap();
        let before = state.clone();
        assert_eq!(
            prepare(&mut state, CaRole::Node, 2, csr, wrapped),
            Err(CaRotationError::AlreadyInProgress(CaRole::Node))
        );
        assert_eq!(state, before, "a refusal allocates nothing");
    }

    #[test]
    fn a_signed_csr_becomes_the_next_generation_and_begin_clears_the_pending_csr() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        let (serial, csr) = prepare_role(&mut state, CaRole::Workload);
        let certificate = operator_signs(&hierarchy, &csr, CaRole::Workload, serial);

        let new_ca = intermediate_from_signed(&state, CaRole::Workload, &certificate).unwrap();
        assert_eq!(new_ca.generation, 1);
        assert_eq!(new_ca.serial, serial);
        assert_eq!(new_ca.issuer_serial, Some(hierarchy.root.ca.serial));
        assert_eq!(new_ca.state, CaState::Active);

        assert_eq!(
            begin(&mut state, CaRole::Workload, &new_ca),
            Ok(BeginOutcome::Started)
        );
        assert!(state.pending_intermediates.is_empty());
        assert_eq!(
            state.active_ca(CaRole::Workload).unwrap().certificate_der,
            certificate
        );
    }

    #[test]
    fn a_signed_certificate_is_refused_without_a_pending_csr() {
        let hierarchy = hierarchy();
        let state = state_from(&hierarchy);
        let (csr, _) = ca::create_intermediate_csr(CaRole::Node, IKM).unwrap();
        let certificate = operator_signs(&hierarchy, &csr, CaRole::Node, SerialNumber(10));
        assert_eq!(
            intermediate_from_signed(&state, CaRole::Node, &certificate),
            Err(CaRotationError::NoPendingRequest(CaRole::Node))
        );
    }

    #[test]
    fn a_certificate_for_another_key_is_refused() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        let (serial, _) = prepare_role(&mut state, CaRole::Node);
        let (other_csr, _) = ca::create_intermediate_csr(CaRole::Node, IKM).unwrap();
        let certificate = operator_signs(&hierarchy, &other_csr, CaRole::Node, serial);
        assert_eq!(
            intermediate_from_signed(&state, CaRole::Node, &certificate),
            Err(CaRotationError::KeyMismatch(CaRole::Node))
        );
    }

    #[test]
    fn a_certificate_with_another_serial_is_refused() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        let (serial, csr) = prepare_role(&mut state, CaRole::Node);
        let certificate = operator_signs(&hierarchy, &csr, CaRole::Node, SerialNumber(999));
        assert_eq!(
            intermediate_from_signed(&state, CaRole::Node, &certificate),
            Err(CaRotationError::SerialMismatch {
                role: CaRole::Node,
                expected: serial.0,
                found: 999
            })
        );
    }

    #[test]
    fn a_certificate_another_root_signed_is_refused() {
        let hierarchy = hierarchy();
        let foreign = generate_ca_hierarchy("someone-else", IKM).unwrap();
        let mut state = state_from(&hierarchy);
        let (serial, csr) = prepare_role(&mut state, CaRole::Ingress);
        let certificate = operator_signs(&foreign, &csr, CaRole::Ingress, serial);
        assert!(matches!(
            intermediate_from_signed(&state, CaRole::Ingress, &certificate),
            Err(CaRotationError::NotSignedByRoot { .. })
        ));
    }

    #[test]
    fn a_certificate_submitted_for_another_role_is_refused() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        let (serial, csr) = prepare_role(&mut state, CaRole::Node);
        // The Node CSR, signed as a Workload CA and submitted for the
        // Workload role, which has nothing pending.
        let certificate = operator_signs(&hierarchy, &csr, CaRole::Workload, serial);
        assert_eq!(
            intermediate_from_signed(&state, CaRole::Workload, &certificate),
            Err(CaRotationError::NoPendingRequest(CaRole::Workload))
        );
    }

    #[test]
    fn a_new_node_record_trusts_the_active_ca_and_a_renewal_keeps_its_acknowledgement() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        record_node_leaf(&mut state, "node-a", 5);
        assert_eq!(state.node_leaves["node-a"].trust_generation, 0);
        begin(
            &mut state,
            CaRole::Node,
            &successor(&hierarchy, CaRole::Node, 1),
        )
        .unwrap();
        acknowledge_trust(&mut state, "node-a", 1).unwrap();
        record_node_leaf(&mut state, "node-a", 11);
        assert_eq!(state.node_leaves["node-a"].trust_generation, 1);
        assert_eq!(state.node_leaves["node-a"].ca_generation, 1);

        // A node that joins mid-rotation got the whole trust set with its
        // leaf, so it starts out acknowledging the active CA.
        record_node_leaf(&mut state, "node-new", 12);
        assert_eq!(state.node_leaves["node-new"].trust_generation, 1);
    }

    #[test]
    fn acknowledging_trust_refuses_an_unknown_node_and_a_generation_the_council_lacks() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        assert_eq!(
            acknowledge_trust(&mut state, "stranger", 0),
            Err(CaRotationError::UnknownNode("stranger".into()))
        );
        record_node_leaf(&mut state, "node-a", 5);
        assert_eq!(
            acknowledge_trust(&mut state, "node-a", 1),
            Err(CaRotationError::UnknownGeneration {
                acknowledged: 1,
                active: 0
            })
        );
        begin(
            &mut state,
            CaRole::Node,
            &successor(&hierarchy, CaRole::Node, 1),
        )
        .unwrap();
        acknowledge_trust(&mut state, "node-a", 1).unwrap();
        // An older acknowledgement arriving late never goes backwards.
        acknowledge_trust(&mut state, "node-a", 0).unwrap();
        assert_eq!(state.node_leaves["node-a"].trust_generation, 1);
    }

    #[test]
    fn finalise_is_refused_until_every_node_acknowledges_the_new_trust_set() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        record_node_leaf(&mut state, "node-a", 5);
        record_node_leaf(&mut state, "node-b", 6);
        let new_ca = successor(&hierarchy, CaRole::Node, 1);
        begin(&mut state, CaRole::Node, &new_ca).unwrap();
        // Both renewed onto the new CA, but node-b never said it trusts it.
        acknowledge_trust(&mut state, "node-a", 1).unwrap();
        record_node_leaf(&mut state, "node-a", 11);
        record_node_leaf(&mut state, "node-b", 12);

        assert_eq!(
            finalize(&mut state, CaRole::Node, new_ca.not_before),
            Err(CaRotationError::UnacknowledgedTrust(vec!["node-b".into()]))
        );
        acknowledge_trust(&mut state, "node-b", 1).unwrap();
        assert_eq!(
            finalize(&mut state, CaRole::Node, new_ca.not_before),
            Ok(())
        );
    }

    #[test]
    fn a_node_acknowledges_only_the_trust_set_the_council_describes() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        record_node_leaf(&mut state, "node-a", 5);
        let old_trust = TrustSet::from_state(&state).unwrap();
        assert_eq!(
            trust_acknowledgement_due(&state, "node-a", &old_trust),
            None,
            "nothing to acknowledge outside a rotation"
        );

        begin(
            &mut state,
            CaRole::Node,
            &successor(&hierarchy, CaRole::Node, 1),
        )
        .unwrap();
        assert_eq!(
            trust_acknowledgement_due(&state, "node-a", &old_trust),
            None,
            "a node still holding the old set has nothing to acknowledge yet"
        );
        let new_trust = TrustSet::from_state(&state).unwrap();
        assert_eq!(
            trust_acknowledgement_due(&state, "node-a", &new_trust),
            Some(1)
        );
        acknowledge_trust(&mut state, "node-a", 1).unwrap();
        assert_eq!(
            trust_acknowledgement_due(&state, "node-a", &new_trust),
            None
        );
    }

    #[test]
    fn nodes_renew_early_one_after_another_once_all_trust_the_new_ca() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        for (serial, node) in ["node-a", "node-b", "node-c"].iter().enumerate() {
            record_node_leaf(&mut state, node, 5 + serial as u64);
        }
        let new_ca = successor(&hierarchy, CaRole::Node, 1);
        begin(&mut state, CaRole::Node, &new_ca).unwrap();
        let now = new_ca.not_before + ca::CLOCK_SKEW_BACKDATE;

        // Nobody renews while a node could still refuse a new leaf.
        acknowledge_trust(&mut state, "node-a", 1).unwrap();
        acknowledge_trust(&mut state, "node-b", 1).unwrap();
        assert!(!early_renewal_due(&state, "node-a", 0, now));
        acknowledge_trust(&mut state, "node-c", 1).unwrap();

        // Then in node order: node-b waits for node-a.
        assert!(early_renewal_due(&state, "node-a", 0, now));
        assert!(!early_renewal_due(&state, "node-b", 0, now));
        record_node_leaf(&mut state, "node-a", 20);
        assert!(early_renewal_due(&state, "node-b", 0, now));
        assert!(!early_renewal_due(&state, "node-c", 0, now));
        // A node that already holds a new leaf has nothing to do.
        assert!(!early_renewal_due(&state, "node-a", 1, now));
    }

    #[test]
    fn a_stuck_node_delays_the_next_one_only_by_its_slot() {
        let hierarchy = hierarchy();
        let mut state = state_from(&hierarchy);
        for (serial, node) in ["node-a", "node-b"].iter().enumerate() {
            record_node_leaf(&mut state, node, 5 + serial as u64);
        }
        let new_ca = successor(&hierarchy, CaRole::Node, 1);
        begin(&mut state, CaRole::Node, &new_ca).unwrap();
        acknowledge_trust(&mut state, "node-a", 1).unwrap();
        acknowledge_trust(&mut state, "node-b", 1).unwrap();
        let started = new_ca.not_before + ca::CLOCK_SKEW_BACKDATE;

        // node-a never renews; node-b goes in its own slot regardless.
        assert!(!early_renewal_due(&state, "node-b", 0, started));
        assert!(early_renewal_due(
            &state,
            "node-b",
            0,
            started + EARLY_RENEWAL_STAGGER
        ));
    }
}
