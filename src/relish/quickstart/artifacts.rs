//! Version-pinned tooling, guest images and signed Linux release binaries.

use super::{download::Downloader, lima::Lima};
use crate::upgrade::{
    BinaryVersion, keys,
    signing::{SignatureEnvelope, verify_binary},
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

const REPOSITORY: &str = "https://github.com/reliaburger/reliaburger/releases/download";

/// Upstream image identity mirrored without modification into a release.
#[derive(Deserialize)]
pub struct GuestImage {
    /// Immutable release attachment name.
    pub asset: String,
    /// Dated upstream download used when packaging the release.
    pub url: String,
    /// SHA-256 pinned into the CLI build.
    pub sha256: String,
}

/// Select the image without consulting a mutable upstream `current` alias.
pub fn guest_image(arch: &str) -> Result<GuestImage> {
    let mut images: BTreeMap<String, GuestImage> =
        serde_json::from_str(include_str!("../../../scripts/release/guest-images.json"))?;
    images
        .remove(arch)
        .context("unsupported guest architecture")
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
pub async fn tooling(root: &Path, downloader: &Downloader) -> Result<Lima> {
    let (url, checksum) = lima_archive(std::env::consts::OS, std::env::consts::ARCH)?;
    let tools = root.join("tools");
    tokio::fs::create_dir_all(&tools).await?;
    let installed = tools.join("lima-2.1.0");
    let executable = installed.join("bin/limactl");
    if !executable.exists() {
        let archive = tools.join("lima-2.1.0.tar.gz");
        downloader
            .fetch(&url, checksum, &archive, 256 * 1024 * 1024)
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
    }
    let lima = Lima::new(executable, Duration::from_secs(240));
    let version = lima.command(&["--version"]).await?;
    if !version.split_whitespace().any(|word| word == "2.1.0") {
        bail!("managed Lima installation has an unexpected version");
    }
    Ok(lima)
}

/// Download the version's mirrored guest image, checking the compiled-in digest.
pub async fn image(
    cache: &Path,
    version: &BinaryVersion,
    downloader: &Downloader,
) -> Result<PathBuf> {
    let image = guest_image(std::env::consts::ARCH)?;
    let path = cache.join(&image.asset);
    downloader
        .fetch(
            &format!("{REPOSITORY}/{version}/{}", image.asset),
            &image.sha256,
            &path,
            2 * 1024 * 1024 * 1024,
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
        .fetch(&artifact.url, &artifact.sha256, &path, 256 * 1024 * 1024)
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
            assert!(image.url.starts_with("https://cloud-images.ubuntu.com/"));
            assert!(!image.url.contains("current"));
            assert_eq!(image.sha256.len(), 64);
            assert!(image.asset.ends_with(&format!("{arch}.img")));
        }
        assert!(guest_image("riscv64").is_err());
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
