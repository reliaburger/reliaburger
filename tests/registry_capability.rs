use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use reliaburger::pickle::capability::{
    RegistryBindError, plan_registry_bind, registry_capability_evidence,
};
use reliaburger::pickle::types::ManifestCatalog;

#[test]
fn clustered_default_derives_the_advertised_address() {
    let advertised = IpAddr::V4(Ipv4Addr::new(10, 20, 30, 40));
    let plan = plan_registry_bind("127.0.0.1", 5050, Some(advertised)).unwrap();

    assert_eq!(plan.listen_addr, SocketAddr::new(advertised, 5050));
    assert!(plan.peer_reachable);
}

#[test]
fn clustered_unspecified_bind_covers_the_advertised_address() {
    let advertised = IpAddr::V4(Ipv4Addr::new(10, 20, 30, 40));
    let plan = plan_registry_bind("0.0.0.0", 5050, Some(advertised)).unwrap();

    assert_eq!(
        plan.listen_addr,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 5050)
    );
    assert!(plan.peer_reachable);
}

#[test]
fn clustered_wildcard_must_match_the_advertised_address_family() {
    let advertised = IpAddr::V4(Ipv4Addr::new(10, 20, 30, 40));
    let error = plan_registry_bind("::", 5050, Some(advertised)).unwrap_err();

    assert!(matches!(
        error,
        RegistryBindError::AdvertisedAddressNotBound { .. }
    ));
}

#[test]
fn clustered_mismatched_interface_fails_closed() {
    let advertised = IpAddr::V4(Ipv4Addr::new(10, 20, 30, 40));
    let error = plan_registry_bind("192.168.50.4", 5050, Some(advertised)).unwrap_err();

    assert!(matches!(
        error,
        RegistryBindError::AdvertisedAddressNotBound { .. }
    ));
}

#[test]
fn standalone_registry_preserves_explicit_loopback() {
    let plan = plan_registry_bind("127.0.0.1", 5050, None).unwrap();

    assert_eq!(
        plan.listen_addr,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5050)
    );
    assert!(!plan.peer_reachable);
}

#[test]
fn cluster_cannot_advertise_an_unspecified_registry_address() {
    let error =
        plan_registry_bind("127.0.0.1", 5050, Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED))).unwrap_err();

    assert!(matches!(
        error,
        RegistryBindError::UnspecifiedAdvertiseAddress
    ));
}

#[test]
fn capability_counts_membership_and_under_replicated_layers() {
    let bind = plan_registry_bind(
        "127.0.0.1",
        5050,
        Some(IpAddr::V4(Ipv4Addr::new(10, 20, 30, 40))),
    )
    .unwrap();
    let catalog = ManifestCatalog {
        layer_locations: vec![
            ("sha256:one".to_string(), BTreeSet::from([1, 2])),
            ("sha256:two".to_string(), BTreeSet::from([1])),
        ],
        ..Default::default()
    };

    let evidence = registry_capability_evidence(bind, true, true, true, 2, 3, &catalog);

    assert!(evidence.ready);
    assert!(evidence.peer_reachable);
    assert!(evidence.redundancy_possible);
    assert_eq!(evidence.under_replicated_layers, 1);
}
