//! Version-pinned tooling, guest images and signed Linux release binaries.

use super::{download::Downloader, lima::Lima, progress::Step};
use crate::upgrade::{
    BinaryVersion, keys,
    signing::{PublicKey, SignatureEnvelope, sha256_hex, verify_binary},
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

const REPOSITORY: &str = "https://github.com/reliaburger/reliaburger/releases/download";

/// The guest image pins compiled into the CLI (`scripts/release/guest-images.json`).
#[derive(Deserialize)]
pub struct GuestImagePins {
    /// Debian packages every node needs. Baked into the release image, and
    /// installed at first boot when a VM starts from the stock Ubuntu image.
    pub packages: Vec<String>,
    /// Per guest architecture: the release asset and the image it's built from.
    pub images: BTreeMap<String, GuestImage>,
}

/// One architecture's guest image.
#[derive(Deserialize)]
pub struct GuestImage {
    /// Immutable release attachment name of the image built in CI.
    pub asset: String,
    /// The dated Ubuntu cloud image the release image is built from.
    pub source: UpstreamImage,
}

/// A dated upstream Ubuntu cloud image, pinned by SHA-256.
#[derive(Deserialize)]
pub struct UpstreamImage {
    /// Cache file name for development runs, which boot this image directly.
    pub file: String,
    /// Dated download URL, never a mutable `current` alias.
    pub url: String,
    /// SHA-256 pinned into the CLI build.
    pub sha256: String,
}

/// The pins compiled into this build.
pub fn guest_image_pins() -> Result<GuestImagePins> {
    Ok(serde_json::from_str(include_str!(
        "../../../scripts/release/guest-images.json"
    ))?)
}

/// Select the image without consulting a mutable upstream `current` alias.
pub fn guest_image(arch: &str) -> Result<GuestImage> {
    guest_image_pins()?
        .images
        .remove(arch)
        .context("unsupported guest architecture")
}

/// Release asset describing the built guest images, signed by the release key.
pub const GUEST_IMAGE_METADATA: &str = "guest-image-metadata.json";

/// `guest-image-metadata.json`: what the release pipeline built, per architecture.
#[derive(Deserialize)]
pub struct GuestImageMetadata {
    /// Format version. Currently 1.
    pub schema: u32,
    /// Release tag the images belong to, such as `v0.1.0`.
    pub version: String,
    /// Built image per guest architecture.
    pub images: BTreeMap<String, BuiltGuestImage>,
}

/// One built image, as recorded and signed by `scripts/release/package.py`.
#[derive(Deserialize)]
pub struct BuiltGuestImage {
    /// Release attachment name.
    pub asset: String,
    /// SHA-256 of the compressed qcow2.
    pub sha256: String,
    /// Base64 Ed25519 signature over [`guest_image_statement`].
    pub signature: String,
    /// The upstream image it was built from.
    pub source: BuiltFrom,
}

/// Provenance of a built image.
#[derive(Deserialize)]
pub struct BuiltFrom {
    /// Upstream download URL.
    pub url: String,
    /// Upstream SHA-256.
    pub sha256: String,
}

/// The exact text the release key signs for one built guest image.
///
/// The image digest can't be compiled into the CLI: CI builds the image in
/// the same run as the CLI, and a rebuilt filesystem never has the same
/// bytes twice. So the release key vouches for it instead, binding the
/// digest to one version, architecture, asset name and upstream source.
/// `scripts/release/package.py` produces the same text.
pub fn guest_image_statement(
    version: &str,
    arch: &str,
    asset: &str,
    sha256: &str,
    source_sha256: &str,
) -> String {
    format!(
        "reliaburger guest image v1\nversion {version}\narch {arch}\nasset {asset}\nsha256 {sha256}\nsource-sha256 {source_sha256}\n"
    )
}

impl GuestImageMetadata {
    /// Return the built image for `arch` only if a release key signed it for
    /// this version, it has the pinned asset name, and it was built from the
    /// pinned upstream image.
    pub fn verified(
        mut self,
        version: &BinaryVersion,
        arch: &str,
        pin: &GuestImage,
        release_keys: &[PublicKey],
    ) -> Result<BuiltGuestImage> {
        if self.schema != 1 {
            bail!("unsupported guest image metadata schema {}", self.schema);
        }
        let version = version.to_string();
        if self.version != version {
            bail!(
                "guest image metadata belongs to {}, not {version}",
                self.version
            );
        }
        let image = self
            .images
            .remove(arch)
            .context("release has no guest image for this architecture")?;
        if image.asset != pin.asset || image.source.sha256 != pin.source.sha256 {
            bail!("release guest image differs from the one pinned into this CLI");
        }
        if image.sha256.len() != 64 || !image.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("release guest image has an invalid SHA-256");
        }
        let statement = guest_image_statement(
            &version,
            arch,
            &image.asset,
            &image.sha256,
            &image.source.sha256,
        );
        let envelope = SignatureEnvelope {
            schema: 1,
            sha256: sha256_hex(statement.as_bytes()),
            embedded: image.signature.clone(),
            external: None,
        };
        verify_binary(statement.as_bytes(), &envelope, release_keys, None, false)
            .context("release guest image signature is not valid")?;
        Ok(image)
    }
}

/// Select a checksummed Lima distribution for the host platform.
pub fn lima_archive(os: &str, arch: &str) -> Result<(String, &'static str)> {
    let (platform, checksum) = match (os, arch) {
        ("macos", "aarch64") => (
            "Darwin-arm64",
            "1da852bce2f98b8310fb53e5047e08ff798880ddf9ae4b3161d4de4e73777b34",
        ),
        ("macos", "x86_64") => (
            "Darwin-x86_64",
            "529d6dad275bded4bd1fb17c6df4d22f11917712aab1693f5bc43286b5e78824",
        ),
        ("linux", "aarch64") => (
            "Linux-aarch64",
            "8b806c81f38ad7ea8104f196b414e243726cad1dfa6aa52843608d55df8c5290",
        ),
        ("linux", "x86_64") => (
            "Linux-x86_64",
            "9e52c605790649aceac1f894eb2bb7cdb45897257faafb54ff30ce6a576bebfa",
        ),
        _ => bail!("managed clusters require macOS or Linux on arm64 or x86_64"),
    };
    Ok((
        format!(
            "https://github.com/lima-vm/lima/releases/download/v2.1.0/lima-2.1.0-{platform}.tar.gz"
        ),
        checksum,
    ))
}

/// Install a private pinned Lima distribution without modifying system packages.
pub async fn tooling(root: &Path, downloader: &Downloader, step: &Step) -> Result<Lima> {
    let (url, checksum) = lima_archive(std::env::consts::OS, std::env::consts::ARCH)?;
    let tools = root.join("tools");
    tokio::fs::create_dir_all(&tools).await?;
    let installed = tools.join("lima-2.1.0");
    let executable = installed.join("bin/limactl");
    if !executable.exists() {
        let archive = tools.join("lima-2.1.0.tar.gz");
        downloader
            .fetch(&url, checksum, &archive, 256 * 1024 * 1024, Some(step))
            .await?;
        let staging = tempfile::Builder::new()
            .prefix("lima-")
            .tempdir_in(&tools)?;
        let status = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::process::Command::new("tar")
                .arg("-xzf")
                .arg(&archive)
                .arg("-C")
                .arg(staging.path())
                .kill_on_drop(true)
                .status(),
        )
        .await??;
        if !status.success() {
            bail!("failed to unpack verified Lima archive");
        }
        tokio::fs::rename(staging.path(), &installed).await?;
        let _ = staging.keep();
    } else {
        step.note("installed");
    }
    let lima = Lima::new(executable, Duration::from_secs(240)).with_home(root.join("lima"));
    let version = lima.command(&["--version"]).await?;
    if !version.split_whitespace().any(|word| word == "2.1.0") {
        bail!("managed Lima installation has an unexpected version");
    }
    Ok(lima)
}

/// Download the version's built guest image, trusting it only once a release
/// key has signed its digest.
pub async fn image(
    cache: &Path,
    version: &BinaryVersion,
    downloader: &Downloader,
    step: &Step,
) -> Result<PathBuf> {
    let arch = std::env::consts::ARCH;
    let pin = guest_image(arch)?;
    let metadata: GuestImageMetadata = downloader
        .json(&format!("{REPOSITORY}/{version}/{GUEST_IMAGE_METADATA}"))
        .await?;
    let keys = keys::release_keys(&Default::default())?;
    let image = metadata.verified(version, arch, &pin, &keys)?;
    let path = cache.join(&image.asset);
    downloader
        .fetch(
            &format!("{REPOSITORY}/{version}/{}", image.asset),
            &image.sha256,
            &path,
            2 * 1024 * 1024 * 1024,
            Some(step),
        )
        .await?;
    Ok(path)
}

/// Download a Linux executable and require a signature from the embedded release keys.
pub async fn binary(
    cache: &Path,
    version: &BinaryVersion,
    name: &str,
    downloader: &Downloader,
    step: &Step,
) -> Result<PathBuf> {
    let manifest = match name {
        "bun" => "metadata.json",
        "relish" => "cli-metadata.json",
        _ => bail!("unknown managed binary"),
    };
    let metadata = downloader
        .metadata(&format!("{REPOSITORY}/{version}/{manifest}"))
        .await?;
    let artifact = metadata
        .artifact_for(version, &format!("linux-{}", std::env::consts::ARCH))
        .context("release has no binary for the requested guest architecture")?
        .clone();
    let path = cache.join(format!("{name}-{version}-linux-{}", std::env::consts::ARCH));
    downloader
        .fetch(
            &artifact.url,
            &artifact.sha256,
            &path,
            256 * 1024 * 1024,
            Some(step),
        )
        .await?;
    let checked_path = path.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let bytes = std::fs::read(&checked_path)?;
        let envelope = SignatureEnvelope {
            schema: 1,
            sha256: artifact.sha256,
            embedded: artifact.embedded_signature,
            external: None,
        };
        verify_binary(
            &bytes,
            &envelope,
            &keys::release_keys(&Default::default())?,
            None,
            false,
        )?;
        Ok(())
    })
    .await??;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_images_are_pinned_and_have_stable_release_asset_names() {
        for arch in ["aarch64", "x86_64"] {
            let image = guest_image(arch).unwrap();
            let source = &image.source;
            assert!(
                source
                    .url
                    .starts_with("https://cloud-images.ubuntu.com/releases/")
            );
            assert!(!source.url.contains("current"));
            assert_eq!(source.sha256.len(), 64);
            assert!(source.file.ends_with(&format!("{arch}.img")));
            assert!(image.asset.starts_with("reliaburger-guest-"));
            assert!(image.asset.ends_with(&format!("{arch}.qcow2")));
        }
        assert!(guest_image("riscv64").is_err());
    }

    #[test]
    fn guest_image_pins_name_every_package_a_node_checks_for() {
        let packages = guest_image_pins().unwrap().packages;
        // `wait_for_guest` checks runc, btrfs and nft; Bun needs newuidmap.
        for package in ["runc", "uidmap", "btrfs-progs", "nftables"] {
            assert!(packages.iter().any(|name| name == package), "{package}");
        }
    }

    #[test]
    fn guest_image_statement_matches_the_release_packager() {
        // scripts/release/test_package.py asserts the same text for the same inputs.
        assert_eq!(
            guest_image_statement("v0.1.0", "aarch64", "guest.qcow2", "ab", "cd"),
            "reliaburger guest image v1\nversion v0.1.0\narch aarch64\nasset guest.qcow2\nsha256 ab\nsource-sha256 cd\n"
        );
    }

    struct SignedRelease {
        pin: GuestImage,
        key: Vec<u8>,
        public: PublicKey,
    }

    impl SignedRelease {
        fn new() -> Self {
            let (key, public) = crate::upgrade::signing::generate_keypair().unwrap();
            Self {
                pin: guest_image("aarch64").unwrap(),
                key,
                public,
            }
        }

        /// Metadata for `aarch64`, signed over `signed_*` but claiming `claimed_sha256`.
        fn metadata(&self, version: &str, signed_sha256: &str, claimed_sha256: &str) -> String {
            let statement = guest_image_statement(
                version,
                "aarch64",
                &self.pin.asset,
                signed_sha256,
                &self.pin.source.sha256,
            );
            let signature = crate::upgrade::signing::sign(&self.key, statement.as_bytes()).unwrap();
            serde_json::json!({
                "schema": 1,
                "version": version,
                "images": {"aarch64": {
                    "asset": self.pin.asset,
                    "sha256": claimed_sha256,
                    "size": 1,
                    "signature": signature,
                    "source": {"url": self.pin.source.url, "sha256": self.pin.source.sha256},
                }}
            })
            .to_string()
        }

        fn verify(&self, metadata: &str, arch: &str) -> Result<BuiltGuestImage> {
            let metadata: GuestImageMetadata = serde_json::from_str(metadata).unwrap();
            metadata.verified(&"v0.1.0".parse().unwrap(), arch, &self.pin, &[self.public])
        }
    }

    #[test]
    fn signed_guest_image_metadata_yields_the_digest_to_download() {
        let release = SignedRelease::new();
        let digest = "a".repeat(64);
        let image = release
            .verify(&release.metadata("v0.1.0", &digest, &digest), "aarch64")
            .unwrap();
        assert_eq!(image.sha256, digest);
        assert_eq!(image.asset, release.pin.asset);
    }

    #[test]
    fn guest_image_metadata_is_refused_unless_signed_for_this_digest_version_and_key() {
        let release = SignedRelease::new();
        let digest = "a".repeat(64);
        // A digest swapped after signing.
        let swapped = release.metadata("v0.1.0", &digest, &"b".repeat(64));
        assert!(release.verify(&swapped, "aarch64").is_err());
        // Another version's genuine metadata.
        let replayed = release.metadata("v0.0.9", &digest, &digest);
        assert!(release.verify(&replayed, "aarch64").is_err());
        // No image for the host's architecture.
        let genuine = release.metadata("v0.1.0", &digest, &digest);
        assert!(release.verify(&genuine, "x86_64").is_err());
        // Signed by a key the CLI doesn't trust.
        let stranger = SignedRelease::new();
        let foreign = stranger.metadata("v0.1.0", &digest, &digest);
        assert!(release.verify(&foreign, "aarch64").is_err());
        // Built from an upstream image other than the pinned one.
        let other_source = genuine.replace(&release.pin.source.sha256, &"c".repeat(64));
        assert!(release.verify(&other_source, "aarch64").is_err());
    }

    #[test]
    fn tooling_archive_is_pinned_for_every_supported_host() {
        for os in ["macos", "linux"] {
            for arch in ["aarch64", "x86_64"] {
                let (url, checksum) = lima_archive(os, arch).unwrap();
                assert!(url.contains("/v2.1.0/"));
                assert_eq!(checksum.len(), 64);
            }
        }
        assert!(lima_archive("windows", "x86_64").is_err());
    }
}
