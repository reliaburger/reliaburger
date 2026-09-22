//! Live TLS credentials shared by one node's clients and listeners.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::sign::CertifiedKey;

use super::identity_store::{NodeIdentity, validate_identity};
use super::mtls::MtlsError;

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
/// Replacement keeps the node identifier and trust anchors fixed; CA rotation
/// is a separate protocol. Debug output never includes credentials.
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
    /// even if the caller stops waiting. Exact certificate retries are idempotent.
    pub async fn replace(&self, identity: NodeIdentity) -> Result<(), MtlsError> {
        let guard = Arc::clone(&self.writer).lock_owned().await;
        let current = self.current.clone();
        let directory = Arc::clone(&self.directory);
        tokio::task::spawn_blocking(move || {
            // The worker owns the writer guard through persistence and publication;
            // dropping the caller's future cannot interrupt that transaction.
            let _guard = guard;
            let replacement = ValidatedIdentity::new(identity)?;
            let previous = current.borrow().clone();
            if replacement.identity.node_id != previous.identity.node_id
                || replacement.identity.node_ca_der != previous.identity.node_ca_der
                || replacement.identity.root_ca_der != previous.identity.root_ca_der
                || replacement.identity.ca_generation != previous.identity.ca_generation
            {
                return Err(MtlsError::InvalidCert(
                    "renewal changes the node identity or trust anchors".into(),
                ));
            }
            if replacement.identity.certificate_der == previous.identity.certificate_der {
                return Ok(());
            }
            if replacement.identity.serial.0 <= previous.identity.serial.0 {
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
        let pending = tokio::spawn(async move { writer.replace(replacement).await });
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
