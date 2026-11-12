//! Live TLS credentials shared by one node's clients and listeners.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::sign::CertifiedKey;

use super::identity_store::{NodeIdentity, validate_identity};
use super::mtls::MtlsError;
use super::trust::TrustSet;

#[derive(Clone)]
struct ValidatedIdentity {
    identity: Arc<NodeIdentity>,
    key: Arc<CertifiedKey>,
}

impl ValidatedIdentity {
    fn new(identity: NodeIdentity) -> Result<Self, MtlsError> {
        let identity = validate_identity(identity)
            .map_err(|error| MtlsError::InvalidCert(error.to_string()))?;
        let (chain, key) = super::mtls::identity_chain_and_key(&identity)?;
        let key = CertifiedKey::from_der(chain, key, &rustls::crypto::ring::default_provider())
            .map_err(|error| MtlsError::ConfigFailed(error.to_string()))?;
        Ok(Self {
            identity: Arc::new(identity),
            key: Arc::new(key),
        })
    }
}

/// A validated identity that existing TLS configurations can observe changing.
/// Replacement keeps the node identifier and cluster root fixed. The trust set
/// changes only to the one the council's state describes (F04 R2). Debug
/// output never includes credentials.
#[derive(Clone)]
pub struct LiveNodeIdentity {
    current: tokio::sync::watch::Sender<ValidatedIdentity>,
    directory: Arc<PathBuf>,
    writer: Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for LiveNodeIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveNodeIdentity").finish_non_exhaustive()
    }
}

impl LiveNodeIdentity {
    /// Load and validate an installed identity, retaining its persistence directory.
    pub fn load(directory: &Path) -> Result<Self, MtlsError> {
        let identity = super::identity_store::load(directory)
            .map_err(|error| MtlsError::InvalidCert(error.to_string()))?
            .ok_or_else(|| MtlsError::InvalidCert("no node identity installed".into()))?;
        let (current, _) = tokio::sync::watch::channel(ValidatedIdentity::new(identity)?);
        Ok(Self {
            current,
            directory: Arc::new(directory.to_owned()),
            writer: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Read a coherent identity snapshot, including its signed validity dates.
    pub fn snapshot(&self) -> Arc<NodeIdentity> {
        Arc::clone(&self.current.borrow().identity)
    }

    /// Validate and persist a newer identity before publishing it to TLS.
    /// Once persistence starts, the operation owns its work through publication
    /// even if the caller stops waiting. Exact retries are idempotent.
    ///
    /// The replacement may carry a different trust set only when that set is
    /// `council_trust`, the one the council's state describes. Anything else
    /// would let a renewal response, or a bug, widen or narrow what this node
    /// accepts from peers.
    pub async fn replace(
        &self,
        identity: NodeIdentity,
        council_trust: &TrustSet,
    ) -> Result<(), MtlsError> {
        let council_trust = council_trust.clone();
        let guard = Arc::clone(&self.writer).lock_owned().await;
        let current = self.current.clone();
        let directory = Arc::clone(&self.directory);
        tokio::task::spawn_blocking(move || {
            // The worker owns the writer guard through persistence and publication;
            // dropping the caller's future cannot interrupt that transaction.
            let _guard = guard;
            let replacement = ValidatedIdentity::new(identity)?;
            let previous = current.borrow().clone();
            // The root names the cluster; only a root rotation may change it,
            // and that isn't supported yet.
            if replacement.identity.node_id != previous.identity.node_id
                || replacement.identity.root_ca_der != previous.identity.root_ca_der
                || replacement.identity.ca_generation < previous.identity.ca_generation
            {
                return Err(MtlsError::InvalidCert(
                    "renewal changes the node identity or cluster root".into(),
                ));
            }
            let trust_changed = replacement.identity.trust != previous.identity.trust;
            if trust_changed && replacement.identity.trust != council_trust {
                return Err(MtlsError::InvalidCert(
                    "the new trust set is not the one the council's state describes".into(),
                ));
            }
            let same_leaf =
                replacement.identity.certificate_der == previous.identity.certificate_der;
            if same_leaf && !trust_changed {
                return Ok(());
            }
            if !same_leaf && replacement.identity.serial.0 <= previous.identity.serial.0 {
                return Err(MtlsError::InvalidCert(
                    "renewal serial is not newer than the current identity".into(),
                ));
            }
            super::identity_store::save(&directory, &replacement.identity).map_err(|error| {
                MtlsError::ConfigFailed(format!("failed to persist renewed identity: {error}"))
            })?;
            current.send_replace(replacement);
            Ok(())
        })
        .await
        .map_err(|error| {
            MtlsError::ConfigFailed(format!("identity replacement worker failed: {error}"))
        })?
    }
}

impl LiveNodeIdentity {
    /// Install the council's trust set, keeping the current leaf. Returns
    /// whether anything changed.
    ///
    /// Bun's security refresh calls this on every tick, the way it refreshes
    /// the CRL. Listeners and clients built from this identity read the trust
    /// set on each handshake, so the next connection uses it. A set that
    /// doesn't include this node's own issuer is refused: installing it would
    /// make the node distrust its own chain.
    pub async fn adopt_trust(&self, council_trust: &TrustSet) -> Result<bool, MtlsError> {
        let current = self.snapshot();
        if current.trust == *council_trust {
            return Ok(false);
        }
        let updated = NodeIdentity {
            trust: council_trust.clone(),
            ..(*current).clone()
        };
        self.replace(updated, council_trust).await?;
        Ok(true)
    }

    /// [`Self::adopt_trust`] with the trust set `state` describes. A state
    /// with no Node CA or root (a council not yet seeded) changes nothing.
    pub async fn adopt_council_trust(
        &self,
        state: &super::types::SecurityState,
    ) -> Result<bool, MtlsError> {
        match TrustSet::from_state(state) {
            Some(trust) => self.adopt_trust(&trust).await,
            None => Ok(false),
        }
    }
}

impl rustls::server::ResolvesServerCert for LiveNodeIdentity {
    fn resolve(&self, _: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let current = self.current.borrow();
        let now = std::time::SystemTime::now();
        (current.identity.not_before <= now && now < current.identity.not_after)
            .then(|| Arc::clone(&current.key))
    }
}

impl rustls::client::ResolvesClientCert for LiveNodeIdentity {
    fn resolve(&self, _: &[&[u8]], _: &[rustls::SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        // None would silently become anonymous client auth on the optional-mTLS
        // API. Present the configured identity even after expiry so the peer
        // refuses it, just as with rustls's static client certificate resolver.
        Some(Arc::clone(&self.current.borrow().key))
    }

    fn has_certs(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sesame::{ca, identity_store, types::SerialNumber};
    use std::time::{Duration, SystemTime};

    fn node_identity(hierarchy: &ca::CaHierarchy, node: &str, serial: u64) -> NodeIdentity {
        let (certificate_der, private_key_der, serial) = ca::issue_node_cert(
            node,
            SerialNumber(serial),
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();
        NodeIdentity {
            node_id: node.into(),
            certificate_der,
            private_key_der,
            serial,
            ca_generation: 0,
            trust: crate::sesame::trust::TrustSet::single(
                hierarchy.node.ca.certificate_der.clone(),
                hierarchy.root.ca.certificate_der.clone(),
            ),
            node_ca_der: hierarchy.node.ca.certificate_der.clone(),
            root_ca_der: hierarchy.root.ca.certificate_der.clone(),
            not_before: SystemTime::UNIX_EPOCH,
            not_after: SystemTime::UNIX_EPOCH,
        }
    }

    /// One TLS handshake: the client certificate the server saw, or the
    /// server's error.
    async fn server_sees(
        server: Arc<rustls::ServerConfig>,
        client: Arc<rustls::ClientConfig>,
    ) -> Result<Vec<u8>, String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let accept = async {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = tokio_rustls::TlsAcceptor::from(server)
                .accept(socket)
                .await
                .map_err(|error| error.to_string())?;
            let peer = stream
                .get_ref()
                .1
                .peer_certificates()
                .map(|chain| chain[0].to_vec())
                .ok_or_else(|| "no client certificate".to_string())?;
            let _ = stream.write_all(b"ok").await;
            Ok(peer)
        };
        let connect = async {
            let socket = tokio::net::TcpStream::connect(address).await.unwrap();
            let name = rustls::pki_types::ServerName::IpAddress(address.ip().into());
            if let Ok(mut stream) = tokio_rustls::TlsConnector::from(client)
                .connect(name, socket)
                .await
            {
                let _ = stream.read_u8().await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(accept, connect).0
        })
        .await
        .unwrap()
    }

    /// The live client resolver presents its identity even once it has
    /// expired, so the optional-mTLS API refuses it rather than treating the
    /// client as anonymous. The identity is published already expired:
    /// `replace` refuses an expired certificate, and the integration test
    /// that let one expire in real time raced its own setup (#1841).
    #[tokio::test]
    async fn an_expired_identity_is_presented_so_optional_mtls_refuses_it() {
        let hierarchy = ca::generate_ca_hierarchy("live-expiry", b"test-ikm").unwrap();
        let server_identity = node_identity(&hierarchy, "server", 10);
        let valid = node_identity(&hierarchy, "client", 11);
        let directory = tempfile::tempdir().unwrap();
        identity_store::save(directory.path(), &valid).unwrap();
        let live = LiveNodeIdentity::load(directory.path()).unwrap();

        let mut expired = node_identity(&hierarchy, "client", 12);
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["client".into()]).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "client");
        params.subject_alt_names.push(rcgen::SanType::URI(
            ca::node_spiffe_uri("client").try_into().unwrap(),
        ));
        params.serial_number = Some(rcgen::SerialNumber::from_slice(&12u64.to_be_bytes()));
        params.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
        ];
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::hours(2);
        params.not_after = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
        let issuer = rcgen::Issuer::from_params(
            &hierarchy.node.certificate_params,
            &hierarchy.node.signing_keypair,
        );
        expired.certificate_der = params.signed_by(&key, &issuer).unwrap().der().to_vec();
        expired.private_key_der = key.serialize_der();
        let (chain, private_key) = super::super::mtls::identity_chain_and_key(&expired).unwrap();
        let expired_key = CertifiedKey::from_der(
            chain,
            private_key,
            &rustls::crypto::ring::default_provider(),
        )
        .unwrap();

        let server = super::super::mtls::build_api_server_config(
            &server_identity,
            super::super::mtls::CrlHandle::default(),
        )
        .unwrap();
        let mut client = (*super::super::mtls::build_mtls_client_config(
            &live.snapshot(),
            super::super::mtls::CrlHandle::default(),
        )
        .unwrap())
        .clone();
        client.client_auth_cert_resolver = Arc::new(live.clone());
        let client = Arc::new(client);

        let seen = server_sees(server.clone(), client.clone()).await;
        assert_eq!(seen.as_deref(), Ok(valid.certificate_der.as_slice()));

        live.current.send_replace(ValidatedIdentity {
            identity: Arc::new(expired),
            key: Arc::new(expired_key),
        });
        let refused = server_sees(server, client).await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|error| error.to_lowercase().contains("expired")),
            "an expired identity must fail TLS, never become anonymous client auth: {refused:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn caller_cancellation_after_persistence_cannot_interrupt_publication() {
        let hierarchy = ca::generate_ca_hierarchy("cancel-renewal", b"test-ikm").unwrap();
        let issue = |serial| {
            let (certificate_der, private_key_der, serial) = ca::issue_node_cert(
                "node",
                SerialNumber(serial),
                &hierarchy.node.signing_keypair,
                &hierarchy.node.certificate_params,
            )
            .unwrap();
            NodeIdentity {
                node_id: "node".into(),
                certificate_der,
                private_key_der,
                serial,
                ca_generation: 0,
                trust: crate::sesame::trust::TrustSet::single(
                    hierarchy.node.ca.certificate_der.clone(),
                    hierarchy.root.ca.certificate_der.clone(),
                ),
                node_ca_der: hierarchy.node.ca.certificate_der.clone(),
                root_ca_der: hierarchy.root.ca.certificate_der.clone(),
                not_before: SystemTime::UNIX_EPOCH,
                not_after: SystemTime::UNIX_EPOCH,
            }
        };
        let directory = tempfile::tempdir().unwrap();
        identity_store::save(directory.path(), &issue(10)).unwrap();
        let live = LiveNodeIdentity::load(directory.path()).unwrap();
        let replacement = issue(20);

        // Hold publication at the watch write boundary on a blocking worker.
        // No asynchronous worker holds the deliberately stalled read guard.
        let current = live.current.clone();
        let (held_tx, held_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _snapshot = current.borrow();
            held_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        });
        held_rx.await.unwrap();
        let writer = live.clone();
        let trust = replacement.trust.clone();
        let pending = tokio::spawn(async move { writer.replace(replacement, &trust).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let path = directory.path().to_owned();
                let stored = tokio::task::spawn_blocking(move || identity_store::load(&path))
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                if stored.serial == SerialNumber(20) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        release_tx.send(()).unwrap();
        blocker.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while live.snapshot().serial != SerialNumber(20) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            identity_store::load(directory.path())
                .unwrap()
                .unwrap()
                .certificate_der,
            live.snapshot().certificate_der
        );
    }
}
