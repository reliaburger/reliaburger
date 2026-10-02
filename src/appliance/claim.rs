//! Claiming a machine over the LAN (W5): a seed delivered over the network.
//!
//! A machine that boots with no seed becomes *unclaimed*. It makes a
//! self-signed key (kept until it's claimed), shows the key's fingerprint
//! on its console, announces itself over mDNS as
//! `_reliaburger-unclaimed._tcp` (no secrets in the announcement) and
//! serves a claim API over TLS on [`CLAIM_PORT`]. `relish machines claim`
//! posts it a seed, over a connection pinned to that fingerprint, so the
//! seed's secrets can only reach the machine whose console shows it. From
//! there the machine goes on exactly as if the seed had come on a stick.
//!
//! The machine accepts the first valid seed it's given. On a LAN where
//! someone else could claim your machines first, compare the fingerprint
//! on its console with what relish shows; that's what the interactive
//! claim does.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::seed::Seed;

/// The claim API's port.
pub const CLAIM_PORT: u16 = 9119;

/// The mDNS service an unclaimed machine announces.
pub const SERVICE_TYPE: &str = "_reliaburger-unclaimed._tcp.local.";

/// What an unclaimed machine tells whoever asks (and its mDNS TXT record):
/// nothing secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineInfo {
    /// Lower-case, colon-separated.
    pub macs: Vec<String>,
    /// `x86_64` or `aarch64`.
    pub arch: String,
    pub os_version: Option<String>,
    /// `sha256:<hex>` of the claim key's certificate.
    pub fingerprint: String,
}

/// The claim key: a self-signed certificate made on the first unclaimed
/// boot and kept until the machine is claimed, so its fingerprint doesn't
/// change under the operator's eyes.
pub struct ClaimKey {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl ClaimKey {
    /// Load the key from `dir`, or make and save one.
    pub fn load_or_create(dir: &Path) -> std::io::Result<Self> {
        let (cert_path, key_path) = (dir.join("claim.crt"), dir.join("claim.key"));
        if let (Ok(cert_der), Ok(key_der)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
            return Ok(ClaimKey { cert_der, key_der });
        }
        let generated =
            rcgen::generate_simple_self_signed(vec!["reliaburger-unclaimed".to_string()])
                .map_err(std::io::Error::other)?;
        let key = ClaimKey {
            cert_der: generated.cert.der().to_vec(),
            key_der: generated.key_pair.serialize_der(),
        };
        super::prepare::write_atomic(&key_path, &key.key_der, 0o600)?;
        super::prepare::write_atomic(&cert_path, &key.cert_der, 0o644)?;
        Ok(key)
    }

    /// `sha256:<hex>` of the certificate.
    pub fn fingerprint(&self) -> String {
        certificate_fingerprint(&self.cert_der)
    }
}

/// `sha256:<hex>` of a DER certificate: what relish pins the claim to.
pub fn certificate_fingerprint(cert_der: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(cert_der)))
}

/// The fingerprint as a person reads it off a console: its first 16 hex
/// digits in groups of four (`3f9a-12bc-77de-0a41`). 64 bits is plenty to
/// tell a stranger's key from this machine's when someone compares them by
/// eye; relish pins the whole digest.
pub fn short_fingerprint(fingerprint: &str) -> String {
    let hex = fingerprint.strip_prefix("sha256:").unwrap_or(fingerprint);
    hex.as_bytes()
        .chunks(4)
        .take(4)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

#[derive(Clone)]
struct ClaimState {
    info: MachineInfo,
    /// Where the claimed seed goes, and the signal that it arrived.
    seed_path: PathBuf,
    claimed: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

/// The claim API: `GET /v1/claim` describes the machine, `POST /v1/claim`
/// with a seed tarball claims it (once).
pub fn router(
    info: MachineInfo,
    seed_path: PathBuf,
    claimed: tokio::sync::oneshot::Sender<()>,
) -> axum::Router {
    let state = ClaimState {
        info,
        seed_path,
        claimed: Arc::new(Mutex::new(Some(claimed))),
    };
    axum::Router::new()
        .route(
            "/v1/claim",
            axum::routing::get(info_handler).post(claim_handler),
        )
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
        .with_state(state)
}

async fn info_handler(State(state): State<ClaimState>) -> impl IntoResponse {
    Json(state.info)
}

async fn claim_handler(State(state): State<ClaimState>, body: Bytes) -> impl IntoResponse {
    let mut claimed = state.claimed.lock().await;
    if claimed.is_none() {
        return (
            StatusCode::CONFLICT,
            "this machine has been claimed already".to_string(),
        );
    }
    if let Err(error) = Seed::from_tar_gz(&body) {
        return (StatusCode::BAD_REQUEST, format!("not a seed: {error}"));
    }
    if let Err(error) = super::prepare::write_atomic(&state.seed_path, &body, 0o600) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("saving the seed: {error}"),
        );
    }
    if let Some(signal) = claimed.take() {
        let _ = signal.send(());
    }
    (StatusCode::OK, "claimed".to_string())
}

/// Serve the claim API over TLS with `key` on [`CLAIM_PORT`] and announce
/// the machine over mDNS, until a seed has been posted.
pub async fn serve_until_claimed(
    key: &ClaimKey,
    info: MachineInfo,
    seed_path: PathBuf,
) -> std::io::Result<()> {
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], CLAIM_PORT))).await?;
    let announcement = announce(&info);
    let served = serve(listener, key, info, seed_path).await;
    if let Some(daemon) = announcement {
        let _ = daemon.shutdown();
    }
    served
}

/// Serve the claim API over TLS with `key` on `listener` until a seed has
/// been posted.
pub async fn serve(
    listener: tokio::net::TcpListener,
    key: &ClaimKey,
    info: MachineInfo,
    seed_path: PathBuf,
) -> std::io::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from(
                key.cert_der.clone(),
            )],
            rustls::pki_types::PrivateKeyDer::try_from(key.key_der.clone())
                .map_err(std::io::Error::other)?,
        )
        .map_err(std::io::Error::other)?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(certified));
    let (claimed_tx, claimed_rx) = tokio::sync::oneshot::channel();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let server = tokio::spawn(crate::sesame::connection::serve_router_over_tls(
        listener,
        acceptor,
        router(info, seed_path, claimed_tx),
        crate::sesame::connection::ConnectionTimeouts::PRODUCTION,
        shutdown.clone(),
    ));
    let _ = claimed_rx.await;
    // Let the claim's response reach relish before the listener goes.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    shutdown.cancel();
    let _ = server.await;
    Ok(())
}

/// Announce `_reliaburger-unclaimed._tcp` with the machine's MACs,
/// architecture and short fingerprint. Best effort: claiming by address
/// works without it.
fn announce(info: &MachineInfo) -> Option<mdns_sd::ServiceDaemon> {
    let daemon = mdns_sd::ServiceDaemon::new().ok()?;
    let instance = info
        .macs
        .first()
        .map(|mac| format!("reliaburger-{}", mac.replace(':', "")))
        .unwrap_or_else(|| "reliaburger".to_string());
    let host = format!("{instance}.local.");
    let properties = [
        ("mac", info.macs.join(",")),
        ("arch", info.arch.clone()),
        ("fp", short_fingerprint(&info.fingerprint)),
    ];
    let service = mdns_sd::ServiceInfo::new(
        SERVICE_TYPE,
        &instance,
        &host,
        "",
        CLAIM_PORT,
        &properties[..],
    )
    .ok()?
    .enable_addr_auto();
    daemon.register(service).ok()?;
    Some(daemon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    fn info() -> MachineInfo {
        MachineInfo {
            macs: vec!["d8:9e:f3:12:34:56".into()],
            arch: "x86_64".into(),
            os_version: Some("2026.41.0".into()),
            fingerprint: "sha256:3f9a12bc77de0a41ffff".into(),
        }
    }

    #[test]
    fn the_short_fingerprint_is_four_groups_of_four() {
        assert_eq!(
            short_fingerprint("sha256:3f9a12bc77de0a41ffffeeee"),
            "3f9a-12bc-77de-0a41"
        );
    }

    #[test]
    fn the_claim_key_is_kept_across_boots() {
        let dir = tempfile::tempdir().unwrap();
        let first = ClaimKey::load_or_create(dir.path()).unwrap();
        let again = ClaimKey::load_or_create(dir.path()).unwrap();
        assert_eq!(first.fingerprint(), again.fingerprint());
        assert!(first.fingerprint().starts_with("sha256:"));
        assert_eq!(first.fingerprint().len(), "sha256:".len() + 64);
    }

    async fn call(
        router: axum::Router,
        request: axum::http::Request<axum::body::Body>,
    ) -> (StatusCode, String) {
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn a_machine_takes_one_valid_seed_and_only_one() {
        let dir = tempfile::tempdir().unwrap();
        let seed_path = dir.path().join("claimed.seed");
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let router = router(info(), seed_path.clone(), tx);

        let (status, body) = call(
            router.clone(),
            axum::http::Request::get("/v1/claim")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(serde_json::from_str::<MachineInfo>(&body).unwrap(), info());

        let (status, _) = call(
            router.clone(),
            axum::http::Request::post("/v1/claim")
                .body(axum::body::Body::from("not a seed"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(rx.try_recv().is_err(), "a bad seed claims nothing");

        let seed = super::super::seed::tests::tarball(&[(
            "seed.toml",
            super::super::seed::tests::JOIN_TOML.as_bytes(),
        )]);
        let (status, _) = call(
            router.clone(),
            axum::http::Request::post("/v1/claim")
                .body(axum::body::Body::from(seed.clone()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(std::fs::read(&seed_path).unwrap(), seed);
        assert!(rx.try_recv().is_ok());

        let (status, _) = call(
            router,
            axum::http::Request::post("/v1/claim")
                .body(axum::body::Body::from(seed))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "claimed already");
    }
}
