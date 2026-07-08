//! Release metadata: what versions exist and where their binaries live.
//!
//! Served as a static JSON file from any HTTPS host (`upgrades.release_url`
//! in node.toml). Metadata is NOT signed — it travels over TLS and can at
//! worst lie about what exists; it cannot make a node run anything, because
//! the per-binary dual signatures gate execution.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::error::UpgradeError;
use super::version::BinaryVersion;

/// The whole metadata document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseMetadata {
    /// Document format version. Currently 1.
    pub schema: u32,
    /// The newest generally-available version.
    pub latest: BinaryVersion,
    pub releases: Vec<Release>,
}

/// One released version, with per-platform artefacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub version: BinaryVersion,
    /// Keyed by `{os}-{arch}` (`linux-x86_64`, `macos-aarch64`, ...).
    pub platforms: BTreeMap<String, PlatformArtifact>,
}

/// A downloadable binary for one platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformArtifact {
    pub url: String,
    /// Hex SHA-256 of the binary.
    pub sha256: String,
    /// Base64 Ed25519 signature from the release key set.
    pub embedded_signature: String,
    /// Operator-specific signature, if the metadata host is private.
    #[serde(default)]
    pub external_signature: Option<String>,
}

/// The platform key for the machine this code runs on.
pub fn platform_key() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

impl ReleaseMetadata {
    /// Parse a metadata JSON document.
    pub fn parse(json: &str) -> Result<Self, UpgradeError> {
        serde_json::from_str(json).map_err(|e| UpgradeError::InvalidMetadata {
            reason: e.to_string(),
        })
    }

    /// The artefact for `version` on `platform`, if released.
    pub fn artifact_for(
        &self,
        version: &BinaryVersion,
        platform: &str,
    ) -> Option<&PlatformArtifact> {
        self.releases
            .iter()
            .find(|release| release.version == *version)?
            .platforms
            .get(platform)
    }
}

/// Fetch and parse metadata from a URL.
pub async fn fetch(url: &str) -> Result<ReleaseMetadata, UpgradeError> {
    let response = reqwest::get(url)
        .await
        .map_err(|e| UpgradeError::FetchFailed {
            url: url.to_string(),
            reason: e.to_string(),
        })?;
    if !response.status().is_success() {
        return Err(UpgradeError::FetchFailed {
            url: url.to_string(),
            reason: format!("status {}", response.status()),
        });
    }
    let body = response
        .text()
        .await
        .map_err(|e| UpgradeError::FetchFailed {
            url: url.to_string(),
            reason: e.to_string(),
        })?;
    ReleaseMetadata::parse(&body)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "schema": 1,
        "latest": "v0.2.0",
        "releases": [
            {
                "version": "v0.2.0",
                "platforms": {
                    "linux-x86_64": {
                        "url": "https://releases.example/bun-v0.2.0-linux-x86_64",
                        "sha256": "abc123",
                        "embedded_signature": "c2ln"
                    },
                    "macos-aarch64": {
                        "url": "https://releases.example/bun-v0.2.0-macos-aarch64",
                        "sha256": "def456",
                        "embedded_signature": "c2ln",
                        "external_signature": "ZXh0"
                    }
                }
            }
        ]
    }"#;

    #[test]
    fn metadata_parses_and_selects_platform() {
        let metadata = ReleaseMetadata::parse(FIXTURE).unwrap();
        assert_eq!(metadata.latest, "v0.2.0".parse().unwrap());

        let artifact = metadata
            .artifact_for(&"v0.2.0".parse().unwrap(), "linux-x86_64")
            .unwrap();
        assert_eq!(artifact.sha256, "abc123");
        assert!(artifact.external_signature.is_none());

        let mac = metadata
            .artifact_for(&"v0.2.0".parse().unwrap(), "macos-aarch64")
            .unwrap();
        assert_eq!(mac.external_signature.as_deref(), Some("ZXh0"));
    }

    #[test]
    fn missing_version_or_platform_is_none() {
        let metadata = ReleaseMetadata::parse(FIXTURE).unwrap();
        assert!(
            metadata
                .artifact_for(&"v9.9.9".parse().unwrap(), "linux-x86_64")
                .is_none()
        );
        assert!(
            metadata
                .artifact_for(&"v0.2.0".parse().unwrap(), "plan9-mips")
                .is_none()
        );
    }

    #[test]
    fn garbage_metadata_is_an_error() {
        assert!(matches!(
            ReleaseMetadata::parse("not json"),
            Err(UpgradeError::InvalidMetadata { .. })
        ));
    }

    #[test]
    fn platform_key_has_os_and_arch() {
        let key = platform_key();
        assert!(key.contains('-'));
    }
}
