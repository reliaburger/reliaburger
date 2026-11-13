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

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::appliance::seed::{JoinSeed, SeedConfig, SeedRole, UpgradeSeed};
use crate::relish::RelishError;
use crate::relish::output::{OutputFormat, print_output, progress};
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
    /// How many voters the council grows to, as the cluster was created
    /// with. Absent in directories made before it was recorded.
    #[serde(default)]
    pub council_size: Option<crate::council::CouncilSize>,
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
    /// How many voters the council grows to.
    pub council_size: crate::council::CouncilSize,
    /// Where the age identity that opens the root CA backup goes: relish's
    /// own state directory, apart from `directory` (#709).
    pub root_backup_keys: PathBuf,
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
            council_size: self.council_size.filter(|_| role == SeedRole::Create),
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

/// The root CA backup's name in a bare-metal cluster directory.
pub const ROOT_BACKUP_FILE: &str = "root-ca-backup.age";

/// What `create` made, for the summary.
#[derive(Debug)]
pub struct Created {
    pub fleet: Fleet,
    pub admin_token: String,
    pub seeds: Vec<PathBuf>,
    /// The only copy of the root CA key, and the identity that opens it.
    pub root_backup: super::quickstart::security::RootBackupFiles,
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
    let mut init = crate::sesame::init::initialize_cluster(&options.cluster, &nodes[0].name)
        .map_err(|e| failed(&format!("generating the cluster's PKI: {e}")))?;
    // The root key's only copy, on this machine: no seed and no appliance
    // ever carries it or anything that opens it (#709).
    super::quickstart::security::create_private_dir(&options.root_backup_keys)?;
    let root_backup = super::quickstart::security::root_backup_files_named(
        dir,
        ROOT_BACKUP_FILE,
        &options.root_backup_keys,
        &init.root_backup.fingerprint,
    );
    crate::sesame::root_backup::write_backup(
        &init.root_backup,
        &crate::sesame::root_backup::BackupDestination::FreshIdentity {
            path: &root_backup.backup,
            identity: &root_backup.identity,
        },
        std::time::SystemTime::now(),
    )?;
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
        council_size: Some(options.council_size),
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
        root_backup,
    })
}

/// The machines `relish image seed` adds: each named after the cluster
/// and the next free number, refusing a MAC or address the fleet has.
pub fn plan_additions(
    fleet: &Fleet,
    machines: &[(String, IpAddr)],
) -> Result<Vec<FleetNode>, RelishError> {
    let prefix = format!("{}-", fleet.cluster);
    let first = fleet
        .nodes
        .iter()
        .filter_map(|n| n.name.strip_prefix(&prefix)?.parse::<usize>().ok())
        .max()
        .unwrap_or(0)
        + 1;
    let mut added: Vec<FleetNode> = Vec::new();
    for (index, (mac, address)) in (first..).zip(machines) {
        let known = fleet.nodes.iter().chain(added.iter());
        if let Some(clash) = known
            .clone()
            .find(|n| &n.mac == mac || &n.address == address)
        {
            return Err(failed(&format!(
                "{mac}@{address} clashes with {} ({}@{})",
                clash.name, clash.mac, clash.address
            )));
        }
        added.push(FleetNode {
            name: node_name(&fleet.cluster, index),
            mac: mac.clone(),
            address: *address,
        });
    }
    Ok(added)
}

/// Whether the fleet's existing nodes let `address` through their firewall
/// before it has joined: only if it's inside the `--network` they were
/// created with.
pub fn admitted(fleet: &Fleet, address: IpAddr) -> bool {
    let Some(network) = &fleet.network else {
        return false;
    };
    let Ok((base, prefix)) = crate::firewall::rules::parse_cidr(network) else {
        return false;
    };
    match (base, address) {
        (IpAddr::V4(base), IpAddr::V4(address)) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            u32::from(base) & mask == u32::from(address) & mask
        }
        (IpAddr::V6(base), IpAddr::V6(address)) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            u128::from(base) & mask == u128::from(address) & mask
        }
        _ => false,
    }
}

/// Add machines to a running cluster: a join token from the cluster for
/// each (relish's context must point at it), a seed beside the others in
/// `<dir>/stick/seeds`, and the fleet list updated. Returns each new node
/// with its seed.
pub async fn add(
    directory: &Path,
    machines: &[(String, IpAddr)],
    token_ttl: Duration,
    ssh_key: Option<Vec<u8>>,
) -> Result<Vec<(FleetNode, PathBuf)>, RelishError> {
    let fleet_path = directory.join("fleet.json");
    let mut fleet: Fleet = serde_json::from_slice(&tokio::fs::read(&fleet_path).await?)
        .map_err(|e| failed(&format!("{}: {e}", fleet_path.display())))?;
    let added = plan_additions(&fleet, machines)?;
    let client = super::client::BunClient::default_local();
    let mut written = Vec::new();
    for node in &added {
        let token = client
            .join_token_create_for_seed(&node.name, token_ttl.as_secs())
            .await?;
        let mut files = BTreeMap::new();
        if let Some(key) = &ssh_key {
            files.insert("authorized_keys".to_string(), key.clone());
        }
        let mut with_node = fleet.clone();
        with_node.nodes.push(node.clone());
        let config = with_node.seed_config(node, SeedRole::Join, Some(token), &None);
        let path = directory.join("stick").join(stick_file(&node.mac));
        // BLOCKING-OK: relish CLI: a one-shot write before the command does anything long-lived
        write_private(&path, &seed_tarball(&config, &files)?)?;
        written.push(path);
    }
    fleet.nodes.extend(added.iter().cloned());
    // BLOCKING-OK: relish CLI: a one-shot write before the command does anything long-lived
    write_private(
        &fleet_path,
        &serde_json::to_vec_pretty(&fleet).map_err(RelishError::SerialiseJson)?,
    )?;
    for node in &added {
        if !admitted(&fleet, node.address) {
            eprintln!("{}", not_admitted_warning(node.address));
        }
    }
    Ok(added.into_iter().zip(written).collect())
}

/// What to tell the user about a machine outside the network the cluster
/// admits (`cluster create --network`): its firewalls drop it until it
/// joins. Claiming it opens a join window on every node, so only a stick
/// boot needs `bootstrap_peers`.
fn not_admitted_warning(address: IpAddr) -> String {
    format!(
        "warning: {address} isn't in a network the cluster admits (cluster create --network), so the \
         existing nodes' firewalls drop it until it joins. `relish machines claim` opens a join window \
         for it; to boot it from its stick instead, add it to [security] bootstrap_peers on the \
         existing nodes first"
    )
}

/// A machine and the seed written for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeededNode {
    pub name: String,
    pub mac: String,
    pub address: IpAddr,
    pub seed: PathBuf,
}

impl SeededNode {
    fn new(node: &FleetNode, seed: &Path) -> Self {
        Self {
            name: node.name.clone(),
            mac: node.mac.clone(),
            address: node.address,
            seed: seed.to_path_buf(),
        }
    }
}

impl std::fmt::Display for SeededNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "  {} ({} at {}): {}",
            self.name,
            self.mac,
            self.address,
            self.seed.display()
        )
    }
}

/// What `relish image seed` answers: the machines added, with their seeds.
#[derive(Debug, Clone, Serialize)]
pub struct Seeds {
    pub nodes: Vec<SeededNode>,
}

impl std::fmt::Display for Seeds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for node in &self.nodes {
            write!(f, "{node}")?;
        }
        writeln!(
            f,
            "Copy the new seeds onto the RBSEED stick (seeds/ keeps one per MAC)."
        )
    }
}

/// `relish image seed <dir> MAC@IP...`: add machines to a running cluster,
/// with seeds for the RBSEED stick.
pub async fn run_add(
    directory: &Path,
    machines: &[(String, IpAddr)],
    token_ttl: Duration,
    ssh_key: Option<Vec<u8>>,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let nodes = add(directory, machines, token_ttl, ssh_key)
        .await?
        .iter()
        .map(|(node, seed)| SeededNode::new(node, seed))
        .collect();
    print_output(&Seeds { nodes }, output)
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

/// What `relish cluster create --bare-metal` answers.
#[derive(Debug, Clone, Serialize)]
pub struct ClusterCreated {
    pub cluster: String,
    pub directory: PathBuf,
    pub root_ca_fingerprint: String,
    /// How many voters the council grows to.
    pub council_size: crate::council::CouncilSize,
    /// Node 1 first.
    pub nodes: Vec<SeededNode>,
    /// The only copy of the root CA key, and the identity that opens it.
    pub root_backup: super::quickstart::security::RootBackupFiles,
    /// The master key's directory, to back up.
    pub secrets: PathBuf,
    pub context: ContextReport,
}

impl ClusterCreated {
    /// The report for `created`, made from `options`.
    pub fn new(created: &Created, options: &CreateOptions, context: ContextReport) -> Self {
        Self {
            cluster: created.fleet.cluster.clone(),
            directory: options.directory.clone(),
            root_ca_fingerprint: created.fleet.ca_fingerprint.clone(),
            council_size: options.council_size,
            nodes: created
                .fleet
                .nodes
                .iter()
                .zip(&created.seeds)
                .map(|(node, seed)| SeededNode::new(node, seed))
                .collect(),
            root_backup: created.root_backup.clone(),
            secrets: options.directory.join("secrets"),
            context,
        }
    }
}

impl std::fmt::Display for ClusterCreated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let dir = self.directory.display();
        writeln!(f, "cluster {} created in {dir}", self.cluster)?;
        writeln!(f, "  root CA: {}", self.root_ca_fingerprint)?;
        writeln!(f, "  council: up to {} voters", self.council_size)?;
        for node in &self.nodes {
            write!(f, "{node}")?;
        }
        writeln!(f)?;
        writeln!(
            f,
            "The root CA key is only in {}, which {} opens. Move both offline, apart \
             from each other: no node holds either, and `relish ca rotate` needs them.",
            self.root_backup.backup.display(),
            self.root_backup.identity.display()
        )?;
        writeln!(
            f,
            "Copy {dir}/stick/seeds onto a USB stick labelled RBSEED (FAT32), then boot each machine with it."
        )?;
        write!(f, "{}", self.context)
    }
}

/// `relish cluster create --bare-metal`: create the cluster, have its
/// master key backed up, save its context (unless another cluster's
/// context is in the way), and say what to do next.
pub fn run_create(
    options: &CreateOptions,
    yes: bool,
    output: OutputFormat,
) -> Result<(), RelishError> {
    use std::io::IsTerminal;
    let secrets = options.directory.join("secrets");
    let check = backup_check(yes, std::io::stdin().is_terminal(), &secrets)?;
    let created = create_confirmed(options, || back_up(check, &secrets, output))?;
    let context = adopt(&created, &options.directory)?;
    print_output(&ClusterCreated::new(&created, options, context), output)
}

/// [`create`], then `confirm` the master-key backup. If the operator
/// doesn't confirm it (Ctrl-D, or a pipe that closes), remove everything
/// `create` wrote: no seed has left this machine, so the cluster never
/// existed, and keeping a master key nobody has a copy of helps no one. It
/// also lets the same command run again, rather than finding a directory
/// that "already holds a cluster". Anything the directory held before is
/// left alone.
pub fn create_confirmed(
    options: &CreateOptions,
    confirm: impl FnOnce() -> Result<(), RelishError>,
) -> Result<Created, RelishError> {
    let dir = &options.directory;
    let before = match std::fs::read_dir(dir) {
        Ok(entries) => Some(
            entries
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<Result<BTreeSet<_>, _>>()?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let created = create(options)?;
    if let Err(error) = confirm() {
        remove_what_create_made(dir, before.as_ref());
        return Err(error);
    }
    Ok(created)
}

/// Remove `dir` if it's new (`before` is `None`), or else only the entries
/// that weren't in `before`. A failure is reported, not returned: the
/// caller is already returning the reason it stopped.
fn remove_what_create_made(dir: &Path, before: Option<&BTreeSet<OsString>>) {
    let remove = |path: &Path| {
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        if let Err(error) = removed {
            eprintln!("warning: couldn't remove {}: {error}", path.display());
        }
    };
    let Some(before) = before else {
        remove(dir);
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !before.contains(&entry.file_name()) {
            remove(&entry.path());
        }
    }
}

/// How a new cluster's master-key backup gets confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupCheck {
    /// `--yes`: say what to back up, but don't wait.
    Remind,
    /// Ask at the terminal, and wait for a yes.
    Ask,
}

/// Settle, before anything is created, how the master-key backup will be
/// confirmed. Without `--yes` there has to be a terminal to ask at, or a
/// script would make a cluster and then wait for an answer that never comes.
pub fn backup_check(yes: bool, terminal: bool, secrets: &Path) -> Result<BackupCheck, RelishError> {
    match (yes, terminal) {
        (true, _) => Ok(BackupCheck::Remind),
        (false, true) => Ok(BackupCheck::Ask),
        (false, false) => Err(RelishError::BackupConfirmationRequired {
            secrets: secrets.to_path_buf(),
        }),
    }
}

/// Tell the operator what to back up and, unless `--yes` said not to,
/// wait at the terminal until they've done it. With `-o json` or `yaml`
/// the reminder and the question go to stderr, leaving stdout for data.
pub fn back_up(
    check: BackupCheck,
    secrets: &Path,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let input = &mut std::io::stdin().lock();
    match (check, output) {
        (BackupCheck::Remind, _) => {
            progress(output, backup_reminder(secrets));
            progress(output, "(--yes: not waiting for you to confirm the backup)");
            Ok(())
        }
        (BackupCheck::Ask, OutputFormat::Human) => {
            confirm_backup(input, &mut std::io::stdout(), secrets)
        }
        (BackupCheck::Ask, OutputFormat::Json | OutputFormat::Yaml) => {
            confirm_backup(input, &mut std::io::stderr(), secrets)
        }
    }
}

fn backup_reminder(secrets: &Path) -> String {
    format!(
        "Back up {}: it holds the master key, and `relish council recover` needs it \
         if the cluster ever loses its quorum. Keep it somewhere safe off this machine, \
         such as your password manager, but not with the root CA backup.",
        secrets.display()
    )
}

/// Show what to back up and ask until the operator answers yes. Input that
/// ends first (Ctrl-D, or a closed pipe) refuses: the cluster isn't
/// started with a master key nobody has confirmed a copy of.
pub fn confirm_backup(
    input: &mut impl std::io::BufRead,
    output: &mut impl std::io::Write,
    secrets: &Path,
) -> Result<(), RelishError> {
    writeln!(output, "{}", backup_reminder(secrets))?;
    loop {
        write!(output, "Type yes once it's backed up: ")?;
        output.flush()?;
        let mut answer = String::new();
        if input.read_line(&mut answer)? == 0 {
            writeln!(output)?;
            return Err(RelishError::BackupNotConfirmed {
                directory: secrets.parent().unwrap_or(secrets).to_path_buf(),
            });
        }
        if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            return Ok(());
        }
    }
}

/// How relish reaches a new cluster: whether its context was saved, and
/// what to export if another cluster's was in the way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextReport {
    /// Whether relish now talks to this cluster.
    pub adopted: bool,
    pub endpoint: String,
    pub ca_cert: PathBuf,
    pub token_file: PathBuf,
}

impl std::fmt::Display for ContextReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.adopted {
            return writeln!(
                f,
                "relish now talks to this cluster (node 1, {}).",
                self.endpoint
            );
        }
        writeln!(
            f,
            "Another cluster's relish context is in place, so it was kept. To use this one:"
        )?;
        writeln!(f, "  export RELIABURGER_ENDPOINT={}", self.endpoint)?;
        writeln!(f, "  export RELIABURGER_CA_CERT={}", self.ca_cert.display())?;
        writeln!(
            f,
            "  export RELIABURGER_TOKEN=$(cat {})",
            self.token_file.display()
        )
    }
}

/// Point relish at a newly created cluster, unless another cluster's
/// context is in the way.
pub fn adopt(created: &Created, directory: &Path) -> Result<ContextReport, RelishError> {
    let context = context(&created.fleet, directory, &created.admin_token)?;
    let path = super::local_context::default_path()?;
    let adopted = match super::local_context::LocalContext::load(&path) {
        Ok(None) => context.save(&path).map(|()| true)?,
        Ok(Some(existing)) if existing.owner == context.owner => {
            context.save(&path).map(|()| true)?
        }
        _ => false,
    };
    Ok(ContextReport {
        adopted,
        endpoint: context.endpoint,
        ca_cert: context.ca_cert,
        token_file: directory.join("secrets").join("admin.token"),
    })
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
            council_size: crate::council::CouncilSize::APPLIANCE,
            root_backup_keys: dir.join("operator-keys"),
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
        assert_eq!(
            config.council_size,
            Some(crate::council::CouncilSize::APPLIANCE),
            "node 1 commits the council size when it bootstraps"
        );
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
        assert_eq!(config.council_size, None);
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
        let recorded: Fleet =
            serde_json::from_slice(&std::fs::read(dir.path().join("fleet.json")).unwrap()).unwrap();
        assert_eq!(
            recorded.council_size,
            Some(crate::council::CouncilSize::APPLIANCE)
        );

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
    fn added_machines_take_the_next_names_and_refuse_clashes() {
        let fleet = Fleet {
            schema: 1,
            cluster: "home".into(),
            ca_fingerprint: "sha256:x".into(),
            operators: vec![],
            network: Some("192.168.1.0/24".into()),
            faults: false,
            council_size: None,
            nodes: vec![
                FleetNode {
                    name: "home-1".into(),
                    mac: "aa:aa:aa:aa:aa:01".into(),
                    address: "192.168.1.51".parse().unwrap(),
                },
                FleetNode {
                    name: "home-3".into(),
                    mac: "aa:aa:aa:aa:aa:03".into(),
                    address: "192.168.1.53".parse().unwrap(),
                },
            ],
        };
        let added = plan_additions(
            &fleet,
            &[
                parse_machine("aa:aa:aa:aa:aa:04@192.168.1.54").unwrap(),
                parse_machine("aa:aa:aa:aa:aa:05@10.0.0.5").unwrap(),
            ],
        )
        .unwrap();
        let names: Vec<&str> = added.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["home-4", "home-5"]);
        assert!(
            plan_additions(
                &fleet,
                &[parse_machine("aa:aa:aa:aa:aa:01@192.168.1.99").unwrap()]
            )
            .is_err()
        );
        assert!(
            plan_additions(
                &fleet,
                &[parse_machine("bb:bb:bb:bb:bb:bb@192.168.1.53").unwrap()]
            )
            .is_err()
        );
        assert!(
            plan_additions(
                &fleet,
                &[
                    parse_machine("cc:cc:cc:cc:cc:01@192.168.1.60").unwrap(),
                    parse_machine("cc:cc:cc:cc:cc:01@192.168.1.61").unwrap()
                ]
            )
            .is_err(),
            "the same MAC twice"
        );
        assert!(admitted(&fleet, "192.168.1.54".parse().unwrap()));
        assert!(!admitted(&fleet, "10.0.0.5".parse().unwrap()));
        assert!(!admitted(
            &Fleet {
                network: None,
                ..fleet
            },
            "192.168.1.54".parse().unwrap()
        ));
    }

    #[test]
    fn an_unadmitted_machine_s_warning_says_claiming_needs_no_bootstrap_peers() {
        // A claim opens a join window on every node, so only a machine
        // booted from its stick needs `bootstrap_peers` (G2).
        let warning = not_admitted_warning("10.0.0.5".parse().unwrap());
        assert!(warning.contains("10.0.0.5"), "{warning}");
        assert!(warning.contains("relish machines claim"), "{warning}");
        assert!(warning.contains("join window"), "{warning}");
        assert!(warning.contains("bootstrap_peers"), "{warning}");
        assert!(warning.contains("stick"), "{warning}");
    }

    #[test]
    fn create_records_the_council_size_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let three = crate::council::CouncilSize::new(3).unwrap();
        let created = create(&CreateOptions {
            council_size: three,
            ..options(dir.path(), &["d8:9e:f3:00:00:01@192.168.1.51"])
        })
        .unwrap();
        assert_eq!(created.fleet.council_size, Some(three));
        let Seed::V1 { config, .. } =
            Seed::from_tar_gz(&std::fs::read(&created.seeds[0]).unwrap()).unwrap()
        else {
            panic!("v1")
        };
        assert_eq!(config.council_size, Some(three));
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

    #[test]
    fn the_create_report_names_the_seeds_the_root_backup_and_how_to_connect() {
        let dir = tempfile::tempdir().unwrap();
        let options = options(
            dir.path(),
            &[
                "d8:9e:f3:00:00:01@192.168.1.51",
                "d8:9e:f3:00:00:02@192.168.1.52",
            ],
        );
        let created = create(&options).unwrap();
        let context = ContextReport {
            adopted: false,
            endpoint: "https://192.168.1.51:9117".into(),
            ca_cert: PathBuf::from("/c/root-ca.crt"),
            token_file: PathBuf::from("/c/secrets/admin.token"),
        };
        let report = ClusterCreated::new(&created, &options, context);

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["cluster"], "home");
        assert_eq!(json["root_ca_fingerprint"], created.fleet.ca_fingerprint);
        assert_eq!(json["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(json["nodes"][1]["mac"], "d8:9e:f3:00:00:02");
        assert_eq!(json["nodes"][1]["address"], "192.168.1.52");
        assert_eq!(
            json["nodes"][1]["seed"],
            created.seeds[1].display().to_string()
        );
        assert_eq!(
            json["root_backup"]["backup"],
            created.root_backup.backup.display().to_string()
        );
        assert_eq!(json["context"]["adopted"], false);
        assert_eq!(json["context"]["token_file"], "/c/secrets/admin.token");

        let text = report.to_string();
        assert!(text.starts_with(&format!(
            "cluster home created in {}\n  root CA: ",
            dir.path().display()
        )));
        assert!(text.contains(&format!(
            "  {} (d8:9e:f3:00:00:02 at 192.168.1.52): {}\n",
            created.fleet.nodes[1].name,
            created.seeds[1].display()
        )));
        assert!(text.contains("Move both offline"));
        assert!(text.ends_with("  export RELIABURGER_TOKEN=$(cat /c/secrets/admin.token)\n"));
    }

    #[test]
    fn the_seed_report_lists_each_new_machine_then_what_to_do_with_the_seeds() {
        let seeds = Seeds {
            nodes: vec![SeededNode {
                name: "home-4".into(),
                mac: "d8:9e:f3:00:00:04".into(),
                address: "192.168.1.54".parse().unwrap(),
                seed: PathBuf::from("/c/stick/seeds/d8-9e-f3-00-00-04.seed"),
            }],
        };
        assert_eq!(
            seeds.to_string(),
            "  home-4 (d8:9e:f3:00:00:04 at 192.168.1.54): /c/stick/seeds/d8-9e-f3-00-00-04.seed\n\
             Copy the new seeds onto the RBSEED stick (seeds/ keeps one per MAC).\n"
        );
        assert_eq!(
            serde_json::to_value(&seeds).unwrap()["nodes"][0]["name"],
            "home-4"
        );
    }

    #[test]
    fn an_adopted_context_says_relish_talks_to_the_new_cluster() {
        let context = ContextReport {
            adopted: true,
            endpoint: "https://192.168.1.51:9117".into(),
            ca_cert: PathBuf::from("/c/root-ca.crt"),
            token_file: PathBuf::from("/c/secrets/admin.token"),
        };
        assert_eq!(
            context.to_string(),
            "relish now talks to this cluster (node 1, https://192.168.1.51:9117).\n"
        );
    }

    #[test]
    fn yes_skips_the_backup_question_with_or_without_a_terminal() {
        let secrets = Path::new("/home/me/home-cluster/secrets");
        for terminal in [true, false] {
            assert_eq!(
                backup_check(true, terminal, secrets).unwrap(),
                BackupCheck::Remind
            );
        }
    }

    #[test]
    fn a_terminal_without_yes_is_asked_about_the_backup() {
        let secrets = Path::new("/home/me/home-cluster/secrets");
        assert_eq!(
            backup_check(false, true, secrets).unwrap(),
            BackupCheck::Ask
        );
    }

    #[test]
    fn no_terminal_and_no_yes_is_refused_rather_than_left_waiting() {
        let secrets = Path::new("/home/me/home-cluster/secrets");
        let error = backup_check(false, false, secrets).unwrap_err();
        assert!(matches!(
            error,
            RelishError::BackupConfirmationRequired { .. }
        ));
        let message = error.to_string();
        assert!(message.contains("--yes"), "{message}");
        assert!(message.contains("master key"), "{message}");
        assert!(
            message.contains("/home/me/home-cluster/secrets"),
            "{message}"
        );
        assert!(message.starts_with(char::is_lowercase), "{message}");
    }

    fn ask(answers: &str) -> (Result<(), RelishError>, String) {
        let mut input = std::io::Cursor::new(answers.as_bytes().to_vec());
        let mut output = Vec::new();
        let result = confirm_backup(
            &mut input,
            &mut output,
            Path::new("/home/me/home-cluster/secrets"),
        );
        (result, String::from_utf8(output).unwrap())
    }

    #[test]
    fn the_backup_prompt_names_what_to_back_up_and_takes_yes() {
        for answer in ["yes\n", "y\n", "YES\n", "  Yes  \n"] {
            let (result, shown) = ask(answer);
            assert!(result.is_ok(), "{answer:?}");
            assert!(shown.contains("/home/me/home-cluster/secrets"), "{shown}");
            assert!(shown.contains("master key"), "{shown}");
            assert!(shown.contains("relish council recover"), "{shown}");
        }
    }

    #[test]
    fn the_backup_prompt_keeps_asking_until_yes() {
        let (result, shown) = ask("\nno\nlater\nyes\n");
        assert!(result.is_ok());
        assert_eq!(shown.matches("Type yes").count(), 4, "{shown}");
    }

    #[test]
    fn the_backup_prompt_refuses_when_input_ends_without_yes() {
        for answers in ["", "no\n", "no\nnope"] {
            let (result, _) = ask(answers);
            let error = result.unwrap_err();
            assert!(
                matches!(error, RelishError::BackupNotConfirmed { .. }),
                "{answers:?}: {error}"
            );
            let message = error.to_string();
            assert!(message.starts_with(char::is_lowercase), "{message}");
            assert!(message.contains("/home/me/home-cluster"), "{message}");
        }
    }

    /// The operator pressing Ctrl-D at the backup question.
    fn ctrl_d(secrets: &Path) -> Result<(), RelishError> {
        confirm_backup(
            &mut std::io::Cursor::new(Vec::new()),
            &mut Vec::new(),
            secrets,
        )
    }

    const ONE_MACHINE: &[&str] = &["d8:9e:f3:00:00:01@192.168.1.51"];

    #[test]
    fn a_cluster_whose_backup_is_not_confirmed_leaves_nothing_behind() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("home-cluster");
        let mut asked_with_keys_written = false;
        let error = create_confirmed(&options(&dir, ONE_MACHINE), || {
            // The question comes once the keys exist, so the operator can
            // copy them before answering.
            asked_with_keys_written = dir.join("fleet.json").is_file()
                && std::fs::read_dir(dir.join("secrets")).unwrap().count() > 0;
            ctrl_d(&dir.join("secrets"))
        })
        .unwrap_err();
        assert!(asked_with_keys_written);
        assert!(
            matches!(error, RelishError::BackupNotConfirmed { .. }),
            "{error}"
        );
        assert!(!dir.exists(), "left behind: {:?}", std::fs::read_dir(&dir));
        let message = error.to_string();
        assert!(message.contains("create it again"), "{message}");

        // So the same command works straight away, rather than finding
        // "already holds a cluster".
        create_confirmed(&options(&dir, ONE_MACHINE), || Ok(())).unwrap();
        assert!(dir.join("fleet.json").is_file());
    }

    #[test]
    fn a_refused_backup_removes_only_what_create_made_in_an_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"mine").unwrap();
        std::fs::create_dir(dir.path().join("photos")).unwrap();

        create_confirmed(&options(dir.path(), ONE_MACHINE), || {
            ctrl_d(&dir.path().join("secrets"))
        })
        .unwrap_err();
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["notes.txt", "photos"]);
        assert_eq!(
            std::fs::read(dir.path().join("notes.txt")).unwrap(),
            b"mine"
        );
    }

    #[test]
    fn a_confirmed_backup_keeps_the_cluster() {
        let dir = tempfile::tempdir().unwrap();
        let created = create_confirmed(&options(dir.path(), ONE_MACHINE), || Ok(())).unwrap();
        assert_eq!(created.fleet.nodes.len(), 1);
        assert!(dir.path().join("fleet.json").is_file());
        assert!(dir.path().join("secrets").is_dir());
    }
}
