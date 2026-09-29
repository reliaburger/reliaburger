//! Bound upstream reads and verify OCI digests before either cache publishes them.

use oci_distribution::errors::OciDistributionError;
use oci_distribution::manifest::{
    IMAGE_MANIFEST_LIST_MEDIA_TYPE, IMAGE_MANIFEST_MEDIA_TYPE, OCI_IMAGE_INDEX_MEDIA_TYPE,
    OCI_IMAGE_MEDIA_TYPE, OciImageManifest, OciManifest, Versioned,
};
use oci_distribution::secrets::RegistryAuth;
use oci_distribution::{Client, Reference};
use sha2::{Digest, Sha256};
use std::time::Duration;

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
    pull_verified_manifest_for_architecture(client, reference, auth, std::env::consts::ARCH).await
}

/// Normalise the advertised release targets to their Linux OCI architecture names.
pub(crate) fn linux_architecture(architecture: &str) -> Result<&'static str, OciDistributionError> {
    match architecture {
        "x86_64" | "amd64" => Ok("amd64"),
        "aarch64" | "arm64" => Ok("arm64"),
        _ => Err(invalid(format!(
            "unsupported Linux container architecture: {architecture}"
        ))),
    }
}

/// Verify an image for the target container host, independently of the client's OS.
pub(crate) async fn pull_verified_manifest_for_architecture(
    client: &Client,
    reference: &Reference,
    auth: &RegistryAuth,
    architecture: &str,
) -> Result<VerifiedImageManifest, OciDistributionError> {
    let architecture = linux_architecture(architecture)?;
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
            let descriptor = index
                .manifests
                .iter()
                .find(|entry| {
                    entry.platform.as_ref().is_some_and(|platform| {
                        platform.os == "linux" && platform.architecture == architecture
                    })
                })
                .ok_or_else(|| {
                    invalid(format!(
                        "upstream index has no manifest for linux/{architecture}"
                    ))
                })?;
            let digest = descriptor.digest.clone();
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

/// How long one registry read may take, and how long all its attempts may take together.
///
/// A slow CDN edge can stall one connection while a fresh one answers at
/// once, so a stalled attempt is abandoned at `attempt` and retried. `total`
/// still bounds the whole read, so a registry that never answers cannot hold a
/// pull open indefinitely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RegistryReadBudget {
    /// Ceiling on one attempt, including every request the attempt makes.
    pub(crate) attempt: Duration,
    /// Ceiling on every attempt plus the backoff between them.
    pub(crate) total: Duration,
}

/// HEAD, manifest and configuration reads: a few small responses per attempt.
pub(crate) const METADATA_READ: RegistryReadBudget = RegistryReadBudget {
    attempt: Duration::from_secs(30),
    total: Duration::from_secs(120),
};

/// One layer blob, which may be tens of megabytes over a slow link.
pub(crate) const LAYER_READ: RegistryReadBudget = RegistryReadBudget {
    attempt: Duration::from_secs(120),
    total: Duration::from_secs(360),
};

/// Attempts per read, the first included.
const MAX_ATTEMPTS: u32 = 4;

/// Whether a failed read may succeed if repeated: rate limits, gateway and
/// service errors, and connections or response streams that broke. Denials,
/// missing content and malformed or corrupt responses are terminal.
fn is_transient(error: &OciDistributionError) -> bool {
    use oci_distribution::errors::OciErrorCode;
    match error {
        OciDistributionError::RegistryError { envelope, .. } => {
            !envelope.errors.is_empty()
                && envelope
                    .errors
                    .iter()
                    .all(|error| error.code == OciErrorCode::Toomanyrequests)
        }
        OciDistributionError::ServerError { code, .. } => {
            matches!(code, 408 | 429 | 500 | 502 | 503 | 504)
        }
        OciDistributionError::RequestError(error) => {
            // Manifest parsing and layer digest checks happen separately. Reqwest
            // decode errors here include interrupted response-byte streams.
            error.is_connect()
                || error.is_request()
                || error.is_timeout()
                || error.is_body()
                || error.is_decode()
                || matches!(
                    error.status().map(|status| status.as_u16()),
                    Some(408 | 429 | 500 | 502 | 503 | 504)
                )
        }
        _ => false,
    }
}

/// Retry transient registry reads with jittered exponential backoff.
///
/// Each attempt gets `budget.attempt`; all attempts share `budget.total`. The
/// closure builds a fresh future per attempt, so a partial response from an
/// abandoned attempt never reaches the next one.
pub(crate) async fn retry_registry_read<T, F>(
    budget: RegistryReadBudget,
    mut read: impl FnMut() -> F,
) -> oci_distribution::errors::Result<T>
where
    F: std::future::Future<Output = oci_distribution::errors::Result<T>>,
{
    use tokio::time::{Instant, sleep_until, timeout_at};

    let deadline = Instant::now() + budget.total;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let attempt_deadline = (Instant::now() + budget.attempt).min(deadline);
        let error = match timeout_at(attempt_deadline, read()).await {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(error)) if is_transient(&error) => error,
            Ok(Err(error)) => return Err(error),
            Err(_) => invalid(format!(
                "registry read deadline exceeded on attempt {attempt} of {MAX_ATTEMPTS}"
            )),
        };
        if attempt == MAX_ATTEMPTS || Instant::now() >= deadline {
            return Err(error);
        }
        // Jitter keeps simultaneous cold nodes from retrying in lockstep.
        let delay =
            Duration::from_millis((1000 << (attempt - 1)) + u64::from(rand::random::<u8>()));
        sleep_until((Instant::now() + delay).min(deadline)).await;
        if Instant::now() >= deadline {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::time::Instant;

    const BUDGET: RegistryReadBudget = RegistryReadBudget {
        attempt: Duration::from_secs(10),
        total: Duration::from_secs(60),
    };

    fn server_error(code: u16) -> OciDistributionError {
        OciDistributionError::ServerError {
            code,
            url: "https://registry.example/v2/".into(),
            message: "injected".into(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_attempt_is_abandoned_and_retried() {
        let attempts = AtomicU32::new(0);
        let started = Instant::now();
        let value = retry_registry_read(BUDGET, || {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt == 0 {
                    std::future::pending::<()>().await;
                }
                Ok(attempt)
            }
        })
        .await
        .unwrap();
        assert_eq!(value, 1);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= BUDGET.attempt,
            "retried before the attempt ceiling"
        );
        assert!(
            elapsed < BUDGET.attempt + Duration::from_secs(2),
            "{elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn persistent_stalls_end_within_the_total_budget() {
        let attempts = AtomicU32::new(0);
        let started = Instant::now();
        let error = retry_registry_read(BUDGET, || {
            attempts.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<oci_distribution::errors::Result<()>>()
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("deadline exceeded"), "{error}");
        assert_eq!(attempts.load(Ordering::SeqCst), MAX_ATTEMPTS);
        assert!(started.elapsed() <= BUDGET.total);
    }

    #[tokio::test(start_paused = true)]
    async fn the_total_budget_truncates_a_long_attempt_ceiling() {
        let budget = RegistryReadBudget {
            attempt: Duration::from_secs(50),
            total: Duration::from_secs(60),
        };
        let attempts = AtomicU32::new(0);
        let started = Instant::now();
        retry_registry_read(budget, || {
            attempts.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<oci_distribution::errors::Result<()>>()
        })
        .await
        .unwrap_err();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(started.elapsed(), budget.total);
    }

    #[tokio::test(start_paused = true)]
    async fn server_and_gateway_errors_are_retried_with_backoff() {
        for code in [408, 429, 500, 502, 503, 504] {
            let attempts = AtomicU32::new(0);
            let started = Instant::now();
            let value = retry_registry_read(BUDGET, || {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt < 2 {
                        return Err(server_error(code));
                    }
                    Ok(attempt)
                }
            })
            .await
            .unwrap();
            assert_eq!(value, 2, "HTTP {code}");
            // One second, then two, each with under 256 ms of jitter.
            assert!(started.elapsed() >= Duration::from_secs(3), "HTTP {code}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_errors_are_not_retried() {
        for error in [
            server_error(401),
            server_error(403),
            server_error(404),
            invalid("upstream content digest mismatch"),
        ] {
            let message = error.to_string();
            let mut error = Some(error);
            let attempts = AtomicU32::new(0);
            let result: oci_distribution::errors::Result<()> = retry_registry_read(BUDGET, || {
                attempts.fetch_add(1, Ordering::SeqCst);
                let error = error.take();
                async move { Err(error.unwrap_or_else(|| invalid("called twice"))) }
            })
            .await;
            assert_eq!(result.unwrap_err().to_string(), message);
            assert_eq!(attempts.load(Ordering::SeqCst), 1, "{message}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn persistent_transient_errors_stop_after_four_attempts() {
        let attempts = AtomicU32::new(0);
        let result: oci_distribution::errors::Result<()> = retry_registry_read(BUDGET, || {
            attempts.fetch_add(1, Ordering::SeqCst);
            async { Err(server_error(503)) }
        })
        .await;
        assert!(result.unwrap_err().to_string().contains("503"));
        assert_eq!(attempts.load(Ordering::SeqCst), MAX_ATTEMPTS);
    }

    #[test]
    fn every_read_class_leaves_room_to_retry_a_stalled_attempt() {
        for budget in [METADATA_READ, LAYER_READ] {
            assert!(budget.total >= budget.attempt * 2 + Duration::from_secs(2));
        }
    }
}
