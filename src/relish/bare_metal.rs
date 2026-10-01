//! Bare-metal clusters from the laptop: `relish cluster create --bare-metal`
//! and `relish image seed` (docs/plans/2026-10-01-plan-appliance-product.md,
//! W3).
//!
//! The cluster's PKI is made here, on the operator's machine, as the
//! quickstart does: the master key and the sealed root CA key never exist
//! anywhere else until node 1's seed carries them. Each other machine gets
//! a seed with a single-use join token bound to its name, minted into the
//! cluster's initial security state, and fetches its certificate and the
//! master key from the cluster when it first boots (`bun appliance
//! prepare`). Seeds go on a USB stick labelled `RBSEED` as
//! `seeds/<mac>.seed`, so one stick serves the whole fleet.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::appliance::seed::{JoinSeed, SeedConfig, SeedRole, UpgradeSeed};
use crate::relish::RelishError;
use crate::sesame::types::{ApiRole, TokenScope};

/// One machine in the fleet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetNode {
    pub name: String,
    /// Lower-case, colon-separated.
    pub mac: String,
    pub address: IpAddr,
}

/// `fleet.json` in the cluster directory: what `relish image seed` needs to
/// add machines later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fleet {
    pub schema: u32,
    pub cluster: String,
    pub ca_fingerprint: String,
    pub operators: Vec<String>,
    pub network: Option<String>,
    pub faults: bool,
    pub nodes: Vec<FleetNode>,
}

/// What `relish cluster create --bare-metal` is asked for.
#[derive(Debug, Clone)]
pub struct CreateOptions {
    pub directory: PathBuf,
    pub cluster: String,
    /// `MAC@IP` per machine, node 1 first.
    pub machines: Vec<(String, IpAddr)>,
    pub operators: Vec<String>,
    pub network: Option<String>,
    pub faults: bool,
    pub ssh_key: Option<Vec<u8>>,
    pub token_ttl: Duration,
    pub external_signing_key: Option<String>,
}

/// Parse `aa:bb:cc:dd:ee:ff@192.168.1.51` (any case, `-` or `:`).
pub fn parse_machine(input: &str) -> Result<(String, IpAddr), String> {
    let (mac, address) = input
        .split_once('@')
        .ok_or_else(|| format!("{input:?} isn't MAC@IP"))?;
    let mac = mac.to_ascii_lowercase().replace('-', ":");
    let octets: Vec<&str> = mac.split(':').collect();
    if octets.len() != 6
        || octets
            .iter()
            .any(|o| o.len() != 2 || !o.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(format!("{mac:?} isn't a MAC address"));
    }
    let address = address
        .parse()
        .map_err(|_| format!("{address:?} isn't an IP address"))?;
    Ok((mac, address))
}

/// `home-3`: the cluster's name and the machine's position (G4).
pub fn node_name(cluster: &str, index: usize) -> String {
    format!("{cluster}-{index}")
}

/// The file a machine's seed has on the stick: `seeds/d8-9e-….seed`.
pub fn stick_file(mac: &str) -> String {
    format!("seeds/{}.seed", mac.replace(':', "-"))
}

/// `[security] bootstrap_peers` for every node: each machine's address, and
/// the network later machines join from when one was given.
fn peers(fleet: &Fleet) -> Vec<String> {
    let mut peers: Vec<String> = fleet.network.iter().cloned().collect();
    peers.extend(fleet.nodes.iter().map(|n| n.address.to_string()));
    peers
}

impl Fleet {
    fn seed_config(
        &self,
        node: &FleetNode,
        role: SeedRole,
        token: Option<String>,
        external_signing_key: &Option<String>,
    ) -> SeedConfig {
        SeedConfig {
            schema: 1,
            cluster: self.cluster.clone(),
            node: node.name.clone(),
            role,
            advertise: Some(node.address),
            peers: peers(self),
            operators: self.operators.clone(),
            faults: self.faults.then(|| "development".to_string()),
            join: token.map(|token| JoinSeed {
                members: self
                    .nodes
                    .iter()
                    .filter(|other| other.name != node.name)
                    .map(|other| other.address)
                    .collect(),
                ca_fingerprint: self.ca_fingerprint.clone(),
                token,
            }),
            upgrades: UpgradeSeed {
                external_signing_key: external_signing_key.clone(),
            },
        }
    }
}

/// A seed tarball: `seed.toml` and `files`.
pub fn seed_tarball(
    config: &SeedConfig,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<u8>, RelishError> {
    let text = toml::to_string_pretty(config).map_err(|e| failed(&e.to_string()))?;
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    let mut add = |name: &str, data: &[u8]| -> std::io::Result<()> {
        let mut header = tar::Header::new_ustar();
        header.set_size(data.len() as u64);
        header.set_mode(0o600);
        header.set_mtime(0);
        header.set_cksum();
        builder.append_data(&mut header, name, data)
    };
    add("seed.toml", text.as_bytes())?;
    for (name, data) in files {
        add(name, data)?;
    }
    Ok(builder.into_inner()?.finish()?)
}

/// What `create` made, for the summary.
#[derive(Debug)]
pub struct Created {
    pub fleet: Fleet,
    pub admin_token: String,
    pub seeds: Vec<PathBuf>,
}

/// Create the cluster's PKI and admin token, mint a join token per extra
/// machine into its initial security state, and write every seed.
/// Refuses a directory that already holds a fleet.
pub fn create(options: &CreateOptions) -> Result<Created, RelishError> {
    let dir = &options.directory;
    if dir.join("fleet.json").exists() {
        return Err(failed(&format!(
            "{} already holds a cluster",
            dir.display()
        )));
    }
    if options.machines.is_empty() {
        return Err(failed("list at least one machine, node 1 first"));
    }
    crate::config::node::ClusterSection {
        name: options.cluster.clone(),
        ..Default::default()
    }
    .validate()
    .map_err(|e| failed(&e.to_string()))?;
    let secrets = dir.join("secrets");
    create_private_dir(&secrets)?;

    let nodes: Vec<FleetNode> = options
        .machines
        .iter()
        .enumerate()
        .map(|(i, (mac, address))| FleetNode {
            name: node_name(&options.cluster, i + 1),
            mac: mac.clone(),
            address: *address,
        })
        .collect();
    let mut init =
        crate::sesame::init::initialize_cluster(&options.cluster, &nodes[0].name, &secrets)
            .map_err(|e| failed(&format!("generating the cluster's PKI: {e}")))?;
    let admin = crate::sesame::token::create_token(
        "bare-metal-admin",
        ApiRole::Admin,
        TokenScope::default(),
        None,
    )
    .map_err(|e| failed(&format!("creating the admin token: {e}")))?;
    init.security_state.api_tokens.push(admin.token);
    let mut tokens = BTreeMap::new();
    for node in &nodes[1..] {
        let (plaintext, token) =
            crate::sesame::join::create_seed_join_token(options.token_ttl, &node.name)
                .map_err(|e| failed(&e.to_string()))?;
        init.security_state.join_tokens.push(token);
        tokens.insert(node.name.clone(), plaintext);
    }
    let identity = super::commands::node_identity_from_init(&init)?;
    let ca_fingerprint = crate::sesame::identity_store::root_ca_fingerprint(&identity.root_ca_der);
    let master_key = hex::encode(init.master_secret);
    let bootstrap =
        serde_json::to_vec_pretty(&init.security_state).map_err(RelishError::SerialiseJson)?;

    write_private(&secrets.join("master.key"), master_key.as_bytes())?;
    write_private(&secrets.join("admin.token"), admin.plaintext.as_bytes())?;
    write_private(&secrets.join("security-bootstrap.json"), &bootstrap)?;
    let identity_dir = secrets.join("identity");
    crate::sesame::identity_store::save(&identity_dir, &identity)
        .map_err(|e| failed(&e.to_string()))?;
    std::fs::write(
        dir.join("root-ca.crt"),
        pem("CERTIFICATE", &identity.root_ca_der),
    )?;

    let fleet = Fleet {
        schema: 1,
        cluster: options.cluster.clone(),
        ca_fingerprint,
        operators: options.operators.clone(),
        network: options.network.clone(),
        faults: options.faults,
        nodes: nodes.clone(),
    };

    // Node 1's seed carries the bootstrap material; the others a token.
    let mut create_files = BTreeMap::new();
    create_files.insert("master.key".to_string(), master_key.into_bytes());
    create_files.insert("security-bootstrap.json".to_string(), bootstrap);
    for entry in std::fs::read_dir(&identity_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        create_files.insert(format!("identity/{name}"), std::fs::read(entry.path())?);
    }
    let mut seeds = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        let mut files = if index == 0 {
            create_files.clone()
        } else {
            BTreeMap::new()
        };
        if let Some(key) = &options.ssh_key {
            files.insert("authorized_keys".to_string(), key.clone());
        }
        let config = if index == 0 {
            fleet.seed_config(node, SeedRole::Create, None, &options.external_signing_key)
        } else {
            fleet.seed_config(
                node,
                SeedRole::Join,
                tokens.remove(&node.name),
                &options.external_signing_key,
            )
        };
        let path = dir.join("stick").join(stick_file(&node.mac));
        write_private(&path, &seed_tarball(&config, &files)?)?;
        seeds.push(path);
    }
    write_private(
        &dir.join("fleet.json"),
        &serde_json::to_vec_pretty(&fleet).map_err(RelishError::SerialiseJson)?,
    )?;
    Ok(Created {
        fleet,
        admin_token: admin.plaintext,
        seeds,
    })
}

/// The relish context for a bare-metal cluster: node 1's API with the admin
/// token, trusting the cluster's root CA.
pub fn context(
    fleet: &Fleet,
    directory: &Path,
    admin_token: &str,
) -> Result<super::local_context::LocalContext, RelishError> {
    let first = fleet
        .nodes
        .first()
        .ok_or_else(|| failed("the fleet has no nodes"))?;
    let host = std::net::SocketAddr::new(first.address, 0).ip();
    let origin =
        |scheme: &str, port: u16| format!("{scheme}://{}", std::net::SocketAddr::new(host, port));
    Ok(super::local_context::LocalContext {
        schema: 1,
        owner: format!("bare-metal/{}", fleet.cluster),
        endpoint: origin("https", 9117),
        token: admin_token.to_string(),
        ca_cert: std::fs::canonicalize(directory.join("root-ca.crt"))?,
        service_endpoints: crate::bun::capabilities::ServiceEndpoints {
            registry: Some(origin("https", 5050)),
            ingress_http: Some(origin("http", 80)),
            ingress_https: Some(origin("https", 443)),
        },
    })
}

/// `relish cluster create --bare-metal`: create the cluster, save its
/// context (unless another cluster's context is in the way), and say what
/// to do next.
pub fn run_create(options: &CreateOptions) -> Result<(), RelishError> {
    let created = create(options)?;
    let context = context(&created.fleet, &options.directory, &created.admin_token)?;
    let path = super::local_context::default_path()?;
    let saved = match super::local_context::LocalContext::load(&path) {
        Ok(None) => context.save(&path).map(|()| true)?,
        Ok(Some(existing)) if existing.owner == context.owner => {
            context.save(&path).map(|()| true)?
        }
        _ => false,
    };
    let dir = options.directory.display();
    println!("cluster {} created in {dir}", created.fleet.cluster);
    println!("  root CA: {}", created.fleet.ca_fingerprint);
    for (node, seed) in created.fleet.nodes.iter().zip(&created.seeds) {
        println!(
            "  {} ({} at {}): {}",
            node.name,
            node.mac,
            node.address,
            seed.display()
        );
    }
    println!();
    println!(
        "Copy {dir}/stick/seeds onto a USB stick labelled RBSEED (FAT32), then boot each machine with it."
    );
    println!("Back up {dir}/secrets: it holds the master key and the sealed root CA key.");
    if saved {
        println!(
            "relish now talks to this cluster (node 1, {}).",
            context.endpoint
        );
    } else {
        println!("Another cluster's relish context is in place, so it was kept. To use this one:");
        println!("  export RELIABURGER_ENDPOINT={}", context.endpoint);
        println!("  export RELIABURGER_CA_CERT={}", context.ca_cert.display());
        println!("  export RELIABURGER_TOKEN=$(cat {dir}/secrets/admin.token)");
    }
    Ok(())
}

fn pem(label: &str, der: &[u8]) -> String {
    use base64::Engine as _;
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = body
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        lines.join("\n")
    )
}

fn create_private_dir(path: &Path) -> Result<(), RelishError> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    Ok(())
}

fn write_private(path: &Path, data: &[u8]) -> Result<(), RelishError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}

fn failed(message: &str) -> RelishError {
    RelishError::InitFailed(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appliance::seed::Seed;

    fn options(dir: &Path, machines: &[&str]) -> CreateOptions {
        CreateOptions {
            directory: dir.to_path_buf(),
            cluster: "home".into(),
            machines: machines.iter().map(|m| parse_machine(m).unwrap()).collect(),
            operators: vec!["192.168.1.10".into()],
            network: Some("192.168.1.0/24".into()),
            faults: false,
            ssh_key: None,
            token_ttl: crate::sesame::join::MAX_SEED_JOIN_TOKEN_TTL,
            external_signing_key: None,
        }
    }

    #[test]
    fn machines_are_mac_at_ip_in_any_case_and_separator() {
        assert_eq!(
            parse_machine("D8-9E-F3-12-34-56@192.168.1.51").unwrap(),
            (
                "d8:9e:f3:12:34:56".to_string(),
                "192.168.1.51".parse().unwrap()
            )
        );
        for bad in [
            "d8:9e:f3:12:34@1.2.3.4",
            "zz:9e:f3:12:34:56@1.2.3.4",
            "d8:9e:f3:12:34:56",
            "d8:9e:f3:12:34:56@host",
        ] {
            assert!(parse_machine(bad).is_err(), "{bad}");
        }
        assert_eq!(
            stick_file("d8:9e:f3:12:34:56"),
            "seeds/d8-9e-f3-12-34-56.seed"
        );
    }

    #[test]
    fn create_writes_a_create_seed_and_a_join_seed_per_other_machine() {
        let dir = tempfile::tempdir().unwrap();
        let created = create(&options(
            dir.path(),
            &[
                "d8:9e:f3:00:00:01@192.168.1.51",
                "d8:9e:f3:00:00:02@192.168.1.52",
                "d8:9e:f3:00:00:03@192.168.1.53",
            ],
        ))
        .unwrap();
        assert_eq!(created.seeds.len(), 3);
        let names: Vec<&str> = created
            .fleet
            .nodes
            .iter()
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(names, ["home-1", "home-2", "home-3"]);

        let first = Seed::from_tar_gz(&std::fs::read(&created.seeds[0]).unwrap()).unwrap();
        let Seed::V1 { config, files } = first else {
            panic!("v1")
        };
        assert_eq!(config.role, SeedRole::Create);
        assert!(
            files.contains_key("master.key") && files.contains_key("identity/bundle.committed")
        );
        assert_eq!(
            config.peers,
            [
                "192.168.1.0/24",
                "192.168.1.51",
                "192.168.1.52",
                "192.168.1.53"
            ]
        );

        let bootstrap: crate::sesame::types::SecurityState =
            serde_json::from_slice(&files["security-bootstrap.json"]).unwrap();
        let second = Seed::from_tar_gz(&std::fs::read(&created.seeds[1]).unwrap()).unwrap();
        let Seed::V1 { config, files } = second else {
            panic!("v1")
        };
        assert_eq!(config.role, SeedRole::Join);
        assert!(
            !files.contains_key("master.key"),
            "a joiner's seed has no master key"
        );
        let join = config.join.unwrap();
        assert_eq!(
            join.members,
            [
                "192.168.1.51".parse::<IpAddr>().unwrap(),
                "192.168.1.53".parse().unwrap()
            ]
        );
        assert_eq!(join.ca_fingerprint, created.fleet.ca_fingerprint);
        // The token in the seed is one the cluster's initial state accepts,
        // for this node only.
        crate::sesame::join::check_join_token(&join.token, "home-2", &bootstrap).unwrap();
        assert!(crate::sesame::join::check_join_token(&join.token, "home-3", &bootstrap).is_err());
        assert!(created.admin_token.starts_with("rbrg_"));

        use std::os::unix::fs::PermissionsExt;
        for secret in ["secrets/master.key", "secrets/admin.token", "fleet.json"] {
            let mode = std::fs::metadata(dir.path().join(secret))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "{secret}");
        }
        assert!(
            create(&options(dir.path(), &["d8:9e:f3:00:00:01@192.168.1.51"])).is_err(),
            "an existing cluster"
        );
    }

    #[test]
    fn the_context_points_at_node_one_with_the_admin_token() {
        let dir = tempfile::tempdir().unwrap();
        let created = create(&options(dir.path(), &["d8:9e:f3:00:00:01@192.168.1.51"])).unwrap();
        let context = context(&created.fleet, dir.path(), &created.admin_token).unwrap();
        assert_eq!(context.endpoint, "https://192.168.1.51:9117");
        assert_eq!(context.owner, "bare-metal/home");
        assert_eq!(
            context.service_endpoints.registry.as_deref(),
            Some("https://192.168.1.51:5050")
        );
        assert!(
            std::fs::read_to_string(&context.ca_cert)
                .unwrap()
                .starts_with("-----BEGIN CERTIFICATE-----")
        );
    }
}
