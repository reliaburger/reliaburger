//! Every public OCI image the gated Linux suites run, pinned by digest, and
//! the local mirror those suites pull them from.
//!
//! `tests/fixtures/pinned-images.txt` lists the same references for
//! `scripts/test-images/mirror.py`, which fetches them once (with retries)
//! into a content-addressed directory and serves it on loopback. When
//! [`TEST_IMAGE_MIRROR_ENV`] names that server, tests hand
//! [`local_test_mirrors`] to Bun and the image store, so a slow or
//! unreachable public registry can't fail a timed test. Digest pinning keeps
//! the mirror honest: every pull verifies the digest chain.

use std::collections::BTreeMap;

use crate::grill::ImageMirrors;
use crate::grill::image::{ImageError, ImageReference};

pub use super::context::PINNED_TEST_WORKLOAD_IMAGE;

/// Alpine 3.22.1: musl's `getent`, for DNS search-list checks.
pub const ALPINE_IMAGE: &str = "public.ecr.aws/docker/library/alpine@sha256:4bcff63911fcb4448bd4fdacec207030997caf25e9bea4045fa6c8c44de311d1";

/// Redis 8.8.0 from the ECR mirror of the official image.
pub const REDIS_IMAGE: &str = "public.ecr.aws/docker/library/redis@sha256:234c902a2db49461a129e2d4aeff85b28cf20187ed274a67f6e50995fa713c7b";

/// nginx 1.29-alpine from the ECR mirror of the official image.
pub const NGINX_IMAGE: &str = "public.ecr.aws/docker/library/nginx@sha256:5616878291a2eed594aee8db4dade5878cf7edcb475e59193904b198d9b830de";

/// podinfo, the Kubernetes demo's frontend and backend.
pub const PODINFO_IMAGE: &str = "ghcr.io/stefanprodan/podinfo@sha256:ec73780a8425f59ea49f5bc8cdff0d598805a224fbaa1f86c67a244f250fa9da";

/// Every pinned image, in `tests/fixtures/pinned-images.txt` order.
pub const PINNED_TEST_IMAGES: &[&str] = &[
    PINNED_TEST_WORKLOAD_IMAGE,
    ALPINE_IMAGE,
    REDIS_IMAGE,
    NGINX_IMAGE,
    PODINFO_IMAGE,
];

/// Names the loopback `host:port` serving the warmed pinned images.
pub const TEST_IMAGE_MIRROR_ENV: &str = "RELIABURGER_TEST_IMAGE_MIRROR";

/// Mirrors sending every pinned image's registry to the local test mirror,
/// or none when [`TEST_IMAGE_MIRROR_ENV`] is unset (tests then pull from the
/// public registries). An unusable value is an error rather than silence:
/// ignoring it would send the suite back to the internet it meant to avoid.
pub fn local_test_mirrors() -> Result<ImageMirrors, ImageError> {
    match std::env::var(TEST_IMAGE_MIRROR_ENV) {
        Ok(mirror) => test_mirrors_to(&mirror),
        Err(_) => Ok(ImageMirrors::default()),
    }
}

fn test_mirrors_to(mirror: &str) -> Result<ImageMirrors, ImageError> {
    let mut mirrors = BTreeMap::new();
    for image in PINNED_TEST_IMAGES {
        mirrors.insert(ImageReference::parse(image)?.registry, mirror.to_owned());
    }
    ImageMirrors::new(mirrors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_warm_list_names_exactly_the_pinned_images() {
        let listed: Vec<&str> = include_str!("../../tests/fixtures/pinned-images.txt")
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect();
        assert_eq!(listed, PINNED_TEST_IMAGES);
    }

    #[test]
    fn every_pinned_image_is_digest_pinned() {
        for image in PINNED_TEST_IMAGES {
            let reference = ImageReference::parse(image).unwrap();
            assert!(reference.tag.starts_with("sha256:"), "{image}");
            assert_eq!(reference.tag.len(), "sha256:".len() + 64, "{image}");
        }
    }

    #[test]
    fn the_kubernetes_demo_uses_only_pinned_images() {
        let demo = include_str!("../../examples/kubernetes/podinfo.yaml");
        let images: Vec<&str> = demo
            .lines()
            .filter_map(|line| line.trim().strip_prefix("image:"))
            .map(str::trim)
            .collect();
        assert!(!images.is_empty());
        for image in images {
            assert!(PINNED_TEST_IMAGES.contains(&image), "{image} is not pinned");
        }
    }

    /// Reads every pinned image from the mirror alone (no upstream fallback),
    /// so a gap in the warm list or a bad cache fails here, not mid-suite.
    #[tokio::test]
    #[ignore = "requires the local test mirror (scripts/test-images/mirror.py run)"]
    async fn pinned_images_serve_verified_from_the_local_test_mirror() {
        use crate::pickle::upstream::{OciUpstream, UpstreamRegistry};
        let mirrors = local_test_mirrors().unwrap();
        assert!(
            !mirrors.as_map().is_empty(),
            "set {TEST_IMAGE_MIRROR_ENV} to the running mirror"
        );
        let upstream = OciUpstream::insecure_http(Default::default());
        for image in PINNED_TEST_IMAGES {
            let reference = ImageReference::parse(image).unwrap();
            let mirrored = mirrors.mirror_for(&reference).unwrap();
            let manifest = upstream
                .fetch_manifest(&mirrored)
                .await
                .unwrap_or_else(|error| panic!("{image}: {error}"));
            for layer in &manifest.layers {
                let bytes = upstream.fetch_blob(&mirrored, layer).await.unwrap();
                assert_eq!(
                    crate::pickle::store::compute_sha256(&bytes),
                    layer.digest,
                    "{image}"
                );
            }
        }
    }

    #[test]
    fn the_test_mirror_covers_every_pinned_registry() {
        let mirrors = test_mirrors_to("127.0.0.1:5099").unwrap();
        for image in PINNED_TEST_IMAGES {
            let reference = ImageReference::parse(image).unwrap();
            let mirrored = mirrors.mirror_for(&reference).unwrap();
            assert_eq!(mirrored.registry, "127.0.0.1:5099");
            assert_eq!(mirrored.repository, reference.repository);
        }
    }
}
