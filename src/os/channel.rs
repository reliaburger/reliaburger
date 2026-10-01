//! `os-channel.json`, its signature, and a release's `SHA256SUMS`.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::upgrade::signing::{PublicKey, sha256_hex, verify_detached};

/// Why an OS channel or release was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OsError {
    #[error("the OS channel's signature doesn't match a release key")]
    ChannelSignature,
    #[error("the OS channel isn't valid: {0}")]
    ChannelFormat(String),
    #[error("OS release {version} has no build for {arch}")]
    NoArchitecture { version: String, arch: String },
    #[error("{name} doesn't match the digest the OS channel names")]
    SumsDigest { name: String },
    #[error("{name} isn't a valid SHA256SUMS: {reason}")]
    SumsFormat { name: String, reason: String },
    #[error("{asset} isn't listed in {sums}")]
    NotListed { asset: String, sums: String },
    #[error("{asset} doesn't match its SHA-256 in {sums}")]
    AssetDigest { asset: String, sums: String },
    #[error("OS version must be YYYY.WW.N, not {0:?}")]
    Version(String),
}

/// An OS build's version: ISO year, ISO week, and the build within that
/// week (`2026.41.0`). Orders by those three numbers, so `2026.40.10` is
/// newer than `2026.40.9`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OsVersion {
    pub year: u16,
    pub week: u8,
    pub build: u32,
}

impl FromStr for OsVersion {
    type Err = OsError;

    fn from_str(input: &str) -> Result<Self, OsError> {
        let invalid = || OsError::Version(input.to_string());
        let mut parts = input.split('.');
        let (Some(year), Some(week), Some(build), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid());
        };
        if year.len() != 4 || week.len() != 2 {
            return Err(invalid());
        }
        let week: u8 = week.parse().map_err(|_| invalid())?;
        if !(1..=53).contains(&week) {
            return Err(invalid());
        }
        Ok(OsVersion {
            year: year.parse().map_err(|_| invalid())?,
            week,
            build: build.parse().map_err(|_| invalid())?,
        })
    }
}

impl fmt::Display for OsVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:02}.{}", self.year, self.week, self.build)
    }
}

/// One architecture's entry in the channel: its GitHub release, the
/// release's `SHA256SUMS` and that file's digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelArch {
    /// `os-<version>-<arch>`: one release per architecture, because both
    /// builds name their files the same.
    pub tag: String,
    pub sums: String,
    pub sums_sha256: String,
}

/// The newest OS release, as `os-channel.json` names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OsChannel {
    pub schema: u32,
    pub version: String,
    pub architectures: BTreeMap<String, ChannelArch>,
}

/// One line of a `SHA256SUMS`: a digest and a file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SumsEntry {
    pub sha256: String,
    pub name: String,
}

impl OsChannel {
    /// Parse `bytes` only if `signature` (raw, 64 bytes) is a release key's
    /// signature over exactly those bytes.
    pub fn verified(bytes: &[u8], signature: &[u8], keys: &[PublicKey]) -> Result<Self, OsError> {
        if !verify_detached(keys, bytes, signature) {
            return Err(OsError::ChannelSignature);
        }
        let channel: OsChannel =
            serde_json::from_slice(bytes).map_err(|e| OsError::ChannelFormat(e.to_string()))?;
        if channel.schema != 1 {
            return Err(OsError::ChannelFormat(format!(
                "unsupported schema {}",
                channel.schema
            )));
        }
        let version = channel.os_version()?;
        for (arch, entry) in &channel.architectures {
            if entry.tag != format!("os-{version}-{arch}") {
                return Err(OsError::ChannelFormat(format!(
                    "{arch}'s tag {} doesn't belong to version {version}",
                    entry.tag
                )));
            }
            if entry.sums != format!("reliaburger-os_{version}.SHA256SUMS") {
                return Err(OsError::ChannelFormat(format!(
                    "{arch} names {}, not version {version}'s SHA256SUMS",
                    entry.sums
                )));
            }
            if !is_sha256(&entry.sums_sha256) {
                return Err(OsError::ChannelFormat(format!(
                    "{arch} has an invalid digest"
                )));
            }
        }
        Ok(channel)
    }

    /// The channel's version, parsed.
    pub fn os_version(&self) -> Result<OsVersion, OsError> {
        self.version.parse()
    }

    /// Check `sums` (the downloaded `SHA256SUMS` for `arch`) against the
    /// digest the channel names, and return its entries.
    pub fn verified_sums(&self, arch: &str, sums: &[u8]) -> Result<Vec<SumsEntry>, OsError> {
        let entry = self
            .architectures
            .get(arch)
            .ok_or_else(|| OsError::NoArchitecture {
                version: self.version.clone(),
                arch: arch.to_string(),
            })?;
        if sha256_hex(sums) != entry.sums_sha256 {
            return Err(OsError::SumsDigest {
                name: entry.sums.clone(),
            });
        }
        parse_sums(&entry.sums, sums)
    }
}

/// Check one downloaded artefact against the `SHA256SUMS` entries.
pub fn check_asset(
    entries: &[SumsEntry],
    sums_name: &str,
    asset: &str,
    bytes_sha256: &str,
) -> Result<(), OsError> {
    let entry = entries
        .iter()
        .find(|e| e.name == asset)
        .ok_or_else(|| OsError::NotListed {
            asset: asset.to_string(),
            sums: sums_name.to_string(),
        })?;
    if !entry.sha256.eq_ignore_ascii_case(bytes_sha256) {
        return Err(OsError::AssetDigest {
            asset: asset.to_string(),
            sums: sums_name.to_string(),
        });
    }
    Ok(())
}

/// Parse `sha256sum` output: `<64 hex>  <name>` per line, `*` before the
/// name in binary mode. Names may not contain a slash: every artefact sits
/// beside its `SHA256SUMS`.
fn parse_sums(name: &str, bytes: &[u8]) -> Result<Vec<SumsEntry>, OsError> {
    let bad = |reason: String| OsError::SumsFormat {
        name: name.to_string(),
        reason,
    };
    let text = std::str::from_utf8(bytes).map_err(|_| bad("not UTF-8".to_string()))?;
    let mut entries = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let (digest, rest) = line
            .split_once(' ')
            .ok_or_else(|| bad(format!("line {} has no file name", number + 1)))?;
        let file = rest.trim_start_matches([' ', '*']);
        if !is_sha256(digest) || file.is_empty() || file.contains('/') {
            return Err(bad(format!("line {} isn't `<sha256>  <file>`", number + 1)));
        }
        entries.push(SumsEntry {
            sha256: digest.to_ascii_lowercase(),
            name: file.to_string(),
        });
    }
    if entries.is_empty() {
        return Err(bad("it lists no files".to_string()));
    }
    Ok(entries)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upgrade::signing::{generate_keypair, sign};
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;

    const SUMS: &[u8] =
        b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  reliaburger-os_2026.41.0.raw.zst\n\
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb *reliaburger-os_2026.41.0.efi\n";

    fn channel_bytes(version: &str, sums: &[u8]) -> Vec<u8> {
        format!(
            "{{\"architectures\":{{\"aarch64\":{{\"sums\":\"reliaburger-os_{version}.SHA256SUMS\",\"sums_sha256\":\"{}\",\"tag\":\"os-{version}-aarch64\"}}}},\"schema\":1,\"version\":\"{version}\"}}\n",
            sha256_hex(sums)
        )
        .into_bytes()
    }

    fn signed(bytes: &[u8]) -> (Vec<u8>, PublicKey) {
        let (pkcs8, public) = generate_keypair().unwrap();
        let signature = BASE64.decode(sign(&pkcs8, bytes).unwrap()).unwrap();
        (signature, public)
    }

    #[test]
    fn a_channel_signed_by_a_release_key_is_accepted() {
        let bytes = channel_bytes("2026.41.0", SUMS);
        let (signature, key) = signed(&bytes);
        let channel = OsChannel::verified(&bytes, &signature, &[key]).unwrap();
        assert_eq!(channel.os_version().unwrap(), "2026.41.0".parse().unwrap());
        assert_eq!(channel.architectures["aarch64"].tag, "os-2026.41.0-aarch64");
    }

    /// `scripts/release/os_release.py` signed `testdata/os-channel.json`
    /// with a throwaway key, so this checks the Python signer and the Rust
    /// verifier agree on the bytes.
    #[test]
    fn a_channel_from_the_release_script_verifies() {
        let key: PublicKey = BASE64
            .decode(include_str!("testdata/fixture-key.b64").trim())
            .unwrap()
            .try_into()
            .unwrap();
        let channel = OsChannel::verified(
            include_bytes!("testdata/os-channel.json"),
            include_bytes!("testdata/os-channel.json.sig"),
            &[key],
        )
        .unwrap();
        let entries = channel
            .verified_sums(
                "aarch64",
                include_bytes!("testdata/reliaburger-os_2026.41.0.SHA256SUMS"),
            )
            .unwrap();
        assert_eq!(entries[0].name, "reliaburger-os_2026.41.0.raw.zst");
    }

    #[test]
    fn a_channel_signed_by_another_key_is_refused() {
        let bytes = channel_bytes("2026.41.0", SUMS);
        let (signature, _) = signed(&bytes);
        let (_, other) = generate_keypair().unwrap();
        assert_eq!(
            OsChannel::verified(&bytes, &signature, &[other]),
            Err(OsError::ChannelSignature)
        );
    }

    #[test]
    fn a_channel_edited_after_signing_is_refused() {
        let bytes = channel_bytes("2026.41.0", SUMS);
        let (signature, key) = signed(&bytes);
        let edited = channel_bytes("2026.41.1", SUMS);
        assert_eq!(
            OsChannel::verified(&edited, &signature, &[key]),
            Err(OsError::ChannelSignature)
        );
    }

    #[test]
    fn a_signed_channel_whose_tag_or_sums_name_another_version_is_refused() {
        let text = String::from_utf8(channel_bytes("2026.41.0", SUMS))
            .unwrap()
            .replace(
                "\"tag\":\"os-2026.41.0-aarch64\"",
                "\"tag\":\"os-2026.40.0-aarch64\"",
            );
        let (signature, key) = signed(text.as_bytes());
        assert!(matches!(
            OsChannel::verified(text.as_bytes(), &signature, &[key]),
            Err(OsError::ChannelFormat(_))
        ));
    }

    #[test]
    fn sums_must_match_the_channel_digest() {
        let bytes = channel_bytes("2026.41.0", SUMS);
        let (signature, key) = signed(&bytes);
        let channel = OsChannel::verified(&bytes, &signature, &[key]).unwrap();
        let entries = channel.verified_sums("aarch64", SUMS).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].name, "reliaburger-os_2026.41.0.efi");
        let mut tampered = SUMS.to_vec();
        tampered[0] = b'c';
        assert!(matches!(
            channel.verified_sums("aarch64", &tampered),
            Err(OsError::SumsDigest { .. })
        ));
        assert!(matches!(
            channel.verified_sums("x86_64", SUMS),
            Err(OsError::NoArchitecture { .. })
        ));
    }

    #[test]
    fn assets_are_checked_against_their_listed_digest() {
        let entries = parse_sums("s", SUMS).unwrap();
        let good = "a".repeat(64);
        check_asset(&entries, "s", "reliaburger-os_2026.41.0.raw.zst", &good).unwrap();
        assert!(matches!(
            check_asset(
                &entries,
                "s",
                "reliaburger-os_2026.41.0.raw.zst",
                &"c".repeat(64)
            ),
            Err(OsError::AssetDigest { .. })
        ));
        assert!(matches!(
            check_asset(&entries, "s", "other.raw", &good),
            Err(OsError::NotListed { .. })
        ));
    }

    #[test]
    fn malformed_sums_are_refused() {
        for bad in [&b""[..], b"zz  file\n", b"aaaa\n", b"\xff\xfe"] {
            assert!(parse_sums("s", bad).is_err(), "{bad:?}");
        }
        let path = format!("{}  ../etc/passwd\n", "a".repeat(64));
        assert!(parse_sums("s", path.as_bytes()).is_err());
    }

    #[test]
    fn versions_order_numerically_and_round_trip() {
        let a: OsVersion = "2026.40.9".parse().unwrap();
        let b: OsVersion = "2026.40.10".parse().unwrap();
        let c: OsVersion = "2027.01.0".parse().unwrap();
        assert!(a < b && b < c);
        assert_eq!(b.to_string(), "2026.40.10");
        assert_eq!(c.to_string(), "2027.01.0");
        for bad in [
            "2026.4.0",
            "26.40.0",
            "2026.40",
            "2026.40.0.1",
            "2026.54.0",
            "v0.1.0",
        ] {
            assert!(bad.parse::<OsVersion>().is_err(), "{bad}");
        }
    }
}
