//! Binding an image reference to a manifest digest (F03 U1, #361).
//!
//! An app's `image` is usually a tag (`nginx:1.27`), and a tag moves. If the
//! spec in Raft keeps the tag, a restart, a replacement or a replica on
//! another node can each resolve it again and run different bytes. So the
//! node that accepts an apply resolves the tag once and stores both,
//! `nginx:1.27@sha256:…`: the digest decides what every pull fetches, and
//! the tag stays for people to read.

use crate::grill::image::ImageReference;
use crate::pickle::types::{Digest, ManifestCatalog};
use crate::pickle::upstream::{UpstreamRegistry, cached_repository};

/// Where a binding's digest came from.
#[derive(Debug, Clone, PartialEq, Eq)]
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
        Some(registry) => match registry.head_manifest_digest(&reference).await {
            Ok(digest) => return Ok(bound(digest, BindSource::Upstream)),
            Err(error) => error.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pickle::types::{ImageManifest, LayerDescriptor, ManifestCommit, PickleError};
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

    /// An upstream that answers every HEAD with one digest, or is down.
    struct Registry(Option<Digest>);

    impl UpstreamRegistry for Registry {
        fn head_manifest_digest<'a>(
            &'a self,
            _image: &'a ImageReference,
        ) -> UpstreamFuture<'a, Digest> {
            let answer = self
                .0
                .clone()
                .ok_or_else(|| PickleError::ReplicationFailed("connection refused".into()));
            Box::pin(async move { answer })
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
