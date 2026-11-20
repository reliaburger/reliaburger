//! A route's `tls` mode chooses the certificate its host is served (#2011).
//!
//! One HTTPS listener serves every route on a node, so the choice happens per
//! SNI hostname: a `cluster` host gets a leaf from the cluster Ingress CA, an
//! `explicit` host gets the operator's `tls_cert`/`tls_key` pair.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reliaburger::config::app::IngressSpec;
use reliaburger::onion::service_id::ServiceId;
use reliaburger::onion::service_map::ServiceMap;
use reliaburger::onion::types::BackendInstance;
use reliaburger::wrapper::proxy::bind_proxy_with_tls;
use reliaburger::wrapper::routing::RoutingTable;
use reliaburger::wrapper::tls::{IngressCertResolver, generate_self_signed_cert};
use reliaburger::wrapper::types::WrapperConfig;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::task_harness;

const CLUSTER_HOST: &str = "cluster.example";
const EXPLICIT_HOST: &str = "explicit.example";
const PLAIN_HOST: &str = "plain.example";

/// A routing table with one route per `(host, tls)` pair.
fn routes(entries: &[(&str, &str)]) -> Arc<RwLock<RoutingTable>> {
    let mut map = ServiceMap::new();
    let mut configs = HashMap::new();
    for (index, (host, tls)) in entries.iter().enumerate() {
        let app = format!("app{index}");
        map.register_app(&app, "default", 8080, None).unwrap();
        map.add_backend(
            &ServiceId::new("default", &app),
            BackendInstance {
                instance_id: format!("{app}-0"),
                node_ip: Ipv4Addr::new(10, 0, 2, 2),
                host_port: 30001 + index as u16,
                healthy: true,
                local: false,
            },
        )
        .unwrap();
        configs.insert(
            ("default".to_string(), app),
            IngressSpec {
                host: host.to_string(),
                path: None,
                tls: Some(tls.to_string()),
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );
    }
    let mut table = RoutingTable::new();
    table.rebuild(&map, &configs).unwrap();
    Arc::new(RwLock::new(table))
}

/// An Ingress CA resolver over `table`, and the CA certificate it signs with.
fn cluster_resolver(
    table: Arc<RwLock<RoutingTable>>,
) -> (Arc<dyn rustls::server::ResolvesServerCert>, Vec<u8>) {
    let hierarchy =
        reliaburger::sesame::ca::generate_ca_hierarchy("test-cluster", b"test-wrap-ikm").unwrap();
    let ca_der = hierarchy.ingress.ca.certificate_der.clone();
    let (default_cert, default_key) = generate_self_signed_cert().unwrap();
    let resolver = IngressCertResolver::new(
        hierarchy.ingress.signing_keypair,
        hierarchy.ingress.certificate_params,
        CertificateDer::from(hierarchy.ingress.ca.certificate_der),
        Duration::from_secs(3600),
        table.try_read().unwrap().certificate_modes(),
        vec![default_cert],
        default_key,
    )
    .unwrap();
    (Arc::new(resolver), ca_der)
}

/// An operator pair on disk for `host`; returns the paths and the leaf.
fn operator_pair(
    directory: &std::path::Path,
    host: &str,
) -> (std::path::PathBuf, std::path::PathBuf, Vec<u8>) {
    let generated = rcgen::generate_simple_self_signed(vec![host.to_string()]).unwrap();
    let cert_path = directory.join("cert.pem");
    let key_path = directory.join("key.pem");
    std::fs::write(&cert_path, generated.cert.pem()).unwrap();
    std::fs::write(&key_path, generated.signing_key.serialize_pem()).unwrap();
    (cert_path, key_path, generated.cert.der().to_vec())
}

/// Bind a listener and return its HTTPS address.
async fn start(
    config: WrapperConfig,
    table: Arc<RwLock<RoutingTable>>,
    cluster: Option<Arc<dyn rustls::server::ResolvesServerCert>>,
) -> (SocketAddr, task_harness::TestTasks) {
    let shutdown = CancellationToken::new();
    let proxy = bind_proxy_with_tls(
        WrapperConfig {
            http_port: 0,
            https_port: 0,
            ..config
        },
        table,
        None,
        cluster,
        None,
        shutdown.clone(),
    )
    .await
    .unwrap();
    let address = (Ipv4Addr::LOCALHOST, proxy.https_addr.port()).into();
    let task = tokio::spawn(async move { proxy.serve().await.unwrap() });
    (address, task_harness::TestTasks::new(shutdown, vec![task]))
}

/// Records nothing and trusts anything: these tests check which certificate
/// the listener picks, not whether a client would trust it.
#[derive(Debug)]
struct AcceptAnyServer(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServer {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// The leaf the listener at `address` presents for SNI `host`.
async fn leaf_for(address: SocketAddr, host: &str) -> Vec<u8> {
    handshake(address, host, true).await
}

/// The leaf the listener presents to a client that sends no SNI at all.
async fn leaf_without_sni(address: SocketAddr) -> Vec<u8> {
    handshake(address, "localhost", false).await
}

async fn handshake(address: SocketAddr, host: &str, send_sni: bool) -> Vec<u8> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServer(provider)))
        .with_no_client_auth();
    config.enable_sni = send_sni;
    let name = ServerName::try_from(host.to_string()).unwrap();
    let stream = tokio::time::timeout(Duration::from_secs(3), async {
        tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, TcpStream::connect(address).await.unwrap())
            .await
            .unwrap()
    })
    .await
    .unwrap();
    stream.get_ref().1.peer_certificates().unwrap()[0].to_vec()
}

/// Whether `leaf` was signed by the CA whose certificate is `ca_der`.
fn signed_by(leaf: &[u8], ca_der: &[u8]) -> bool {
    use x509_parser::prelude::FromDer;
    let (_, leaf) = x509_parser::certificate::X509Certificate::from_der(leaf).unwrap();
    let (_, ca) = x509_parser::certificate::X509Certificate::from_der(ca_der).unwrap();
    leaf.verify_signature(Some(ca.public_key())).is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_and_explicit_routes_on_one_listener_get_their_own_certificates() {
    let directory = tempfile::tempdir().unwrap();
    let (cert_path, key_path, operator_leaf) = operator_pair(directory.path(), EXPLICIT_HOST);
    let table = routes(&[(CLUSTER_HOST, "cluster"), (EXPLICIT_HOST, "explicit")]);
    let (cluster, ingress_ca) = cluster_resolver(Arc::clone(&table));
    let (address, _tasks) = start(
        WrapperConfig {
            tls_cert_path: Some(cert_path),
            tls_key_path: Some(key_path),
            ..WrapperConfig::default()
        },
        table,
        Some(cluster),
    )
    .await;

    let explicit = leaf_for(address, EXPLICIT_HOST).await;
    assert_eq!(
        explicit, operator_leaf,
        "an explicit route gets the operator's pair"
    );

    let cluster = leaf_for(address, CLUSTER_HOST).await;
    assert_ne!(
        cluster, operator_leaf,
        "a cluster route must not get the operator's pair"
    );
    assert!(
        signed_by(&cluster, &ingress_ca),
        "a cluster route gets a leaf from the Ingress CA"
    );

    assert_eq!(
        leaf_without_sni(address).await,
        operator_leaf,
        "a handshake with no SNI names no route, so it gets the operator's pair"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cluster_only_node_mints_no_leaf_for_plain_routes_or_no_sni() {
    let table = routes(&[(CLUSTER_HOST, "cluster"), (PLAIN_HOST, "none")]);
    let (cluster, ingress_ca) = cluster_resolver(Arc::clone(&table));
    let (address, _tasks) = start(WrapperConfig::default(), table, Some(cluster)).await;

    assert!(signed_by(
        &leaf_for(address, CLUSTER_HOST).await,
        &ingress_ca
    ));
    // Before #2011 the Ingress CA minted a leaf for any routed host, plain
    // ones included. Only `tls = "cluster"` hosts get one now.
    assert!(
        !signed_by(&leaf_for(address, PLAIN_HOST).await, &ingress_ca),
        "a plain route's host gets the self-signed certificate"
    );
    assert!(
        !signed_by(&leaf_without_sni(address).await, &ingress_ca),
        "a handshake with no SNI gets the self-signed certificate"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_route_without_an_operator_pair_never_gets_a_cluster_leaf() {
    let table = routes(&[(EXPLICIT_HOST, "explicit")]);
    let (cluster, ingress_ca) = cluster_resolver(Arc::clone(&table));
    let (address, _tasks) = start(WrapperConfig::default(), table, Some(cluster)).await;

    let leaf = leaf_for(address, EXPLICIT_HOST).await;
    assert!(
        !signed_by(&leaf, &ingress_ca),
        "an explicit route on a node with no tls_cert/tls_key pair must not be \
         served a cluster-CA leaf"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cluster_route_without_the_ingress_ca_never_gets_the_operator_pair() {
    let directory = tempfile::tempdir().unwrap();
    let (cert_path, key_path, operator_leaf) = operator_pair(directory.path(), CLUSTER_HOST);
    let table = routes(&[(CLUSTER_HOST, "cluster")]);
    let (address, _tasks) = start(
        WrapperConfig {
            tls_cert_path: Some(cert_path),
            tls_key_path: Some(key_path),
            ..WrapperConfig::default()
        },
        table,
        None,
    )
    .await;

    assert_ne!(
        leaf_for(address, CLUSTER_HOST).await,
        operator_leaf,
        "a cluster route on a node with no Ingress CA must not be served the \
         operator's pair"
    );
}

/// A rebuild holds the routing table's write lock, and tokio's fair lock
/// refuses readers while a writer waits. Handshakes in that window must still
/// get each host's own certificate, not a fallback a validating client rejects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certificates_follow_the_route_mode_while_a_rebuild_holds_the_table() {
    let directory = tempfile::tempdir().unwrap();
    let (cert_path, key_path, operator_leaf) = operator_pair(directory.path(), EXPLICIT_HOST);
    let table = routes(&[(CLUSTER_HOST, "cluster"), (EXPLICIT_HOST, "explicit")]);
    let (cluster, ingress_ca) = cluster_resolver(Arc::clone(&table));
    let (address, _tasks) = start(
        WrapperConfig {
            tls_cert_path: Some(cert_path),
            tls_key_path: Some(key_path),
            ..WrapperConfig::default()
        },
        Arc::clone(&table),
        Some(cluster),
    )
    .await;

    let rebuilding = table.write().await;
    assert_eq!(
        leaf_for(address, EXPLICIT_HOST).await,
        operator_leaf,
        "an explicit host keeps the operator's pair during a rebuild"
    );
    assert!(
        signed_by(&leaf_for(address, CLUSTER_HOST).await, &ingress_ca),
        "a cluster host keeps its Ingress CA leaf during a rebuild"
    );
    drop(rebuilding);
}
