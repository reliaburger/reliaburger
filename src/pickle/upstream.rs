//! Pull-through cache: upstream registry client and cache decisions
//! (Phase 12, slice D).
//!
//! External images (`docker.io`, `ghcr.io`, ECR, …) are fetched once,
//! stored in Pickle under a `cache/<host>/<repo>` repository, and
//! served to the rest of the cluster from there. The decision logic is
//! pure; the network side hides behind [`UpstreamRegistry`], so every
//! path tests without touching the internet.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use crate::grill::image::ImageReference;

use super::types::{Digest, LayerDescriptor, ManifestCatalog, PickleError};

/// Where cached copies of an upstream image live in the catalog:
/// `cache/<registry-host>/<repository>`. The prefix marks them as
/// upstream content — exempt from `require_signatures` (they were
/// never signable by us) and never confused with cluster-pushed
/// repositories.
pub fn cached_repository(image: &ImageReference) -> String {
    format!("cache/{}/{}", image.registry, image.repository)
}

/// What the catalog says about a cached upstream image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheState {
    /// Not cached — fetch from upstream.
    Miss,
    /// Cached and inside the recheck window — serve it.
    Fresh,
    /// Cached but the recheck window has passed: the tag may have
    /// moved upstream. Carries the cached manifest digest for the
    /// HEAD comparison.
    Stale(Digest),
}

/// The outcome of a staleness recheck against upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheDecision {
    /// The upstream digest matches the cache — serve the cached copy.
    Hit,
    /// The tag moved upstream — refetch and recommit.
    Refetch,
}

/// Pure cache-state decision from the catalog.
///
/// Freshness rides on the cached manifest's `pushed_at` (the commit
/// time of the cache fill): younger than `recheck` is `Fresh`, older
/// is `Stale` and the caller HEADs upstream. Mutable tags are the
/// whole problem here — `redis:7` moves — so there is no "cached
/// forever" state. (Digest-pinned references aren't special-cased:
/// image references parse as tags only; a pin would simply recheck
/// and always hit.)
pub fn decide(
    catalog: &ManifestCatalog,
    cached_repo: &str,
    tag: &str,
    now: SystemTime,
    recheck: Duration,
) -> CacheState {
    let Some(manifest) = catalog.get_manifest_by_tag(cached_repo, tag) else {
        return CacheState::Miss;
    };
    let age = now
        .duration_since(manifest.pushed_at)
        .unwrap_or(Duration::ZERO);
    if age < recheck {
        CacheState::Fresh
    } else {
        CacheState::Stale(manifest.digest.clone())
    }
}

/// For a stale entry: HEAD the upstream manifest digest and compare.
pub async fn refresh_or_refetch(
    upstream: &dyn UpstreamRegistry,
    image: &ImageReference,
    cached_digest: &Digest,
) -> Result<CacheDecision, PickleError> {
    let upstream_digest = upstream.head_manifest_digest(image).await?;
    if upstream_digest == *cached_digest {
        Ok(CacheDecision::Hit)
    } else {
        Ok(CacheDecision::Refetch)
    }
}

/// A manifest fetched from upstream, reduced to what the cache fill
/// needs: the manifest digest, the config blob (content included —
/// upstream returns it alongside the manifest), and the layers.
#[derive(Debug, Clone)]
pub struct UpstreamManifest {
    pub digest: Digest,
    pub config: LayerDescriptor,
    pub config_bytes: Vec<u8>,
    pub layers: Vec<LayerDescriptor>,
}

/// Boxed future for [`UpstreamRegistry`] methods (`dyn`-safe trait).
pub type UpstreamFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, PickleError>> + Send + 'a>>;

/// An upstream OCI registry, as much of it as the pull-through cache
/// needs. Two implementations: [`OciUpstream`] speaks the real OCI
/// Distribution protocol; the test mock scripts responses and counts
/// hits, so cache behaviour is provable with zero internet.
pub trait UpstreamRegistry: Send + Sync {
    /// The digest the tag currently points at (a HEAD, no body).
    fn head_manifest_digest<'a>(&'a self, image: &'a ImageReference) -> UpstreamFuture<'a, Digest>;

    /// The platform-resolved manifest plus its config blob.
    fn fetch_manifest<'a>(
        &'a self,
        image: &'a ImageReference,
    ) -> UpstreamFuture<'a, UpstreamManifest>;

    /// One layer blob, digest-verified by the caller's blob store.
    fn fetch_blob<'a>(
        &'a self,
        image: &'a ImageReference,
        layer: &'a LayerDescriptor,
    ) -> UpstreamFuture<'a, Vec<u8>>;
}

/// Per-host credentials for upstream registries, resolved once at
/// startup. `lookup` maps a secret name to its plaintext (the Sesame
/// secret store in production; a closure in tests). Hosts without
/// credentials — or whose secret can't be resolved — fall back to
/// anonymous access.
pub fn resolve_credentials(
    registries: &[crate::config::node::ExternalRegistrySection],
    lookup: impl Fn(&str) -> Option<String>,
) -> HashMap<String, (String, String)> {
    let mut credentials = HashMap::new();
    for registry in registries {
        let (Some(username), Some(secret_name)) = (&registry.username, &registry.password_secret)
        else {
            continue;
        };
        if let Some(password) = lookup(secret_name) {
            credentials.insert(registry.host.clone(), (username.clone(), password));
        }
    }
    credentials
}

/// The real upstream client, wrapping `oci_distribution`.
pub struct OciUpstream {
    client: oci_distribution::Client,
    /// host → (username, password); anonymous when absent.
    credentials: HashMap<String, (String, String)>,
}

impl OciUpstream {
    /// HTTPS client for real upstreams (Docker Hub, GHCR, ECR).
    pub fn new(credentials: HashMap<String, (String, String)>) -> Self {
        Self::with_protocol(credentials, oci_distribution::client::ClientProtocol::Https)
    }

    /// Plain-HTTP client — integration tests point this at an
    /// in-process registry standing in for the upstream.
    pub fn insecure_http(credentials: HashMap<String, (String, String)>) -> Self {
        Self::with_protocol(credentials, oci_distribution::client::ClientProtocol::Http)
    }

    fn with_protocol(
        credentials: HashMap<String, (String, String)>,
        protocol: oci_distribution::client::ClientProtocol,
    ) -> Self {
        let config = oci_distribution::client::ClientConfig {
            protocol,
            ..Default::default()
        };
        Self {
            client: oci_distribution::Client::new(config),
            credentials,
        }
    }

    fn auth_for(&self, host: &str) -> oci_distribution::secrets::RegistryAuth {
        match self.credentials.get(host) {
            Some((user, pass)) => {
                oci_distribution::secrets::RegistryAuth::Basic(user.clone(), pass.clone())
            }
            None => oci_distribution::secrets::RegistryAuth::Anonymous,
        }
    }

    fn oci_reference(image: &ImageReference) -> Result<oci_distribution::Reference, PickleError> {
        image
            .to_oci_reference()
            .map_err(|e| PickleError::ReplicationFailed(format!("bad upstream reference: {e}")))
    }
}

impl UpstreamRegistry for OciUpstream {
    fn head_manifest_digest<'a>(&'a self, image: &'a ImageReference) -> UpstreamFuture<'a, Digest> {
        Box::pin(async move {
            let reference = Self::oci_reference(image)?;
            let auth = self.auth_for(&image.registry);
            let digest = self
                .client
                .fetch_manifest_digest(&reference, &auth)
                .await
                .map_err(|e| {
                    PickleError::ReplicationFailed(format!(
                        "upstream HEAD {} failed: {e}",
                        image.full_reference()
                    ))
                })?;
            Digest::new(&digest)
                .map_err(|e| PickleError::ReplicationFailed(format!("upstream digest: {e}")))
        })
    }

    fn fetch_manifest<'a>(
        &'a self,
        image: &'a ImageReference,
    ) -> UpstreamFuture<'a, UpstreamManifest> {
        Box::pin(async move {
            let reference = Self::oci_reference(image)?;
            let auth = self.auth_for(&image.registry);
            let (manifest, digest, config_json) = self
                .client
                .pull_manifest_and_config(&reference, &auth)
                .await
                .map_err(|e| {
                    PickleError::ReplicationFailed(format!(
                        "upstream manifest {} failed: {e}",
                        image.full_reference()
                    ))
                })?;

            let config_bytes = config_json.into_bytes();
            let config = LayerDescriptor {
                digest: Digest::new(&manifest.config.digest).map_err(|e| {
                    PickleError::ReplicationFailed(format!("upstream config digest: {e}"))
                })?,
                size: config_bytes.len() as u64,
                media_type: manifest.config.media_type.clone(),
            };
            let layers = manifest
                .layers
                .iter()
                .map(|layer| {
                    Ok(LayerDescriptor {
                        digest: Digest::new(&layer.digest).map_err(|e| {
                            PickleError::ReplicationFailed(format!("upstream layer digest: {e}"))
                        })?,
                        size: layer.size as u64,
                        media_type: layer.media_type.clone(),
                    })
                })
                .collect::<Result<Vec<_>, PickleError>>()?;

            Ok(UpstreamManifest {
                digest: Digest::new(&digest).map_err(|e| {
                    PickleError::ReplicationFailed(format!("upstream manifest digest: {e}"))
                })?,
                config,
                config_bytes,
                layers,
            })
        })
    }

    fn fetch_blob<'a>(
        &'a self,
        image: &'a ImageReference,
        layer: &'a LayerDescriptor,
    ) -> UpstreamFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let reference = Self::oci_reference(image)?;
            let descriptor = oci_distribution::manifest::OciDescriptor {
                digest: layer.digest.as_str().to_string(),
                media_type: layer.media_type.clone(),
                size: layer.size as i64,
                ..Default::default()
            };
            let mut bytes = Vec::with_capacity(layer.size as usize);
            self.client
                .pull_blob(&reference, &descriptor, &mut bytes)
                .await
                .map_err(|e| {
                    PickleError::ReplicationFailed(format!(
                        "upstream blob {} failed: {e}",
                        layer.digest
                    ))
                })?;
            Ok(bytes)
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pickle::types::{ImageManifest, ManifestCommit};
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn digest(i: u64) -> Digest {
        Digest::new(&format!("sha256:{i:064x}")).unwrap()
    }

    fn layer(i: u64, size: u64) -> LayerDescriptor {
        LayerDescriptor {
            digest: digest(i),
            size,
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
        }
    }

    /// A catalog holding one cached image committed at `pushed_at`.
    fn cached_catalog(repo: &str, pushed_at: SystemTime) -> ManifestCatalog {
        let manifest = ImageManifest {
            digest: digest(99),
            config: layer(1, 24),
            layers: vec![layer(2, 100)],
            repository: repo.to_string(),
            tags: BTreeSet::new(),
            total_size: 100,
            pushed_at,
            pushed_by: 1,
            signature: None,
        };
        let mut catalog = ManifestCatalog::default();
        catalog.apply_manifest_commit(&ManifestCommit {
            manifest,
            tag: "7".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });
        catalog
    }

    /// Scripted upstream with hit counters.
    struct MockUpstream {
        digest: Digest,
        pub heads: AtomicUsize,
    }

    impl UpstreamRegistry for MockUpstream {
        fn head_manifest_digest<'a>(
            &'a self,
            _image: &'a ImageReference,
        ) -> UpstreamFuture<'a, Digest> {
            self.heads.fetch_add(1, Ordering::SeqCst);
            let digest = self.digest.clone();
            Box::pin(async move { Ok(digest) })
        }

        fn fetch_manifest<'a>(
            &'a self,
            _image: &'a ImageReference,
        ) -> UpstreamFuture<'a, UpstreamManifest> {
            unimplemented!("not used by these tests")
        }

        fn fetch_blob<'a>(
            &'a self,
            _image: &'a ImageReference,
            _layer: &'a LayerDescriptor,
        ) -> UpstreamFuture<'a, Vec<u8>> {
            unimplemented!("not used by these tests")
        }
    }

    const RECHECK: Duration = Duration::from_secs(3600);

    #[test]
    fn cache_decision_miss_when_absent() {
        let catalog = ManifestCatalog::default();
        let state = decide(
            &catalog,
            "cache/docker.io/library/redis",
            "7",
            SystemTime::UNIX_EPOCH,
            RECHECK,
        );
        assert_eq!(state, CacheState::Miss);
    }

    #[test]
    fn cache_decision_hit_when_fresh() {
        let cached_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let catalog = cached_catalog("cache/docker.io/library/redis", cached_at);

        // 30 minutes later: inside the hour window.
        let now = cached_at + Duration::from_secs(1800);
        let state = decide(&catalog, "cache/docker.io/library/redis", "7", now, RECHECK);
        assert_eq!(state, CacheState::Fresh);
    }

    #[test]
    fn cache_decision_stale_after_recheck_window() {
        let cached_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let catalog = cached_catalog("cache/docker.io/library/redis", cached_at);

        let now = cached_at + Duration::from_secs(7200);
        let state = decide(&catalog, "cache/docker.io/library/redis", "7", now, RECHECK);
        assert_eq!(state, CacheState::Stale(digest(99)));
    }

    #[tokio::test]
    async fn stale_with_same_upstream_digest_is_a_hit() {
        let upstream = MockUpstream {
            digest: digest(99),
            heads: AtomicUsize::new(0),
        };
        let image = ImageReference::parse("redis:7").unwrap();

        let decision = refresh_or_refetch(&upstream, &image, &digest(99))
            .await
            .unwrap();
        assert_eq!(decision, CacheDecision::Hit);
        assert_eq!(upstream.heads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stale_with_new_upstream_digest_refetches() {
        let upstream = MockUpstream {
            digest: digest(123),
            heads: AtomicUsize::new(0),
        };
        let image = ImageReference::parse("redis:7").unwrap();

        let decision = refresh_or_refetch(&upstream, &image, &digest(99))
            .await
            .unwrap();
        assert_eq!(decision, CacheDecision::Refetch);
    }

    #[test]
    fn cached_repository_naming() {
        let image = ImageReference::parse("redis:7").unwrap();
        assert_eq!(cached_repository(&image), "cache/docker.io/library/redis");

        let image = ImageReference::parse("ghcr.io/org/app:v1").unwrap();
        assert_eq!(cached_repository(&image), "cache/ghcr.io/org/app");
    }

    #[test]
    fn credentials_resolved_from_secret() {
        use crate::config::node::ExternalRegistrySection;
        let registries = vec![
            ExternalRegistrySection {
                host: "ghcr.io".to_string(),
                username: Some("bot".to_string()),
                password_secret: Some("ghcr-token".to_string()),
            },
            ExternalRegistrySection {
                host: "docker.io".to_string(),
                username: None,
                password_secret: None,
            },
            ExternalRegistrySection {
                host: "registry.example.com".to_string(),
                username: Some("ci".to_string()),
                password_secret: Some("missing-secret".to_string()),
            },
        ];

        let creds = resolve_credentials(&registries, |name| {
            (name == "ghcr-token").then(|| "s3cret".to_string())
        });

        assert_eq!(
            creds.get("ghcr.io"),
            Some(&("bot".to_string(), "s3cret".to_string()))
        );
        // No credentials configured → anonymous.
        assert!(!creds.contains_key("docker.io"));
        // Unresolvable secret → anonymous, not an error.
        assert!(!creds.contains_key("registry.example.com"));
    }
}
