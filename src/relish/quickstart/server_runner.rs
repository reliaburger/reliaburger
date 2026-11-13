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
    report::{ServerFacts, SetupReport},
    runner::{
        JOIN_SCRIPT, NodeSources, check_release_source, checkpoint, demo_config, node_files,
        open_operation, probe_demo, saved_endpoint, wait_for_quorum,
    },
    security::Bootstrap,
    server::{Facts, PACKAGES, PREFLIGHT},
    ssh::{Ssh, SshTarget, Tunnel},
    state::{ClusterSpec, NodePhase, Operation},
};
use crate::relish::{
    client::BunClient,
    local_context::LocalContext,
    output::{self, OutputFormat},
};
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
    /// Lines for people, or the [`SetupReport`] as JSON or YAML with the
    /// progress on standard error.
    pub output: OutputFormat,
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
    spec: ClusterSpec,
    bootstrap: &'a Bootstrap,
    directory: PathBuf,
    service: PathBuf,
    binaries: &'a [(String, PathBuf, PathBuf)],
    arches: Vec<String>,
    peers: Vec<Ipv4Addr>,
    operator: Option<Ipv4Addr>,
}

impl<'a> Setup<'a> {
    /// Gather what every node's steps share, once [`prepare`] has checked the
    /// machines and recorded their addresses.
    async fn new(
        ssh: &'a Ssh,
        progress: &'a Progress,
        operation: &Operation,
        bootstrap: &'a Bootstrap,
        facts: &[Facts],
        binaries: &'a [(String, PathBuf, PathBuf)],
    ) -> Result<Self> {
        let service = operation.directory.join("reliaburger.service");
        tokio::fs::write(&service, provision::SERVICE).await?;
        let operator = facts.first().context("no machines")?.operator;
        if operator.is_none() {
            progress.note("couldn't tell this machine's address; relish will need an SSH tunnel");
        }
        Ok(Self {
            ssh,
            progress,
            spec: operation.state.spec.clone(),
            bootstrap,
            directory: operation.directory.clone(),
            service,
            binaries,
            arches: facts.iter().map(|facts| facts.arch.clone()).collect(),
            peers: operation
                .state
                .nodes
                .iter()
                .filter_map(|n| n.address)
                .collect(),
            operator,
        })
    }
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
                &setup.peers,
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
///
/// Returns what to tell the operator when this machine can't reach the API
/// directly, which the rest of `relish` needs once setup's tunnels close.
async fn build(
    root: &Path,
    ssh: &Ssh,
    operation: &mut Operation,
    bootstrap: &Bootstrap,
    facts: &[Facts],
    binaries: &[(String, PathBuf, PathBuf)],
    progress: &Progress,
) -> Result<Option<String>> {
    let setup = Setup::new(ssh, progress, operation, bootstrap, facts, binaries).await?;
    let spec = setup.spec.clone();
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
    if matches!(reachable, Ok(Ok(_))) {
        return Ok(None);
    }
    Ok(Some(format!(
        "the cluster is up, but relish on this machine can't reach its API at {endpoint}, \
         and every relish command needs it.\n{}",
        unreachable_api_advice(first, spec.api_port, &bootstrap.directory, setup.operator)
    )))
}

/// How an operator gets `relish` through to the API on `first`, once setup's
/// own tunnels have closed.
///
/// `operator` is the address `first` saw setup connect from, which Bun's
/// perimeter already admits; without it only a tunnel or a config change
/// helps. `security` holds the cluster CA and admin token a tunnelled
/// `relish` needs, because an explicit endpoint doesn't use the saved
/// context's credentials. `local_port` is the port setup tunnelled the
/// first machine's API through, so it's free here.
pub(crate) fn unreachable_api_advice(
    first: &SshTarget,
    local_port: u16,
    security: &Path,
    operator: Option<Ipv4Addr>,
) -> String {
    let host = &first.host;
    let cause = match operator {
        Some(operator) => format!(
            "Bun on {host} already admits {operator}, the address setup saw this machine \
             connect from, so a firewall is dropping TCP port {API_PORT}. Open that port on \
             {host} to {operator} (with UFW: `sudo ufw allow proto tcp from {operator} to any \
             port {API_PORT}`) and in any cloud firewall in front of it. If this machine's \
             address has changed since setup, tunnel instead."
        ),
        None => format!(
            "Setup couldn't tell this machine's address, so Bun on {host} doesn't admit it. \
             Add the address to `operator_cidrs` in /etc/reliaburger/node.toml on {host}, run \
             `sudo systemctl restart reliaburger.service`, and open TCP port {API_PORT} to it."
        ),
    };
    let port = if first.port == 22 {
        String::new()
    } else {
        format!(" -p {}", first.port)
    };
    let destination = match &first.user {
        Some(user) => format!("{user}@{host}"),
        None => host.clone(),
    };
    let security = security.display();
    format!(
        "{cause}\nOr tunnel through SSH, which needs no firewall change:\n  \
         ssh -f -N -L {local_port}:127.0.0.1:{API_PORT}{port} {destination}\n  \
         export RELIABURGER_ENDPOINT=https://127.0.0.1:{local_port}\n  \
         export RELIABURGER_CA_CERT='{security}/identity/root-ca.crt'\n  \
         export RELIABURGER_TOKEN=\"$(cat '{security}/admin.token')\"\n\
         (docs/linux-servers.md, section 5.3)"
    )
}

/// What to tell someone whose `relish` couldn't reach the active cluster,
/// when `relish setup --ssh` built it; `None` for any other cluster.
pub fn unreachable_hint() -> Option<String> {
    let root = crate::relish::local_context::root_directory().ok()?;
    unreachable_hint_in(&root)
}

fn unreachable_hint_in(root: &Path) -> Option<String> {
    let context = LocalContext::load(&root.join("context.json")).ok()??;
    let (directory, state) = super::state::find_owned(root, &context.owner)?;
    let first = state.spec.ssh.first()?;
    let config = directory.join(format!("{}.toml", state.nodes.first()?.name));
    Some(format!(
        "relish can't reach the API of cluster {} at {}, a machine `relish setup --ssh` set up.\n{}",
        state.spec.name,
        context.endpoint,
        unreachable_api_advice(
            first,
            state.spec.api_port,
            &directory.join("security"),
            saved_operator(&config),
        )
    ))
}

/// The operator address setup admitted in the first node's config, if any.
fn saved_operator(config: &Path) -> Option<Ipv4Addr> {
    let text = std::fs::read_to_string(config).ok()?;
    let config: crate::config::node::NodeConfig = toml::from_str(&text).ok()?;
    let cidr = config.security.operator_cidrs.first()?;
    cidr.strip_suffix("/32")?.parse().ok()
}

/// Check every machine, then configure them and prove the cluster works:
/// everything `relish setup --ssh` does to machines, through `ssh`.
async fn provision_cluster(
    root: &Path,
    ssh: &Ssh,
    operation: &mut Operation,
    bootstrap: &Bootstrap,
    options: &Options,
    downloader: &Downloader,
    progress: &Progress,
) -> Result<Option<String>> {
    let (facts, binaries) = prepare(
        root,
        ssh,
        operation,
        &options.advertise,
        options.development_binaries.as_deref(),
        downloader,
        progress,
    )
    .await?;
    build(root, ssh, operation, bootstrap, &facts, &binaries, progress).await
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
        name: options.name.clone(),
        nodes: options.targets.len(),
        version,
        api_port: options.api_port,
        ingress_port: options.ingress_port,
        registry_port: 15050,
        ssh: options.targets.clone(),
    };
    let (mut operation, bootstrap, _setup_lock) = open_operation(&root, spec).await?;
    let ssh = Ssh::new(COMMAND_DEADLINE);
    let progress = Progress::for_output(options.output);
    let result = provision_cluster(
        &root,
        &ssh,
        &mut operation,
        &bootstrap,
        &options,
        &downloader,
        &progress,
    )
    .await;
    let timings = progress.finish().await;
    output::progress(options.output, timings.summary());
    match result {
        Ok(unreachable) => {
            let context = root.join("context.json");
            let relish = crate::relish::install::invocation();
            let report = SetupReport::servers(ServerFacts {
                operation: &operation,
                timings: &timings,
                api: saved_endpoint(&context).await,
                context,
                back_up: bootstrap.directory.clone(),
                unreachable,
                relish: &relish,
                root_backup: bootstrap.root_backup.clone(),
            });
            output::print_output(&report, options.output)?;
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

    /// Stands in for `ssh` to a set of machines, as `fake_lima` does for
    /// `limactl`. It logs `<host> <user|root> <step>` for every call, names
    /// the step from the script it was sent, and plays the machine's part:
    /// a pre-flight report from `facts/<host>` (with `used=yes` once files
    /// are installed there), the list of files it was sent, and a peer that
    /// already holds an identity, so enrolment needs no API. Any step fails
    /// while `fail/<host>-<step>` exists.
    ///
    /// It can't stand in for Bun, so a tunnel to a node's API always closes:
    /// that's where these tests stop. Readiness, the council and the demo
    /// need a real cluster (`scripts/release/qualify-ssh-setup.sh`).
    const FAKE_SSH: &str = r#"tunnel=no
while [ "$1" != -- ]; do [ "$1" = -N ] && tunnel=yes; shift; done
shift
host=${1#*@}
remote=$2
machine="$FAKE/machines/$host"
mkdir -p "$machine"
case "$remote" in "sudo -n "*) as=root;; *) as=user;; esac
if [ "$tunnel" = yes ]; then step=tunnel
else
  case "$remote" in
    *os-release*) step=preflight;;
    *apt-get*) step=packages;;
    *rb-install*) step=files;;
    *"ln -sfn"*) step=link;;
    *bundle.committed*) step=enrolled;;
    *systemctl*) step=start;;
    *) step=unknown;;
  esac
fi
echo "$host $as $step" >> "$FAKE/calls"
if [ -e "$FAKE/fail/$host-$step" ]; then echo "$step failed on $host" >&2; exit 1; fi
case "$step" in
  preflight) cat "$FAKE/facts/$host"; [ -e "$machine/etc" ] && echo used=yes;;
  files) mkdir -p "$machine/etc"; tar -t -f - > "$machine/files";;
  tunnel) echo "no Bun here" >&2; exit 255;;
esac
exit 0
"#;

    const HOSTS: [&str; 3] = ["a", "b", "c"];

    /// Three fake machines, and a `RELIABURGER_HOME` to set them up from.
    struct Machines {
        fake: tempfile::TempDir,
        home: tempfile::TempDir,
        ssh: Ssh,
        api_port: u16,
    }

    impl Machines {
        fn new() -> Self {
            let fake = tempfile::tempdir().unwrap();
            let script = fake.path().join("ssh");
            let preamble = format!("FAKE='{}'\n", fake.path().display());
            std::fs::write(&script, format!("{preamble}{FAKE_SSH}")).unwrap();
            for directory in ["facts", "fail", "bin"] {
                std::fs::create_dir(fake.path().join(directory)).unwrap();
            }
            for (index, host) in HOSTS.iter().enumerate() {
                let facts = format!(
                    "os=ubuntu\nkernel=6.8.0-45-generic\narch=aarch64\ncpus=4\n\
                     memory_kib=8000000\ndisk_kib=40000000\ncgroup2=yes\nsystemd=yes\n\
                     sudo=yes\naddresses=10.0.0.{}\noperator=10.0.0.50\n",
                    index + 1
                );
                std::fs::write(fake.path().join("facts").join(host), facts).unwrap();
            }
            for binary in ["bun", "relish"] {
                std::fs::write(fake.path().join("bin").join(binary), binary).unwrap();
            }
            // Nothing may listen on any machine's port, so a tunnel can't seem
            // to open by accident. A port from `bind(0)` is in the ephemeral
            // range, where the next outgoing connections on the host land:
            // one of those, or a listener another test bound there, answered
            // a tunnel's probe, and the test waited out the 60 s readiness
            // deadline (#1608). Below the ephemeral ranges of both macOS
            // (49152) and Linux (32768), only a fixed port collides, and the
            // integration fixtures keep theirs in 27000..30000.
            let api_port = std::iter::repeat_with(|| rand::random_range(20_000..27_000))
                .find(|base: &u16| {
                    (*base..*base + HOSTS.len() as u16)
                        .all(|port| std::net::TcpListener::bind(("127.0.0.1", port)).is_ok())
                })
                .unwrap();
            Self {
                ssh: Ssh::shell_script(script, Duration::from_secs(10), ""),
                fake,
                home: tempfile::tempdir().unwrap(),
                api_port,
            }
        }

        fn options(&self) -> Options {
            Options {
                name: "servers".into(),
                targets: HOSTS
                    .iter()
                    .map(|host| format!("u@{host}").parse().unwrap())
                    .collect(),
                advertise: Vec::new(),
                api_port: self.api_port,
                ingress_port: 18080,
                development_binaries: Some(self.fake.path().join("bin")),
                release_mirror: None,
                output: OutputFormat::Human,
            }
        }

        fn spec(&self) -> ClusterSpec {
            let options = self.options();
            ClusterSpec {
                name: options.name,
                nodes: options.targets.len(),
                version: env!("CARGO_PKG_VERSION").parse().unwrap(),
                api_port: options.api_port,
                ingress_port: options.ingress_port,
                registry_port: 15050,
                ssh: options.targets,
            }
        }

        /// What `run` does after parsing its options, up to the first tunnel.
        async fn set_up(&self) -> Result<Option<String>> {
            let options = self.options();
            let (mut operation, bootstrap, _lock) =
                open_operation(self.home.path(), self.spec()).await?;
            let progress = super::super::progress::tests_support::silent();
            let downloader = Downloader::new(DOWNLOAD_STALL)?;
            let result = provision_cluster(
                self.home.path(),
                &self.ssh,
                &mut operation,
                &bootstrap,
                &options,
                &downloader,
                &progress,
            )
            .await;
            progress.finish().await;
            result
        }

        fn fail(&self, host: &str, step: &str) {
            std::fs::write(self.fake.path().join(format!("fail/{host}-{step}")), "").unwrap();
        }

        fn recover(&self, host: &str, step: &str) {
            std::fs::remove_file(self.fake.path().join(format!("fail/{host}-{step}"))).unwrap();
        }

        /// Every call so far, then forget them.
        fn take_calls(&self) -> Vec<String> {
            let path = self.fake.path().join("calls");
            let calls = std::fs::read_to_string(&path).unwrap_or_default();
            let _ = std::fs::remove_file(path);
            calls.lines().map(str::to_owned).collect()
        }

        /// The steps one machine was asked to do, in order.
        fn steps_on(calls: &[String], host: &str) -> Vec<String> {
            calls
                .iter()
                .filter_map(|call| call.strip_prefix(&format!("{host} ")))
                .map(str::to_owned)
                .collect()
        }

        fn phases(&self) -> Vec<NodePhase> {
            let operation = Operation::load(self.home.path(), "servers").unwrap();
            operation
                .state
                .nodes
                .iter()
                .map(|node| node.phase)
                .collect()
        }

        fn installed(&self, host: &str) -> String {
            std::fs::read_to_string(self.fake.path().join("machines").join(host).join("files"))
                .unwrap_or_default()
        }
    }

    fn stopped_at_the_tunnel(error: &anyhow::Error) -> bool {
        format!("{error:#}").contains("the tunnel to port 9117 closed")
    }

    /// Every machine gets the same steps in the same order, after every
    /// machine has passed pre-flight. `build` runs them for the first node
    /// and then the rest; here each node runs on its own, so a missing Bun
    /// on the first doesn't stop the others.
    #[tokio::test]
    async fn each_machine_is_checked_then_set_up_step_by_step() {
        let machines = Machines::new();
        let (mut operation, bootstrap, _lock) =
            open_operation(machines.home.path(), machines.spec())
                .await
                .unwrap();
        let progress = super::super::progress::tests_support::silent();
        let downloader = Downloader::new(DOWNLOAD_STALL).unwrap();
        let options = machines.options();
        let (facts, binaries) = prepare(
            machines.home.path(),
            &machines.ssh,
            &mut operation,
            &[],
            options.development_binaries.as_deref(),
            &downloader,
            &progress,
        )
        .await
        .unwrap();
        let addresses: Vec<_> = operation.state.nodes.iter().map(|n| n.address).collect();
        assert_eq!(
            addresses,
            ["10.0.0.1", "10.0.0.2", "10.0.0.3"].map(|a| Some(a.parse().unwrap()))
        );
        let setup = Setup::new(
            &machines.ssh,
            &progress,
            &operation,
            &bootstrap,
            &facts,
            &binaries,
        )
        .await
        .unwrap();
        let client = bootstrap.client(&setup.tunnel_endpoint(0)).unwrap();
        let checkpoints = tokio::sync::Mutex::new(&mut operation);
        for index in 0..HOSTS.len() {
            let client = (index > 0).then_some(&client);
            let error = tokio::time::timeout(
                Duration::from_secs(30),
                configure(&setup, index, client, &checkpoints),
            )
            .await
            .unwrap_or_else(|_| panic!("node {} never stopped at its tunnel", index + 1))
            .unwrap_err();
            assert!(stopped_at_the_tunnel(&error), "{error:#}");
        }
        // Release the operation lock, which `phases` takes.
        drop(operation);
        progress.finish().await;

        let calls = machines.take_calls();
        let first_change = calls.iter().position(|call| call.contains(" root "));
        let last_check = calls.iter().rposition(|call| call.ends_with("preflight"));
        assert!(last_check < first_change, "{calls:#?}");
        assert_eq!(
            Machines::steps_on(&calls, "a"),
            [
                "user preflight",
                "root packages",
                "root files",
                "root link",
                "root start",
                "user tunnel"
            ]
        );
        for host in ["b", "c"] {
            assert_eq!(
                Machines::steps_on(&calls, host),
                [
                    "user preflight",
                    "root packages",
                    "root files",
                    "root link",
                    "root enrolled",
                    "root start",
                    "user tunnel"
                ],
                "{host}"
            );
        }
        assert_eq!(machines.phases(), [NodePhase::Started; 3]);
        let bun = format!("usr/local/bin/{}", versioned_bun(&machines.spec().version));
        for host in HOSTS {
            let installed = machines.installed(host);
            assert!(installed.contains(&bun), "{host}: {installed}");
            assert!(installed.contains("etc/reliaburger/node.toml"), "{host}");
            assert!(installed.contains("etc/reliaburger/master.key"), "{host}");
        }
        // Only the first node is handed the cluster's first identity.
        assert!(machines.installed("a").contains("security-bootstrap.json"));
        assert!(!machines.installed("b").contains("security-bootstrap.json"));
    }

    #[tokio::test]
    async fn a_machine_that_fails_preflight_means_no_machine_is_touched() {
        let machines = Machines::new();
        let facts = machines.fake.path().join("facts/b");
        let report = std::fs::read_to_string(&facts).unwrap();
        std::fs::write(&facts, format!("{report}disk_kib=1000\n")).unwrap();
        machines.fail("c", "preflight");

        let error = format!("{:#}", machines.set_up().await.unwrap_err());
        assert!(
            error.contains("nothing was changed on any machine"),
            "{error}"
        );
        assert!(error.contains("u@b has 0 GiB free"), "{error}");
        assert!(error.contains("preflight failed on c"), "{error}");
        let calls = machines.take_calls();
        let mut checked: Vec<_> = calls.iter().map(String::as_str).collect();
        checked.sort_unstable();
        assert_eq!(
            checked,
            ["a user preflight", "b user preflight", "c user preflight"]
        );
        assert_eq!(machines.phases(), [NodePhase::Planned; 3]);
    }

    #[tokio::test]
    async fn a_rerun_after_a_failure_resumes_without_redoing_finished_steps() {
        let machines = Machines::new();
        machines.fail("a", "link");
        let error = machines.set_up().await.unwrap_err();
        assert!(
            format!("{error:#}").contains("link failed on a"),
            "{error:#}"
        );
        let calls = machines.take_calls();
        assert_eq!(
            Machines::steps_on(&calls, "a"),
            ["user preflight", "root packages", "root files", "root link"]
        );
        assert_eq!(machines.phases()[0], NodePhase::Created);

        // The machine now has /etc/reliaburger, which pre-flight accepts
        // because this operation put it there.
        machines.recover("a", "link");
        let error = machines.set_up().await.unwrap_err();
        assert!(stopped_at_the_tunnel(&error), "{error:#}");
        let calls = machines.take_calls();
        assert_eq!(
            Machines::steps_on(&calls, "a"),
            [
                "user preflight",
                "root files",
                "root link",
                "root start",
                "user tunnel"
            ]
        );
        assert_eq!(machines.phases()[0], NodePhase::Started);

        // Once started, a rerun only restarts the service and reconnects.
        machines.set_up().await.unwrap_err();
        let calls = machines.take_calls();
        assert_eq!(
            Machines::steps_on(&calls, "a"),
            ["user preflight", "root start", "user tunnel"]
        );
    }

    #[tokio::test]
    async fn a_new_setup_refuses_machines_another_setup_started() {
        let machines = Machines::new();
        machines.set_up().await.unwrap_err();
        machines.take_calls();
        let elsewhere = Machines {
            fake: machines.fake,
            home: tempfile::tempdir().unwrap(),
            ssh: machines.ssh,
            api_port: machines.api_port,
        };
        let error = format!("{:#}", elsewhere.set_up().await.unwrap_err());
        assert!(
            error.contains("u@a already has /etc/reliaburger"),
            "{error}"
        );
        let calls = elsewhere.take_calls();
        assert!(calls.iter().all(|call| call.ends_with("user preflight")));
    }

    #[test]
    fn unreachable_api_advice_names_the_address_to_open_and_the_tunnel() {
        let security = Path::new("/home/me/.reliaburger/clusters/servers/security");
        let first: SshTarget = "ubuntu@203.0.113.10:2222".parse().unwrap();
        let advice = unreachable_api_advice(
            &first,
            19117,
            security,
            Some("198.51.100.7".parse().unwrap()),
        );
        assert!(
            advice.contains("sudo ufw allow proto tcp from 198.51.100.7 to any port 9117"),
            "{advice}"
        );
        assert!(advice.contains("Open that port on 203.0.113.10 to 198.51.100.7"));
        assert!(
            advice.contains("ssh -f -N -L 19117:127.0.0.1:9117 -p 2222 ubuntu@203.0.113.10\n"),
            "{advice}"
        );
        assert!(advice.contains("export RELIABURGER_ENDPOINT=https://127.0.0.1:19117\n"));
        assert!(advice.contains(&format!(
            "RELIABURGER_CA_CERT='{}/identity/root-ca.crt'",
            security.display()
        )));
        assert!(advice.contains(&format!("cat '{}/admin.token'", security.display())));
        assert!(advice.contains("section 5.3"));

        let unknown = unreachable_api_advice(&"node-1".parse().unwrap(), 19117, security, None);
        assert!(unknown.contains("doesn't admit it"), "{unknown}");
        assert!(unknown.contains("operator_cidrs"), "{unknown}");
        assert!(unknown.contains("ssh -f -N -L 19117:127.0.0.1:9117 node-1\n"));
        assert!(!unknown.contains("ufw"), "{unknown}");
    }

    /// After setup, `relish` on a machine a firewall keeps out of port 9117
    /// prints this under its error, naming the address setup admitted.
    #[tokio::test]
    async fn an_unreachable_ssh_cluster_says_what_to_open_or_how_to_tunnel() {
        let machines = Machines::new();
        machines.set_up().await.unwrap_err();
        let root = machines.home.path();
        assert!(unreachable_hint_in(root).is_none(), "no context, no hint");

        let operation = Operation::load(root, "servers").unwrap();
        let mut context = LocalContext {
            schema: 1,
            owner: operation.state.id.clone(),
            endpoint: "https://a:9117".into(),
            token: "rbrg_private".into(),
            ca_cert: operation.directory.join("security/identity/root-ca.crt"),
            service_endpoints: Default::default(),
        };
        drop(operation);
        context.save(&root.join("context.json")).unwrap();
        let hint = unreachable_hint_in(root).unwrap();
        assert!(
            hint.starts_with("relish can't reach the API of cluster servers at https://a:9117"),
            "{hint}"
        );
        assert!(hint.contains("from 10.0.0.50 to any port 9117"), "{hint}");
        let tunnel = format!("ssh -f -N -L {}:127.0.0.1:9117 u@a", machines.api_port);
        assert!(hint.contains(&tunnel), "{hint}");
        assert!(
            !hint.contains("rbrg_private"),
            "the token stays in its file"
        );

        std::fs::remove_file(root.join("context.json")).unwrap();
        context.owner = "f".repeat(32);
        context.save(&root.join("context.json")).unwrap();
        assert!(
            unreachable_hint_in(root).is_none(),
            "another cluster's context"
        );
    }
}
