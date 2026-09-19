//! Verify the raw OCI digest chain before either image cache publishes it.

use oci_distribution::client::current_platform_resolver;
use oci_distribution::errors::OciDistributionError;
use oci_distribution::manifest::{
    IMAGE_MANIFEST_LIST_MEDIA_TYPE, IMAGE_MANIFEST_MEDIA_TYPE, OCI_IMAGE_INDEX_MEDIA_TYPE,
    OCI_IMAGE_MEDIA_TYPE, OciImageManifest, OciManifest, Versioned,
};
use oci_distribution::secrets::RegistryAuth;
use oci_distribution::{Client, Reference};
use sha2::{Digest, Sha256};

const MEDIA_TYPES: &[&str] = &[
    OCI_IMAGE_MEDIA_TYPE,
    IMAGE_MANIFEST_MEDIA_TYPE,
    OCI_IMAGE_INDEX_MEDIA_TYPE,
    IMAGE_MANIFEST_LIST_MEDIA_TYPE,
];

/// Platform-resolved metadata and the exact bytes authenticated by its digests.
pub(crate) struct VerifiedImageManifest {
    pub(crate) manifest: OciImageManifest,
    pub(crate) manifest_bytes: Vec<u8>,
    pub(crate) digest: String,
    pub(crate) config_bytes: Vec<u8>,
}

fn invalid(message: impl Into<String>) -> OciDistributionError {
    OciDistributionError::GenericError(Some(message.into()))
}

fn verify(bytes: &[u8], expected: &str, size: Option<i64>) -> Result<(), OciDistributionError> {
    let actual = format!("sha256:{:x}", Sha256::digest(bytes));
    if actual != expected {
        return Err(invalid(format!(
            "upstream content digest mismatch: expected {expected}, received {actual}"
        )));
    }
    if let Some(size) = size
        && u64::try_from(size).ok() != Some(bytes.len() as u64)
    {
        return Err(invalid(format!(
            "upstream descriptor size mismatch for {expected}: expected {size}, received {}",
            bytes.len()
        )));
    }
    Ok(())
}

fn parse_manifest(bytes: &[u8]) -> Result<OciManifest, OciDistributionError> {
    let versioned: Versioned = serde_json::from_slice(bytes)
        .map_err(|e| OciDistributionError::VersionedParsingError(e.to_string()))?;
    if versioned.schema_version != 2 {
        return Err(OciDistributionError::UnsupportedSchemaVersionError(
            versioned.schema_version,
        ));
    }
    if let Some(media_type) = versioned.media_type
        && !MEDIA_TYPES.contains(&media_type.as_str())
    {
        return Err(OciDistributionError::UnsupportedMediaTypeError(media_type));
    }
    serde_json::from_slice(bytes)
        .map_err(|e| OciDistributionError::ManifestParsingError(e.to_string()))
}

/// Fetch and verify a pinned root, its selected platform manifest and config.
/// Callers bound the complete operation with their existing read deadline.
pub(crate) async fn pull_verified_manifest(
    client: &Client,
    reference: &Reference,
    auth: &RegistryAuth,
) -> Result<VerifiedImageManifest, OciDistributionError> {
    // Headers are registry assertions, not evidence of the bytes received.
    let (mut manifest_bytes, _) = client
        .pull_manifest_raw(reference, auth, MEDIA_TYPES)
        .await?;
    if let Some(expected) = reference.digest() {
        verify(&manifest_bytes, expected, None)?;
    }
    let manifest = match parse_manifest(&manifest_bytes)? {
        OciManifest::Image(manifest) => manifest,
        OciManifest::ImageIndex(index) => {
            let digest = current_platform_resolver(&index.manifests)
                .ok_or_else(|| invalid("upstream index has no manifest for this platform"))?;
            let descriptor = index
                .manifests
                .iter()
                .find(|entry| entry.digest == digest)
                .ok_or_else(|| invalid("upstream platform descriptor is missing"))?;
            let child_reference = Reference::with_digest(
                reference.registry().to_owned(),
                reference.repository().to_owned(),
                digest,
            );
            (manifest_bytes, _) = client
                .pull_manifest_raw(&child_reference, auth, MEDIA_TYPES)
                .await?;
            verify(&manifest_bytes, &descriptor.digest, Some(descriptor.size))?;
            match parse_manifest(&manifest_bytes)? {
                OciManifest::Image(manifest) => manifest,
                OciManifest::ImageIndex(_) => {
                    return Err(invalid(
                        "upstream platform descriptor refers to another index",
                    ));
                }
            }
        }
    };
    // Cache accounting sums layer lengths as u64. Validate before any cast,
    // allocation or publication, even when the raw manifest digest is valid.
    manifest.layers.iter().try_fold(0_u64, |total, layer| {
        let size = u64::try_from(layer.size)
            .map_err(|_| invalid(format!("negative upstream layer size for {}", layer.digest)))?;
        total
            .checked_add(size)
            .ok_or_else(|| invalid("upstream layer sizes overflow cache accounting"))
    })?;
    let mut config_bytes = Vec::new();
    client
        .pull_blob(reference, &manifest.config, &mut config_bytes)
        .await?;
    verify(
        &config_bytes,
        &manifest.config.digest,
        Some(manifest.config.size),
    )?;
    std::str::from_utf8(&config_bytes)
        .map_err(|e| invalid(format!("upstream configuration is not UTF-8: {e}")))?;
    Ok(VerifiedImageManifest {
        digest: format!("sha256:{:x}", Sha256::digest(&manifest_bytes)),
        manifest,
        manifest_bytes,
        config_bytes,
    })
}
