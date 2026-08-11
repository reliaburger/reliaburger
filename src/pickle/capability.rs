//! Live Pickle registry reachability and redundancy evidence.

use std::net::{IpAddr, SocketAddr};

use serde::{Deserialize, Serialize};

use super::types::ManifestCatalog;

/// Resolved registry listener and whether cluster peers can address it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryBindPlan {
    /// Socket Bun must bind.
    pub listen_addr: SocketAddr,
    /// The listener covers the cluster-advertised address.
    pub peer_reachable: bool,
}

/// Invalid or incomplete registry listener configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryBindError {
    /// The bind must be a numeric IP so its relationship with the advertised
    /// cluster address is unambiguous.
    #[error("invalid registry bind address {value:?}: expected a numeric IP address")]
    InvalidAddress { value: String },
    /// Peers cannot address an unspecified gossip address.
    #[error("cluster advertise address cannot be unspecified")]
    UnspecifiedAdvertiseAddress,
    /// Peers use the advertised IP, but the configured listener excludes it.
    #[error(
        "registry bind {configured} does not cover cluster advertise address {advertised}; bind that address or an unspecified address"
    )]
    AdvertisedAddressNotBound {
        configured: IpAddr,
        advertised: IpAddr,
    },
}

/// Resolve the registry listener for standalone or clustered Bun.
///
/// A loopback default remains loopback in standalone mode. In cluster mode it
/// follows the advertised gossip address, unless the cluster itself is an
/// intentional single-host loopback cluster. Explicit wildcard binds cover
/// the advertised address; an explicit different interface fails closed.
pub fn plan_registry_bind(
    configured: &str,
    port: u16,
    cluster_advertise: Option<IpAddr>,
) -> Result<RegistryBindPlan, RegistryBindError> {
    let configured =
        configured
            .parse::<IpAddr>()
            .map_err(|_| RegistryBindError::InvalidAddress {
                value: configured.to_string(),
            })?;

    let Some(advertised) = cluster_advertise else {
        return Ok(RegistryBindPlan {
            listen_addr: SocketAddr::new(configured, port),
            peer_reachable: false,
        });
    };
    if advertised.is_unspecified() {
        return Err(RegistryBindError::UnspecifiedAdvertiseAddress);
    }

    let same_family = matches!(
        (configured, advertised),
        (IpAddr::V4(_), IpAddr::V4(_)) | (IpAddr::V6(_), IpAddr::V6(_))
    );
    let listen_ip = if configured.is_loopback() {
        // A loopback *default* on a routable cluster must bind the unspecified
        // address of the advertise family, not the advertised IP itself. The
        // wildcard covers both the routable address peers use AND loopback —
        // and the build pipeline pushes to `localhost:{port}`, so binding only
        // the advertised IP left nothing on loopback and every in-cluster
        // `buildah push` (and the node's own registry reads) got connection
        // refused. `peer_reachable` still holds (the wildcard covers the
        // advertised address) and `require_read_auth` still holds because the
        // wildcard is not loopback.
        match advertised {
            IpAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        }
    } else if (configured.is_unspecified() && same_family) || configured == advertised {
        configured
    } else {
        return Err(RegistryBindError::AdvertisedAddressNotBound {
            configured,
            advertised,
        });
    };

    Ok(RegistryBindPlan {
        listen_addr: SocketAddr::new(listen_ip, port),
        peer_reachable: true,
    })
}

/// Operator-visible Pickle reachability and redundancy evidence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryCapabilityEvidence {
    /// Whether the listener was bound and entered its run loop.
    pub ready: bool,
    /// Actual listener selected after cluster-aware derivation.
    pub listen_addr: Option<SocketAddr>,
    /// Whether the listener covers the address other nodes use.
    pub peer_reachable: bool,
    /// Whether the registry listener uses cluster TLS.
    pub tls: bool,
    /// Whether cluster P2P pulls have at least one worker slot.
    pub p2p_enabled: bool,
    /// Desired number of holders per image layer.
    pub redundancy_target: u32,
    /// Active nodes currently visible through membership.
    pub known_nodes: usize,
    /// Whether the current membership can satisfy the configured target.
    pub redundancy_possible: bool,
    /// Catalogue layers with fewer holders than the configured target.
    pub under_replicated_layers: usize,
}

/// Assemble a current capability snapshot from listener, membership and
/// catalogue state.
pub fn registry_capability_evidence(
    bind: RegistryBindPlan,
    ready: bool,
    tls: bool,
    p2p_enabled: bool,
    redundancy_target: u32,
    known_nodes: usize,
    catalog: &ManifestCatalog,
) -> RegistryCapabilityEvidence {
    let redundancy_target = redundancy_target.max(1);
    let under_replicated_layers = catalog
        .layer_locations
        .iter()
        .filter(|(_, holders)| holders.len() < redundancy_target as usize)
        .count();
    RegistryCapabilityEvidence {
        ready,
        listen_addr: Some(bind.listen_addr),
        peer_reachable: bind.peer_reachable,
        tls,
        p2p_enabled,
        redundancy_target,
        known_nodes,
        redundancy_possible: known_nodes >= redundancy_target as usize,
        under_replicated_layers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn standalone_loopback_default_stays_loopback() {
        let plan = plan_registry_bind("127.0.0.1", 5050, None).unwrap();
        assert_eq!(plan.listen_addr, "127.0.0.1:5050".parse().unwrap());
        assert!(!plan.peer_reachable);
    }

    /// B1: a loopback default on a routable cluster must bind the wildcard of
    /// the advertise family — not the advertised IP — so `localhost:{port}`
    /// (the build pipeline's push target and the node's own reads) keeps a
    /// listener while peers still reach the routable address.
    #[test]
    fn loopback_default_on_a_routable_cluster_binds_the_unspecified_address() {
        let advertised = IpAddr::V4(Ipv4Addr::new(10, 0, 1, 5));
        let plan = plan_registry_bind("127.0.0.1", 5050, Some(advertised)).unwrap();
        assert_eq!(plan.listen_addr.ip(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(plan.listen_addr.port(), 5050);
        // Peers can still reach the listener through the advertised address.
        assert!(plan.peer_reachable);
        // The wildcard is not loopback, so routable-read auth stays on.
        assert!(!plan.listen_addr.ip().is_loopback());
    }

    #[test]
    fn loopback_default_on_an_ipv6_cluster_binds_the_v6_unspecified_address() {
        let advertised = IpAddr::V6("2001:db8::5".parse().unwrap());
        let plan = plan_registry_bind("::1", 5050, Some(advertised)).unwrap();
        assert_eq!(plan.listen_addr.ip(), IpAddr::V6(Ipv6Addr::UNSPECIFIED));
        assert!(plan.peer_reachable);
    }

    #[test]
    fn an_explicit_wildcard_bind_is_preserved() {
        let advertised = IpAddr::V4(Ipv4Addr::new(10, 0, 1, 5));
        let plan = plan_registry_bind("0.0.0.0", 5050, Some(advertised)).unwrap();
        assert_eq!(plan.listen_addr.ip(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert!(plan.peer_reachable);
    }

    #[test]
    fn an_explicit_foreign_interface_fails_closed() {
        let advertised = IpAddr::V4(Ipv4Addr::new(10, 0, 1, 5));
        let err = plan_registry_bind("192.168.0.9", 5050, Some(advertised)).unwrap_err();
        assert!(matches!(
            err,
            RegistryBindError::AdvertisedAddressNotBound { .. }
        ));
    }
}
