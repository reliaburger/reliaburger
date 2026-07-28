//! Building and pushing a tiny synthetic OCI image, for the image-registry
//! cases.
//!
//! The dev nodes have no image builder and a loopback-only registry, so the
//! harness constructs an image itself — a config blob, one gzipped tar layer,
//! and a manifest tying them together — and speaks the raw `/v2` protocol to
//! push it. The construction is pure and unit-tested here; the push runs
//! against a live registry, so it's exercised only at acceptance.

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use tar::{Builder, Header};

/// A synthetic image: the three blobs a registry needs, each with its digest.
pub struct SyntheticImage {
    pub config: Vec<u8>,
    pub config_digest: String,
    pub layer: Vec<u8>,
    pub layer_digest: String,
    pub manifest: Vec<u8>,
    pub manifest_digest: String,
}

/// `sha256:<hex>` for `data`, the digest form the registry keys blobs by.
pub fn sha256_digest(data: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(data))
}

/// One gzipped tar layer carrying a single marker file.
fn build_layer() -> Vec<u8> {
    let content = b"reliaburger testkit fixture\n";
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut tar = Builder::new(&mut encoder);
        let mut header = Header::new_gnu();
        header.set_path("marker.txt").expect("static path is valid");
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, &content[..])
            .expect("append to in-memory tar");
        tar.finish().expect("finish in-memory tar");
    }
    encoder.finish().expect("finish gzip")
}

/// Construct a fresh synthetic image. `salt` varies the layer content so two
/// calls produce distinct digests when a test needs uniqueness.
pub fn build_synthetic_image(salt: &str) -> SyntheticImage {
    // Vary the config's created field via the salt so digests differ per call.
    let config = serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "config": {},
        "rootfs": { "type": "layers", "diff_ids": [] },
        "comment": format!("reliaburger-testkit-{salt}"),
    });
    let config = serde_json::to_vec(&config).expect("config serialises");
    let config_digest = sha256_digest(&config);

    let layer = build_layer();
    let layer_digest = sha256_digest(&layer);

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "digest": config_digest,
            "size": config.len(),
            "mediaType": "application/vnd.oci.image.config.v1+json",
        },
        "layers": [{
            "digest": layer_digest,
            "size": layer.len(),
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
        }],
    });
    let manifest = serde_json::to_vec(&manifest).expect("manifest serialises");
    let manifest_digest = sha256_digest(&manifest);

    SyntheticImage {
        config,
        config_digest,
        layer,
        layer_digest,
        manifest,
        manifest_digest,
    }
}

/// Resolve a `Location` header (which may be a path or a full URL) against the
/// registry base.
fn resolve_location(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        location.to_string()
    } else {
        format!("{base}{location}")
    }
}

/// Push one blob through the `/v2` monolithic-upload dance: POST to start,
/// PATCH the bytes, PUT with the digest to finish.
async fn push_blob(
    http: &reqwest::Client,
    base: &str,
    repo: &str,
    data: &[u8],
    digest: &str,
) -> Result<(), String> {
    let start = http
        .post(format!("{base}/v2/{repo}/blobs/uploads/"))
        .send()
        .await
        .map_err(|e| format!("blob upload POST failed: {e}"))?;
    let location = start
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(|l| resolve_location(base, l))
        .ok_or_else(|| "upload POST returned no Location".to_string())?;

    let patched = http
        .patch(&location)
        .body(data.to_vec())
        .send()
        .await
        .map_err(|e| format!("blob PATCH failed: {e}"))?;
    let location = patched
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(|l| resolve_location(base, l))
        .unwrap_or(location);

    let separator = if location.contains('?') { '&' } else { '?' };
    let finish = http
        .put(format!("{location}{separator}digest={digest}"))
        .send()
        .await
        .map_err(|e| format!("blob PUT failed: {e}"))?;
    if !finish.status().is_success() {
        return Err(format!("blob PUT returned {}", finish.status()));
    }
    Ok(())
}

/// Push a synthetic image to `base` (e.g. `http://127.0.0.1:5050`) as
/// `repo:tag`: both blobs, then the manifest.
pub async fn push_image(
    http: &reqwest::Client,
    base: &str,
    repo: &str,
    tag: &str,
    image: &SyntheticImage,
) -> Result<(), String> {
    push_blob(http, base, repo, &image.config, &image.config_digest).await?;
    push_blob(http, base, repo, &image.layer, &image.layer_digest).await?;

    let manifest = http
        .put(format!("{base}/v2/{repo}/manifests/{tag}"))
        .header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
        .body(image.manifest.clone())
        .send()
        .await
        .map_err(|e| format!("manifest PUT failed: {e}"))?;
    if !manifest.status().is_success() {
        return Err(format!("manifest PUT returned {}", manifest.status()));
    }
    Ok(())
}

/// Fetch a manifest's raw bytes back, for a round-trip digest check.
pub async fn fetch_manifest(
    http: &reqwest::Client,
    base: &str,
    repo: &str,
    reference: &str,
) -> Result<Vec<u8>, String> {
    let response = http
        .get(format!("{base}/v2/{repo}/manifests/{reference}"))
        .header("Accept", "application/vnd.oci.image.manifest.v1+json")
        .send()
        .await
        .map_err(|e| format!("manifest GET failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("manifest GET returned {}", response.status()));
    }
    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("reading manifest body failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_synthetic_image_has_well_formed_digests() {
        let image = build_synthetic_image("a");
        for digest in [
            &image.config_digest,
            &image.layer_digest,
            &image.manifest_digest,
        ] {
            assert!(digest.starts_with("sha256:"), "{digest}");
            assert_eq!(digest.len(), "sha256:".len() + 64, "{digest}");
        }
        // The digest of the bytes matches the recorded digest.
        assert_eq!(sha256_digest(&image.config), image.config_digest);
        assert_eq!(sha256_digest(&image.layer), image.layer_digest);
        assert_eq!(sha256_digest(&image.manifest), image.manifest_digest);
    }

    #[test]
    fn the_manifest_references_both_blobs() {
        let image = build_synthetic_image("b");
        let manifest: serde_json::Value = serde_json::from_slice(&image.manifest).unwrap();
        assert_eq!(
            manifest["config"]["digest"].as_str(),
            Some(image.config_digest.as_str())
        );
        assert_eq!(
            manifest["layers"][0]["digest"].as_str(),
            Some(image.layer_digest.as_str())
        );
        assert_eq!(
            manifest["config"]["size"].as_u64(),
            Some(image.config.len() as u64)
        );
    }

    #[test]
    fn the_salt_changes_the_digest() {
        assert_ne!(
            build_synthetic_image("one").config_digest,
            build_synthetic_image("two").config_digest
        );
    }

    #[test]
    fn a_relative_location_resolves_against_the_base() {
        assert_eq!(
            resolve_location("http://127.0.0.1:5050", "/v2/x/blobs/uploads/1"),
            "http://127.0.0.1:5050/v2/x/blobs/uploads/1"
        );
        assert_eq!(
            resolve_location("http://127.0.0.1:5050", "http://host/abs"),
            "http://host/abs"
        );
    }
}
