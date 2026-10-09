//! `relish image download` and `relish image write`: published appliance
//! OS builds on the operator's machine (docs/plans/2026-10-01-plan-
//! appliance-product.md, W3).
//!
//! `download` trusts nothing it hasn't checked: the channel's signature
//! against the release keys this relish carries (or, with `--key`, only
//! the operator's lab key: [`ChannelTrust`]), each architecture's
//! `SHA256SUMS` against the channel, and every file against `SHA256SUMS`
//! as it streams to disk (`crate::os`). The layout it writes is what a
//! netboot server serves: `<dir>/<arch>/` with iPXE in `netboot/`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};

use crate::os::{OsChannel, OsError, SumsEntry};
use crate::relish::RelishError;
use crate::relish::netboot::Arch;
use crate::upgrade::signing::PublicKey;

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

/// An architecture `--arch` names, by its directory name (`x86_64` or
/// `aarch64`).
pub fn parse_arch(name: &str) -> Result<Arch, String> {
    Arch::ALL
        .into_iter()
        .find(|arch| arch.directory() == name)
        .ok_or_else(|| format!("unknown architecture {name} ({})", known_names()))
}

/// Which architectures to fetch from a release that offers `offered`:
/// every one relish knows unless `requested` narrows it. The operator's
/// machine is rarely the architecture of the machines it serves (an arm64
/// laptop netbooting x86_64 boxes), so the default doesn't guess.
pub fn select_architectures(
    offered: &[String],
    requested: &[Arch],
    version: &str,
) -> Result<Vec<Arch>, RelishError> {
    let is_offered = |arch: Arch| offered.iter().any(|name| name == arch.directory());
    if let Some(missing) = requested.iter().find(|arch| !is_offered(**arch)) {
        return Err(failed(&format!(
            "OS {version} has no {} build",
            missing.directory()
        )));
    }
    let chosen: Vec<Arch> = Arch::ALL
        .into_iter()
        .filter(|arch| is_offered(*arch) && (requested.is_empty() || requested.contains(arch)))
        .collect();
    if chosen.is_empty() {
        return Err(failed(&format!(
            "OS {version} has no {} build",
            known_names()
        )));
    }
    Ok(chosen)
}

/// The last line `download` prints: which architectures it saved, and where.
pub fn saved_summary(directory: &Path, saved: &[Arch]) -> String {
    let names: Vec<&str> = saved.iter().map(|arch| arch.directory()).collect();
    let paths: Vec<String> = saved
        .iter()
        .map(|arch| format!("{}/", directory.join(arch.directory()).display()))
        .collect();
    format!(
        "Saved {} under {} ({})",
        names.join(" and "),
        directory.display(),
        paths.join(", ")
    )
}

/// "x86_64 or aarch64".
fn known_names() -> String {
    let names: Vec<&str> = Arch::ALL.iter().map(|arch| arch.directory()).collect();
    names.join(" or ")
}

/// The keys an OS channel's signature is checked against.
///
/// By default, the release keys this relish carries. `--key <PEM>` swaps
/// them for one Ed25519 public key, such as the throwaway key a CI lab
/// build signs its lab channel with. It replaces rather than adds, so a
/// lab run reads only its own build's channel, and every command that
/// uses it says so on stderr ([`ChannelTrust::warning`]).
#[derive(Debug, Clone)]
pub struct ChannelTrust {
    keys: Vec<PublicKey>,
    /// The `--key` file, when there is one.
    operator_key: Option<PathBuf>,
}

impl ChannelTrust {
    /// The release keys, or only the key in `key_file` when it's given.
    pub fn from_key_file(key_file: Option<&Path>) -> Result<Self, RelishError> {
        let Some(path) = key_file else {
            let keys = crate::upgrade::keys::release_keys(&Default::default())
                .map_err(|e| failed(&e.to_string()))?;
            return Ok(Self {
                keys,
                operator_key: None,
            });
        };
        let pem = std::fs::read_to_string(path)
            .map_err(|e| failed(&format!("--key {}: {e}", path.display())))?;
        let key = crate::upgrade::signing::parse_pem_public_key(&pem).map_err(|e| {
            failed(&format!(
                "--key {} isn't an Ed25519 public key in PEM ({e})",
                path.display()
            ))
        })?;
        Ok(Self {
            keys: vec![key],
            operator_key: Some(path.to_path_buf()),
        })
    }

    /// What to print on stderr before trusting anything: `None` for the
    /// release keys.
    pub fn warning(&self) -> Option<String> {
        self.operator_key.as_ref().map(|path| {
            format!(
                "warning: checking the OS channel against {} instead of the release keys; \
                 use it only for builds you made yourself",
                path.display()
            )
        })
    }

    /// Which key vouched for a channel, for the line that reports it.
    pub fn checked_against(&self) -> String {
        match &self.operator_key {
            None => "the release key".to_string(),
            Some(path) => format!("the key in {}, not a release key", path.display()),
        }
    }

    /// Parse `bytes` only if `signature` is one of these keys' over them.
    pub fn verify(&self, bytes: &[u8], signature: &[u8]) -> Result<OsChannel, RelishError> {
        OsChannel::verified(bytes, signature, &self.keys).map_err(|error| {
            match (&error, &self.operator_key) {
                (OsError::ChannelSignature, Some(path)) => failed(&format!(
                    "the OS channel's signature doesn't match the key in {}",
                    path.display()
                )),
                (OsError::ChannelSignature, None) => failed(&format!(
                    "{error} (a CI lab build's channel? pass --key with that build's signing key)"
                )),
                _ => failed(&error.to_string()),
            }
        })
    }
}

/// `relish image download`: fetch and verify the newest build for each
/// architecture in `requested`, or for every one the release offers when
/// `requested` is empty.
pub async fn download(
    channel_url: &str,
    requested: &[Arch],
    directory: &Path,
    everything: bool,
    trust: &ChannelTrust,
) -> Result<(), RelishError> {
    if let Some(warning) = trust.warning() {
        eprintln!("relish image download: {warning}");
    }
    let client = reqwest::Client::builder()
        .user_agent("relish")
        .build()
        .map_err(|e| failed(&e.to_string()))?;
    let channel_bytes = fetch(&client, channel_url).await?;
    let signature = fetch(&client, &format!("{channel_url}.sig")).await?;
    let channel = trust.verify(&channel_bytes, &signature)?;
    let offered: Vec<String> = channel.architectures.keys().cloned().collect();
    let chosen = select_architectures(&offered, requested, &channel.version)?;
    for arch in &chosen {
        download_arch(
            &client,
            channel_url,
            &channel,
            *arch,
            directory,
            everything,
            trust,
        )
        .await?;
    }
    println!("{}", saved_summary(directory, &chosen));
    Ok(())
}

/// Fetch and verify one architecture's build into `<directory>/<arch>/`.
async fn download_arch(
    client: &reqwest::Client,
    channel_url: &str,
    channel: &OsChannel,
    arch: Arch,
    directory: &Path,
    everything: bool,
    trust: &ChannelTrust,
) -> Result<(), RelishError> {
    let arch = arch.directory();
    let entry = channel
        .architectures
        .get(arch)
        .ok_or_else(|| failed(&format!("OS {} has no {arch} build", channel.version)))?;
    let sums = fetch(client, &asset_url(channel_url, &entry.tag, &entry.sums)?).await?;
    let sums_signature = fetch(
        client,
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
        "OS {} for {arch} ({}), checked against {}",
        channel.version,
        entry.tag,
        trust.checked_against()
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
        let bytes = stream_to(client, &url, &path).await?;
        let actual = sha256_file(&path)?;
        crate::os::check_asset(&entries, &entry.sums, &name, &actual).map_err(|e| {
            let _ = std::fs::remove_file(&path);
            failed(&e.to_string())
        })?;
        println!("  {name}: {} MB, checked", bytes / 1_000_000);
    }
    Ok(())
}

pub(crate) async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, RelishError> {
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

/// The hex SHA-256 of a file, read in 1 MiB pieces (disk images are
/// bigger than some machines' memory).
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
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

    fn offered(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn by_default_every_architecture_the_release_offers_is_fetched() {
        // An arm64 Mac serving x86_64 machines must not get only its own.
        assert_eq!(
            select_architectures(&offered(&["aarch64", "x86_64"]), &[], "2026.41.0").unwrap(),
            [Arch::X86_64, Arch::Arm64]
        );
        assert_eq!(
            select_architectures(&offered(&["x86_64"]), &[], "2026.41.0").unwrap(),
            [Arch::X86_64]
        );
    }

    #[test]
    fn arch_narrows_the_download_to_the_named_architectures() {
        let both = offered(&["aarch64", "x86_64"]);
        assert_eq!(
            select_architectures(&both, &[Arch::Arm64], "2026.41.0").unwrap(),
            [Arch::Arm64]
        );
        assert_eq!(
            select_architectures(
                &both,
                &[Arch::Arm64, Arch::X86_64, Arch::Arm64],
                "2026.41.0"
            )
            .unwrap(),
            [Arch::X86_64, Arch::Arm64]
        );
    }

    #[test]
    fn an_architecture_the_release_lacks_is_refused() {
        let error = select_architectures(&offered(&["x86_64"]), &[Arch::Arm64], "2026.41.0")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("OS 2026.41.0 has no aarch64 build"),
            "{error}"
        );
        let error = select_architectures(&offered(&["riscv64"]), &[], "2026.41.0")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("OS 2026.41.0 has no x86_64 or aarch64 build"),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_architecture_is_refused_by_name() {
        assert_eq!(parse_arch("x86_64").unwrap(), Arch::X86_64);
        assert_eq!(parse_arch("aarch64").unwrap(), Arch::Arm64);
        assert_eq!(
            parse_arch("riscv64").unwrap_err(),
            "unknown architecture riscv64 (x86_64 or aarch64)"
        );
        assert!(parse_arch("arm64").is_err());
    }

    #[test]
    fn the_summary_names_each_architecture_saved() {
        assert_eq!(
            saved_summary(Path::new("os"), &[Arch::X86_64, Arch::Arm64]),
            "Saved x86_64 and aarch64 under os (os/x86_64/, os/aarch64/)"
        );
        assert_eq!(
            saved_summary(Path::new("os"), &[Arch::X86_64]),
            "Saved x86_64 under os (os/x86_64/)"
        );
    }

    const LAB_CHANNEL: &[u8] = include_bytes!("../os/testdata/lab/os-channel.json");
    const LAB_SIGNATURE: &[u8] = include_bytes!("../os/testdata/lab/os-channel.json.sig");
    const LAB_KEY_PEM: &str = include_str!("../os/testdata/lab/lab-signing-key.pub.pem");

    #[test]
    fn by_default_a_channel_is_checked_against_the_release_keys_only() {
        let trust = ChannelTrust::from_key_file(None).unwrap();
        assert_eq!(
            trust.keys,
            crate::upgrade::keys::release_keys(&Default::default()).unwrap()
        );
        assert_eq!(trust.warning(), None);
        assert_eq!(trust.checked_against(), "the release key");
    }

    #[test]
    fn without_key_a_lab_channel_is_refused_with_a_pointer_to_key() {
        let trust = ChannelTrust::from_key_file(None).unwrap();
        let error = trust
            .verify(LAB_CHANNEL, LAB_SIGNATURE)
            .unwrap_err()
            .to_string();
        assert!(error.contains("doesn't match a release key"), "{error}");
        assert!(error.contains("--key"), "{error}");
    }

    #[test]
    fn with_the_lab_builds_key_its_channel_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab-signing-key.pub.pem");
        std::fs::write(&path, LAB_KEY_PEM).unwrap();
        let trust = ChannelTrust::from_key_file(Some(&path)).unwrap();
        let channel = trust.verify(LAB_CHANNEL, LAB_SIGNATURE).unwrap();
        assert_eq!(channel.version, "2026.41.8");
        assert_eq!(
            trust.checked_against(),
            format!("the key in {}, not a release key", path.display())
        );
    }

    #[test]
    fn key_replaces_the_release_keys_rather_than_adding_to_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab.pem");
        std::fs::write(&path, LAB_KEY_PEM).unwrap();
        let trust = ChannelTrust::from_key_file(Some(&path)).unwrap();
        let lab_key = crate::upgrade::signing::parse_pem_public_key(LAB_KEY_PEM).unwrap();
        assert_eq!(trust.keys, [lab_key]);
    }

    #[test]
    fn with_key_a_channel_signed_by_any_other_key_is_refused_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab.pem");
        // Not the key that signed the lab channel.
        std::fs::write(
            &path,
            "-----BEGIN PUBLIC KEY-----\n\
             MCowBQYDK2VwAyEAi3zTXySVFXL+z98nJjP9w9GZqgBsxFYI0PChdhNgzRc=\n\
             -----END PUBLIC KEY-----\n",
        )
        .unwrap();
        let trust = ChannelTrust::from_key_file(Some(&path)).unwrap();
        let error = trust
            .verify(LAB_CHANNEL, LAB_SIGNATURE)
            .unwrap_err()
            .to_string();
        assert!(
            error.ends_with(&format!(
                "the OS channel's signature doesn't match the key in {}",
                path.display()
            )),
            "{error}"
        );
    }

    #[test]
    fn using_key_is_announced_with_a_warning_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab.pem");
        std::fs::write(&path, LAB_KEY_PEM).unwrap();
        let trust = ChannelTrust::from_key_file(Some(&path)).unwrap();
        assert_eq!(
            trust.warning().unwrap(),
            format!(
                "warning: checking the OS channel against {} instead of the release keys; \
                 use it only for builds you made yourself",
                path.display()
            )
        );
    }

    #[test]
    fn a_key_file_that_is_missing_or_not_an_ed25519_public_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.pem");
        let error = ChannelTrust::from_key_file(Some(&missing))
            .unwrap_err()
            .to_string();
        assert!(error.contains(&missing.display().to_string()), "{error}");
        let private = dir.path().join("private.pem");
        std::fs::write(&private, LAB_KEY_PEM.replace("PUBLIC KEY", "PRIVATE KEY")).unwrap();
        let error = ChannelTrust::from_key_file(Some(&private))
            .unwrap_err()
            .to_string();
        assert!(error.contains(&private.display().to_string()), "{error}");
        assert!(error.contains("Ed25519 public key"), "{error}");
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
