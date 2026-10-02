//! The seed: what an appliance node is told before it first starts bun.
//!
//! A seed is a gzipped tarball, delivered as the `reliaburger.seed` systemd
//! credential or as `seeds/<mac>.seed` on a USB stick labelled `RBSEED`.
//! It holds `seed.toml` and, for the node that creates the cluster, the
//! cluster's bootstrap material (`master.key`, `security-bootstrap.json`
//! and `identity/`). A joiner's seed holds no secret but a single-use,
//! node-bound join token: it fetches its certificate and the master key
//! from the cluster itself (`super::prepare`).
//!
//! The spike's seeds (a tarball of `node.toml` and the files it names) are
//! still accepted as [`Seed::Legacy`], so lab fleets keep working.

use std::collections::BTreeMap;
use std::io::Read;
use std::net::IpAddr;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use crate::config::node::NodeConfig;

/// The largest seed accepted: a few KiB of config and certificates, so
/// anything near this is not a seed.
const MAX_SEED_BYTES: u64 = 1024 * 1024;

/// Where an appliance keeps its node configuration and secrets. `etc-sync`
/// never touches `/etc/reliaburger` (it's the node's own).
pub const CONFIG_DIR: &str = "/etc/reliaburger";

/// Why a seed was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SeedError {
    #[error("the seed isn't a gzipped tarball: {0}")]
    Archive(String),
    #[error("the seed is larger than {MAX_SEED_BYTES} bytes")]
    TooLarge,
    #[error("the seed has an unsafe path: {0}")]
    UnsafePath(String),
    #[error("the seed has neither seed.toml nor node.toml")]
    Empty,
    #[error("seed.toml is invalid: {0}")]
    Config(String),
    #[error("a {role} seed must carry {file}")]
    Missing {
        role: &'static str,
        file: &'static str,
    },
}

/// What this node does with its seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeedRole {
    /// Start the cluster from the bootstrap material in the seed.
    Create,
    /// Join an existing cluster with a join token.
    Join,
}

/// How to join: the members to enrol through and the token to present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinSeed {
    /// Addresses of existing members: their API (port 9117) for enrolment,
    /// their gossip (port 9443) once running.
    pub members: Vec<IpAddr>,
    /// The cluster root CA's `sha256:` fingerprint. A member presenting
    /// another CA is refused before the token is sent.
    pub ca_fingerprint: String,
    /// The single-use join token for this node's name.
    pub token: String,
}

/// Optional `[upgrades]` settings carried into `node.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpgradeSeed {
    pub external_signing_key: Option<String>,
}

/// `seed.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedConfig {
    pub schema: u32,
    pub cluster: String,
    /// This node's name, which its join token is bound to (G4).
    pub node: String,
    pub role: SeedRole,
    /// The address other nodes reach this one on. Absent means: detect it
    /// from the default route (G3).
    #[serde(default)]
    pub advertise: Option<IpAddr>,
    /// Peers (addresses or networks) let through the firewall before
    /// they've joined: `[security] bootstrap_peers`.
    #[serde(default)]
    pub peers: Vec<String>,
    /// Where relish runs from: `[security] operator_cidrs`.
    #[serde(default)]
    pub operators: Vec<String>,
    /// `development` admits workload and node faults, as the quickstart
    /// does; absent keeps the protected default (no faults).
    #[serde(default)]
    pub faults: Option<String>,
    #[serde(default)]
    pub join: Option<JoinSeed>,
    #[serde(default)]
    pub upgrades: UpgradeSeed,
}

/// A parsed seed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seed {
    /// A seed for this format: `seed.toml`, plus the bootstrap files for
    /// the node that creates the cluster.
    V1 {
        config: Box<SeedConfig>,
        files: BTreeMap<String, Vec<u8>>,
    },
    /// A spike seed: `node.toml` and the files it names, installed as-is.
    Legacy { files: BTreeMap<String, Vec<u8>> },
}

impl Seed {
    /// Parse a seed tarball. Every entry must be a plain file or directory
    /// under a relative path without `..`.
    pub fn from_tar_gz(bytes: &[u8]) -> Result<Self, SeedError> {
        if bytes.len() as u64 > MAX_SEED_BYTES {
            return Err(SeedError::TooLarge);
        }
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
        let mut files = BTreeMap::new();
        let entries = archive
            .entries()
            .map_err(|e| SeedError::Archive(e.to_string()))?;
        for entry in entries {
            let mut entry = entry.map_err(|e| SeedError::Archive(e.to_string()))?;
            let kind = entry.header().entry_type();
            let path = entry
                .path()
                .map_err(|e| SeedError::Archive(e.to_string()))?
                .into_owned();
            let name = safe_name(&path)?;
            if kind.is_dir() {
                continue;
            }
            if !kind.is_file() {
                return Err(SeedError::UnsafePath(format!("{name} isn't a plain file")));
            }
            if entry.size() > MAX_SEED_BYTES {
                return Err(SeedError::TooLarge);
            }
            let mut data = Vec::new();
            entry
                .read_to_end(&mut data)
                .map_err(|e| SeedError::Archive(e.to_string()))?;
            if !name.is_empty() {
                files.insert(name, data);
            }
        }
        if let Some(text) = files.remove("seed.toml") {
            let text = String::from_utf8(text).map_err(|e| SeedError::Config(e.to_string()))?;
            let config: SeedConfig =
                toml::from_str(&text).map_err(|e| SeedError::Config(e.to_string()))?;
            config.check(&files)?;
            return Ok(Seed::V1 {
                config: Box::new(config),
                files,
            });
        }
        if files.contains_key("node.toml") {
            return Ok(Seed::Legacy { files });
        }
        Err(SeedError::Empty)
    }
}

/// The bootstrap files a `create` seed carries.
pub const CREATE_FILES: [&str; 3] = [
    "master.key",
    "security-bootstrap.json",
    "identity/bundle.committed",
];

impl SeedConfig {
    fn check(&self, files: &BTreeMap<String, Vec<u8>>) -> Result<(), SeedError> {
        let bad = |reason: String| Err(SeedError::Config(reason));
        if self.schema != 1 {
            return bad(format!("unsupported schema {}", self.schema));
        }
        crate::config::node::ClusterSection {
            name: self.cluster.clone(),
            ..Default::default()
        }
        .validate()
        .map_err(|e| SeedError::Config(e.to_string()))?;
        if self.node.is_empty()
            || !self
                .node
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return bad(format!("node name {:?} isn't [A-Za-z0-9-]+", self.node));
        }
        if let Some(faults) = &self.faults
            && faults != "development"
        {
            return bad(format!(
                "faults must be \"development\" or absent, not {faults:?}"
            ));
        }
        match (self.role, &self.join) {
            (SeedRole::Join, None) => bad("a join seed needs [join]".into()),
            (SeedRole::Join, Some(join)) if join.members.is_empty() => {
                bad("[join] members is empty".into())
            }
            (SeedRole::Join, Some(join)) if !join.ca_fingerprint.starts_with("sha256:") => {
                bad("[join] ca_fingerprint must be sha256:<hex>".into())
            }
            (SeedRole::Join, Some(_)) if files.contains_key("master.key") => {
                bad("a join seed must not carry the master key: the node fetches it".into())
            }
            (SeedRole::Create, Some(_)) => bad("a create seed has no [join]".into()),
            (SeedRole::Create, None) => {
                for file in CREATE_FILES {
                    if !files.contains_key(file) {
                        return Err(SeedError::Missing {
                            role: "create",
                            file,
                        });
                    }
                }
                Ok(())
            }
            (SeedRole::Join, Some(_)) => Ok(()),
        }
    }

    /// The `node.toml` this seed gives the node, with the appliance profile
    /// (research §9.3): one old bun kept, two days of unreferenced images, a
    /// 1 GiB build cache, everything under [`CONFIG_DIR`].
    pub fn node_config(&self, advertise: IpAddr) -> NodeConfig {
        let mut config = NodeConfig::default();
        config.node.name = Some(self.node.clone());
        config.cluster.name = self.cluster.clone();
        if let Some(join) = &self.join {
            config.cluster.join = join
                .members
                .iter()
                .map(|member| std::net::SocketAddr::new(*member, 9443).to_string())
                .collect();
        }
        config.network.advertise_address = Some(advertise.to_string());
        let security = &mut config.security;
        security.require_mtls = true;
        security.allow_insecure_cluster = false;
        security.identity_dir = Some(format!("{CONFIG_DIR}/identity").into());
        security.master_key_path = Some(format!("{CONFIG_DIR}/master.key").into());
        if self.role == SeedRole::Create {
            security.bootstrap_path = Some(format!("{CONFIG_DIR}/security-bootstrap.json").into());
        }
        security.bootstrap_peers = self.peers.clone();
        security.operator_cidrs = self.operators.clone();
        config.ebpf.enabled = true;
        config.dns.enabled = true;
        config.dns.listen = std::net::SocketAddr::new(advertise, 53).to_string();
        config.ingress.enabled = true;
        config.images.gc_retain_days = 2;
        config.images.build_cache_max_bytes = 1024 * 1024 * 1024;
        config.upgrades.retain_versions = 1;
        config.upgrades.external_signing_key = self.upgrades.external_signing_key.clone();
        if self.faults.is_some() {
            config.testing = crate::relish::quickstart::provision::laptop_test_policy();
        }
        config
    }
}

/// A tar path as a relative `a/b` string, refusing anything that could
/// land outside the directory it's unpacked into.
fn safe_name(path: &Path) -> Result<String, SeedError> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(
                part.to_str()
                    .ok_or_else(|| SeedError::UnsafePath(path.display().to_string()))?
                    .to_string(),
            ),
            Component::CurDir => {}
            _ => return Err(SeedError::UnsafePath(path.display().to_string())),
        }
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A seed tarball from `(name, bytes)` pairs.
    pub(crate) fn tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    pub(crate) const JOIN_TOML: &str = r#"
schema = 1
cluster = "home"
node = "home-2"
role = "join"
peers = ["192.168.1.0/24"]
operators = ["192.168.1.10"]

[join]
members = ["192.168.1.51"]
ca_fingerprint = "sha256:abc"
token = "rbrg_join_1_00"
"#;

    const CREATE_TOML: &str = r#"
schema = 1
cluster = "home"
node = "home-1"
role = "create"
advertise = "192.168.1.51"
"#;

    #[test]
    fn a_join_seed_parses_and_becomes_an_appliance_node_toml() {
        let seed = Seed::from_tar_gz(&tarball(&[("seed.toml", JOIN_TOML.as_bytes())])).unwrap();
        let Seed::V1 { config, files } = seed else {
            panic!("not a v1 seed")
        };
        assert!(files.is_empty());
        assert_eq!(config.role, SeedRole::Join);
        let node = config.node_config("192.168.1.52".parse().unwrap());
        assert_eq!(node.node.name.as_deref(), Some("home-2"));
        assert_eq!(node.cluster.join, vec!["192.168.1.51:9443"]);
        assert_eq!(
            node.network.advertise_address.as_deref(),
            Some("192.168.1.52")
        );
        assert!(node.security.require_mtls);
        assert!(node.security.bootstrap_path.is_none());
        assert_eq!(node.security.bootstrap_peers, vec!["192.168.1.0/24"]);
        assert_eq!(node.security.operator_cidrs, vec!["192.168.1.10"]);
        assert_eq!(node.upgrades.retain_versions, 1);
        assert_eq!(node.images.gc_retain_days, 2);
        assert_eq!(node.dns.listen, "192.168.1.52:53");
        // It's a config bun accepts.
        toml::to_string(&node)
            .unwrap()
            .parse::<toml::Table>()
            .unwrap();
        node.validate().unwrap();
    }

    #[test]
    fn a_create_seed_needs_its_bootstrap_files() {
        let missing = Seed::from_tar_gz(&tarball(&[("seed.toml", CREATE_TOML.as_bytes())]));
        assert_eq!(
            missing,
            Err(SeedError::Missing {
                role: "create",
                file: "master.key"
            })
        );
        let seed = Seed::from_tar_gz(&tarball(&[
            ("seed.toml", CREATE_TOML.as_bytes()),
            ("master.key", b"00"),
            ("security-bootstrap.json", b"{}"),
            ("./identity/node.crt", b"c"),
            ("identity/bundle.committed", b""),
        ]))
        .unwrap();
        let Seed::V1 { config, files } = seed else {
            panic!("not a v1 seed")
        };
        assert!(files.contains_key("identity/node.crt"));
        let node = config.node_config("192.168.1.51".parse().unwrap());
        assert!(node.cluster.join.is_empty());
        assert_eq!(
            node.security.bootstrap_path.as_deref(),
            Some(Path::new("/etc/reliaburger/security-bootstrap.json"))
        );
    }

    #[test]
    fn a_join_seed_carrying_the_master_key_is_refused() {
        let seed = Seed::from_tar_gz(&tarball(&[
            ("seed.toml", JOIN_TOML.as_bytes()),
            ("master.key", b"00"),
        ]));
        assert!(matches!(seed, Err(SeedError::Config(_))));
    }

    #[test]
    fn paths_that_escape_and_odd_entries_are_refused() {
        for name in ["../etc/passwd", "/etc/shadow", "a/../../b"] {
            let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::default(),
            ));
            let mut header = tar::Header::new_gnu();
            header.set_size(1);
            // set_path refuses some of these, so write the name raw.
            header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_cksum();
            builder.append(&header, &b"x"[..]).unwrap();
            let bytes = builder.into_inner().unwrap().finish().unwrap();
            assert!(
                matches!(Seed::from_tar_gz(&bytes), Err(SeedError::UnsafePath(_))),
                "{name}"
            );
        }
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder
            .append_link(&mut header, "identity", "/etc/shadow")
            .unwrap();
        let bytes = builder.into_inner().unwrap().finish().unwrap();
        assert!(matches!(
            Seed::from_tar_gz(&bytes),
            Err(SeedError::UnsafePath(_))
        ));
    }

    #[test]
    fn bad_configs_are_refused_with_a_reason() {
        for (from, to) in [
            ("schema = 1", "schema = 2"),
            ("node = \"home-2\"", "node = \"home 2\""),
            (
                "ca_fingerprint = \"sha256:abc\"",
                "ca_fingerprint = \"abc\"",
            ),
            ("members = [\"192.168.1.51\"]", "members = []"),
            ("role = \"join\"", "role = \"join\"\nfaults = \"anything\""),
            ("role = \"join\"", "role = \"join\"\nextra = 1"),
        ] {
            let text = JOIN_TOML.replace(from, to);
            let seed = Seed::from_tar_gz(&tarball(&[("seed.toml", text.as_bytes())]));
            assert!(matches!(seed, Err(SeedError::Config(_))), "{to}: {seed:?}");
        }
    }

    #[test]
    fn spike_seeds_are_still_accepted_as_legacy() {
        let seed = Seed::from_tar_gz(&tarball(&[
            ("./node.toml", b"[node]\nname = \"n\"\n"),
            ("master.key", b"00"),
        ]))
        .unwrap();
        assert!(matches!(seed, Seed::Legacy { ref files } if files.contains_key("node.toml")));
        assert_eq!(
            Seed::from_tar_gz(&tarball(&[("other", b"x")])),
            Err(SeedError::Empty)
        );
        assert!(matches!(
            Seed::from_tar_gz(b"not gzip"),
            Err(SeedError::Archive(_))
        ));
    }
}
