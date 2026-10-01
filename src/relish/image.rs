//! `relish image download` and `relish image write`: published appliance
//! OS builds on the operator's machine (docs/plans/2026-10-01-plan-
//! appliance-product.md, W3).
//!
//! `download` trusts nothing it hasn't checked: the channel's signature
//! against the release keys this relish carries, each architecture's
//! `SHA256SUMS` against the channel, and every file against `SHA256SUMS`
//! as it streams to disk (`crate::os`). The layout it writes is what a
//! netboot server serves: `<dir>/<arch>/` with iPXE in `netboot/`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};

use crate::os::{OsChannel, SumsEntry};
use crate::relish::RelishError;

/// Where the newest OS release is named.
pub const CHANNEL_URL: &str =
    "https://github.com/reliaburger/reliaburger/releases/download/os-channel/os-channel.json";

/// Which files of a release `download` fetches.
pub fn wanted(entries: &[SumsEntry], version: &str, everything: bool) -> Vec<String> {
    entries
        .iter()
        .map(|entry| entry.name.clone())
        .filter(|name| {
            everything
                || *name == format!("reliaburger-os_{version}.raw.zst")
                || *name == format!("reliaburger-os-installer_{version}.efi")
                || name.starts_with("ipxe-")
                || name == "boot.ipxe"
        })
        .collect()
}

/// Where a release file goes under `<dir>/<arch>/`: iPXE and its script in
/// `netboot/`, the rest beside `SHA256SUMS`.
pub fn local_path(arch_dir: &Path, name: &str) -> PathBuf {
    if name.starts_with("ipxe-") || name == "boot.ipxe" {
        arch_dir.join("netboot").join(name)
    } else {
        arch_dir.join(name)
    }
}

/// The release URL beside the channel: `…/releases/download/<tag>/<file>`.
pub fn asset_url(channel_url: &str, tag: &str, file: &str) -> Result<String, RelishError> {
    let base = channel_url
        .rsplit_once("/os-channel/")
        .map(|(base, _)| base)
        .ok_or_else(|| {
            failed("the channel URL isn't …/releases/download/os-channel/os-channel.json")
        })?;
    Ok(format!("{base}/{tag}/{file}"))
}

/// `relish image download`: fetch and verify the newest build for `arch`.
pub async fn download(
    channel_url: &str,
    arch: &str,
    directory: &Path,
    everything: bool,
) -> Result<(), RelishError> {
    let keys = crate::upgrade::keys::release_keys(&Default::default())
        .map_err(|e| failed(&e.to_string()))?;
    let client = reqwest::Client::builder()
        .user_agent("relish")
        .build()
        .map_err(|e| failed(&e.to_string()))?;
    let channel_bytes = fetch(&client, channel_url).await?;
    let signature = fetch(&client, &format!("{channel_url}.sig")).await?;
    let channel = OsChannel::verified(&channel_bytes, &signature, &keys)
        .map_err(|e| failed(&e.to_string()))?;
    let entry = channel
        .architectures
        .get(arch)
        .ok_or_else(|| failed(&format!("OS {} has no {arch} build", channel.version)))?;
    let sums = fetch(&client, &asset_url(channel_url, &entry.tag, &entry.sums)?).await?;
    let sums_signature = fetch(
        &client,
        &asset_url(channel_url, &entry.tag, &format!("{}.sig", entry.sums))?,
    )
    .await?;
    let entries = channel
        .verified_sums(arch, &sums)
        .map_err(|e| failed(&e.to_string()))?;

    let arch_dir = directory.join(arch);
    std::fs::create_dir_all(arch_dir.join("netboot"))?;
    std::fs::write(arch_dir.join(&entry.sums), &sums)?;
    std::fs::write(
        arch_dir.join(format!("{}.sig", entry.sums)),
        &sums_signature,
    )?;
    println!(
        "OS {} for {arch} ({}), checked against the release key",
        channel.version, entry.tag
    );
    for name in wanted(&entries, &channel.version, everything) {
        let expected = entries
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.sha256.clone())
            .unwrap_or_default();
        let path = local_path(&arch_dir, &name);
        if path.is_file() && sha256_file(&path)? == expected {
            println!("  {name}: already here");
            continue;
        }
        let url = asset_url(channel_url, &entry.tag, &name)?;
        let bytes = stream_to(&client, &url, &path).await?;
        let actual = sha256_file(&path)?;
        crate::os::check_asset(&entries, &entry.sums, &name, &actual).map_err(|e| {
            let _ = std::fs::remove_file(&path);
            failed(&e.to_string())
        })?;
        println!("  {name}: {} MB, checked", bytes / 1_000_000);
    }
    println!("Saved under {}", arch_dir.display());
    Ok(())
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, RelishError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| failed(&format!("{url}: {e}")))?;
    if !response.status().is_success() {
        return Err(failed(&format!("{url}: {}", response.status())));
    }
    Ok(response
        .bytes()
        .await
        .map_err(|e| failed(&format!("{url}: {e}")))?
        .to_vec())
}

/// Stream `url` to a temporary file beside `path`, then rename it.
async fn stream_to(client: &reqwest::Client, url: &str, path: &Path) -> Result<u64, RelishError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| failed(&format!("{url}: {e}")))?;
    if !response.status().is_success() {
        return Err(failed(&format!("{url}: {}", response.status())));
    }
    let temporary = path.with_extension("part");
    let mut file = std::fs::File::create(&temporary)?;
    let mut stream = response.bytes_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| failed(&format!("{url}: {e}")))?;
        file.write_all(&chunk)?;
        total += chunk.len() as u64;
    }
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    Ok(total)
}

fn sha256_file(path: &Path) -> Result<String, RelishError> {
    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(path)?;
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// `relish image write`: copy a disk image (decompressing `.zst`) onto a
/// device, such as a disk you install the appliance on directly or a stick
/// to run it from. Everything on the device is lost.
pub fn write(image: &Path, device: &Path, confirmed: bool) -> Result<u64, RelishError> {
    if !confirmed {
        return Err(failed(&format!(
            "this erases everything on {}; run again with --yes if that's what you want",
            device.display()
        )));
    }
    let source = std::fs::File::open(image)?;
    let mut reader: Box<dyn Read> = if image.extension().is_some_and(|e| e == "zst") {
        Box::new(
            ruzstd::decoding::StreamingDecoder::new(std::io::BufReader::new(source))
                .map_err(|e| failed(&format!("{}: {e}", image.display())))?,
        )
    } else {
        Box::new(std::io::BufReader::new(source))
    };
    let mut target = std::fs::OpenOptions::new().write(true).open(device)?;
    let mut buffer = vec![0u8; 4 << 20];
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        target.write_all(&buffer[..read])?;
        total += read as u64;
    }
    target.sync_all()?;
    Ok(total)
}

fn failed(message: &str) -> RelishError {
    RelishError::InitFailed(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> SumsEntry {
        SumsEntry {
            sha256: "a".repeat(64),
            name: name.into(),
        }
    }

    #[test]
    fn an_install_needs_the_disk_the_installer_and_ipxe_but_not_the_update_parts() {
        let entries = [
            entry("reliaburger-os_2026.41.0.raw.zst"),
            entry("reliaburger-os_2026.41.0.efi"),
            entry("reliaburger-os_2026.41.0.usr.ab12.raw.zst"),
            entry("reliaburger-os_2026.41.0.usr-verity.cd34.raw.zst"),
            entry("reliaburger-os-installer_2026.41.0.efi"),
            entry("ipxe-snp-x86_64.efi"),
            entry("ipxe-x86_64.efi"),
            entry("boot.ipxe"),
        ];
        assert_eq!(
            wanted(&entries, "2026.41.0", false),
            [
                "reliaburger-os_2026.41.0.raw.zst",
                "reliaburger-os-installer_2026.41.0.efi",
                "ipxe-snp-x86_64.efi",
                "ipxe-x86_64.efi",
                "boot.ipxe"
            ]
        );
        assert_eq!(wanted(&entries, "2026.41.0", true).len(), entries.len());
    }

    #[test]
    fn files_land_where_a_netboot_server_looks_for_them() {
        let dir = Path::new("/d/x86_64");
        assert_eq!(
            local_path(dir, "boot.ipxe"),
            Path::new("/d/x86_64/netboot/boot.ipxe")
        );
        assert_eq!(
            local_path(dir, "ipxe-snp-x86_64.efi"),
            Path::new("/d/x86_64/netboot/ipxe-snp-x86_64.efi")
        );
        assert_eq!(
            local_path(dir, "reliaburger-os_1.raw.zst"),
            Path::new("/d/x86_64/reliaburger-os_1.raw.zst")
        );
    }

    #[test]
    fn assets_are_fetched_from_the_tag_beside_the_channel() {
        assert_eq!(
            asset_url(CHANNEL_URL, "os-2026.41.0-x86_64", "boot.ipxe").unwrap(),
            "https://github.com/reliaburger/reliaburger/releases/download/os-2026.41.0-x86_64/boot.ipxe"
        );
        assert!(asset_url("https://example.com/channel.json", "t", "f").is_err());
    }

    #[test]
    fn write_decompresses_zst_and_refuses_without_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let raw: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let compressed = ruzstd::encoding::compress_to_vec(
            &raw[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let image = dir.path().join("disk.raw.zst");
        std::fs::write(&image, compressed).unwrap();
        let device = dir.path().join("device");
        std::fs::write(&device, b"").unwrap();
        assert!(write(&image, &device, false).is_err());
        assert_eq!(std::fs::read(&device).unwrap(), b"");
        assert_eq!(write(&image, &device, true).unwrap(), raw.len() as u64);
        assert_eq!(std::fs::read(&device).unwrap(), raw);
    }
}
