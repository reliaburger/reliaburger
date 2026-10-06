//! The CAs a node trusts (F04 R2).
//!
//! Outside a rotation a node trusts one Node CA and one root. During one it
//! trusts the active Node CA and the retiring one, so a peer still holding a
//! leaf from the old CA and a peer with a fresh leaf from the new one can both
//! authenticate. A [`TrustSet`] is that list, taken from the council's
//! `SecurityState`, and every node-identity verifier tries each entry.

use serde::{Deserialize, Serialize};

use super::cert::{self, CertError};
use super::types::{CaRole, SecurityState};

/// The Node CAs and roots a verifier accepts, DER-encoded, active first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustSet {
    /// Node CAs whose leaves authenticate as cluster nodes.
    pub node_cas: Vec<Vec<u8>>,
    /// Roots the Node CAs must chain to.
    pub roots: Vec<Vec<u8>>,
}

/// The CAs that vouch for one node leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedChain<'a> {
    /// The trusted Node CA that signed the leaf.
    pub node_ca: &'a [u8],
    /// The trusted root that signed that Node CA.
    pub root: &'a [u8],
}

impl TrustSet {
    /// Trust exactly one Node CA and one root: a cluster that isn't rotating.
    pub fn single(node_ca: Vec<u8>, root: Vec<u8>) -> Self {
        Self {
            node_cas: vec![node_ca],
            roots: vec![root],
        }
    }

    /// The trust set the council's state describes: every trusted Node CA and
    /// root, active first. `None` when the state has no Node CA or no root.
    pub fn from_state(state: &SecurityState) -> Option<Self> {
        let ders = |role| -> Vec<Vec<u8>> {
            state
                .trusted_cas(role)
                .into_iter()
                .map(|ca| ca.certificate_der.clone())
                .collect()
        };
        let trust = Self {
            node_cas: ders(CaRole::Node),
            roots: ders(CaRole::Root),
        };
        (!trust.node_cas.is_empty() && !trust.roots.is_empty()).then_some(trust)
    }

    /// Whether `node_ca` is one of the trusted Node CAs.
    pub fn trusts_node_ca(&self, node_ca: &[u8]) -> bool {
        self.node_cas.iter().any(|trusted| trusted == node_ca)
    }

    /// Whether `root` is one of the trusted roots.
    pub fn trusts_root(&self, root: &[u8]) -> bool {
        self.roots.iter().any(|trusted| trusted == root)
    }

    /// Validate a node leaf against every trusted Node CA and root: some
    /// trusted Node CA signed it (signature and issuer name), some trusted
    /// root signed that CA, and all three are within their validity windows.
    /// Returns the chain that vouched for it.
    ///
    /// When nothing vouches, the error is the most specific one seen: a leaf
    /// signed by a trusted CA but expired reports `Expired`, not "untrusted".
    pub fn validate_node_leaf(&self, leaf: &[u8]) -> Result<TrustedChain<'_>, CertError> {
        let mut last_error = None;
        for node_ca in &self.node_cas {
            if let Err(error) = cert::verify_signature(leaf, node_ca)
                .and_then(|()| cert::check_issuer_binding(leaf, node_ca))
            {
                last_error.get_or_insert(error);
                continue;
            }
            for root in &self.roots {
                let chained = cert::check_issuer_binding(node_ca, root)
                    .and_then(|()| cert::validate_chain(leaf, node_ca, root));
                match chained {
                    Ok(()) => {
                        return Ok(TrustedChain {
                            node_ca: node_ca.as_slice(),
                            root: root.as_slice(),
                        });
                    }
                    // A signature that verified against this Node CA is the
                    // better story to tell than the first CA's mismatch.
                    Err(error) => last_error = Some(error),
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            CertError::ChainInvalid("no trusted Node CA issued this certificate".into())
        }))
    }

    /// Check the set is usable: not empty, and every Node CA chains to one of
    /// the roots. A set that fails this would refuse every peer.
    pub fn validate(&self) -> Result<(), CertError> {
        if self.node_cas.is_empty() || self.roots.is_empty() {
            return Err(CertError::ChainInvalid(
                "a trust set needs at least one Node CA and one root".into(),
            ));
        }
        for node_ca in &self.node_cas {
            let chains = self.roots.iter().any(|root| {
                cert::verify_signature(node_ca, root).is_ok()
                    && cert::check_issuer_binding(node_ca, root).is_ok()
            });
            if !chains {
                return Err(CertError::ChainInvalid(
                    "a trusted Node CA does not chain to a trusted root".into(),
                ));
            }
        }
        Ok(())
    }
}

/// The CA bundle a workload's `ca.pem` holds: every trusted Workload CA,
/// active first, then every trusted root. A workload verifies peers against
/// it, so during a Workload CA rotation a peer whose certificate came from
/// either CA is accepted.
pub fn workload_ca_bundle(state: &SecurityState) -> Vec<Vec<u8>> {
    state
        .trusted_cas(CaRole::Workload)
        .into_iter()
        .chain(state.trusted_cas(CaRole::Root))
        .map(|ca| ca.certificate_der.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sesame::ca::{self, CaHierarchy};
    use crate::sesame::types::{CertificateAuthority, SerialNumber};

    const IKM: &[u8] = b"trust-test-ikm";

    fn leaf(node_ca: &ca::GeneratedCa, serial: u64) -> Vec<u8> {
        ca::issue_node_cert(
            "node-a",
            SerialNumber(serial),
            &node_ca.signing_keypair,
            &node_ca.certificate_params,
        )
        .unwrap()
        .0
    }

    fn successor(hierarchy: &CaHierarchy) -> ca::GeneratedCa {
        ca::generate_intermediate_ca(
            CaRole::Node,
            "trust",
            SerialNumber(90),
            hierarchy.root.ca.serial,
            &hierarchy.root.signing_keypair,
            &hierarchy.root.certificate_params,
            IKM,
        )
        .unwrap()
    }

    #[test]
    fn a_leaf_from_either_trusted_node_ca_validates() {
        let hierarchy = ca::generate_ca_hierarchy("trust", IKM).unwrap();
        let new_ca = successor(&hierarchy);
        let trust = TrustSet {
            node_cas: vec![
                new_ca.ca.certificate_der.clone(),
                hierarchy.node.ca.certificate_der.clone(),
            ],
            roots: vec![hierarchy.root.ca.certificate_der.clone()],
        };
        trust.validate().unwrap();

        let old_leaf = leaf(&hierarchy.node, 10);
        let new_leaf = leaf(&new_ca, 11);
        assert_eq!(
            trust.validate_node_leaf(&old_leaf).unwrap().node_ca,
            hierarchy.node.ca.certificate_der.as_slice()
        );
        assert_eq!(
            trust.validate_node_leaf(&new_leaf).unwrap().node_ca,
            new_ca.ca.certificate_der.as_slice()
        );
    }

    #[test]
    fn a_leaf_from_an_untrusted_ca_is_refused() {
        let hierarchy = ca::generate_ca_hierarchy("trust", IKM).unwrap();
        let foreign = ca::generate_ca_hierarchy("someone-else", IKM).unwrap();
        let trust = TrustSet::single(
            hierarchy.node.ca.certificate_der.clone(),
            hierarchy.root.ca.certificate_der.clone(),
        );
        let stranger = leaf(&foreign.node, 10);
        assert!(trust.validate_node_leaf(&stranger).is_err());

        // Neither is a leaf from our own root's other intermediate, which a
        // retired Node CA would be once it leaves the set.
        let retired = successor(&hierarchy);
        let orphan = leaf(&retired, 12);
        assert!(trust.validate_node_leaf(&orphan).is_err());
    }

    #[test]
    fn from_state_lists_the_active_ca_first_and_the_retiring_one_after() {
        let hierarchy = ca::generate_ca_hierarchy("trust", IKM).unwrap();
        let new_ca = successor(&hierarchy);
        let mut state = SecurityState {
            certificate_authorities: vec![
                CertificateAuthority {
                    private_key_wrapped: None,
                    ..hierarchy.root.ca.clone()
                },
                hierarchy.node.ca.clone(),
            ],
            ..SecurityState::default()
        };
        assert_eq!(
            TrustSet::from_state(&state),
            Some(TrustSet::single(
                hierarchy.node.ca.certificate_der.clone(),
                hierarchy.root.ca.certificate_der.clone()
            ))
        );
        crate::sesame::ca_rotation::begin(
            &mut state,
            CaRole::Node,
            &CertificateAuthority {
                generation: 1,
                ..new_ca.ca.clone()
            },
        )
        .unwrap();
        let trust = TrustSet::from_state(&state).unwrap();
        assert_eq!(
            trust.node_cas,
            [
                new_ca.ca.certificate_der.clone(),
                hierarchy.node.ca.certificate_der.clone()
            ]
        );
        assert!(TrustSet::from_state(&SecurityState::default()).is_none());
    }

    #[test]
    fn the_workload_bundle_carries_both_workload_cas_during_a_rotation() {
        let hierarchy = ca::generate_ca_hierarchy("trust", IKM).unwrap();
        let mut state = SecurityState {
            certificate_authorities: vec![
                CertificateAuthority {
                    private_key_wrapped: None,
                    ..hierarchy.root.ca.clone()
                },
                hierarchy.workload.ca.clone(),
            ],
            ..SecurityState::default()
        };
        let new_ca = ca::generate_intermediate_ca(
            CaRole::Workload,
            "trust",
            SerialNumber(91),
            hierarchy.root.ca.serial,
            &hierarchy.root.signing_keypair,
            &hierarchy.root.certificate_params,
            IKM,
        )
        .unwrap();
        crate::sesame::ca_rotation::begin(
            &mut state,
            CaRole::Workload,
            &CertificateAuthority {
                generation: 1,
                ..new_ca.ca.clone()
            },
        )
        .unwrap();
        assert_eq!(
            workload_ca_bundle(&state),
            [
                new_ca.ca.certificate_der.clone(),
                hierarchy.workload.ca.certificate_der.clone(),
                hierarchy.root.ca.certificate_der.clone(),
            ]
        );
    }

    #[test]
    fn a_set_whose_node_ca_has_no_trusted_root_is_invalid() {
        let hierarchy = ca::generate_ca_hierarchy("trust", IKM).unwrap();
        let foreign = ca::generate_ca_hierarchy("someone-else", IKM).unwrap();
        let mixed = TrustSet::single(
            foreign.node.ca.certificate_der.clone(),
            hierarchy.root.ca.certificate_der.clone(),
        );
        assert!(mixed.validate().is_err());
        let empty = TrustSet {
            node_cas: Vec::new(),
            roots: vec![hierarchy.root.ca.certificate_der.clone()],
        };
        assert!(empty.validate().is_err());
    }
}
