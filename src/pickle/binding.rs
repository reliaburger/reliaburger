//! Binding an image reference to a manifest digest (F03 U1, #361).
//!
//! An app's `image` is usually a tag (`nginx:1.27`), and a tag moves. If the
//! spec in Raft keeps the tag, a restart, a replacement or a replica on
//! another node can each resolve it again and run different bytes. So the
//! node that accepts an apply resolves the tag once and stores both,
//! `nginx:1.27@sha256:…`: the digest decides what every pull fetches, and
//! the tag stays for people to read.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::grill::image::ImageReference;
use crate::pickle::types::{Digest, ManifestCatalog};
use crate::pickle::upstream::{UpstreamRegistry, cached_repository};

/// How long an apply waits for an upstream registry to name a tag's digest
/// before it falls back to the pull-through cache. The registry client
/// retries on its own for up to two minutes, which is too long for someone
/// waiting on `relish apply`.
pub const UPSTREAM_BIND_TIMEOUT: Duration = Duration::from_secs(20);

/// Where a binding's digest came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindSource {
    /// An image built into or pushed to Pickle: the catalogue's tag.
    Pickle,
    /// The upstream registry's answer for the tag.
    Upstream,
    /// Upstream couldn't be reached; the pull-through cache's copy of the tag.
    Cache,
}

/// The result of binding one image reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    /// The reference already names a digest; it's stored as written.
    AlreadyBound,
    /// The tag resolved to `digest`; store `reference` instead.
    Bound {
        reference: String,
        digest: Digest,
        source: BindSource,
    },
}

/// Why an image couldn't be bound.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error("{image}: {reason}")]
    InvalidReference { image: String, reason: String },
    #[error(
        "cannot resolve {image} to a digest: {reason}; pin it as {image}@sha256:… or try again when the registry answers"
    )]
    Unresolved { image: String, reason: String },
    /// The node's upstream trust rules don't allow the image (F03 U2).
    #[error(transparent)]
    NotAllowed(#[from] crate::pickle::trust::UpstreamRefused),
}

/// Bind `image` to a manifest digest.
///
/// A reference with a digest is left alone. A Pickle image binds to the
/// catalogue's digest for its tag. Anything else asks the upstream registry
/// which digest the tag points at (a HEAD, no body); when it can't answer,
/// the pull-through cache's copy of the tag stands in, as it would for a
/// pull. With neither, the apply fails rather than store a tag that could
/// resolve differently later.
pub async fn bind_image(
    image: &str,
    catalog: &ManifestCatalog,
    upstream: Option<&dyn UpstreamRegistry>,
) -> Result<Binding, BindError> {
    if image.contains('@') {
        return Ok(Binding::AlreadyBound);
    }
    let bound = |digest: Digest, source| Binding::Bound {
        reference: format!("{image}@{}", digest.as_str()),
        digest,
        source,
    };

    let (name, tag) = crate::meat::scheduler::split_repo_tag(image);
    let repository = crate::meat::scheduler::canonical_repository(name);
    if !repository.starts_with("cache/")
        && let Some(manifest) = catalog.get_manifest_by_tag(repository, tag)
    {
        return Ok(bound(manifest.digest.clone(), BindSource::Pickle));
    }

    let reference = ImageReference::parse(image).map_err(|error| BindError::InvalidReference {
        image: image.to_string(),
        reason: error.to_string(),
    })?;
    let upstream_error = match upstream {
        Some(registry) => match tokio::time::timeout(
            UPSTREAM_BIND_TIMEOUT,
            registry.head_manifest_digest(&reference),
        )
        .await
        {
            Ok(Ok(digest)) => return Ok(bound(digest, BindSource::Upstream)),
            Ok(Err(error)) => error.to_string(),
            Err(_) => format!(
                "{} did not answer within {}s",
                reference.registry,
                UPSTREAM_BIND_TIMEOUT.as_secs()
            ),
        },
        None => "no upstream registry is configured on this node".to_string(),
    };
    match catalog.get_manifest_by_tag(&cached_repository(&reference), &reference.tag) {
        Some(manifest) => Ok(bound(manifest.digest.clone(), BindSource::Cache)),
        None => Err(BindError::Unresolved {
            image: image.to_string(),
            reason: upstream_error,
        }),
    }
}

/// One image an apply bound, as `relish apply` prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedBinding {
    /// What names the image: `web`, `web (init)` or `job migrate`.
    pub workload: String,
    /// The image as written in the config.
    pub image: String,
    /// The manifest digest it now runs.
    pub digest: Digest,
    /// Where the digest came from.
    pub source: BindSource,
}

impl std::fmt::Display for AppliedBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source = match self.source {
            BindSource::Pickle => "from Pickle",
            BindSource::Upstream => "from the registry",
            BindSource::Cache => "from the pull-through cache; the registry did not answer",
        };
        write!(
            f,
            "{}: {} → {} ({source})",
            self.workload, self.image, self.digest
        )
    }
}

/// Binds the images in an apply on the node that accepts it.
///
/// Bun attaches one only when its own runtime pulls images: under
/// ProcessGrill an app's `image` is a placeholder nobody pulls, and asking
/// Docker Hub about it would fail the apply.
#[derive(Clone)]
pub struct ImageBinder {
    /// Asks registries over HTTPS.
    remote: Arc<dyn UpstreamRegistry>,
    /// Asks loopback registries (`localhost:5000`) over plain HTTP, the way
    /// the runtime pulls them.
    loopback: Arc<dyn UpstreamRegistry>,
    /// This node's `[images.trust_policy]`, whose upstream rules an apply
    /// checks before it asks any registry (F03 U2).
    policy: Arc<crate::config::node::TrustPolicySection>,
}

impl ImageBinder {
    /// A binder that asks `remote` about registries elsewhere and
    /// `loopback` about those on this host. It allows every upstream image
    /// until [`Self::with_policy`] says otherwise.
    pub fn new(remote: Arc<dyn UpstreamRegistry>, loopback: Arc<dyn UpstreamRegistry>) -> Self {
        Self {
            remote,
            loopback,
            policy: Arc::default(),
        }
    }

    /// A binder that asks `upstream` about every registry.
    pub fn with_upstream(upstream: Arc<dyn UpstreamRegistry>) -> Self {
        Self::new(upstream.clone(), upstream)
    }

    /// Check upstream images against `policy`'s rules before binding them.
    pub fn with_policy(mut self, policy: crate::config::node::TrustPolicySection) -> Self {
        self.policy = Arc::new(policy);
        self
    }

    /// Bind one image reference (see [`bind_image`]).
    pub async fn bind(&self, image: &str, catalog: &ManifestCatalog) -> Result<Binding, BindError> {
        let loopback = ImageReference::parse(image)
            .is_ok_and(|reference| crate::grill::image::is_loopback_registry(&reference.registry));
        let upstream = if loopback {
            self.loopback.as_ref()
        } else {
            self.remote.as_ref()
        };
        bind_image(image, catalog, Some(upstream)).await
    }

    /// Bind every image `config` names, in place: each app's image, its
    /// init containers' and each job's. A tag named twice is resolved once,
    /// so every workload in one apply that names it runs the same bytes.
    ///
    /// Returns one [`AppliedBinding`] per workload whose image changed. On
    /// an error `config` may be partly bound; the caller drops it.
    pub async fn bind_config(
        &self,
        config: &mut crate::config::Config,
        catalog: &ManifestCatalog,
    ) -> Result<Vec<AppliedBinding>, BindError> {
        let slots: Vec<(String, &mut Option<String>)> = config
            .app
            .iter_mut()
            .flat_map(|(name, spec)| {
                std::iter::once((name.clone(), &mut spec.image)).chain(
                    spec.init
                        .iter_mut()
                        .map(move |init| (format!("{name} (init)"), &mut init.image)),
                )
            })
            .chain(
                config
                    .job
                    .iter_mut()
                    .map(|(name, spec)| (format!("job {name}"), &mut spec.image)),
            )
            .collect();
        self.bind_slots(slots, catalog).await
    }

    /// Bind one app's images in place, as [`Self::bind_config`] does for a
    /// whole config. GitOps applies apps one change at a time.
    pub async fn bind_app(
        &self,
        name: &str,
        spec: &mut crate::config::app::AppSpec,
        catalog: &ManifestCatalog,
    ) -> Result<Vec<AppliedBinding>, BindError> {
        let slots: Vec<(String, &mut Option<String>)> =
            std::iter::once((name.to_string(), &mut spec.image))
                .chain(
                    spec.init
                        .iter_mut()
                        .map(|init| (format!("{name} (init)"), &mut init.image)),
                )
                .collect();
        self.bind_slots(slots, catalog).await
    }

    /// Bind each `(workload, image)` slot, resolving each distinct image once.
    async fn bind_slots(
        &self,
        slots: Vec<(String, &mut Option<String>)>,
        catalog: &ManifestCatalog,
    ) -> Result<Vec<AppliedBinding>, BindError> {
        // Every image passes the upstream rules before any registry hears
        // about any of them. Pickle's own images (not the cache's copies)
        // answer to `require_signatures` instead.
        // An absolute or relative path is a root filesystem runc runs as it
        // is (`/empty-fixture`): no registry holds it, so skip it.
        let slots: Vec<(String, &mut Option<String>)> = slots
            .into_iter()
            .filter(|(_, slot)| {
                slot.as_deref()
                    .is_none_or(crate::grill::image::looks_like_image_ref)
            })
            .collect();
        for image in slots.iter().filter_map(|(_, slot)| slot.as_deref()) {
            if crate::meat::scheduler::lookup_pickle_manifest(image, catalog).is_none() {
                crate::pickle::trust::check_upstream(&self.policy, image)?;
            }
        }
        let mut resolved: HashMap<String, Binding> = HashMap::new();
        let mut applied = Vec::new();
        for (workload, slot) in slots {
            let Some(image) = slot.as_deref() else {
                continue;
            };
            let binding = match resolved.get(image) {
                Some(binding) => binding.clone(),
                None => {
                    let binding = self.bind(image, catalog).await?;
                    resolved.insert(image.to_string(), binding.clone());
                    binding
                }
            };
            if let Binding::Bound {
                reference,
                digest,
                source,
            } = binding
            {
                applied.push(AppliedBinding {
                    workload,
                    image: image.to_string(),
                    digest,
                    source,
                });
                *slot = Some(reference);
            }
        }
        Ok(applied)
    }
}

/// An upstream that answers every HEAD with one digest, or is down, for
/// tests of binding here and in the apply routes.
#[cfg(test)]
pub(crate) struct FixedUpstream(pub Option<Digest>);

#[cfg(test)]
impl UpstreamRegistry for FixedUpstream {
    fn head_manifest_digest<'a>(
        &'a self,
        _image: &'a ImageReference,
    ) -> crate::pickle::upstream::UpstreamFuture<'a, Digest> {
        let answer = self.0.clone().ok_or_else(|| {
            crate::pickle::types::PickleError::ReplicationFailed("connection refused".into())
        });
        Box::pin(async move { answer })
    }

    fn fetch_manifest<'a>(
        &'a self,
        _image: &'a ImageReference,
    ) -> crate::pickle::upstream::UpstreamFuture<'a, crate::pickle::upstream::UpstreamManifest>
    {
        unimplemented!("binding only asks for the digest")
    }

    fn fetch_root<'a>(
        &'a self,
        _image: &'a ImageReference,
    ) -> crate::pickle::upstream::UpstreamFuture<'a, crate::pickle::upstream::UpstreamRoot> {
        unimplemented!("binding only asks for the digest")
    }

    fn fetch_blob<'a>(
        &'a self,
        _image: &'a ImageReference,
        _layer: &'a crate::pickle::types::LayerDescriptor,
    ) -> crate::pickle::upstream::UpstreamFuture<'a, Vec<u8>> {
        unimplemented!("binding only asks for the digest")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pickle::types::{ImageManifest, LayerDescriptor, ManifestCommit};
    use crate::pickle::upstream::{UpstreamFuture, UpstreamManifest, UpstreamRoot};
    use std::collections::BTreeSet;

    fn digest(i: u64) -> Digest {
        Digest::new(&format!("sha256:{i:064x}")).unwrap()
    }

    /// A catalogue holding `repository:tag` at `digest(i)`.
    fn catalog_with(entries: &[(&str, &str, u64)]) -> ManifestCatalog {
        let mut catalog = ManifestCatalog::default();
        for (repository, tag, i) in entries {
            catalog.apply_manifest_commit(&ManifestCommit {
                observed_gc_generation: 0,
                manifest: ImageManifest {
                    digest: digest(*i),
                    config: LayerDescriptor {
                        digest: digest(1000 + i),
                        size: 10,
                        media_type: "application/vnd.oci.image.config.v1+json".into(),
                        platform: None,
                    },
                    layers: vec![],
                    repository: repository.to_string(),
                    tags: BTreeSet::new(),
                    total_size: 10,
                    pushed_at: std::time::SystemTime::UNIX_EPOCH,
                    pushed_by: 1,
                    signature: None,
                },
                tag: tag.to_string(),
                holder_nodes: BTreeSet::from([1]),
            });
        }
        catalog
    }

    use super::FixedUpstream as Registry;

    fn bound_to(binding: Binding) -> (String, Digest, BindSource) {
        match binding {
            Binding::Bound {
                reference,
                digest,
                source,
            } => (reference, digest, source),
            Binding::AlreadyBound => panic!("expected a binding"),
        }
    }

    /// A registry that counts its HEADs and answers each with one digest.
    struct Counting {
        digest: Digest,
        heads: std::sync::atomic::AtomicUsize,
    }

    impl UpstreamRegistry for Counting {
        fn head_manifest_digest<'a>(
            &'a self,
            _image: &'a ImageReference,
        ) -> UpstreamFuture<'a, Digest> {
            self.heads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let digest = self.digest.clone();
            Box::pin(async move { Ok(digest) })
        }

        fn fetch_manifest<'a>(
            &'a self,
            _image: &'a ImageReference,
        ) -> UpstreamFuture<'a, UpstreamManifest> {
            unimplemented!("binding only asks for the digest")
        }

        fn fetch_root<'a>(
            &'a self,
            _image: &'a ImageReference,
        ) -> UpstreamFuture<'a, UpstreamRoot> {
            unimplemented!("binding only asks for the digest")
        }

        fn fetch_blob<'a>(
            &'a self,
            _image: &'a ImageReference,
            _layer: &'a LayerDescriptor,
        ) -> UpstreamFuture<'a, Vec<u8>> {
            unimplemented!("binding only asks for the digest")
        }
    }

    fn counting(i: u64) -> std::sync::Arc<Counting> {
        std::sync::Arc::new(Counting {
            digest: digest(i),
            heads: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn heads(registry: &Counting) -> usize {
        registry.heads.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[tokio::test]
    async fn binding_a_config_binds_every_app_init_and_job_image() {
        let registry = counting(7);
        let binder = ImageBinder::with_upstream(registry.clone());
        let mut config = crate::config::Config::parse(
            r#"
[app.web]
image = "nginx:1.27"
[[app.web.init]]
image = "busybox:1.36"
command = ["true"]
[job.migrate]
image = "acme/migrate:v3"
command = ["migrate"]
"#,
        )
        .unwrap();
        let bindings = binder
            .bind_config(&mut config, &ManifestCatalog::default())
            .await
            .unwrap();
        let bound = |image: &str| format!("{image}@{}", digest(7).as_str());
        assert_eq!(
            config.app["web"].image.as_deref(),
            Some(bound("nginx:1.27").as_str())
        );
        assert_eq!(
            config.app["web"].init[0].image.as_deref(),
            Some(bound("busybox:1.36").as_str())
        );
        assert_eq!(
            config.job["migrate"].image.as_deref(),
            Some(bound("acme/migrate:v3").as_str())
        );
        let lines: Vec<String> = bindings.iter().map(ToString::to_string).collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[0].starts_with("web: nginx:1.27 → sha256:"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.starts_with("job migrate: ")),
            "{lines:?}"
        );
    }

    /// Two apps naming one tag must run one digest, so the tag is resolved
    /// once per apply, not once per app.
    #[tokio::test]
    async fn one_tag_named_twice_is_resolved_once() {
        let registry = counting(7);
        let binder = ImageBinder::with_upstream(registry.clone());
        let mut config = crate::config::Config::parse(
            "[app.a]\nimage = \"redis:7\"\n[app.b]\nimage = \"redis:7\"\n",
        )
        .unwrap();
        binder
            .bind_config(&mut config, &ManifestCatalog::default())
            .await
            .unwrap();
        assert_eq!(heads(&registry), 1);
        assert_eq!(config.app["a"].image, config.app["b"].image);
    }

    /// runc runs an absolute path (`/empty-fixture`) as a ready-made root
    /// filesystem; no registry holds it, so there's nothing to bind or judge.
    #[tokio::test]
    async fn a_local_root_filesystem_path_is_left_alone() {
        let binder = ImageBinder::with_upstream(std::sync::Arc::new(Registry(None)))
            .with_policy(official_images_only());
        let mut config =
            crate::config::Config::parse("[app.web]\nimage = \"/empty-fixture\"\n").unwrap();
        let bindings = binder
            .bind_config(&mut config, &ManifestCatalog::default())
            .await
            .unwrap();
        assert!(bindings.is_empty());
        assert_eq!(config.app["web"].image.as_deref(), Some("/empty-fixture"));
    }

    #[tokio::test]
    async fn a_config_already_pinned_needs_no_registry() {
        let binder = ImageBinder::with_upstream(std::sync::Arc::new(Registry(None)));
        let pinned = format!("nginx:1.27@{}", digest(3).as_str());
        let mut config =
            crate::config::Config::parse(&format!("[app.web]\nimage = \"{pinned}\"\n")).unwrap();
        let bindings = binder
            .bind_config(&mut config, &ManifestCatalog::default())
            .await
            .unwrap();
        assert!(bindings.is_empty());
        assert_eq!(config.app["web"].image.as_deref(), Some(pinned.as_str()));
    }

    #[tokio::test]
    async fn a_config_whose_image_cannot_be_resolved_fails_and_names_it() {
        let binder = ImageBinder::with_upstream(std::sync::Arc::new(Registry(None)));
        let mut config = crate::config::Config::parse("[app.web]\nimage = \"redis:7\"\n").unwrap();
        let error = binder
            .bind_config(&mut config, &ManifestCatalog::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("redis:7"), "{error}");
        assert_eq!(config.app["web"].image.as_deref(), Some("redis:7"));
    }

    /// The runtime pulls a loopback registry over plain HTTP, so binding
    /// must ask it the same way.
    #[tokio::test]
    async fn a_loopback_registry_is_asked_through_the_loopback_client() {
        let remote = counting(1);
        let loopback = counting(2);
        let binder = ImageBinder::new(remote.clone(), loopback.clone());
        let mut config = crate::config::Config::parse(
            "[app.web]\nimage = \"127.0.0.1:5000/web:v1\"\n[app.db]\nimage = \"redis:7\"\n",
        )
        .unwrap();
        binder
            .bind_config(&mut config, &ManifestCatalog::default())
            .await
            .unwrap();
        assert_eq!(heads(&loopback), 1);
        assert_eq!(heads(&remote), 1);
        assert!(
            config.app["web"]
                .image
                .as_deref()
                .unwrap()
                .ends_with(digest(2).as_str())
        );
    }

    /// An allow-list naming only Docker Hub's official images.
    fn official_images_only() -> crate::config::node::TrustPolicySection {
        crate::config::node::TrustPolicySection {
            upstream: vec![crate::config::node::UpstreamTrustRule {
                pattern: "docker.io/library/*".to_string(),
                require_signatures: false,
            }],
            upstream_default: crate::config::node::UpstreamDefault { allow: false },
            ..Default::default()
        }
    }

    /// F03 U2: the apply refuses an image the node's upstream rules don't
    /// allow, names it, and doesn't ask its registry anything.
    #[tokio::test]
    async fn an_image_the_upstream_rules_refuse_fails_the_apply_by_name() {
        let registry = counting(7);
        let binder =
            ImageBinder::with_upstream(registry.clone()).with_policy(official_images_only());
        let mut config = crate::config::Config::parse(
            "[app.web]\nimage = \"nginx:1.27\"\n[app.miner]\nimage = \"ghcr.io/evil/miner:1\"\n",
        )
        .unwrap();
        let error = binder
            .bind_config(&mut config, &ManifestCatalog::default())
            .await
            .unwrap_err();
        assert!(matches!(error, BindError::NotAllowed(_)), "{error}");
        assert!(
            error.to_string().contains("ghcr.io/evil/miner:1"),
            "{error}"
        );
        // `miner` sorts before `web`, so nothing was resolved at all.
        assert_eq!(heads(&registry), 0);
    }

    #[tokio::test]
    async fn an_image_a_rule_allows_binds_as_usual() {
        let binder = ImageBinder::with_upstream(counting(7)).with_policy(official_images_only());
        let mut config =
            crate::config::Config::parse("[app.web]\nimage = \"nginx:1.27\"\n").unwrap();
        let bindings = binder
            .bind_config(&mut config, &ManifestCatalog::default())
            .await
            .unwrap();
        assert_eq!(bindings.len(), 1);
    }

    /// The rules are for images from outside Pickle; the cluster's own
    /// images answer to `require_signatures` instead.
    #[tokio::test]
    async fn the_upstream_rules_leave_pickle_images_alone() {
        let catalog = catalog_with(&[("team/web", "v2", 5)]);
        let binder = ImageBinder::with_upstream(std::sync::Arc::new(Registry(None)))
            .with_policy(official_images_only());
        let mut config =
            crate::config::Config::parse("[app.web]\nimage = \"localhost:5050/team/web:v2\"\n")
                .unwrap();
        binder.bind_config(&mut config, &catalog).await.unwrap();
        // A digest-pinned Pickle reference is Pickle's too.
        let pinned = format!("localhost:5050/team/web@{}", digest(5).as_str());
        let mut config =
            crate::config::Config::parse(&format!("[app.web]\nimage = \"{pinned}\"\n")).unwrap();
        binder.bind_config(&mut config, &catalog).await.unwrap();
    }

    /// An image pinned by digest is still checked: pinning doesn't make an
    /// image trusted.
    #[tokio::test]
    async fn a_pinned_upstream_image_is_checked_too() {
        let binder = ImageBinder::with_upstream(counting(7)).with_policy(official_images_only());
        let pinned = format!("ghcr.io/evil/miner@{}", digest(3).as_str());
        let mut config =
            crate::config::Config::parse(&format!("[app.miner]\nimage = \"{pinned}\"\n")).unwrap();
        let error = binder
            .bind_config(&mut config, &ManifestCatalog::default())
            .await
            .unwrap_err();
        assert!(matches!(error, BindError::NotAllowed(_)), "{error}");
    }

    /// A registry that never answers.
    struct Silent;

    impl UpstreamRegistry for Silent {
        fn head_manifest_digest<'a>(
            &'a self,
            _image: &'a ImageReference,
        ) -> UpstreamFuture<'a, Digest> {
            Box::pin(std::future::pending())
        }

        fn fetch_manifest<'a>(
            &'a self,
            _image: &'a ImageReference,
        ) -> UpstreamFuture<'a, UpstreamManifest> {
            unimplemented!("binding only asks for the digest")
        }

        fn fetch_root<'a>(
            &'a self,
            _image: &'a ImageReference,
        ) -> UpstreamFuture<'a, UpstreamRoot> {
            unimplemented!("binding only asks for the digest")
        }

        fn fetch_blob<'a>(
            &'a self,
            _image: &'a ImageReference,
            _layer: &'a LayerDescriptor,
        ) -> UpstreamFuture<'a, Vec<u8>> {
            unimplemented!("binding only asks for the digest")
        }
    }

    /// An apply waits for the registry for a bounded time, then binds from
    /// the cache as it would for an unreachable one.
    #[tokio::test(start_paused = true)]
    async fn a_registry_that_never_answers_times_out_to_the_cache() {
        let catalog = catalog_with(&[("cache/docker.io/library/redis", "7", 9)]);
        let binding = bind_image("redis:7", &catalog, Some(&Silent))
            .await
            .unwrap();
        let (_, bound, source) = bound_to(binding);
        assert_eq!(bound, digest(9));
        assert_eq!(source, BindSource::Cache);
    }

    #[tokio::test]
    async fn an_upstream_tag_binds_to_the_registrys_digest() {
        let registry = Registry(Some(digest(7)));
        let binding = bind_image("nginx:1.27", &ManifestCatalog::default(), Some(&registry))
            .await
            .unwrap();
        let (reference, bound, source) = bound_to(binding);
        assert_eq!(reference, format!("nginx:1.27@{}", digest(7).as_str()));
        assert_eq!(bound, digest(7));
        assert_eq!(source, BindSource::Upstream);
    }

    #[tokio::test]
    async fn a_reference_with_a_digest_is_left_alone() {
        let registry = Registry(Some(digest(7)));
        for image in [
            format!("nginx@{}", digest(3).as_str()),
            format!("nginx:1.27@{}", digest(3).as_str()),
        ] {
            let binding = bind_image(&image, &ManifestCatalog::default(), Some(&registry))
                .await
                .unwrap();
            assert_eq!(binding, Binding::AlreadyBound, "{image}");
        }
    }

    #[tokio::test]
    async fn a_pickle_image_binds_to_the_catalogues_digest_without_asking_upstream() {
        let catalog = catalog_with(&[("team/web", "v2", 5)]);
        // A registry that's down proves the catalogue answered.
        let binding = bind_image(
            "localhost:5050/team/web:v2",
            &catalog,
            Some(&Registry(None)),
        )
        .await
        .unwrap();
        let (reference, bound, source) = bound_to(binding);
        assert_eq!(
            reference,
            format!("localhost:5050/team/web:v2@{}", digest(5).as_str())
        );
        assert_eq!(bound, digest(5));
        assert_eq!(source, BindSource::Pickle);
    }

    #[tokio::test]
    async fn an_unreachable_upstream_falls_back_to_the_cached_tag() {
        let catalog = catalog_with(&[("cache/docker.io/library/redis", "7", 9)]);
        let binding = bind_image("redis:7", &catalog, Some(&Registry(None)))
            .await
            .unwrap();
        let (_, bound, source) = bound_to(binding);
        assert_eq!(bound, digest(9));
        assert_eq!(source, BindSource::Cache);
    }

    #[tokio::test]
    async fn an_unreachable_upstream_without_a_cached_copy_fails_the_bind() {
        let error = bind_image(
            "redis:7",
            &ManifestCatalog::default(),
            Some(&Registry(None)),
        )
        .await
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("redis:7"), "{message}");
        assert!(message.contains("connection refused"), "{message}");
        assert!(message.contains("@sha256"), "says how to pin: {message}");
    }

    /// The cache answers for upstream only: a cached copy must not shadow
    /// what upstream says the tag is now.
    #[tokio::test]
    async fn a_reachable_upstream_wins_over_a_stale_cached_tag() {
        let catalog = catalog_with(&[("cache/docker.io/library/redis", "7", 9)]);
        let registry = Registry(Some(digest(10)));
        let binding = bind_image("redis:7", &catalog, Some(&registry))
            .await
            .unwrap();
        let (_, bound, source) = bound_to(binding);
        assert_eq!(bound, digest(10));
        assert_eq!(source, BindSource::Upstream);
    }
}
