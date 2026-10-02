//! The files `relish netboot` serves, checked before it serves any.
//!
//! `relish image download` writes `<dir>/<arch>/` for each architecture:
//! the release's `SHA256SUMS` and its raw Ed25519 signature, the installer
//! and the disk image beside them, and iPXE in `netboot/`. A CI lab build
//! has the same layout. At start-up [`load`] checks the signature against
//! the trusted keys, then the SHA-256 of every listed file that's present.
//! Only files that pass are served, and a file that fails stops the start:
//! a netboot server that hands out something unverified would install it
//! on every machine that asks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crate::os::{OsError, OsVersion, check_asset, signed_sums};
use crate::upgrade::signing::{PublicKey, sha256_hex};

use super::{Arch, NetbootError};

/// One file the HTTP server may send, as it was when it was checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedFile {
    /// Where it is on disk.
    pub path: PathBuf,
    /// Its size when it was checked.
    pub size: u64,
    /// Its modification time when it was checked.
    pub modified: Option<SystemTime>,
}

impl ServedFile {
    fn checked(path: PathBuf) -> Result<Self, NetbootError> {
        let metadata = std::fs::metadata(&path)
            .map_err(|e| NetbootError::io(path.display().to_string(), e))?;
        Ok(ServedFile {
            size: metadata.len(),
            modified: metadata.modified().ok(),
            path,
        })
    }

    /// True while the file on disk still has the size and modification
    /// time it had when it was checked. Anything else means someone
    /// replaced it, and it must not be served unchecked.
    pub fn unchanged(&self) -> bool {
        std::fs::metadata(&self.path)
            .is_ok_and(|m| m.len() == self.size && m.modified().ok() == self.modified)
    }
}

/// One architecture's checked release.
#[derive(Debug, Clone)]
pub struct ArchArtefacts {
    /// The OS version its `SHA256SUMS` belongs to.
    pub version: OsVersion,
    /// Every file that may be served, by its name in `SHA256SUMS` (plus
    /// `SHA256SUMS` itself and its `.sig`).
    pub files: BTreeMap<String, ServedFile>,
    /// The SNP iPXE build, held in memory so TFTP sends exactly the bytes
    /// that were checked.
    pub ipxe: Arc<[u8]>,
}

impl ArchArtefacts {
    /// The installer UKI's name in this release.
    pub fn installer_name(&self) -> String {
        format!("reliaburger-os-installer_{}.efi", self.version)
    }

    /// The installer UKI, which iPXE fetches as `installer.efi`.
    pub fn installer(&self) -> Option<&ServedFile> {
        self.files.get(&self.installer_name())
    }
}

/// Every architecture found under the served directory, checked.
#[derive(Debug, Clone, Default)]
pub struct Artefacts {
    /// The checked releases, by architecture.
    pub architectures: BTreeMap<Arch, ArchArtefacts>,
}

/// Check what `directory` holds against `keys` and return what may be
/// served. `progress` hears about each large file as it's hashed.
pub fn load(
    directory: &Path,
    keys: &[PublicKey],
    mut progress: impl FnMut(&str),
) -> Result<Artefacts, NetbootError> {
    let mut artefacts = Artefacts::default();
    for arch in Arch::ALL {
        let arch_dir = directory.join(arch.directory());
        if arch_dir.is_dir() {
            let checked = load_arch(arch, &arch_dir, keys, &mut progress)?;
            artefacts.architectures.insert(arch, checked);
        }
    }
    if artefacts.architectures.is_empty() {
        return Err(NetbootError::NothingToServe {
            directory: directory.to_path_buf(),
        });
    }
    Ok(artefacts)
}

fn load_arch(
    arch: Arch,
    arch_dir: &Path,
    keys: &[PublicKey],
    progress: &mut impl FnMut(&str),
) -> Result<ArchArtefacts, NetbootError> {
    let version = newest_sums(arch_dir)?;
    let sums_name = format!("reliaburger-os_{version}.SHA256SUMS");
    let signature_name = format!("{sums_name}.sig");
    let sums = read(&arch_dir.join(&sums_name))?;
    let signature = read(&arch_dir.join(&signature_name))?;
    let entries = signed_sums(&sums_name, &sums, &signature, keys)?;

    let snp = arch.snp_file();
    let required = [
        format!("reliaburger-os-installer_{version}.efi"),
        format!("reliaburger-os_{version}.raw.zst"),
        snp.clone(),
    ];
    let mut files = BTreeMap::new();
    let mut ipxe: Option<Arc<[u8]>> = None;
    for entry in &entries {
        let path = crate::relish::image::local_path(arch_dir, &entry.name);
        if !path.is_file() {
            if required.contains(&entry.name) {
                return Err(NetbootError::Missing { file: path });
            }
            continue;
        }
        let digest = if entry.name == snp {
            let bytes = read(&path)?;
            let digest = sha256_hex(&bytes);
            ipxe = Some(bytes.into());
            digest
        } else {
            progress(&format!("checking {}", path.display()));
            crate::relish::image::sha256_file(&path)
                .map_err(|e| NetbootError::io(path.display().to_string(), e))?
        };
        check_asset(&entries, &sums_name, &entry.name, &digest)?;
        files.insert(entry.name.clone(), ServedFile::checked(path)?);
    }
    for name in &required {
        if !files.contains_key(name) {
            return Err(OsError::NotListed {
                asset: name.clone(),
                sums: sums_name.clone(),
            }
            .into());
        }
    }
    for name in [sums_name, signature_name] {
        let served = ServedFile::checked(arch_dir.join(&name))?;
        files.insert(name, served);
    }
    let ipxe = ipxe.ok_or_else(|| NetbootError::Missing {
        file: arch_dir.join("netboot").join(&snp),
    })?;
    Ok(ArchArtefacts {
        version,
        files,
        ipxe,
    })
}

/// The newest version with a `SHA256SUMS` in `arch_dir`.
fn newest_sums(arch_dir: &Path) -> Result<OsVersion, NetbootError> {
    let entries = std::fs::read_dir(arch_dir)
        .map_err(|e| NetbootError::io(arch_dir.display().to_string(), e))?;
    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let version = name
                .strip_prefix("reliaburger-os_")?
                .strip_suffix(".SHA256SUMS")?;
            version.parse::<OsVersion>().ok()
        })
        .max()
        .ok_or_else(|| NetbootError::NoSums {
            directory: arch_dir.to_path_buf(),
        })
}

fn read(path: &Path) -> Result<Vec<u8>, NetbootError> {
    std::fs::read(path).map_err(|e| NetbootError::io(path.display().to_string(), e))
}

/// Parse an Ed25519 public key in PEM, as `openssl pkey -pubout` writes
/// it (a lab build's `spike-signing-key.pub.pem`).
pub fn parse_pem_public_key(text: &str) -> Result<PublicKey, NetbootError> {
    crate::upgrade::signing::parse_pem_public_key(text).map_err(NetbootError::InvalidKey)
}

/// A served directory, signed with a throwaway key, for tests here and in
/// the HTTP and TFTP modules.
#[cfg(test)]
pub(crate) mod fixture {
    use std::path::Path;

    use base64::Engine as _;

    use crate::upgrade::signing::{PublicKey, generate_keypair, sha256_hex, sign};

    /// The fixture's version.
    pub const VERSION: &str = "2026.41.0";

    /// Write `<dir>/<arch_dir>/` like `relish image download` does, with
    /// `files` (name, contents) listed in a signed `SHA256SUMS`, and return
    /// the signing key.
    pub fn write(dir: &Path, arch_dir: &str, files: &[(&str, &[u8])]) -> (Vec<u8>, PublicKey) {
        let (pkcs8, public) = generate_keypair().unwrap();
        write_signed(dir, arch_dir, files, &pkcs8);
        (pkcs8, public)
    }

    /// As [`write`], signed with `pkcs8`.
    pub fn write_signed(dir: &Path, arch_dir: &str, files: &[(&str, &[u8])], pkcs8: &[u8]) {
        let root = dir.join(arch_dir);
        std::fs::create_dir_all(root.join("netboot")).unwrap();
        let mut sums = String::new();
        for (name, contents) in files {
            let path = crate::relish::image::local_path(&root, name);
            std::fs::write(path, contents).unwrap();
            sums.push_str(&format!("{}  {name}\n", sha256_hex(contents)));
        }
        let sums_name = format!("reliaburger-os_{VERSION}.SHA256SUMS");
        std::fs::write(root.join(&sums_name), &sums).unwrap();
        let signature = base64::engine::general_purpose::STANDARD
            .decode(sign(pkcs8, sums.as_bytes()).unwrap())
            .unwrap();
        std::fs::write(root.join(format!("{sums_name}.sig")), signature).unwrap();
    }

    /// The files an install needs, for `ipxe_name` (x86_64 or arm64).
    pub fn release(ipxe_name: &str) -> Vec<(String, Vec<u8>)> {
        vec![
            (
                format!("reliaburger-os_{VERSION}.raw.zst"),
                b"disk image".to_vec(),
            ),
            (
                format!("reliaburger-os-installer_{VERSION}.efi"),
                b"installer".to_vec(),
            ),
            (format!("ipxe-snp-{ipxe_name}.efi"), vec![7u8; 3000]),
            (
                format!("ipxe-{ipxe_name}.efi"),
                b"ipxe with drivers".to_vec(),
            ),
            ("boot.ipxe".to_string(), b"#!ipxe\n".to_vec()),
        ]
    }

    /// Borrow [`release`]'s files the way [`write`] takes them.
    pub fn borrowed(files: &[(String, Vec<u8>)]) -> Vec<(&str, &[u8])> {
        files
            .iter()
            .map(|(name, contents)| (name.as_str(), contents.as_slice()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{VERSION, borrowed, release, write, write_signed};
    use super::*;
    use crate::upgrade::signing::generate_keypair;

    fn silent(_: &str) {}

    #[test]
    fn a_signed_release_is_served_with_every_listed_file() {
        let dir = tempfile::tempdir().unwrap();
        let files = release("x86_64");
        let (_, key) = write(dir.path(), "x86_64", &borrowed(&files));
        let artefacts = load(dir.path(), &[key], silent).unwrap();
        let x86 = &artefacts.architectures[&Arch::X86_64];
        assert_eq!(x86.version.to_string(), VERSION);
        assert_eq!(&x86.ipxe[..], &[7u8; 3000][..]);
        assert!(
            x86.installer()
                .unwrap()
                .path
                .ends_with(x86.installer_name())
        );
        assert!(x86.files["boot.ipxe"].path.ends_with("netboot/boot.ipxe"));
        assert!(
            x86.files
                .contains_key(&format!("reliaburger-os_{VERSION}.SHA256SUMS.sig"))
        );
        assert!(!artefacts.architectures.contains_key(&Arch::Arm64));
    }

    #[test]
    fn both_architectures_load_from_their_own_directories() {
        let dir = tempfile::tempdir().unwrap();
        let (pkcs8, key) = generate_keypair().unwrap();
        write_signed(dir.path(), "x86_64", &borrowed(&release("x86_64")), &pkcs8);
        write_signed(dir.path(), "aarch64", &borrowed(&release("arm64")), &pkcs8);
        let artefacts = load(dir.path(), &[key], silent).unwrap();
        assert_eq!(artefacts.architectures.len(), 2);
        assert!(
            artefacts.architectures[&Arch::Arm64]
                .files
                .contains_key("ipxe-snp-arm64.efi")
        );
    }

    #[test]
    fn a_release_signed_by_another_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "x86_64", &borrowed(&release("x86_64")));
        let (_, other) = generate_keypair().unwrap();
        let error = load(dir.path(), &[other], silent).unwrap_err();
        assert!(matches!(
            error,
            NetbootError::Unverified(OsError::SumsSignature { .. })
        ));
    }

    #[test]
    fn a_file_changed_after_signing_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (_, key) = write(dir.path(), "x86_64", &borrowed(&release("x86_64")));
        std::fs::write(
            dir.path()
                .join(format!("x86_64/reliaburger-os_{VERSION}.raw.zst")),
            b"something else",
        )
        .unwrap();
        let error = load(dir.path(), &[key], silent).unwrap_err();
        assert!(matches!(
            error,
            NetbootError::Unverified(OsError::AssetDigest { .. })
        ));
    }

    #[test]
    fn a_missing_installer_is_refused_but_missing_update_pieces_are_fine() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = release("x86_64");
        files.push((format!("reliaburger-os_{VERSION}.efi"), b"uki".to_vec()));
        let (_, key) = write(dir.path(), "x86_64", &borrowed(&files));
        std::fs::remove_file(
            dir.path()
                .join(format!("x86_64/reliaburger-os_{VERSION}.efi")),
        )
        .unwrap();
        let artefacts = load(dir.path(), &[key], silent).unwrap();
        assert!(
            !artefacts.architectures[&Arch::X86_64]
                .files
                .contains_key(&format!("reliaburger-os_{VERSION}.efi"))
        );

        std::fs::remove_file(
            dir.path()
                .join(format!("x86_64/reliaburger-os-installer_{VERSION}.efi")),
        )
        .unwrap();
        assert!(matches!(
            load(dir.path(), &[key], silent),
            Err(NetbootError::Missing { .. })
        ));
    }

    #[test]
    fn a_release_whose_sums_omit_the_installer_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<_> = release("x86_64")
            .into_iter()
            .filter(|(name, _)| !name.contains("installer"))
            .collect();
        let (_, key) = write(dir.path(), "x86_64", &borrowed(&files));
        assert!(matches!(
            load(dir.path(), &[key], silent),
            Err(NetbootError::Unverified(OsError::NotListed { .. }))
        ));
    }

    #[test]
    fn the_newest_sums_wins_when_a_directory_holds_two_versions() {
        let dir = tempfile::tempdir().unwrap();
        let (_, key) = write(dir.path(), "x86_64", &borrowed(&release("x86_64")));
        std::fs::write(
            dir.path()
                .join("x86_64/reliaburger-os_2026.40.9.SHA256SUMS"),
            "stale",
        )
        .unwrap();
        let artefacts = load(dir.path(), &[key], silent).unwrap();
        assert_eq!(
            artefacts.architectures[&Arch::X86_64].version.to_string(),
            VERSION
        );
    }

    #[test]
    fn a_directory_without_architectures_or_sums_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            load(dir.path(), &[], silent),
            Err(NetbootError::NothingToServe { .. })
        ));
        std::fs::create_dir(dir.path().join("aarch64")).unwrap();
        assert!(matches!(
            load(dir.path(), &[], silent),
            Err(NetbootError::NoSums { .. })
        ));
    }

    #[test]
    fn a_served_file_notices_when_it_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"one").unwrap();
        let served = ServedFile::checked(path.clone()).unwrap();
        assert!(served.unchanged());
        std::fs::write(&path, b"three").unwrap();
        assert!(!served.unchanged());
    }

    /// Made with `openssl genpkey -algorithm ed25519 | openssl pkey -pubout`,
    /// as the appliance workflow makes its throwaway key.
    const OPENSSL_PEM: &str = "-----BEGIN PUBLIC KEY-----\n\
MCowBQYDK2VwAyEAi3zTXySVFXL+z98nJjP9w9GZqgBsxFYI0PChdhNgzRc=\n\
-----END PUBLIC KEY-----\n";

    #[test]
    fn an_openssl_public_key_pem_parses() {
        let key = parse_pem_public_key(OPENSSL_PEM).unwrap();
        assert_eq!(key[0], 0x8b);
        assert_eq!(key[31], 0x17);
    }

    #[test]
    fn other_pem_blocks_and_key_types_are_refused() {
        assert!(parse_pem_public_key("not pem").is_err());
        let private = OPENSSL_PEM.replace("PUBLIC KEY", "PRIVATE KEY");
        assert!(parse_pem_public_key(&private).is_err());
        // The right tag and length, but the Ed448 algorithm identifier.
        let ed448 = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VxAyEAGb9ECWmEzf6FQbrBZ9w7lshQhqowtrbLDFw4rXAxZuE=\n-----END PUBLIC KEY-----\n";
        assert!(parse_pem_public_key(ed448).is_err());
    }
}
