//! `relish netboot`: install appliances over the network from the
//! directory `relish image download` wrote (docs/plans/2026-10-01-plan-
//! appliance-product.md, W4).
//!
//! Three small servers share one verified set of files:
//! - [`dhcp`], a ProxyDHCP on UDP 67 and 4011. It never hands out
//!   addresses (the LAN's router keeps doing that); it only tells PXE
//!   firmware which boot file to fetch, and from where.
//! - [`tftp`], a read-only TFTP server on UDP 69 for iPXE and a two-line
//!   `boot.ipxe` that hands over to HTTP.
//! - [`http`], which serves the installer and the signed disk image, and
//!   writes each machine's real boot script: install, or `exit 1` for a
//!   machine that has installed already ([`installed`]).
//!
//! Nothing is served unless [`artefacts`] checked it against a signed
//! `SHA256SUMS` first. Each server's decisions are pure functions with
//! unit tests; the network loops around them are thin.

pub mod artefacts;
pub mod dhcp;
pub mod http;
pub mod installed;
pub mod interface;
mod server;
pub mod tftp;
pub mod wipe;

use std::fmt;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use crate::upgrade::signing::PublicKey;

pub use server::run;

/// Where the servers' one-line reports of each boot request go.
pub type Log = std::sync::Arc<dyn Fn(String) + Send + Sync>;

/// The port the HTTP server listens on unless `--http-port` says otherwise.
pub const DEFAULT_HTTP_PORT: u16 = 8080;

/// How long `relish netboot` runs unless `--for` says otherwise.
pub const DEFAULT_DURATION: Duration = Duration::from_secs(3600);

/// A CPU architecture the appliance is built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Arch {
    /// 64-bit x86 (Intel and AMD).
    X86_64,
    /// 64-bit Arm.
    Arm64,
}

impl Arch {
    /// Both architectures, in the order they're listed.
    pub const ALL: [Arch; 2] = [Arch::X86_64, Arch::Arm64];

    /// The directory `relish image download` writes this architecture to.
    pub fn directory(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Arm64 => "aarch64",
        }
    }

    /// iPXE's name for it (`${buildarch}`), which the HTTP paths use.
    pub fn ipxe_name(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Arm64 => "arm64",
        }
    }

    /// The architecture iPXE calls `name`, if it's one we build.
    pub fn from_ipxe_name(name: &str) -> Option<Arch> {
        Arch::ALL.into_iter().find(|arch| arch.ipxe_name() == name)
    }

    /// The name PXE firmware is told to fetch over TFTP: `ipxe-<a>.efi`.
    /// It's also the release's full-driver iPXE build's name, but TFTP
    /// serves whichever build `--ipxe` chose under it.
    pub fn boot_file(self) -> String {
        format!("ipxe-{}.efi", self.ipxe_name())
    }

    /// The release's iPXE build that uses the firmware's own network
    /// driver (SNP), the safer default.
    pub fn snp_file(self) -> String {
        format!("ipxe-snp-{}.efi", self.ipxe_name())
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.directory())
    }
}

/// Which of the release's two iPXE builds TFTP hands to PXE firmware
/// (`--ipxe`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IpxeBuild {
    /// `snp.efi`, which drives the NIC through the firmware's own network
    /// driver: what odd NICs (the Wyse's Realtek) need.
    #[default]
    Snp,
    /// `ipxe.efi`, with iPXE's own drivers: for firmware whose network
    /// stack is broken but whose NIC iPXE knows.
    Full,
}

impl IpxeBuild {
    /// The build's file name in the release, under `netboot/`.
    pub fn file(self, arch: Arch) -> String {
        match self {
            IpxeBuild::Snp => arch.snp_file(),
            IpxeBuild::Full => arch.boot_file(),
        }
    }
}

impl FromStr for IpxeBuild {
    type Err = NetbootError;

    fn from_str(input: &str) -> Result<Self, NetbootError> {
        match input {
            "snp" => Ok(IpxeBuild::Snp),
            "full" => Ok(IpxeBuild::Full),
            _ => Err(NetbootError::InvalidIpxe(input.to_string())),
        }
    }
}

impl fmt::Display for IpxeBuild {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            IpxeBuild::Snp => "snp",
            IpxeBuild::Full => "full",
        })
    }
}

/// An Ethernet MAC address, shown as `aa:bb:cc:dd:ee:ff`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MacAddress(pub [u8; 6]);

impl FromStr for MacAddress {
    type Err = NetbootError;

    /// Accepts `:` or `-` between the six bytes, in either case.
    fn from_str(input: &str) -> Result<Self, NetbootError> {
        let invalid = || NetbootError::InvalidMac(input.to_string());
        let parts: Vec<&str> = input.split([':', '-']).collect();
        if parts.len() != 6 {
            return Err(invalid());
        }
        let mut bytes = [0u8; 6];
        for (byte, part) in bytes.iter_mut().zip(&parts) {
            if part.len() != 2 {
                return Err(invalid());
            }
            *byte = u8::from_str_radix(part, 16).map_err(|_| invalid())?;
        }
        Ok(MacAddress(bytes))
    }
}

impl fmt::Display for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

/// Which local address the servers answer from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterfaceChoice {
    /// The interface the default route leaves through.
    DefaultRoute,
    /// A named interface (`--interface eth0`).
    Named(String),
    /// The interface holding this address (`--address 192.168.1.20`).
    Address(Ipv4Addr),
}

/// Everything `relish netboot` needs to start.
#[derive(Debug, Clone)]
pub struct NetbootOptions {
    /// The directory `relish image download --dir` wrote.
    pub directory: PathBuf,
    /// Where to answer from.
    pub interface: InterfaceChoice,
    /// The HTTP server's port.
    pub http_port: u16,
    /// When not empty, only these machines are answered.
    pub allowed: Vec<MacAddress>,
    /// How long to run before exiting.
    pub duration: Duration,
    /// The keys a `SHA256SUMS` signature must match.
    pub keys: Vec<PublicKey>,
    /// Install again on machines that installed already.
    pub reinstall: bool,
    /// Machines whose used disk is wiped without asking (`--wipe`).
    pub wipe: Vec<MacAddress>,
    /// The iPXE build PXE firmware gets.
    pub ipxe: IpxeBuild,
}

/// Why `relish netboot` refused to start or stopped.
#[derive(Debug, thiserror::Error)]
pub enum NetbootError {
    #[error(
        "relish netboot needs root for UDP 67, 69 and 4011: run it with sudo (on Linux, setcap cap_net_bind_service=+ep is the alternative)"
    )]
    NeedsRoot,
    #[error("{what} (port {port}) is in use already{hint}")]
    PortInUse {
        what: &'static str,
        port: u16,
        hint: &'static str,
    },
    #[error(
        "another netboot server ({server}) already answers PXE requests on this network; stop it first, or the two race to boot every machine"
    )]
    CompetingServer { server: Ipv4Addr },
    #[error("{0}")]
    Interface(String),
    #[error(
        "{directory} holds no x86_64/ or aarch64/ directory to serve (relish image download --dir writes them)"
    )]
    NothingToServe { directory: PathBuf },
    #[error("{directory} has no reliaburger-os_<version>.SHA256SUMS")]
    NoSums { directory: PathBuf },
    #[error("{file} is missing, and installs need it")]
    Missing { file: PathBuf },
    #[error("{0}; refusing to serve it")]
    Unverified(#[from] crate::os::OsError),
    #[error("{0} isn't an Ed25519 public key in PEM")]
    InvalidKey(String),
    #[error("invalid MAC address {0:?} (want aa:bb:cc:dd:ee:ff)")]
    InvalidMac(String),
    #[error("invalid iPXE build {0:?} (want full or snp)")]
    InvalidIpxe(String),
    #[error(
        "--ipxe full needs {file}, which this release doesn't have or doesn't list in its SHA256SUMS; --ipxe snp serves the SNP build instead"
    )]
    IpxeMissing { file: PathBuf },
    #[error("invalid duration {0:?} (want a number and s, m, h or d, such as 90m)")]
    InvalidDuration(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl NetbootError {
    /// An I/O error, with what was being done when it happened.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        NetbootError::Io {
            context: context.into(),
            source,
        }
    }
}

/// The keys a served directory's `SHA256SUMS` may be signed with.
///
/// The release keys this relish carries, plus `extra_pem` (`--key`, a lab
/// build's throwaway key) with a warning. `override_key` (`--trust-key
/// ed25519:…`) replaces them all, but only in debug builds, so tests can
/// sign with a key of their own; release builds ignore it with a warning,
/// as `upgrades.release_keys_override` does.
pub fn trusted_keys(
    extra_pem: Option<&str>,
    override_key: Option<&str>,
) -> Result<Vec<PublicKey>, NetbootError> {
    if let Some(key) = override_key {
        if cfg!(debug_assertions) {
            let key = crate::upgrade::signing::parse_public_key(key)
                .map_err(|e| NetbootError::InvalidKey(e.to_string()))?;
            return Ok(vec![key]);
        }
        eprintln!("relish netboot: warning: --trust-key is ignored in release builds");
    }
    let mut keys = crate::upgrade::keys::release_keys(&Default::default())
        .map_err(|e| NetbootError::InvalidKey(e.to_string()))?;
    if let Some(pem) = extra_pem {
        keys.push(artefacts::parse_pem_public_key(pem)?);
        eprintln!(
            "relish netboot: warning: also trusting the --key you gave, which isn't a release key; serve only builds you made yourself with it"
        );
    }
    Ok(keys)
}

/// Parse `--for`: a whole number followed by `s`, `m`, `h` or `d`.
pub fn parse_duration(input: &str) -> Result<Duration, NetbootError> {
    let invalid = || NetbootError::InvalidDuration(input.to_string());
    let unit = input.chars().last().ok_or_else(invalid)?;
    let multiplier = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return Err(invalid()),
    };
    let number: u64 = input[..input.len() - 1].parse().map_err(|_| invalid())?;
    let seconds = number.checked_mul(multiplier).ok_or_else(invalid)?;
    if seconds == 0 {
        return Err(invalid());
    }
    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn architectures_map_between_ipxe_names_and_download_directories() {
        assert_eq!(Arch::from_ipxe_name("arm64"), Some(Arch::Arm64));
        assert_eq!(Arch::Arm64.directory(), "aarch64");
        assert_eq!(Arch::from_ipxe_name("x86_64"), Some(Arch::X86_64));
        assert_eq!(Arch::from_ipxe_name("aarch64"), None);
        assert_eq!(Arch::from_ipxe_name("i386"), None);
        assert_eq!(Arch::Arm64.boot_file(), "ipxe-arm64.efi");
        assert_eq!(Arch::X86_64.snp_file(), "ipxe-snp-x86_64.efi");
    }

    #[test]
    fn the_ipxe_build_is_snp_unless_full_is_asked_for() {
        assert_eq!(IpxeBuild::default(), IpxeBuild::Snp);
        assert_eq!("snp".parse::<IpxeBuild>().unwrap(), IpxeBuild::Snp);
        assert_eq!("full".parse::<IpxeBuild>().unwrap(), IpxeBuild::Full);
        let error = "ipxe".parse::<IpxeBuild>().unwrap_err().to_string();
        assert!(error.contains("full or snp"), "{error}");
        assert_eq!(IpxeBuild::Full.to_string(), "full");
        assert_eq!(IpxeBuild::Snp.file(Arch::X86_64), "ipxe-snp-x86_64.efi");
        assert_eq!(IpxeBuild::Full.file(Arch::X86_64), "ipxe-x86_64.efi");
    }

    #[test]
    fn mac_addresses_parse_with_colons_or_dashes_and_print_lowercase() {
        let mac: MacAddress = "D8-9E-F3-12-34-56".parse().unwrap();
        assert_eq!(mac.to_string(), "d8:9e:f3:12:34:56");
        assert_eq!("d8:9e:f3:12:34:56".parse::<MacAddress>().unwrap(), mac);
        for bad in [
            "",
            "d8:9e:f3:12:34",
            "d8:9e:f3:12:34:56:78",
            "d8:9e:f3:12:34:5g",
            "d89:e:f3:12:34:56",
        ] {
            assert!(bad.parse::<MacAddress>().is_err(), "{bad}");
        }
    }

    #[test]
    fn durations_take_seconds_minutes_hours_and_days() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("1h").unwrap(), DEFAULT_DURATION);
        assert_eq!(parse_duration("2d").unwrap(), Duration::from_secs(172_800));
        for bad in [
            "",
            "h",
            "1",
            "1w",
            "0m",
            "-1h",
            "1.5h",
            "99999999999999999999d",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_release_keys_are_trusted_and_a_pem_key_is_added_to_them() {
        let release = crate::upgrade::keys::release_keys(&Default::default()).unwrap();
        assert_eq!(trusted_keys(None, None).unwrap(), release);
        let pem = "-----BEGIN PUBLIC KEY-----\n\
MCowBQYDK2VwAyEAi3zTXySVFXL+z98nJjP9w9GZqgBsxFYI0PChdhNgzRc=\n\
-----END PUBLIC KEY-----\n";
        let keys = trusted_keys(Some(pem), None).unwrap();
        assert_eq!(keys.len(), release.len() + 1);
        assert_eq!(keys[..release.len()], release[..]);
        assert!(trusted_keys(Some("garbage"), None).is_err());
    }

    #[test]
    fn a_trust_key_override_replaces_every_key_in_debug_builds() {
        let (_, public) = crate::upgrade::signing::generate_keypair().unwrap();
        let encoded = crate::upgrade::signing::encode_public_key(&public);
        // cargo test builds with debug assertions, so the override applies.
        assert_eq!(trusted_keys(None, Some(&encoded)).unwrap(), vec![public]);
        assert!(trusted_keys(None, Some("ed25519:nope")).is_err());
    }

    #[test]
    fn the_root_error_says_what_to_do() {
        assert_eq!(
            NetbootError::NeedsRoot.to_string(),
            "relish netboot needs root for UDP 67, 69 and 4011: run it with sudo (on Linux, setcap cap_net_bind_service=+ep is the alternative)"
        );
    }
}
