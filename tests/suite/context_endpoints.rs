//! A relish context that lists several nodes keeps working when the node it
//! asks first is down (#2241).
//!
//! The real `relish` binary runs against a context whose first endpoint
//! refuses connections and whose second is a stand-in cluster API served
//! over TLS with a certificate from the context's own CA. It must pass over
//! the dead node, say which node answered, remember it, and still refuse a
//! node whose certificate another CA signed.

use std::path::Path;
use std::time::{Duration, SystemTime};

use reliaburger::sesame::ca::{self, CaHierarchy};
use reliaburger::sesame::identity_store::NodeIdentity;
use reliaburger::sesame::mtls::{CrlHandle, build_api_server_config};
use reliaburger::sesame::types::SerialNumber;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "rbrg_context_admin";

fn identity(hierarchy: &CaHierarchy, node: &str) -> NodeIdentity {
    let (certificate_der, private_key_der, serial) = ca::issue_node_cert(
        node,
        SerialNumber(10),
        &hierarchy.node.signing_keypair,
        &hierarchy.node.certificate_params,
    )
    .unwrap();
    let now = SystemTime::now();
    NodeIdentity {
        node_id: node.to_string(),
        certificate_der,
        private_key_der,
        serial,
        ca_generation: 0,
        trust: reliaburger::sesame::trust::TrustSet::single(
            hierarchy.node.ca.certificate_der.clone(),
            hierarchy.root.ca.certificate_der.clone(),
        ),
        node_ca_der: hierarchy.node.ca.certificate_der.clone(),
        root_ca_der: hierarchy.root.ca.certificate_der.clone(),
        not_before: now,
        not_after: now + Duration::from_secs(365 * 24 * 3600),
    }
}

/// A stand-in node of the cluster `hierarchy` signs for, answering
/// `/v1/cluster/nodes` with one node named `node` to the context's token.
/// Returns its endpoint.
async fn stand_in_node(
    hierarchy: &CaHierarchy,
    node: &str,
    shutdown: &CancellationToken,
) -> String {
    let acceptor = tokio_rustls::TlsAcceptor::from(
        build_api_server_config(&identity(hierarchy, node), CrlHandle::default()).unwrap(),
    );
    let answer = json!([{
        "node_id": node,
        "address": "127.0.0.1:9443",
        "state": "alive",
        "incarnation": 1,
        "is_council": true,
        "is_leader": true,
        "labels": {},
    }]);
    let app = axum::Router::new().route(
        "/v1/cluster/nodes",
        axum::routing::get(move |headers: axum::http::HeaderMap| {
            let answer = answer.clone();
            async move {
                let bearer = headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok());
                if bearer == Some(&format!("Bearer {TOKEN}")) {
                    (axum::http::StatusCode::OK, axum::Json(answer))
                } else {
                    (axum::http::StatusCode::UNAUTHORIZED, axum::Json(json!({})))
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("https://{}", listener.local_addr().unwrap());
    tokio::spawn(reliaburger::sesame::connection::serve_router_over_tls(
        listener,
        acceptor,
        app,
        reliaburger::sesame::connection::ConnectionTimeouts::PRODUCTION,
        shutdown.clone(),
    ));
    endpoint
}

/// Endpoints on this host that refuse connections, as a node whose Bun has
/// stopped does. Privileged ports nothing here listens on, rather than a
/// port bound and dropped, which a parallel test could take and answer on
/// (#2156). Two, because a context lists an endpoint once.
const REFUSING: [&str; 2] = ["https://127.0.0.1:1", "https://127.0.0.1:2"];

/// Save a bare-metal context in `home` that trusts `hierarchy`'s root CA
/// and lists `endpoints`, the first one first.
fn save_context(home: &Path, hierarchy: &CaHierarchy, endpoints: &[&str]) {
    let ca_cert = home.join("root-ca.crt");
    std::fs::write(
        &ca_cert,
        reliaburger::sesame::cert::der_to_pem(&hierarchy.root.ca.certificate_der, "CERTIFICATE"),
    )
    .unwrap();
    let context = json!({
        "schema": 1,
        "owner": "bare-metal/lab",
        "endpoint": endpoints[0],
        "other_endpoints": endpoints[1..],
        "token": TOKEN,
        "ca_cert": ca_cert,
        "service_endpoints": {},
    });
    write_private(&home.join("context.json"), &context.to_string());
}

fn write_private(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn saved_context(home: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(home.join("context.json")).unwrap()).unwrap()
}

/// `relish nodes -o json` with `home` as its state directory: exit success,
/// stdout and stderr.
async fn relish_nodes(home: &Path) -> (bool, String, String) {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(["nodes", "-o", "json"])
        .env("RELIABURGER_HOME", home)
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .env_remove("RELIABURGER_ENDPOINT")
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test]
async fn with_its_first_node_down_relish_answers_through_the_next_and_remembers_it() {
    let home = tempfile::tempdir().unwrap();
    let shutdown = CancellationToken::new();
    let hierarchy = ca::generate_ca_hierarchy("lab", b"ikm").unwrap();
    let dead = REFUSING[0];
    let live = stand_in_node(&hierarchy, "wyse-2", &shutdown).await;
    save_context(home.path(), &hierarchy, &[dead, &live]);

    let (ok, stdout, stderr) = relish_nodes(home.path()).await;

    assert!(ok, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("wyse-2"), "{stdout}");
    assert!(
        stderr.contains(&format!("{dead} didn't answer")),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("relish is using {live}")),
        "{stderr}"
    );
    assert_eq!(saved_context(home.path())["last_answered"], live.as_str());

    // The next command asks the node that answered first, so it neither
    // waits on the dead one nor says anything about it.
    let (ok, stdout, stderr) = relish_nodes(home.path()).await;
    assert!(ok, "stdout: {stdout}\nstderr: {stderr}");
    assert!(!stderr.contains("didn't answer"), "{stderr}");
    shutdown.cancel();
}

#[tokio::test]
async fn when_no_node_answers_the_error_lists_every_endpoint_tried() {
    let home = tempfile::tempdir().unwrap();
    let hierarchy = ca::generate_ca_hierarchy("lab", b"ikm").unwrap();
    let [first, second] = REFUSING;
    save_context(home.path(), &hierarchy, &[first, second]);

    let (ok, _, stderr) = relish_nodes(home.path()).await;

    assert!(!ok);
    assert!(
        stderr.contains("none of the context's 2 endpoints answered"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("{first}: connection refused")),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("{second}: connection refused")),
        "{stderr}"
    );
}

/// Falling over only skips a node that doesn't accept a connection. One
/// that does, and proves another CA's identity, fails the command as it
/// would have before: relish never sends the token past an untrusted
/// handshake to look for a node it trusts.
#[tokio::test]
async fn a_node_signed_by_another_ca_is_refused_not_passed_over() {
    let home = tempfile::tempdir().unwrap();
    let shutdown = CancellationToken::new();
    let hierarchy = ca::generate_ca_hierarchy("lab", b"ikm").unwrap();
    let impostor_ca = ca::generate_ca_hierarchy("impostor", b"other").unwrap();
    let impostor = stand_in_node(&impostor_ca, "wyse-9", &shutdown).await;
    let live = stand_in_node(&hierarchy, "wyse-2", &shutdown).await;
    save_context(home.path(), &hierarchy, &[&impostor, &live]);

    let (ok, stdout, stderr) = relish_nodes(home.path()).await;

    assert!(!ok, "stdout: {stdout}");
    assert!(stderr.contains("certificate isn't trusted"), "{stderr}");
    assert!(stderr.contains(&impostor), "{stderr}");
    shutdown.cancel();
}
