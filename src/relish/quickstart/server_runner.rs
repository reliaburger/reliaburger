//! `relish setup --ssh`: a cluster on Linux machines you already have.
//!
//! The same steps as the managed laptop cluster, on real machines: nothing
//! changes anywhere until every machine has passed its checks, each node's
//! progress is checkpointed so a re-run resumes, and credentials stay on the
//! operator's machine. It's a flow of its own rather than a mode of
//! [`super::runner`], so this can't disturb the laptop path.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{
    artifacts,
    download::Downloader,
    lima::GuestFile,
    progress::{Progress, Stage, Step},
    provision,
    runner::{
        JOIN_SCRIPT, NodeSources, check_release_source, checkpoint, demo_config, node_files,
        open_operation, probe_demo, wait_for_quorum,
    },
    security::Bootstrap,
    server::{Facts, PACKAGES, PREFLIGHT},
    ssh::{Ssh, SshTarget, Tunnel},
    state::{ClusterSpec, NodePhase, Operation},
};
use crate::relish::{client::BunClient, local_context::LocalContext};
use crate::upgrade::BinaryVersion;

/// One command on a machine, which for `apt-get` can take minutes.
const COMMAND_DEADLINE: Duration = Duration::from_secs(600);
const DOWNLOAD_STALL: Duration = Duration::from_secs(30);
/// Bun's API, and the ports setup tunnels to on the first machine.
const API_PORT: u16 = 9117;
const INGRESS_PORT: u16 = 80;
const REGISTRY_PORT: u16 = 5050;

/// Choices for creating or resuming a cluster on existing machines.
pub struct Options {
    /// Stable cluster name.
    pub name: String,
    /// The machines, the first of which bootstraps the cluster.
    pub targets: Vec<SshTarget>,
    /// Addresses peers should use, one per machine, overriding detection.
    pub advertise: Vec<Ipv4Addr>,
    /// First loopback port setup tunnels each machine's API through.
    pub api_port: u16,
    /// Loopback port setup tunnels the first machine's ingress through.
    pub ingress_port: u16,
    /// Directory of explicitly supplied Linux binaries.
    pub development_binaries: Option<PathBuf>,
    /// HTTPS directory of unchanged signed release candidate assets.
    pub release_mirror: Option<String>,
}

/// The shell that installs what a node needs from the distribution.
fn package_script() -> String {
    format!(
        "set -eu\nexport DEBIAN_FRONTEND=noninteractive\napt-get update -qq\n\
         apt-get install -y -qq {}\n",
        PACKAGES.join(" ")
    )
}

/// Bun's versioned path, which self-upgrade and rollback expect behind a
/// `bun` symlink.
fn versioned_bun(version: &BinaryVersion) -> String {
    format!("bun-{version}")
}

/// A node's files, as on a laptop VM but with Bun in its versioned path.
fn server_files(
    sources: &NodeSources<'_>,
    index: usize,
    config: PathBuf,
    version: &BinaryVersion,
) -> Vec<GuestFile> {
    let bun = format!("/usr/local/bin/{}", versioned_bun(version));
    node_files(sources, index, config)
        .into_iter()
        .map(|file| {
            if file.destination == "/usr/local/bin/bun" {
                GuestFile {
                    destination: bun.clone(),
                    ..file
                }
            } else {
                file
            }
        })
        .collect()
}

/// What every machine reported, or everything wrong with them.
fn judge(reports: &[(SshTarget, Result<Facts>, bool)]) -> Result<()> {
    let mut problems = Vec::new();
    for (target, facts, resuming) in reports {
        match facts {
            Ok(facts) => problems.extend(
                facts
                    .problems(*resuming)
                    .into_iter()
                    .map(|problem| format!("{target} {problem}")),
            ),
            Err(error) => problems.push(format!("{error:#}")),
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    bail!(
        "nothing was changed on any machine. Fix these and run the same command again:\n  {}",
        problems.join("\n  ")
    )
}

/// Say what's needed when `ssh` couldn't get in, which is usually first contact.
fn connection_hint(error: anyhow::Error) -> anyhow::Error {
    let text = format!("{error:#}");
    if text.contains("Host key verification failed") {
        return error.context(
            "ssh doesn't know this machine yet; connect to it once by hand to accept its host key",
        );
    }
    if text.contains("Permission denied") {
        return error.context("ssh couldn't log in without a password; load a key into your agent");
    }
    error
}

/// Everything shared by the per-node steps.
struct Setup<'a> {
    ssh: &'a Ssh,
    progress: &'a Progress,
    spec: &'a ClusterSpec,
    bootstrap: &'a Bootstrap,
    directory: PathBuf,
    service: PathBuf,
    binaries: &'a [(String, PathBuf, PathBuf)],
    arches: &'a [String],
    peers: &'a [Ipv4Addr],
    operator: Option<Ipv4Addr>,
}

impl Setup<'_> {
    fn sources(&self, index: usize) -> Result<NodeSources<'_>> {
        let (_, bun, relish) = self
            .binaries
            .iter()
            .find(|(arch, _, _)| *arch == self.arches[index])
            .context("no binaries for this machine's architecture")?;
        Ok(NodeSources {
            bun,
            relish,
            service: &self.service,
            security: &self.bootstrap.directory,
        })
    }

    fn tunnel_endpoint(&self, index: usize) -> String {
        format!("https://127.0.0.1:{}", self.spec.api_port + index as u16)
    }
}

/// Install a node's files and start it, then wait until it's ready. `client`
/// reaches the first node, which peers enrol through; the first has no one to
/// enrol through and returns the tunnel the rest will use.
async fn configure(
    setup: &Setup<'_>,
    index: usize,
    client: Option<&BunClient>,
    operation: &tokio::sync::Mutex<&mut Operation>,
) -> Result<Tunnel> {
    let node = operation.lock().await.state.nodes[index].clone();
    let target = &setup.spec.ssh[index];
    let number = index + 1;
    let version = &setup.spec.version;
    let phase = |phase| node.phase as u8 >= phase as u8;
    if !phase(NodePhase::Created) {
        let step = setup.progress.step(
            Stage::Configure,
            format!("install packages on node {number}"),
        );
        step.record(setup.ssh.root_script(target, &package_script(), &[]).await)?;
        checkpoint(operation, index, NodePhase::Created).await?;
    }
    if !phase(NodePhase::Configured) {
        let step = setup
            .progress
            .step(Stage::Configure, format!("install files on node {number}"));
        let result = async {
            let config = provision::server_node_config(
                &setup.spec.name,
                &node.name,
                node.address.context("machine has no address")?,
                (index > 0).then_some(setup.peers[0]),
                setup.peers,
                setup.operator,
            )?;
            let config_path = setup.directory.join(format!("{}.toml", node.name));
            tokio::fs::write(&config_path, config).await?;
            let files = server_files(&setup.sources(index)?, index, config_path, version);
            setup.ssh.install_files(target, files).await?;
            let link = format!("ln -sfn '{}' /usr/local/bin/bun\n", versioned_bun(version));
            setup.ssh.root_script(target, &link, &[]).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        step.record(result)?;
        checkpoint(operation, index, NodePhase::Configured).await?;
    }
    if !phase(NodePhase::Enrolled) {
        if let Some(client) = client {
            let step = setup
                .progress
                .step(Stage::Configure, format!("enrol node {number}"));
            step.record(enrol(setup, client, target, &node.name, &step).await)?;
        }
        checkpoint(operation, index, NodePhase::Enrolled).await?;
    }
    let step = setup
        .progress
        .step(Stage::Configure, format!("start node {number}"));
    let result = async {
        let start = "systemctl daemon-reload && systemctl enable --now reliaburger.service";
        setup.ssh.root_script(target, start, &[]).await?;
        checkpoint(operation, index, NodePhase::Started).await?;
        let local_port = setup.spec.api_port + index as u16;
        let tunnel = setup.ssh.tunnel(target, local_port, API_PORT).await?;
        let node_client = setup.bootstrap.client(&setup.tunnel_endpoint(index))?;
        crate::relish::readiness::wait_for_node(&node_client, Duration::from_secs(60)).await?;
        let running: BinaryVersion = node_client.node_version().await?.parse()?;
        if running != *version {
            bail!("{target} is running {running}, expected {version}");
        }
        Ok(tunnel)
    }
    .await;
    step.record(result)
}

/// Enrol a peer through the first node, unless it already has an identity.
async fn enrol(
    setup: &Setup<'_>,
    client: &BunClient,
    target: &SshTarget,
    name: &str,
    step: &Step,
) -> Result<()> {
    let committed = "test -f /etc/reliaburger/identity/bundle.committed";
    if setup.ssh.root_script(target, committed, &[]).await.is_ok() {
        step.note("already enrolled");
        return Ok(());
    }
    let token = client.join_token_create(name, 300).await?;
    let token_path = setup.directory.join(format!("{name}.join-token"));
    let result = async {
        let path = token_path.clone();
        tokio::task::spawn_blocking(move || {
            crate::sesame::identity::atomic_write_mode(&path, token.as_bytes(), Some(0o600))
        })
        .await??;
        let input = tokio::fs::File::open(&token_path).await?.into_std().await;
        let url = format!("https://{}:{API_PORT}", setup.peers[0]);
        let args = [
            name,
            setup.bootstrap.root_fingerprint.as_str(),
            url.as_str(),
        ];
        setup
            .ssh
            .root_script_with_input(target, JOIN_SCRIPT, &args, input)
            .await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let _ = tokio::fs::remove_file(&token_path).await;
    result
}

/// Check every machine, then fetch binaries for each architecture among them.
async fn prepare(
    root: &Path,
    ssh: &Ssh,
    operation: &mut Operation,
    advertise: &[Ipv4Addr],
    development: Option<&Path>,
    downloader: &Downloader,
    progress: &Progress,
) -> Result<(Vec<Facts>, Vec<(String, PathBuf, PathBuf)>)> {
    let spec = operation.state.spec.clone();
    let step = progress.step(Stage::Host, "check machines");
    let checks = futures_util::future::join_all(spec.ssh.iter().map(|target| async move {
        ssh.user_script(target, PREFLIGHT, &[])
            .await
            .map(|report| Facts::parse(&report))
            .map_err(connection_hint)
    }))
    .await;
    let reports: Vec<_> = spec
        .ssh
        .iter()
        .zip(checks)
        .zip(&operation.state.nodes)
        .map(|((target, facts), node)| (target.clone(), facts, node.phase != NodePhase::Planned))
        .collect();
    step.record(judge(&reports))?;
    let facts: Vec<Facts> = reports
        .into_iter()
        .map(|(_, facts, _)| facts)
        .collect::<Result<_>>()?;
    for (index, facts) in facts.iter().enumerate() {
        let address = match advertise.get(index) {
            Some(address) => *address,
            None => facts.cluster_address(&spec.ssh[index])?,
        };
        operation.state.nodes[index].address = Some(address);
    }
    operation.save_async().await?;
    let peers: Vec<_> = operation
        .state
        .nodes
        .iter()
        .filter_map(|n| n.address)
        .collect();
    for (target, facts) in spec.ssh.iter().zip(&facts) {
        for warning in facts.warnings(&peers) {
            progress.note(format!("{target}: {warning}"));
        }
    }
    if spec.nodes.is_multiple_of(2) {
        progress.note(
            "an even number of machines tolerates no more failures than one fewer; an odd number is better",
        );
    }
    let cache = root.join("cache");
    tokio::fs::create_dir_all(&cache).await?;
    let arches: BTreeSet<&String> = facts.iter().map(|facts| &facts.arch).collect();
    let mut binaries = Vec::new();
    for arch in arches {
        let step = progress.step(Stage::Download, format!("binaries for {arch}"));
        let fetched = match development {
            Some(directory) => Ok((
                tokio::fs::canonicalize(directory.join("bun")).await?,
                tokio::fs::canonicalize(directory.join("relish")).await?,
            )),
            None => {
                async {
                    let version = &spec.version;
                    let bun =
                        artifacts::binary(&cache, version, "bun", arch, downloader, &step).await?;
                    let relish =
                        artifacts::binary(&cache, version, "relish", arch, downloader, &step)
                            .await?;
                    Ok::<_, anyhow::Error>((bun, relish))
                }
                .await
            }
        };
        let (bun, relish) = step.record(fetched)?;
        binaries.push((arch.clone(), bun, relish));
    }
    Ok((facts, binaries))
}

/// Configure every machine, then prove the cluster runs a container.
async fn build(
    root: &Path,
    ssh: &Ssh,
    operation: &mut Operation,
    bootstrap: &Bootstrap,
    facts: &[Facts],
    binaries: &[(String, PathBuf, PathBuf)],
    progress: &Progress,
) -> Result<()> {
    let spec = operation.state.spec.clone();
    let peers: Vec<_> = operation
        .state
        .nodes
        .iter()
        .filter_map(|n| n.address)
        .collect();
    let service = operation.directory.join("reliaburger.service");
    tokio::fs::write(&service, provision::SERVICE).await?;
    let operator = facts[0].operator;
    if operator.is_none() {
        progress.note("couldn't tell this machine's address; relish will need an SSH tunnel");
    }
    let arches: Vec<String> = facts.iter().map(|facts| facts.arch.clone()).collect();
    let setup = Setup {
        ssh,
        progress,
        spec: &spec,
        bootstrap,
        directory: operation.directory.clone(),
        service,
        binaries,
        arches: &arches,
        peers: &peers,
        operator,
    };
    let names: Vec<_> = operation
        .state
        .nodes
        .iter()
        .map(|n| n.name.clone())
        .collect();
    let first = spec.ssh.first().context("no machines")?;
    let client = bootstrap.client(&setup.tunnel_endpoint(0))?;
    {
        // Peers enrol through the first node, so it must be ready first.
        let checkpoints = tokio::sync::Mutex::new(&mut *operation);
        let _first_tunnel = configure(&setup, 0, None, &checkpoints).await?;
        let results = futures_util::future::join_all(
            (1..spec.nodes).map(|index| configure(&setup, index, Some(&client), &checkpoints)),
        )
        .await;
        let _tunnels = results.into_iter().collect::<Result<Vec<_>>>()?;
        let quorum = progress.step(Stage::Verify, "form council quorum");
        quorum.record(wait_for_quorum(&client, &names).await)?;
        let demo_step = progress.step(Stage::Verify, "run hello through ingress");
        let result = async {
            let demo = demo_config(INGRESS_PORT, &first.host);
            tokio::fs::write(setup.directory.join("hello.toml"), &demo).await?;
            client.apply(&crate::config::Config::parse(&demo)?).await?;
            let _ingress = ssh.tunnel(first, spec.ingress_port, INGRESS_PORT).await?;
            probe_demo(spec.ingress_port, &first.host).await
        }
        .await;
        demo_step.record(result)?;
    }
    let endpoint = format!("https://{}:{API_PORT}", first.host);
    let context = LocalContext {
        schema: 1,
        owner: operation.state.id.clone(),
        endpoint: endpoint.clone(),
        token: tokio::fs::read_to_string(bootstrap.directory.join("admin.token")).await?,
        ca_cert: bootstrap.directory.join("identity/root-ca.crt"),
        service_endpoints: crate::bun::capabilities::ServiceEndpoints {
            registry: Some(format!("https://{}:{REGISTRY_PORT}", first.host)),
            ingress_http: Some(format!("http://{}", first.host)),
            ingress_https: None,
        },
    };
    let path = root.join("context.json");
    tokio::task::spawn_blocking(move || context.save(&path)).await??;
    let direct = bootstrap.client(&endpoint)?;
    let reachable = tokio::time::timeout(Duration::from_secs(5), direct.node_version()).await;
    if !matches!(reachable, Ok(Ok(_))) {
        progress.note(format!(
            "the cluster is up, but this machine can't reach {endpoint}: check any firewall in \
             front of port {API_PORT}, or use an SSH tunnel (docs/linux-servers.md, section 5.3)"
        ));
    }
    Ok(())
}

/// Build or resume a cluster on the machines in `options`.
pub async fn run(options: Options) -> Result<()> {
    if options.release_mirror.is_some() && options.development_binaries.is_some() {
        bail!("a release mirror cannot be combined with development binaries");
    }
    if !options.advertise.is_empty() && options.advertise.len() != options.targets.len() {
        bail!("--advertise needs one address for each machine, in the same order");
    }
    let version = env!("CARGO_PKG_VERSION").parse::<BinaryVersion>()?;
    check_release_source(
        &version,
        options.development_binaries.is_some(),
        options.release_mirror.is_some(),
    )?;
    let mut downloader = Downloader::new(DOWNLOAD_STALL)?;
    if let Some(mirror) = &options.release_mirror {
        downloader = downloader.with_release_mirror(&version, mirror)?;
    }
    let root = crate::relish::local_context::root_directory()?;
    let spec = ClusterSpec {
        name: options.name,
        nodes: options.targets.len(),
        version,
        api_port: options.api_port,
        ingress_port: options.ingress_port,
        registry_port: 15050,
        ssh: options.targets,
    };
    let (mut operation, bootstrap, _setup_lock) = open_operation(&root, spec).await?;
    let ssh = Ssh::new(COMMAND_DEADLINE);
    let progress = Progress::stdout();
    let result = async {
        let (facts, binaries) = prepare(
            &root,
            &ssh,
            &mut operation,
            &options.advertise,
            options.development_binaries.as_deref(),
            &downloader,
            &progress,
        )
        .await?;
        build(
            &root,
            &ssh,
            &mut operation,
            &bootstrap,
            &facts,
            &binaries,
            &progress,
        )
        .await
    }
    .await;
    let timings = progress.finish().await;
    println!("{}", timings.summary());
    match result {
        Ok(()) => {
            let spec = &operation.state.spec;
            println!(
                "cluster {} ready in {:.1}s",
                spec.name, timings.total_seconds
            );
            println!("  app: http://{}", spec.ssh[0].host);
            let relish = crate::relish::install::invocation();
            println!("  next: {relish} status; {relish} logs hello; {relish} dashboard");
            println!(
                "  back up {}: it holds the cluster's master key and the only copy of the root CA",
                bootstrap.directory.display()
            );
            Ok(())
        }
        Err(error) => Err(error.context(format!(
            "setup stopped; checkpoints are in {}. Run the same setup command to resume",
            operation.directory.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version() -> BinaryVersion {
        "0.2.6".parse().unwrap()
    }

    fn sources(directory: &Path) -> Vec<GuestFile> {
        let bun = directory.join("bun");
        let sources = NodeSources {
            bun: &bun,
            relish: &bun,
            service: &bun,
            security: directory,
        };
        server_files(&sources, 0, directory.join("node.toml"), &version())
    }

    #[test]
    fn bun_installs_under_its_version_for_self_upgrade() {
        let directory = tempfile::tempdir().unwrap();
        let files = sources(directory.path());
        let destinations: Vec<_> = files.iter().map(|file| file.destination.as_str()).collect();
        assert!(
            destinations
                .contains(&format!("/usr/local/bin/{}", versioned_bun(&version())).as_str())
        );
        assert!(!destinations.contains(&"/usr/local/bin/bun"));
        assert!(destinations.contains(&"/usr/local/bin/relish"));
    }

    #[test]
    fn packages_come_from_the_runbook_without_prompting() {
        let script = package_script();
        assert!(script.contains("DEBIAN_FRONTEND=noninteractive"));
        assert!(script.contains("apt-get install -y"));
        assert!(script.contains("runc") && script.contains("nftables"));
    }

    #[test]
    fn every_problem_on_every_machine_is_reported_at_once() {
        let target = |text: &str| -> SshTarget { text.parse().unwrap() };
        let error = judge(&[
            (target("u@a"), Ok(Facts::parse("sudo=yes")), false),
            (
                target("u@b"),
                Err(anyhow::anyhow!("u@b: no route to host")),
                false,
            ),
        ])
        .unwrap_err()
        .to_string();
        assert!(error.contains("nothing was changed"), "{error}");
        assert!(error.contains("u@a runs"), "{error}");
        assert!(error.contains("u@a doesn't run systemd"), "{error}");
        assert!(error.contains("u@b: no route to host"), "{error}");
    }

    #[test]
    fn a_host_key_or_login_failure_says_what_to_do() {
        let hint = |text: &str| format!("{:#}", connection_hint(anyhow::anyhow!(text.to_owned())));
        assert!(hint("Host key verification failed.").contains("accept its host key"));
        assert!(hint("Permission denied (publickey).").contains("load a key"));
        assert_eq!(hint("no route to host"), "no route to host");
    }

    #[test]
    fn the_demo_answers_on_the_first_machines_name() {
        let demo = demo_config(INGRESS_PORT, "10.0.0.1");
        let config = crate::config::Config::parse(&demo).unwrap();
        config.validate_intrinsic().unwrap();
        assert_eq!(
            config.app["hello"].ingress.as_ref().unwrap().host,
            "10.0.0.1"
        );
    }
}
