//! The one place outbound HTTP clients start from.
//!
//! reqwest 0.13 changed three defaults that 0.12 gave us: it verifies public
//! certificates against the operating system's trust store instead of the
//! bundled Mozilla roots, it falls back to the aws-lc crypto provider instead
//! of ring, and, because `object_store` turns on its `http2` feature, it offers
//! HTTP/2 in every TLS handshake. Cargo unifies features across the graph, so
//! our own `Cargo.toml` can't switch those back off. [`builder`] pins all
//! three to what 0.12 did.
//!
//! Clients that bring their own rustls config (the cluster CA, mTLS) still
//! start here; their config replaces the roots, and the rest still applies.

/// A reqwest client builder with the defaults Reliaburger relies on.
///
/// The caller adds timeouts, redirect policy, proxies and any extra roots.
pub fn builder() -> reqwest::ClientBuilder {
    // reqwest asks rustls for the process provider before its own aws-lc
    // fallback. Installing ring fails harmlessly once one is in place.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .http1_only()
        .tls_certs_only(bundled_roots().iter().cloned())
}

/// The Mozilla root programme as shipped in `webpki-root-certs`, parsed once.
fn bundled_roots() -> &'static [reqwest::Certificate] {
    static ROOTS: std::sync::LazyLock<Vec<reqwest::Certificate>> = std::sync::LazyLock::new(|| {
        webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .filter_map(|der| reqwest::Certificate::from_der(der).ok())
            .collect()
    });
    &ROOTS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A private CA and a `localhost` server leaf it signed.
    struct TestPki {
        ca_der: Vec<u8>,
        leaf_der: Vec<u8>,
        key_der: Vec<u8>,
    }

    fn test_pki(name: &str) -> TestPki {
        let hierarchy = crate::sesame::ca::generate_ca_hierarchy(name, b"ikm").unwrap();
        let (leaf_der, key_der, _) = crate::sesame::ca::issue_end_entity_cert(
            "localhost",
            crate::sesame::types::SerialNumber(7),
            std::time::Duration::from_secs(3600),
            &["localhost".to_string()],
            &[rcgen::ExtendedKeyUsagePurpose::ServerAuth],
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();
        TestPki {
            ca_der: hierarchy.node.ca.certificate_der.clone(),
            leaf_der,
            key_der,
        }
    }

    /// Serve one canned HTTP/1.1 response per TLS connection, offering both
    /// `h2` and `http/1.1` in ALPN. Returns the address and the protocol each
    /// handshake settled on.
    async fn spawn_server(
        pki: &TestPki,
    ) -> (std::net::SocketAddr, Arc<Mutex<Vec<Option<Vec<u8>>>>>) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(
                    pki.leaf_der.clone(),
                )],
                rustls::pki_types::PrivateKeyDer::try_from(pki.key_der.clone()).unwrap(),
            )
            .unwrap();
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let negotiated = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&negotiated);
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    continue;
                };
                let protocol = tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
                seen.lock().unwrap().push(protocol);
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&chunk[..n]),
                    }
                }
                let _ = tls
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    )
                    .await;
                let _ = tls.shutdown().await;
            }
        });
        (address, negotiated)
    }

    fn certificate(der: &[u8]) -> reqwest::Certificate {
        reqwest::Certificate::from_der(der).unwrap()
    }

    #[tokio::test]
    async fn outbound_client_offers_only_http1_over_tls() {
        let pki = test_pki("alpn-ca");
        let (address, negotiated) = spawn_server(&pki).await;
        let client = builder()
            .tls_certs_merge([certificate(&pki.ca_der)])
            .resolve("localhost", address)
            .build()
            .unwrap();

        let response = client
            .get(format!("https://localhost:{}/", address.port()))
            .send()
            .await
            .unwrap();

        assert_eq!(response.version(), reqwest::Version::HTTP_11);
        assert_eq!(
            negotiated.lock().unwrap().as_slice(),
            [Some(b"http/1.1".to_vec())]
        );
    }

    #[tokio::test]
    async fn outbound_client_refuses_a_server_signed_by_a_private_ca() {
        let pki = test_pki("private-ca");
        let (address, _) = spawn_server(&pki).await;
        let client = builder().resolve("localhost", address).build().unwrap();

        let result = client
            .get(format!("https://localhost:{}/", address.port()))
            .send()
            .await;

        assert!(result.is_err(), "{result:?}");
    }

    #[tokio::test]
    async fn outbound_client_with_a_different_ca_refuses_the_server() {
        let pki = test_pki("server-ca");
        let other = test_pki("other-ca");
        let (address, _) = spawn_server(&pki).await;
        let client = builder()
            .tls_certs_merge([certificate(&other.ca_der)])
            .resolve("localhost", address)
            .build()
            .unwrap();

        let result = client
            .get(format!("https://localhost:{}/", address.port()))
            .send()
            .await;

        assert!(result.is_err(), "{result:?}");
    }

    /// reqwest's own fallback is aws-lc; 0.12 used ring, and so does every
    /// rustls config we build by hand.
    #[test]
    fn building_an_outbound_client_uses_ring() {
        builder().build().unwrap();

        let installed = rustls::crypto::CryptoProvider::get_default()
            .expect("a process crypto provider is installed");
        let ring = rustls::crypto::ring::default_provider();
        let names = |provider: &rustls::crypto::CryptoProvider| {
            provider
                .cipher_suites
                .iter()
                .map(|suite| suite.suite())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(installed), names(&ring));
    }

    /// Public HTTPS (webhooks, release downloads, scrapes) trusts the same
    /// bundled Mozilla roots 0.12 did, not whatever the host has installed.
    /// A host with no CA bundle at all must still build a client.
    #[test]
    fn outbound_client_trusts_the_bundled_mozilla_roots() {
        assert_eq!(
            bundled_roots().len(),
            webpki_root_certs::TLS_SERVER_ROOT_CERTS.len()
        );
        assert!(!bundled_roots().is_empty());
    }

    /// A direct `reqwest::Client::builder()`, `Client::new()` or
    /// `reqwest::get()` in production code would quietly get reqwest's own
    /// defaults back. Test modules and test-only files may still use them.
    #[test]
    fn production_code_builds_http_clients_only_through_this_module() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let banned = [
            "reqwest::Client::builder()",
            "reqwest::Client::new()",
            "reqwest::get(",
            " Client::builder()",
            " Client::new()",
        ];
        let mut offenders = Vec::new();
        let mut directories = vec![root.clone()];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(&directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    directories.push(path);
                    continue;
                }
                let relative = path
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                let test_only = relative.contains("tests")
                    || relative.contains("testkit")
                    || relative.contains("test_");
                if path.extension().is_none_or(|e| e != "rs")
                    || test_only
                    || relative == "http_client.rs"
                {
                    continue;
                }
                let source = std::fs::read_to_string(&path).unwrap();
                let production = source.split("#[cfg(test)]\nmod tests").next().unwrap();
                for (number, line) in production.lines().enumerate() {
                    if banned.iter().any(|pattern| line.contains(pattern)) {
                        offenders.push(format!("src/{relative}:{}", number + 1));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "use crate::http_client::builder() instead: {offenders:?}"
        );
    }
}
