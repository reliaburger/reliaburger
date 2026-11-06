//! `bun appliance prepare`: from a seed to a node that bun can start.
//!
//! The steps are a pure function of the seed and what's already on disk
//! ([`next_step`]), so every transition is a unit test, and a node that
//! reboots halfway (after enrolling, say, but before it had the master
//! key) carries on from where it stopped instead of enrolling again with a
//! token that's already spent. `node.toml` is always written last.

use std::collections::BTreeMap;
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::seed::{Seed, SeedConfig, SeedError, SeedRole};

/// Where prepare reads and writes. [`Paths::system`] in production; tests
/// point it at a temporary directory.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `/etc/reliaburger`: node.toml, the identity, the master key, and a
    /// copy of the seed's `seed.toml` for the steps after enrolling.
    pub config_dir: PathBuf,
    /// systemd's `$CREDENTIALS_DIRECTORY`, holding `reliaburger.seed`.
    pub credentials_dir: Option<PathBuf>,
    /// The `RBSEED` stick's device node.
    pub stick_device: PathBuf,
    /// Where the stick is mounted while it's read.
    pub stick_mount: PathBuf,
    /// How long to wait for the stick to appear: USB storage can take a few
    /// seconds after boot. Every unseeded boot pays it.
    pub stick_wait: Duration,
    /// `/sys/class/net`, for this machine's MAC addresses.
    pub net_dir: PathBuf,
    /// Where an unclaimed machine keeps its claim key and a claimed seed
    /// (`super::claim`). `None` runs no claim server: no seed then means
    /// bun runs standalone.
    pub claim_dir: Option<PathBuf>,
}

impl Paths {
    pub fn system() -> Self {
        Paths {
            config_dir: PathBuf::from(super::seed::CONFIG_DIR),
            credentials_dir: std::env::var_os("CREDENTIALS_DIRECTORY").map(PathBuf::from),
            stick_device: PathBuf::from("/dev/disk/by-label/RBSEED"),
            stick_mount: PathBuf::from("/run/reliaburger-seed-stick"),
            stick_wait: Duration::from_secs(10),
            net_dir: PathBuf::from("/sys/class/net"),
            claim_dir: Some(PathBuf::from("/var/lib/reliaburger/claim")),
        }
    }

    fn node_toml(&self) -> PathBuf {
        self.config_dir.join("node.toml")
    }
    fn identity_dir(&self) -> PathBuf {
        self.config_dir.join("identity")
    }
    fn master_key(&self) -> PathBuf {
        self.config_dir.join("master.key")
    }
    fn kept_seed(&self) -> PathBuf {
        self.config_dir.join("seed.toml")
    }
}

/// What's on the node's disk already.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Disk {
    pub node_toml: bool,
    pub identity: bool,
    pub master_key: bool,
}

impl Disk {
    pub fn read(paths: &Paths) -> Self {
        Disk {
            node_toml: paths.node_toml().is_file(),
            identity: paths.identity_dir().join("bundle.committed").is_file(),
            master_key: paths.master_key().is_file(),
        }
    }
}

/// The next thing prepare does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// node.toml exists: bun can start.
    Done,
    /// No seed and nothing on disk: bun runs standalone.
    NoSeed,
    /// A spike seed: install its files as they are.
    InstallLegacy,
    /// A create seed: install the bootstrap material.
    InstallBootstrap,
    /// A join seed and no identity yet: enrol with the token.
    Enrol,
    /// Enrolled, no master key yet: fetch it with the new certificate.
    FetchMasterKey,
    /// Everything but node.toml: write it.
    WriteConfig,
}

/// The step to take, given the seed (if any) and the disk.
pub fn next_step(seed: Option<&Seed>, disk: Disk) -> Step {
    if disk.node_toml {
        return Step::Done;
    }
    match seed {
        None => Step::NoSeed,
        Some(Seed::Legacy { .. }) => Step::InstallLegacy,
        Some(Seed::V1 { config, .. }) => match (config.role, disk.identity, disk.master_key) {
            (SeedRole::Create, true, true) => Step::WriteConfig,
            (SeedRole::Create, _, _) => Step::InstallBootstrap,
            (SeedRole::Join, false, _) => Step::Enrol,
            (SeedRole::Join, true, false) => Step::FetchMasterKey,
            (SeedRole::Join, true, true) => Step::WriteConfig,
        },
    }
}

/// Why prepare stopped.
#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error("{0}")]
    Seed(#[from] SeedError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("enrolling as {node} failed for good: {reason}")]
    Enrol { node: String, reason: String },
    #[error("this node has no address on its default route yet")]
    NoAddress,
    #[error("{0}")]
    Config(String),
}

/// Where the seed came from, for the log and for wiping a create seed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Posted by `relish machines claim`.
    Claim,
    Credential,
    Stick {
        file: String,
    },
    /// The copy of `seed.toml` kept from an earlier boot.
    Kept,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Claim => write!(f, "a claim over the LAN"),
            Source::Credential => write!(f, "the reliaburger.seed credential"),
            Source::Stick { file } => write!(f, "the RBSEED stick ({file})"),
            Source::Kept => write!(f, "the seed kept from an earlier boot"),
        }
    }
}

/// Run every step until the node is ready (or has no seed). Retries what
/// the network can fix; stops on what it can't, such as a spent token.
pub async fn run(paths: &Paths) -> Result<Step, PrepareError> {
    if Disk::read(paths).node_toml {
        say("node.toml is in place");
        return Ok(Step::Done);
    }
    let mut found = find_seed(paths)?;
    if found.is_none()
        && let Some(claim_dir) = &paths.claim_dir
    {
        wait_for_claim(paths, claim_dir).await?;
        found = find_seed(paths)?;
    }
    let (seed, source) = match found {
        Some((seed, source)) => (Some(seed), Some(source)),
        None => (None, None),
    };
    if let Some(source) = &source {
        say(&format!("seed from {source}"));
    }
    loop {
        let step = next_step(seed.as_ref(), Disk::read(paths));
        match (step, &seed) {
            (Step::Done, _) => {
                say("node.toml is in place");
                if let Some(claim_dir) = &paths.claim_dir {
                    // The claimed seed may hold the master key; its files are
                    // installed now.
                    let _ = std::fs::remove_file(claim_dir.join("claimed.seed"));
                }
                if let (Some(Source::Stick { file }), Some(Seed::V1 { config, .. })) =
                    (&source, &seed)
                    && config.role == SeedRole::Create
                {
                    wipe_from_stick(paths, file);
                }
                return Ok(Step::Done);
            }
            (Step::NoSeed, _) => {
                say("no seed; bun runs standalone");
                return Ok(Step::NoSeed);
            }
            (Step::InstallLegacy, Some(Seed::Legacy { files })) => {
                install_files(paths, files)?;
                install_ssh_key(paths);
            }
            (Step::InstallBootstrap, Some(Seed::V1 { config, files })) => {
                if super::seed::CREATE_FILES
                    .iter()
                    .any(|file| !files.contains_key(*file))
                {
                    // Only the kept seed.toml is left (the stick was wiped or
                    // removed before the files landed): nothing to install.
                    return Err(PrepareError::Config(
                        "the create seed's bootstrap files are gone; seed this node again".into(),
                    ));
                }
                keep_seed(paths, config)?;
                install_files(paths, files)?;
                install_ssh_key(paths);
            }
            (Step::Enrol, Some(Seed::V1 { config, files })) => {
                keep_seed(paths, config)?;
                write_private(
                    &paths.config_dir.join("authorized_keys"),
                    files.get("authorized_keys"),
                )?;
                install_ssh_key(paths);
                enrol(paths, config).await?;
            }
            (Step::FetchMasterKey, Some(Seed::V1 { config, .. })) => {
                fetch_master_key(paths, config).await?;
            }
            (Step::WriteConfig, Some(Seed::V1 { config, .. })) => {
                let advertise = advertise_address(config).await?;
                let node = config.node_config(advertise);
                node.validate()
                    .map_err(|e| PrepareError::Config(e.to_string()))?;
                let text = toml::to_string_pretty(&node)
                    .map_err(|e| PrepareError::Config(e.to_string()))?;
                write_atomic(&paths.node_toml(), text.as_bytes(), 0o600)?;
                say(&format!(
                    "{} is ready to {} {} as {} on {advertise}",
                    paths.node_toml().display(),
                    if config.role == SeedRole::Create {
                        "create"
                    } else {
                        "join"
                    },
                    config.cluster,
                    config.node,
                ));
            }
            (step, _) => unreachable!("next_step returned {step:?} for this seed"),
        }
    }
}

/// The seed, from (in order) the credential, the stick, or the copy kept
/// from an earlier boot.
fn find_seed(paths: &Paths) -> Result<Option<(Seed, Source)>, PrepareError> {
    if let Some(claim_dir) = &paths.claim_dir {
        let path = claim_dir.join("claimed.seed");
        if path.is_file() {
            return Ok(Some((
                Seed::from_tar_gz(&std::fs::read(path)?)?,
                Source::Claim,
            )));
        }
    }
    if let Some(dir) = &paths.credentials_dir {
        let path = dir.join("reliaburger.seed");
        if path.is_file() {
            return Ok(Some((
                Seed::from_tar_gz(&std::fs::read(path)?)?,
                Source::Credential,
            )));
        }
    }
    if let Some((bytes, file)) = read_stick(paths) {
        return Ok(Some((Seed::from_tar_gz(&bytes)?, Source::Stick { file })));
    }
    let kept = paths.kept_seed();
    if kept.is_file() {
        let text = std::fs::read_to_string(kept)?;
        let config: SeedConfig =
            toml::from_str(&text).map_err(|e| SeedError::Config(e.to_string()))?;
        return Ok(Some((
            Seed::V1 {
                config: Box::new(config),
                files: BTreeMap::new(),
            },
            Source::Kept,
        )));
    }
    Ok(None)
}

/// No seed anywhere: become unclaimed until `relish machines claim` posts one.
async fn wait_for_claim(paths: &Paths, claim_dir: &Path) -> Result<(), PrepareError> {
    let key = super::claim::ClaimKey::load_or_create(claim_dir)?;
    let info = super::claim::MachineInfo {
        macs: stick_names(&paths.net_dir)
            .into_iter()
            .map(|name| name.replace('-', ":"))
            .collect(),
        arch: std::env::consts::ARCH.to_string(),
        os_version: std::fs::read_to_string("/usr/lib/os-release")
            .ok()
            .and_then(|text| {
                text.lines()
                    .find_map(|line| line.strip_prefix("IMAGE_VERSION="))
                    .map(|v| v.trim_matches('"').to_string())
            }),
        fingerprint: key.fingerprint(),
    };
    say(&format!(
        "no seed: unclaimed, claim key {} on port {}",
        super::claim::short_fingerprint(&info.fingerprint),
        super::claim::CLAIM_PORT
    ));
    super::claim::serve_until_claimed(&key, info, claim_dir.join("claimed.seed")).await?;
    say("claimed");
    Ok(())
}

/// This machine's MAC addresses as the stick names them (`aa-bb-…`).
pub fn stick_names(net_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(net_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("address")).ok())
        .map(|mac| mac.trim().to_ascii_lowercase().replace(':', "-"))
        .filter(|mac| !mac.is_empty() && mac != "00-00-00-00-00-00")
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Mount the stick read-only (waiting [`Paths::stick_wait`] for it to appear) and read
/// `seeds/<mac>.seed` for one of this machine's NICs, or `reliaburger.seed`.
fn read_stick(paths: &Paths) -> Option<(Vec<u8>, String)> {
    let deadline = std::time::Instant::now() + paths.stick_wait;
    while !paths.stick_device.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
    }
    if !paths.stick_device.exists() {
        return None;
    }
    let mount = &paths.stick_mount;
    std::fs::create_dir_all(mount).ok()?;
    if !run_command(
        "mount",
        &[
            "-o",
            "ro",
            &paths.stick_device.to_string_lossy(),
            &mount.to_string_lossy(),
        ],
    ) {
        say("couldn't mount the RBSEED stick");
        return None;
    }
    let mut found = None;
    for name in stick_names(&paths.net_dir) {
        let file = format!("seeds/{name}.seed");
        if let Ok(bytes) = std::fs::read(mount.join(&file)) {
            found = Some((bytes, file));
            break;
        }
    }
    if found.is_none()
        && let Ok(bytes) = std::fs::read(mount.join("reliaburger.seed"))
    {
        found = Some((bytes, "reliaburger.seed".to_string()));
    }
    if found.is_none() {
        say("the RBSEED stick has no seed for this machine's MAC addresses");
    }
    run_command("umount", &[&mount.to_string_lossy()]);
    found
}

/// Overwrite a create seed on the stick and delete it: it carries the
/// cluster's master key, which mustn't outlive its job on removable media.
fn wipe_from_stick(paths: &Paths, file: &str) {
    let mount = &paths.stick_mount;
    if !paths.stick_device.exists()
        || !run_command(
            "mount",
            &[
                "-o",
                "rw",
                &paths.stick_device.to_string_lossy(),
                &mount.to_string_lossy(),
            ],
        )
    {
        say(&format!(
            "couldn't remount the RBSEED stick to wipe {file}; remove it by hand"
        ));
        return;
    }
    let path = mount.join(file);
    let wiped = std::fs::metadata(&path)
        .and_then(|meta| {
            let mut handle = std::fs::OpenOptions::new().write(true).open(&path)?;
            handle.write_all(&vec![0u8; meta.len() as usize])?;
            handle.sync_all()?;
            std::fs::remove_file(&path)
        })
        .is_ok();
    run_command("sync", &[]);
    run_command("umount", &[&mount.to_string_lossy()]);
    say(&if wiped {
        format!("wiped {file} from the RBSEED stick: it held the master key")
    } else {
        format!("couldn't wipe {file} from the RBSEED stick; remove it by hand")
    });
}

/// Enrol with the seed's token through each member in turn, until one
/// answers. A refusal (a spent, expired or mismatched token, another CA)
/// won't change by retrying, so it ends prepare.
async fn enrol(paths: &Paths, config: &SeedConfig) -> Result<(), PrepareError> {
    let join = config
        .join
        .as_ref()
        .ok_or_else(|| PrepareError::Config("no [join]".into()))?;
    let mut delay = Duration::from_secs(2);
    loop {
        for member in &join.members {
            let base = format!("https://{}", std::net::SocketAddr::new(*member, 9117));
            match crate::sesame::join::enrol(
                &base,
                &join.token,
                &config.node,
                Some(&join.ca_fingerprint),
            )
            .await
            {
                Ok(identity) => {
                    crate::sesame::identity_store::save(&paths.identity_dir(), &identity)
                        .map_err(|e| PrepareError::Config(format!("saving the identity: {e}")))?;
                    say(&format!("enrolled as {} through {member}", config.node));
                    return Ok(());
                }
                Err(error) if is_final(&error) => {
                    return Err(PrepareError::Enrol {
                        node: config.node.clone(),
                        reason: error.to_string(),
                    });
                }
                Err(error) => say(&format!(
                    "enrolling through {member}: {error}; trying again"
                )),
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

/// A refusal that retrying can't fix. A member that isn't ready yet (no
/// council, a follower that can't write) answers 503 or 400 with a
/// "leader" hint, and is worth asking again.
fn is_final(error: &crate::sesame::join::JoinClientError) -> bool {
    use crate::sesame::join::JoinClientError;
    match error {
        JoinClientError::FingerprintMismatch { .. } | JoinClientError::Incompatible(_) => true,
        JoinClientError::Rejected(reason) => {
            let reason = reason.to_ascii_lowercase();
            [
                "consumed",
                "expired",
                "invalid join token",
                "different node id",
                "retired",
            ]
            .iter()
            .any(|word| reason.contains(word))
        }
        JoinClientError::Transport(_) | JoinClientError::Malformed(_) => false,
    }
}

/// Fetch the master key with the identity just enrolled (G1), retrying
/// until a member answers.
async fn fetch_master_key(paths: &Paths, config: &SeedConfig) -> Result<(), PrepareError> {
    let join = config
        .join
        .as_ref()
        .ok_or_else(|| PrepareError::Config("no [join]".into()))?;
    let identity = crate::sesame::identity_store::load(&paths.identity_dir())
        .map_err(|e| PrepareError::Config(format!("loading the identity: {e}")))?
        .ok_or_else(|| PrepareError::Config("the identity vanished".into()))?;
    let mut delay = Duration::from_secs(2);
    loop {
        for member in &join.members {
            let base = format!("https://{}", std::net::SocketAddr::new(*member, 9117));
            match crate::sesame::join::fetch_master_key(&base, &identity).await {
                Ok(key) => {
                    write_atomic(&paths.master_key(), hex::encode(key).as_bytes(), 0o600)?;
                    say(&format!("fetched the master key from {member}"));
                    return Ok(());
                }
                Err(error) => say(&format!(
                    "fetching the master key from {member}: {error}; trying again"
                )),
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

/// The seed's address, or the default route's, waiting up to a minute for
/// DHCP.
async fn advertise_address(config: &SeedConfig) -> Result<IpAddr, PrepareError> {
    if let Some(address) = config.advertise {
        return Ok(address);
    }
    for _ in 0..30 {
        if let Some(address) = super::address::detect() {
            return Ok(address);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(PrepareError::NoAddress)
}

/// Keep `seed.toml` (with its now-spent token) for the steps after enrol,
/// which may run on a later boot without the stick.
fn keep_seed(paths: &Paths, config: &SeedConfig) -> Result<(), PrepareError> {
    let text = toml::to_string_pretty(config).map_err(|e| PrepareError::Config(e.to_string()))?;
    write_atomic(&paths.kept_seed(), text.as_bytes(), 0o600)?;
    Ok(())
}

/// Write the seed's files under the config directory, node.toml last.
fn install_files(paths: &Paths, files: &BTreeMap<String, Vec<u8>>) -> Result<(), PrepareError> {
    let mut ordered: Vec<(&String, &Vec<u8>)> = files.iter().collect();
    // node.toml, then bundle.committed, last: each marks a step as done.
    ordered.sort_by_key(|(name, _)| match name.as_str() {
        "node.toml" => 2,
        "identity/bundle.committed" => 1,
        _ => 0,
    });
    for (name, data) in ordered {
        write_atomic(&paths.config_dir.join(name), data, 0o600)?;
    }
    Ok(())
}

fn write_private(path: &Path, data: Option<&Vec<u8>>) -> Result<(), PrepareError> {
    if let Some(data) = data {
        write_atomic(path, data, 0o600)?;
    }
    Ok(())
}

/// Write beside the target and rename into place, with owner-only modes.
pub(crate) fn write_atomic(path: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let temporary = parent.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&temporary)?;
    file.write_all(data)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)
}

/// A seed's `authorized_keys` (the lab image only: it has sshd) becomes
/// root's, and starts sshd.
fn install_ssh_key(paths: &Paths) {
    let keys = paths.config_dir.join("authorized_keys");
    if !keys.is_file() || paths.config_dir != Path::new(super::seed::CONFIG_DIR) {
        return;
    }
    if std::fs::create_dir_all("/root/.ssh").is_ok()
        && std::fs::copy(&keys, "/root/.ssh/authorized_keys").is_ok()
    {
        run_command("systemctl", &["--no-block", "start", "ssh.socket"]);
        say("seed installed root's SSH key");
    }
}

fn run_command(program: &str, args: &[&str]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .status()
        .is_ok_and(|status| status.success())
}

/// Console lines carry the prefix the boot test and the lab grep for.
fn say(message: &str) {
    println!("reliaburger: {message}");
}

#[cfg(test)]
mod tests {
    use super::super::seed::tests::{JOIN_TOML, tarball};
    use super::*;

    fn join_seed() -> Seed {
        Seed::from_tar_gz(&tarball(&[("seed.toml", JOIN_TOML.as_bytes())])).unwrap()
    }

    fn create_seed() -> Seed {
        let toml = "schema = 1\ncluster = \"home\"\nnode = \"home-1\"\nrole = \"create\"\n";
        Seed::from_tar_gz(&tarball(&[
            ("seed.toml", toml.as_bytes()),
            ("master.key", b"00"),
            ("security-bootstrap.json", b"{}"),
            ("identity/bundle.committed", b""),
        ]))
        .unwrap()
    }

    fn disk(node_toml: bool, identity: bool, master_key: bool) -> Disk {
        Disk {
            node_toml,
            identity,
            master_key,
        }
    }

    #[test]
    fn a_prepared_node_is_done_whatever_the_seed() {
        for seed in [None, Some(join_seed()), Some(create_seed())] {
            assert_eq!(
                next_step(seed.as_ref(), disk(true, false, false)),
                Step::Done
            );
        }
    }

    #[test]
    fn no_seed_runs_standalone() {
        assert_eq!(next_step(None, Disk::default()), Step::NoSeed);
    }

    #[test]
    fn a_joiner_enrols_then_fetches_the_key_then_writes_its_config() {
        let seed = join_seed();
        assert_eq!(
            next_step(Some(&seed), disk(false, false, false)),
            Step::Enrol
        );
        assert_eq!(
            next_step(Some(&seed), disk(false, true, false)),
            Step::FetchMasterKey
        );
        assert_eq!(
            next_step(Some(&seed), disk(false, true, true)),
            Step::WriteConfig
        );
    }

    #[test]
    fn a_creator_installs_its_bootstrap_then_writes_its_config() {
        let seed = create_seed();
        assert_eq!(
            next_step(Some(&seed), disk(false, false, false)),
            Step::InstallBootstrap
        );
        assert_eq!(
            next_step(Some(&seed), disk(false, true, false)),
            Step::InstallBootstrap
        );
        assert_eq!(
            next_step(Some(&seed), disk(false, true, true)),
            Step::WriteConfig
        );
    }

    #[test]
    fn a_spike_seed_is_installed_as_is() {
        let seed = Seed::from_tar_gz(&tarball(&[("node.toml", b"[node]\n")])).unwrap();
        assert_eq!(next_step(Some(&seed), Disk::default()), Step::InstallLegacy);
    }

    fn paths(dir: &Path) -> Paths {
        Paths {
            config_dir: dir.join("etc"),
            credentials_dir: Some(dir.join("creds")),
            stick_device: dir.join("no-stick"),
            stick_mount: dir.join("mnt"),
            stick_wait: Duration::ZERO,
            net_dir: dir.join("net"),
            claim_dir: None,
        }
    }

    #[tokio::test]
    async fn a_create_seed_becomes_a_node_ready_to_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let toml = "schema = 1\ncluster = \"home\"\nnode = \"home-1\"\nrole = \"create\"\nadvertise = \"10.0.0.5\"\n";
        std::fs::create_dir_all(paths.credentials_dir.as_ref().unwrap()).unwrap();
        std::fs::write(
            paths
                .credentials_dir
                .as_ref()
                .unwrap()
                .join("reliaburger.seed"),
            tarball(&[
                ("seed.toml", toml.as_bytes()),
                ("master.key", b"ab"),
                ("security-bootstrap.json", b"{}"),
                ("identity/node.crt", b"c"),
                ("identity/bundle.committed", b""),
            ]),
        )
        .unwrap();
        assert_eq!(run(&paths).await.unwrap(), Step::Done);
        let node: crate::config::node::NodeConfig =
            toml::from_str(&std::fs::read_to_string(paths.config_dir.join("node.toml")).unwrap())
                .unwrap();
        assert_eq!(node.network.advertise_address.as_deref(), Some("10.0.0.5"));
        assert!(node.security.bootstrap_path.is_some());
        assert_eq!(
            std::fs::read(paths.config_dir.join("master.key")).unwrap(),
            b"ab"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(paths.config_dir.join("master.key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // A second boot changes nothing.
        assert_eq!(run(&paths).await.unwrap(), Step::Done);
    }

    #[tokio::test]
    async fn without_a_seed_nothing_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        assert_eq!(run(&paths).await.unwrap(), Step::NoSeed);
        assert!(!paths.config_dir.exists());
    }

    #[test]
    fn stick_names_are_this_machine_s_macs_with_dashes() {
        let dir = tempfile::tempdir().unwrap();
        for (iface, mac) in [
            ("lo", "00:00:00:00:00:00"),
            ("enp1s0", "D8:9E:F3:12:34:56"),
            ("wlan0", "aa:bb:cc:dd:ee:ff"),
        ] {
            std::fs::create_dir_all(dir.path().join(iface)).unwrap();
            std::fs::write(dir.path().join(iface).join("address"), format!("{mac}\n")).unwrap();
        }
        assert_eq!(
            stick_names(dir.path()),
            vec!["aa-bb-cc-dd-ee-ff", "d8-9e-f3-12-34-56"]
        );
    }

    #[test]
    fn spent_or_foreign_tokens_are_final_but_a_busy_member_is_not() {
        use crate::sesame::join::JoinClientError;
        assert!(is_final(&JoinClientError::Rejected(
            "400: join token has already been consumed".into()
        )));
        assert!(is_final(&JoinClientError::Rejected(
            "400: invalid join token".into()
        )));
        assert!(is_final(&JoinClientError::FingerprintMismatch {
            offered: "a".into(),
            expected: "b".into()
        }));
        assert!(!is_final(&JoinClientError::Rejected(
            "503: no council available".into()
        )));
        assert!(!is_final(&JoinClientError::Transport(
            "connection refused".into()
        )));
    }
}
