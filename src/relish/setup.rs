//! `relish setup` — guided install and first configuration.
//!
//! Walks a newcomer from "empty machine" to "running node": detect an
//! existing bun (on PATH, in the managed install directory, or already
//! running), install or upgrade it through the same dual-signed release
//! pipeline the node's self-upgrade uses, answer a few questions, and write
//! a starter `reliaburger.toml`.
//!
//! The stdin/stdout conversation lives in [`run`]; everything it decides is
//! delegated to small pure functions ([`decide_install`],
//! [`build_node_config`], [`render_config`], [`find_on_path`]) so the logic
//! is testable without a terminal.

use std::path::{Path, PathBuf};

use crate::config::node::NodeConfig;
use crate::upgrade::metadata::{self, ReleaseMetadata};
use crate::upgrade::signing::SignatureEnvelope;
use crate::upgrade::store::BinaryStore;
use crate::upgrade::{BinaryVersion, keys};

use super::RelishError;
use super::client::BunClient;

/// What the install step should do, given what is installed and what the
/// release channel offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallDecision {
    /// No bun anywhere: offer to install `latest`.
    NotInstalled { latest: BinaryVersion },
    /// The installed bun is current (or ahead of the release channel).
    UpToDate { current: BinaryVersion },
    /// The installed bun is behind: offer the upgrade.
    Upgradable {
        from: BinaryVersion,
        to: BinaryVersion,
    },
}

/// The user's answers to the configuration questions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupAnswers {
    /// Node name (`[node] name`).
    pub node_name: String,
    /// Stable cluster name and SPIFFE trust domain (`[cluster] name`).
    pub cluster_name: String,
    /// Address of an existing cluster member to join; `None` = single node.
    pub join_address: Option<String>,
    /// Root directory for node state; the storage paths live under it.
    pub data_dir: PathBuf,
    /// Start the `.internal` DNS responder (needs port 53, usually root).
    pub enable_dns: bool,
    /// Load the eBPF data path (Linux only, needs root).
    pub enable_ebpf: bool,
    /// Bind the ingress proxy listeners (ports 80/443 by default).
    pub enable_ingress: bool,
}

impl Default for SetupAnswers {
    fn default() -> Self {
        Self {
            node_name: "node-01".to_string(),
            cluster_name: "default".to_string(),
            join_address: None,
            data_dir: default_data_dir(),
            enable_dns: false,
            enable_ebpf: false,
            enable_ingress: false,
        }
    }
}

/// Options from the `relish setup` command line.
#[derive(Debug, Clone)]
pub struct SetupOptions {
    /// Accept the default answer to every question (non-interactive).
    pub yes: bool,
    /// Directory to write `reliaburger.toml` into.
    pub dir: PathBuf,
    /// Release metadata URL.
    pub release_url: String,
    /// Directory to install the bun binary into; `None` = `~/.reliaburger/bin`.
    pub binary_dir: Option<PathBuf>,
}

/// The default managed install directory (`~/.reliaburger/bin`).
pub fn default_binary_dir() -> PathBuf {
    default_data_dir().join("bin")
}

/// The default node state directory (`~/.reliaburger`, or the system
/// `/var/lib/reliaburger` when there is no home directory).
pub fn default_data_dir() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".reliaburger"))
        .unwrap_or_else(|| PathBuf::from("/var/lib/reliaburger"))
}

/// Compare the installed version (if any) against the latest release.
pub fn decide_install(installed: Option<BinaryVersion>, latest: BinaryVersion) -> InstallDecision {
    match installed {
        None => InstallDecision::NotInstalled { latest },
        Some(current) if current < latest => InstallDecision::Upgradable {
            from: current,
            to: latest,
        },
        Some(current) => InstallDecision::UpToDate { current },
    }
}

/// Build the full [`NodeConfig`] the answers describe. Everything not asked
/// about keeps its default.
pub fn build_node_config(answers: &SetupAnswers) -> NodeConfig {
    use crate::config::node::{
        ClusterSection, DnsSection, EbpfSection, IngressSection, NodeSection, StorageSection,
    };

    NodeConfig {
        node: NodeSection {
            name: Some(answers.node_name.clone()),
            ..NodeSection::default()
        },
        cluster: ClusterSection {
            name: answers.cluster_name.clone(),
            join: answers.join_address.iter().cloned().collect(),
            ..ClusterSection::default()
        },
        storage: StorageSection {
            data: answers.data_dir.join("data"),
            images: answers.data_dir.join("images"),
            logs: answers.data_dir.join("logs"),
            metrics: answers.data_dir.join("metrics"),
            volumes: answers.data_dir.join("volumes"),
            ..StorageSection::default()
        },
        dns: DnsSection {
            enabled: answers.enable_dns,
            ..DnsSection::default()
        },
        ebpf: EbpfSection {
            enabled: answers.enable_ebpf,
            ..EbpfSection::default()
        },
        ingress: IngressSection {
            enabled: answers.enable_ingress,
            ..IngressSection::default()
        },
        ..NodeConfig::default()
    }
}

/// Render the answers as a minimal `reliaburger.toml`: only the sections the
/// user actually answered, so the file stays short and readable. Parsing the
/// result with [`NodeConfig::parse`] yields exactly
/// [`build_node_config`]`(answers)`.
pub fn render_config(answers: &SetupAnswers) -> String {
    use toml::Value;

    let mut root = toml::Table::new();

    let mut node = toml::Table::new();
    node.insert("name".to_string(), Value::String(answers.node_name.clone()));
    root.insert("node".to_string(), Value::Table(node));

    let mut cluster = toml::Table::new();
    cluster.insert(
        "name".to_string(),
        Value::String(answers.cluster_name.clone()),
    );
    if let Some(address) = &answers.join_address {
        cluster.insert(
            "join".to_string(),
            Value::Array(vec![Value::String(address.clone())]),
        );
    }
    root.insert("cluster".to_string(), Value::Table(cluster));

    let mut storage = toml::Table::new();
    for key in ["data", "images", "logs", "metrics", "volumes"] {
        storage.insert(
            key.to_string(),
            Value::String(answers.data_dir.join(key).to_string_lossy().into_owned()),
        );
    }
    root.insert("storage".to_string(), Value::Table(storage));

    for (section, enabled) in [
        ("dns", answers.enable_dns),
        ("ebpf", answers.enable_ebpf),
        ("ingress", answers.enable_ingress),
    ] {
        if enabled {
            let mut table = toml::Table::new();
            table.insert("enabled".to_string(), Value::Boolean(true));
            root.insert(section.to_string(), Value::Table(table));
        }
    }

    // A table of strings and booleans always serialises.
    let body = toml::to_string_pretty(&root).expect("generated config serialises");
    format!("# Generated by `relish setup`. Every omitted section keeps its default.\n\n{body}")
}

/// Find an executable called `name` on a PATH-style list of directories.
pub fn find_on_path(path_var: &str, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The version the managed store's entry symlink points at, if `binary_dir`
/// holds one.
pub fn detect_installed(binary_dir: &Path) -> Option<BinaryVersion> {
    BinaryStore::new(binary_dir.to_path_buf(), "bun".to_string())
        .current_target()
        .ok()
}

/// `relish setup` — the interactive flow.
pub async fn run(options: SetupOptions) -> Result<(), RelishError> {
    let binary_dir = options
        .binary_dir
        .clone()
        .unwrap_or_else(default_binary_dir);

    // 1. Detect what is already here.
    let path_var = std::env::var("PATH").unwrap_or_default();
    let on_path = find_on_path(&path_var, "bun");
    let installed = detect_installed(&binary_dir);
    let client = BunClient::default_local();
    let running = match client.health().await {
        Ok(()) => client.node_version().await.ok(),
        Err(_) => None,
    };

    println!("Looking for bun:");
    match &on_path {
        Some(path) => println!("  on PATH:   {}", path.display()),
        None => println!("  on PATH:   not found"),
    }
    match &installed {
        Some(version) => println!("  installed: {version} in {}", binary_dir.display()),
        None => println!("  installed: nothing in {}", binary_dir.display()),
    }
    match &running {
        Some(version) => println!("  running:   {version} at {}", client.base_url()),
        None => println!("  running:   no"),
    }

    // 2. Resolve against the release channel. A live node's report beats the
    // store symlink (the node knows what it is actually executing).
    let current: Option<BinaryVersion> = running
        .as_deref()
        .and_then(|version| version.parse().ok())
        .or_else(|| installed.clone());

    let installed_now = match metadata::fetch(&options.release_url).await {
        Ok(release_metadata) => {
            install_step(
                &release_metadata,
                current,
                running.is_some(),
                on_path.is_some(),
                &binary_dir,
                options.yes,
            )
            .await?
        }
        Err(error) if current.is_some() || on_path.is_some() => {
            println!("release check failed ({error}); keeping the bun you have");
            None
        }
        Err(error) => {
            return Err(RelishError::FormatFailed(format!(
                "no bun found and the release check failed: {error}"
            )));
        }
    };

    // 3. Configure.
    let config_path = options.dir.join("reliaburger.toml");
    let config_written = if config_path.exists() {
        println!("{} already exists; leaving it alone", config_path.display());
        false
    } else if confirm("Generate reliaburger.toml here?", true, options.yes)? {
        let answers = collect_answers(options.yes)?;
        std::fs::create_dir_all(&options.dir)?;
        std::fs::write(&config_path, render_config(&answers))?;
        println!("wrote {}", config_path.display());
        true
    } else {
        false
    };

    // 4. Offer to start the node.
    if running.is_none() && (config_written || config_path.exists()) {
        let bun_binary = installed_now
            .or_else(|| {
                installed
                    .map(|_| BinaryStore::new(binary_dir.clone(), "bun".to_string()).symlink_path())
            })
            .or(on_path);
        if let Some(bun) = bun_binary {
            offer_to_start(&client, &bun, &config_path, &options.dir, options.yes).await?;
        }
    } else if running.is_some() {
        println!("bun is already running — check it with: relish status");
    }

    Ok(())
}

/// Decide and (with consent) perform the install/upgrade. Returns the path
/// of a binary this step installed, if it installed one.
async fn install_step(
    release_metadata: &ReleaseMetadata,
    current: Option<BinaryVersion>,
    running: bool,
    on_path: bool,
    binary_dir: &Path,
    yes: bool,
) -> Result<Option<PathBuf>, RelishError> {
    match decide_install(current, release_metadata.latest.clone()) {
        InstallDecision::UpToDate { current } => {
            println!("bun {current} is up to date");
            Ok(None)
        }
        InstallDecision::Upgradable { from, to } if running => {
            // Never swap the binary under a live node from here; the node's
            // own upgrade pipeline handles markers, verification and revert.
            println!("bun {from} is running and {to} is available");
            println!("upgrade the running node with: relish upgrade start {to}");
            Ok(None)
        }
        InstallDecision::Upgradable { from, to } => {
            let question = format!("Upgrade bun {from} -> {to} in {}?", binary_dir.display());
            if confirm(&question, true, yes)? {
                Ok(Some(install(release_metadata, binary_dir).await?))
            } else {
                Ok(None)
            }
        }
        InstallDecision::NotInstalled { latest } => {
            if on_path {
                println!(
                    "note: a bun outside {} is on PATH; setup only manages that directory",
                    binary_dir.display()
                );
            }
            let question = format!("Install bun {latest} into {}?", binary_dir.display());
            if confirm(&question, true, yes)? {
                Ok(Some(install(release_metadata, binary_dir).await?))
            } else {
                Ok(None)
            }
        }
    }
}

/// Download, verify and activate the latest release into `binary_dir`,
/// through the same store the node's self-upgrade uses. Returns the entry
/// symlink path.
async fn install(
    release_metadata: &ReleaseMetadata,
    binary_dir: &Path,
) -> Result<PathBuf, RelishError> {
    let version = &release_metadata.latest;
    let platform = metadata::platform_key();
    let artifact = release_metadata
        .artifact_for(version, &platform)
        .ok_or_else(|| {
            RelishError::FormatFailed(format!(
                "no artefact for {version} on {platform} in the release metadata"
            ))
        })?;

    println!("downloading {} ...", artifact.url);
    let bytes = super::upgrade::download(&artifact.url).await?;

    let envelope = SignatureEnvelope {
        schema: 1,
        sha256: artifact.sha256.clone(),
        embedded: artifact.embedded_signature.clone(),
        external: artifact.external_signature.clone(),
    };
    // First install: no node.toml exists yet, so there is no operator key to
    // check an external signature against. The embedded release signature
    // (over TLS to the metadata host) gates the download — the same trust an
    // air-gapped upgrade gets.
    let release_keys = keys::release_keys(&crate::config::node::UpgradeSection::default())?;
    crate::upgrade::signing::verify_binary(&bytes, &envelope, &release_keys, None, false)?;

    let store = BinaryStore::new(binary_dir.to_path_buf(), "bun".to_string());
    store.stage(version, &bytes, &envelope)?;
    store.activate(version)?;
    println!(
        "installed and verified bun {version} -> {}",
        store.symlink_path().display()
    );
    Ok(store.symlink_path())
}

/// Ask the configuration questions (or take every default with `--yes`).
fn collect_answers(yes: bool) -> Result<SetupAnswers, RelishError> {
    let defaults = SetupAnswers::default();
    if yes {
        return Ok(defaults);
    }
    let node_name = ask_string("node name", &defaults.node_name)?;
    let cluster_name = ask_string("cluster name", &defaults.cluster_name)?;
    let cluster_identity = crate::config::node::ClusterSection {
        name: cluster_name.clone(),
        ..crate::config::node::ClusterSection::default()
    };
    cluster_identity
        .validate()
        .map_err(|error| RelishError::InitFailed(error.to_string()))?;
    let join = ask_string("cluster member to join (empty = single node)", "")?;
    let data_dir = ask_string("data directory", &defaults.data_dir.to_string_lossy())?;
    let enable_dns = ask_bool("enable .internal DNS (binds port 53, needs root)?", false)?;
    let enable_ebpf = ask_bool("enable eBPF service discovery (Linux, root)?", false)?;
    let enable_ingress = ask_bool("enable the ingress proxy (binds ports 80/443)?", false)?;
    Ok(SetupAnswers {
        node_name,
        cluster_name,
        join_address: (!join.is_empty()).then_some(join),
        data_dir: PathBuf::from(data_dir),
        enable_dns,
        enable_ebpf,
        enable_ingress,
    })
}

/// Start bun with the generated config, logging to `bun.log`, and wait
/// briefly for it to answer health checks.
async fn offer_to_start(
    client: &BunClient,
    bun: &Path,
    config_path: &Path,
    dir: &Path,
    yes: bool,
) -> Result<(), RelishError> {
    let hint = format!(
        "start it later with: {} --config {}",
        bun.display(),
        config_path.display()
    );
    // Starting a daemon is the one step that defaults to "no": --yes must
    // not leave a background process the user didn't ask for.
    if !confirm("Start bun now?", false, yes)? {
        println!("{hint}");
        return Ok(());
    }

    let log_path = dir.join("bun.log");
    let log = std::fs::File::create(&log_path)?;
    let child = std::process::Command::new(bun)
        .arg("--config")
        .arg(config_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log))
        .spawn()?;
    println!(
        "started bun (pid {}), logs in {}",
        child.id(),
        log_path.display()
    );

    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if client.health().await.is_ok() {
            println!("bun is up — try: relish status");
            return Ok(());
        }
    }
    println!(
        "bun has not answered a health check yet; watch {}",
        log_path.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Terminal prompts (deliberately untested: everything they feed into is
// pure and covered above)
// ---------------------------------------------------------------------------

/// Ask a free-text question; an empty reply (or EOF) takes the default.
fn ask_string(prompt: &str, default: &str) -> Result<String, RelishError> {
    use std::io::Write as _;

    let mut stdout = std::io::stdout();
    if default.is_empty() {
        write!(stdout, "{prompt}: ")?;
    } else {
        write!(stdout, "{prompt} [{default}]: ")?;
    }
    stdout.flush()?;

    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let trimmed = line.trim();
    Ok(if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    })
}

/// Ask a yes/no question; an empty or unrecognised reply takes the default.
fn ask_bool(prompt: &str, default: bool) -> Result<bool, RelishError> {
    let hint = if default { "Y/n" } else { "y/N" };
    let reply = ask_string(&format!("{prompt} [{hint}]"), "")?;
    Ok(match reply.chars().next() {
        Some('y') | Some('Y') => true,
        Some('n') | Some('N') => false,
        _ => default,
    })
}

/// Like [`ask_bool`], but `--yes` skips the question and takes the default.
fn confirm(prompt: &str, default: bool, yes: bool) -> Result<bool, RelishError> {
    if yes {
        let answer = if default { "yes" } else { "no" };
        println!("{prompt} {answer} (--yes)");
        return Ok(default);
    }
    ask_bool(prompt, default)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> BinaryVersion {
        s.parse().unwrap()
    }

    fn answers() -> SetupAnswers {
        SetupAnswers {
            node_name: "kitchen-01".to_string(),
            cluster_name: "production".to_string(),
            join_address: None,
            data_dir: PathBuf::from("/srv/reliaburger"),
            enable_dns: false,
            enable_ebpf: false,
            enable_ingress: false,
        }
    }

    // -- install decision --------------------------------------------------

    #[test]
    fn nothing_installed_offers_the_latest_release() {
        assert_eq!(
            decide_install(None, v("0.2.0")),
            InstallDecision::NotInstalled { latest: v("0.2.0") }
        );
    }

    #[test]
    fn matching_version_is_up_to_date() {
        assert_eq!(
            decide_install(Some(v("0.2.0")), v("0.2.0")),
            InstallDecision::UpToDate {
                current: v("0.2.0")
            }
        );
    }

    #[test]
    fn older_install_is_upgradable_to_latest() {
        assert_eq!(
            decide_install(Some(v("0.1.0")), v("0.2.0")),
            InstallDecision::Upgradable {
                from: v("0.1.0"),
                to: v("0.2.0"),
            }
        );
    }

    #[test]
    fn install_ahead_of_the_release_channel_is_up_to_date() {
        // A dev build newer than the channel must not be "upgraded" backwards.
        assert_eq!(
            decide_install(Some(v("0.3.0-rc.1")), v("0.2.0")),
            InstallDecision::UpToDate {
                current: v("0.3.0-rc.1")
            }
        );
    }

    // -- answers -> config -------------------------------------------------

    #[test]
    fn rendered_config_round_trips_through_node_config_parse() {
        let answers = answers();
        let parsed = NodeConfig::parse(&render_config(&answers)).unwrap();
        assert_eq!(parsed, build_node_config(&answers));
        assert_eq!(parsed.cluster.name, "production");
    }

    #[test]
    fn join_answer_round_trips_and_sets_cluster_join() {
        let answers = SetupAnswers {
            join_address: Some("10.0.1.5:9443".to_string()),
            ..answers()
        };
        let parsed = NodeConfig::parse(&render_config(&answers)).unwrap();
        assert_eq!(parsed, build_node_config(&answers));
        assert_eq!(parsed.cluster.join, vec!["10.0.1.5:9443"]);
    }

    #[test]
    fn feature_toggles_round_trip_and_enable_their_sections() {
        let answers = SetupAnswers {
            enable_dns: true,
            enable_ebpf: true,
            enable_ingress: true,
            ..answers()
        };
        let parsed = NodeConfig::parse(&render_config(&answers)).unwrap();
        assert_eq!(parsed, build_node_config(&answers));
        assert!(parsed.dns.enabled);
        assert!(parsed.ebpf.enabled);
        assert!(parsed.ingress.enabled);
    }

    #[test]
    fn config_places_storage_paths_under_the_data_dir() {
        let config = build_node_config(&answers());
        assert_eq!(config.storage.data, PathBuf::from("/srv/reliaburger/data"));
        assert_eq!(
            config.storage.images,
            PathBuf::from("/srv/reliaburger/images")
        );
        assert_eq!(config.storage.logs, PathBuf::from("/srv/reliaburger/logs"));
        assert_eq!(
            config.storage.metrics,
            PathBuf::from("/srv/reliaburger/metrics")
        );
        assert_eq!(
            config.storage.volumes,
            PathBuf::from("/srv/reliaburger/volumes")
        );
    }

    #[test]
    fn rendered_config_stays_minimal() {
        let rendered = render_config(&answers());
        // Cluster identity is explicit even on a single node; optional feature
        // sections and unrelated defaults stay absent.
        assert!(rendered.contains("[cluster]"));
        assert!(rendered.contains("name = \"production\""));
        assert!(!rendered.contains("[dns]"));
        assert!(!rendered.contains("[ebpf]"));
        assert!(!rendered.contains("[ingress]"));
        assert!(!rendered.contains("[metrics]"));
        assert!(rendered.contains("name = \"kitchen-01\""));
    }

    // -- detection ---------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn find_on_path_returns_the_first_executable_match() {
        use std::os::unix::fs::PermissionsExt;

        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        // First dir has a non-executable decoy; second has the real thing.
        std::fs::write(first.path().join("bun"), b"decoy").unwrap();
        std::fs::set_permissions(
            first.path().join("bun"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        std::fs::write(second.path().join("bun"), b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            second.path().join("bun"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let path_var = std::env::join_paths([first.path(), second.path()])
            .unwrap()
            .into_string()
            .unwrap();

        assert_eq!(
            find_on_path(&path_var, "bun"),
            Some(second.path().join("bun"))
        );
        assert_eq!(find_on_path(&path_var, "missing"), None);
        assert_eq!(find_on_path("", "bun"), None);
    }

    #[test]
    fn detect_installed_reads_the_store_symlink() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_installed(dir.path()), None);

        let store = BinaryStore::new(dir.path().to_path_buf(), "bun".to_string());
        let envelope = SignatureEnvelope {
            schema: 1,
            sha256: "00".to_string(),
            embedded: "AA==".to_string(),
            external: None,
        };
        store.stage(&v("0.2.0"), b"binary", &envelope).unwrap();
        store.activate(&v("0.2.0")).unwrap();

        assert_eq!(detect_installed(dir.path()), Some(v("0.2.0")));
    }
}
