//! F04 R2: every node-identity verifier trusts each CA in the council's trust
//! set, and a new set reaches running listeners without a restart.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use reliaburger::sesame::{
    ca, ca_rotation,
    credentials::LiveNodeIdentity,
    identity_store::{self, NodeIdentity},
    mtls,
    trust::TrustSet,
    types::{CaRole, CertificateAuthority, SecurityState, SerialNumber},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const IKM: &[u8] = b"ca-trust-rotation";

/// The Node CA a rotation would install: generation 1, signed by the same root.
fn rotated_node_ca(hierarchy: &ca::CaHierarchy) -> ca::GeneratedCa {
    let mut generated = ca::generate_intermediate_ca(
        CaRole::Node,
        "rotation",
        SerialNumber(60),
        hierarchy.root.ca.serial,
        &hierarchy.root.signing_keypair,
        &hierarchy.root.certificate_params,
        IKM,
    )
    .unwrap();
    generated.ca.generation = 1;
    generated
}

/// A node identity whose leaf `node_ca` signed, trusting `trust`.
fn identity(
    hierarchy: &ca::CaHierarchy,
    node_ca: &ca::GeneratedCa,
    node: &str,
    serial: u64,
    trust: &TrustSet,
) -> NodeIdentity {
    let (certificate_der, private_key_der, serial) = ca::issue_node_cert(
        node,
        SerialNumber(serial),
        &node_ca.signing_keypair,
        &node_ca.certificate_params,
    )
    .unwrap();
    NodeIdentity {
        node_id: node.into(),
        certificate_der,
        private_key_der,
        serial,
        ca_generation: node_ca.ca.generation,
        node_ca_der: node_ca.ca.certificate_der.clone(),
        root_ca_der: hierarchy.root.ca.certificate_der.clone(),
        trust: trust.clone(),
        not_before: SystemTime::UNIX_EPOCH,
        not_after: SystemTime::UNIX_EPOCH,
    }
}

fn installed(directory: &std::path::Path, identity: &NodeIdentity) -> LiveNodeIdentity {
    identity_store::save(directory, identity).unwrap();
    LiveNodeIdentity::load(directory).unwrap()
}

/// The council's trust set during a Node CA rotation: new first, then old.
fn rotating(hierarchy: &ca::CaHierarchy, new: &ca::GeneratedCa) -> TrustSet {
    TrustSet {
        node_cas: vec![
            new.ca.certificate_der.clone(),
            hierarchy.node.ca.certificate_der.clone(),
        ],
        roots: vec![hierarchy.root.ca.certificate_der.clone()],
    }
}

fn not_rotating(hierarchy: &ca::CaHierarchy) -> TrustSet {
    TrustSet::single(
        hierarchy.node.ca.certificate_der.clone(),
        hierarchy.root.ca.certificate_der.clone(),
    )
}

/// One mutually authenticated exchange. The server writes after accepting, so
/// a client certificate the server refuses shows up as a failure on either
/// side even under TLS 1.3, where the client's handshake finishes first.
async fn try_handshake(
    server: Arc<rustls::ServerConfig>,
    client: Arc<rustls::ClientConfig>,
) -> Result<(), String> {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let server = async {
        let mut stream = tokio_rustls::TlsAcceptor::from(server)
            .accept(server_io)
            .await
            .map_err(|error| format!("server: {error}"))?;
        stream
            .write_all(b"ok")
            .await
            .map_err(|error| format!("server: {error}"))?;
        stream
            .flush()
            .await
            .map_err(|error| format!("server: {error}"))
    };
    let client = async {
        let name = rustls::pki_types::ServerName::try_from("node").unwrap();
        let mut stream = tokio_rustls::TlsConnector::from(client)
            .connect(name, client_io)
            .await
            .map_err(|error| format!("client: {error}"))?;
        let mut reply = [0; 2];
        stream
            .read_exact(&mut reply)
            .await
            .map_err(|error| format!("client: {error}"))?;
        Ok::<_, String>(())
    };
    let (server, client) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(server, client)
    })
    .await
    .map_err(|_| "handshake timed out".to_string())?;
    server.and(client)
}

/// Every pairing of `server`'s live listeners with `client`'s live clients,
/// bound to the server's node id and unbound.
async fn handshakes(server: &LiveNodeIdentity, client: &LiveNodeIdentity) -> Result<(), String> {
    let server_id = server.snapshot().node_id.clone();
    let servers = [
        mtls::build_live_mtls_server_config(server, mtls::CrlHandle::default()).unwrap(),
        mtls::build_live_api_server_config(server, mtls::CrlHandle::default()).unwrap(),
    ];
    let clients = [
        mtls::build_live_mtls_client_config(client, mtls::CrlHandle::default(), Some(&server_id))
            .unwrap(),
        mtls::build_live_mtls_client_config(client, mtls::CrlHandle::default(), None).unwrap(),
    ];
    for server_config in &servers {
        for client_config in &clients {
            try_handshake(server_config.clone(), client_config.clone()).await?;
        }
    }
    Ok(())
}

/// The plan's first R2 test: during the window, a node still on its old-CA
/// leaf and a node already renewed onto the new CA authenticate each other in
/// both directions.
#[tokio::test]
async fn nodes_on_the_old_and_new_node_ca_authenticate_each_other_both_ways() {
    let hierarchy = ca::generate_ca_hierarchy("rotation", IKM).unwrap();
    let new_ca = rotated_node_ca(&hierarchy);
    let trust = rotating(&hierarchy, &new_ca);
    let old_directory = tempfile::tempdir().unwrap();
    let new_directory = tempfile::tempdir().unwrap();
    let old = installed(
        old_directory.path(),
        &identity(&hierarchy, &hierarchy.node, "old-node", 10, &trust),
    );
    let new = installed(
        new_directory.path(),
        &identity(&hierarchy, &new_ca, "new-node", 11, &trust),
    );

    handshakes(&old, &new).await.unwrap();
    handshakes(&new, &old).await.unwrap();
}

/// A leaf from a Node CA outside the trust set is refused whichever side
/// presents it: one from a CA the cluster finalised away, and one from
/// another cluster entirely.
#[tokio::test]
async fn a_leaf_from_an_untrusted_node_ca_is_refused_both_ways() {
    let hierarchy = ca::generate_ca_hierarchy("rotation", IKM).unwrap();
    let new_ca = rotated_node_ca(&hierarchy);
    let foreign = ca::generate_ca_hierarchy("someone-else", IKM).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let trusted = installed(
        directory.path(),
        &identity(
            &hierarchy,
            &hierarchy.node,
            "trusted",
            10,
            &not_rotating(&hierarchy),
        ),
    );
    // Each stranger trusts the trusted node's CA, so only one side refuses.
    let strangers = [
        identity(
            &hierarchy,
            &new_ca,
            "unannounced",
            11,
            &rotating(&hierarchy, &new_ca),
        ),
        identity(
            &foreign,
            &foreign.node,
            "foreign",
            12,
            &TrustSet {
                node_cas: vec![
                    foreign.node.ca.certificate_der.clone(),
                    hierarchy.node.ca.certificate_der.clone(),
                ],
                roots: vec![
                    foreign.root.ca.certificate_der.clone(),
                    hierarchy.root.ca.certificate_der.clone(),
                ],
            },
        ),
    ];
    for stranger in strangers {
        let directory = tempfile::tempdir().unwrap();
        let stranger = installed(directory.path(), &stranger);
        let inbound = handshakes(&trusted, &stranger).await;
        assert!(inbound.is_err(), "the listener must refuse the stranger");
        let outbound = handshakes(&stranger, &trusted).await;
        assert!(outbound.is_err(), "the client must refuse the stranger");
    }
}

/// The plan's third R2 test: a listener built before the rotation refuses a
/// new-CA peer, then accepts it once the security refresh installs the
/// council's new trust set. Same config object, no restart.
#[tokio::test]
async fn a_new_trust_set_reaches_a_running_node_without_a_restart() {
    let hierarchy = ca::generate_ca_hierarchy("rotation", IKM).unwrap();
    let new_ca = rotated_node_ca(&hierarchy);

    // The council state the refresh reads: a Node CA rotation has begun.
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
    ca_rotation::begin(&mut state, CaRole::Node, &new_ca.ca).unwrap();

    let directory = tempfile::tempdir().unwrap();
    let running = installed(
        directory.path(),
        &identity(
            &hierarchy,
            &hierarchy.node,
            "running",
            10,
            &not_rotating(&hierarchy),
        ),
    );
    let server = mtls::build_live_mtls_server_config(&running, mtls::CrlHandle::default()).unwrap();
    let peer_directory = tempfile::tempdir().unwrap();
    let renewed_peer = installed(
        peer_directory.path(),
        &identity(
            &hierarchy,
            &new_ca,
            "renewed",
            11,
            &rotating(&hierarchy, &new_ca),
        ),
    );
    let client =
        mtls::build_live_mtls_client_config(&renewed_peer, mtls::CrlHandle::default(), None)
            .unwrap();

    assert!(
        try_handshake(server.clone(), client.clone()).await.is_err(),
        "before the refresh, the running node doesn't know the new Node CA"
    );

    assert!(running.adopt_council_trust(&state).await.unwrap());
    assert_eq!(
        running.snapshot().trust,
        TrustSet::from_state(&state).unwrap()
    );
    try_handshake(server, client).await.unwrap();

    // The new set is durable: a restart would come back with it.
    let reloaded = identity_store::load(directory.path()).unwrap().unwrap();
    assert_eq!(reloaded.trust, rotating(&hierarchy, &new_ca));
    // And installing it again changes nothing.
    assert!(!running.adopt_council_trust(&state).await.unwrap());
}

/// `LiveNodeIdentity::replace` takes a new trust set only when it is the
/// council's, and never one that drops this node's own issuer.
#[tokio::test]
async fn replace_accepts_only_the_councils_trust_set() {
    let hierarchy = ca::generate_ca_hierarchy("rotation", IKM).unwrap();
    let new_ca = rotated_node_ca(&hierarchy);
    let directory = tempfile::tempdir().unwrap();
    let original = identity(
        &hierarchy,
        &hierarchy.node,
        "node",
        10,
        &not_rotating(&hierarchy),
    );
    let live = installed(directory.path(), &original);

    // A renewal response that widens trust on its own say-so is refused.
    let widened = identity(
        &hierarchy,
        &hierarchy.node,
        "node",
        20,
        &rotating(&hierarchy, &new_ca),
    );
    assert!(
        live.replace(widened.clone(), &not_rotating(&hierarchy))
            .await
            .is_err()
    );
    assert_eq!(live.snapshot().trust, not_rotating(&hierarchy));

    // When the council says so, the same replacement goes through, and a
    // renewal onto the new Node CA is accepted under it.
    live.replace(widened, &rotating(&hierarchy, &new_ca))
        .await
        .unwrap();
    let moved = identity(
        &hierarchy,
        &new_ca,
        "node",
        30,
        &rotating(&hierarchy, &new_ca),
    );
    live.replace(moved.clone(), &rotating(&hierarchy, &new_ca))
        .await
        .unwrap();
    assert_eq!(live.snapshot().node_ca_der, new_ca.ca.certificate_der);

    // A council set that no longer holds this node's issuer would make it
    // distrust its own chain: refused, current set kept.
    let only_old = not_rotating(&hierarchy);
    assert!(live.adopt_trust(&only_old).await.is_err());
    assert_eq!(live.snapshot().trust, rotating(&hierarchy, &new_ca));
}
