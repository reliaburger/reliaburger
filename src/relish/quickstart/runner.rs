//! Managed cluster creation, secure enrolment and application verification.

use super::{
    artifacts,
    download::Downloader,
    lima::{GuestFile, Lima},
    progress::{Progress, Stage, Step, Timings},
    provision,
    security::{self, Bootstrap},
    state::{ClusterSpec, NodePhase, NodeState, Operation},
};
use crate::{
    bun::agent::CouncilStatus,
    relish::{client::BunClient, local_context::LocalContext},
    upgrade::BinaryVersion,
};
use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, stream::FuturesUnordered};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// Choices for creating or resuming one laptop cluster.
pub struct Options {
    /// Stable cluster name.
    pub name: String,
    /// One or three VMs.
    pub nodes: usize,
    /// First host API port.
    pub api_port: u16,
    /// Browser-facing HTTP ingress port.
    pub ingress_port: u16,
    /// Host port forwarded to Pickle on the first node.
    pub registry_port: u16,
    /// Explicit development-only directory containing prebuilt Linux bun and relish.
    pub development_binaries: Option<PathBuf>,
    /// HTTPS directory containing unchanged signed release candidate assets.
    pub release_mirror: Option<String>,
    /// Print every step's timing, not just the per-stage summary.
    pub timings: bool,
}

/// How long a download may go without receiving a byte before it fails.
const DOWNLOAD_STALL: Duration = Duration::from_secs(30);

/// Backstop for all downloads together. A transfer that keeps making progress
/// is left alone until then, and its partial file survives for a re-run.
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// Budget for everything after the downloads: VMs, nodes, quorum and demo.
const CLUSTER_DEADLINE: Duration = Duration::from_secs(300);

/// Create or resume a cluster, preserving checkpoints. Building the cluster
/// has a five-minute deadline; downloads have their own, because their speed
/// depends on the network rather than on us.
pub async fn run(options: Options) -> Result<()> {
    if options.release_mirror.is_some() && options.development_binaries.is_some() {
        bail!("a release mirror cannot be combined with development binaries");
    }
    let version = env!("CARGO_PKG_VERSION").parse::<BinaryVersion>()?;
    let mut downloader = Downloader::new(DOWNLOAD_STALL)?;
    if let Some(mirror) = &options.release_mirror {
        downloader = downloader.with_release_mirror(&version, mirror)?;
        eprintln!("using an explicit release mirror with checksum and signature verification");
    }
    let root = crate::relish::local_context::root_directory()?;
    let spec = ClusterSpec {
        name: options.name,
        nodes: options.nodes,
        version,
        api_port: options.api_port,
        ingress_port: options.ingress_port,
        registry_port: options.registry_port,
    };
    let operation_root = root.clone();
    let (mut operation, bootstrap, _setup_lock) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            let operation = Operation::open(&operation_root, &spec)?;
            let mut lock_options = std::fs::OpenOptions::new();
            lock_options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                lock_options.mode(0o600);
            }
            let setup_lock = lock_options.open(operation_root.join("setup.lock"))?;
            setup_lock.try_lock().context("another managed setup is using this state directory")?;
            if LocalContext::load(&operation_root.join("context.json"))?
                .is_some_and(|context| context.owner != operation.state.id)
            {
                bail!("another cluster owns the active context; use a separate RELIABURGER_HOME for a second cluster");
            }
            let bootstrap = security::prepare(&operation)?;
            Ok((operation, bootstrap, setup_lock))
        })
        .await??;
    super::preflight::socket_paths(&root, operation.state.nodes.iter().map(|node| &node.name))?;
    let progress = Progress::stdout();
    let result = setup(
        &root,
        &mut operation,
        &bootstrap,
        options.development_binaries.as_deref(),
        &downloader,
        &progress,
    )
    .await;
    let timings = progress.finish().await;
    println!("{}", timings.summary());
    if options.timings {
        println!("every step (duration, start offset):\n{}", timings.table());
    }
    let report = TimingsReport {
        schema: 1,
        version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        nodes: operation.state.spec.nodes,
        development_binaries: options.development_binaries.is_some(),
        succeeded: result.is_ok(),
        timings: &timings,
    };
    let timings_path = operation.directory.join("timings.json");
    // Timings are a diagnostic; failing to save them mustn't fail setup.
    match save_timings(&timings_path, &report).await {
        Ok(()) => println!("timings saved to {}", timings_path.display()),
        Err(error) => eprintln!("could not save timings: {error:#}"),
    }
    match result {
        Ok(()) => {
            println!(
                "cluster {} ready in {:.1}s",
                operation.state.spec.name, timings.total_seconds
            );
            println!(
                "  app: http://localhost:{}",
                operation.state.spec.ingress_port
            );
            // Straight after install.sh, `relish` may not be on PATH yet.
            let relish = crate::relish::install::invocation();
            println!("  next: {relish} manual tour   (the five-minute tour of this cluster)");
            println!("  or: {relish} status; {relish} logs hello; {relish} dashboard");
            println!(
                "  lifecycle: {relish} local status|stop|start|destroy --name {}",
                operation.state.spec.name
            );
            Ok(())
        }
        Err(error) => Err(error.context(format!(
            "setup stopped; checkpoints are in {}. Re-run the same setup command to resume",
            operation.directory.display()
        ))),
    }
}

/// What `timings.json` in the cluster directory holds after every run.
#[derive(serde::Serialize)]
struct TimingsReport<'a> {
    schema: u32,
    version: &'static str,
    os: &'static str,
    arch: &'static str,
    nodes: usize,
    development_binaries: bool,
    succeeded: bool,
    #[serde(flatten)]
    timings: &'a Timings,
}

async fn save_timings(path: &Path, report: &TimingsReport<'_>) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(report)?;
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        crate::sesame::identity::atomic_write_mode(&path, &bytes, Some(0o600))
    })
    .await??;
    Ok(())
}

/// Check the host, fetch everything, then build the cluster, each under its deadline.
async fn setup(
    root: &Path,
    operation: &mut Operation,
    bootstrap: &Bootstrap,
    development: Option<&Path>,
    downloader: &Downloader,
    progress: &Progress,
) -> Result<()> {
    let host = progress.step(Stage::Host, "check host");
    host.record(super::preflight::host(root).await)?;
    let artifacts = tokio::time::timeout(
        DOWNLOAD_DEADLINE,
        prepare_artifacts(
            root,
            &operation.state.spec.version,
            development,
            downloader,
            progress,
        ),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "downloads did not finish within {} minutes; partial files are kept and a re-run resumes them",
            DOWNLOAD_DEADLINE.as_secs() / 60
        )
    })??;
    tokio::time::timeout(
        CLUSTER_DEADLINE,
        build_cluster(root, operation, bootstrap, &artifacts, progress),
    )
    .await
    .map_err(|_| anyhow::anyhow!("building the cluster took longer than five minutes"))?
}

/// Verified local inputs for building the cluster.
struct Artifacts {
    lima: Lima,
    image: PathBuf,
    bun: PathBuf,
    relish: PathBuf,
}

/// Fetch Lima, the guest image and the Linux binaries concurrently.
async fn prepare_artifacts(
    root: &Path,
    version: &BinaryVersion,
    development: Option<&Path>,
    downloader: &Downloader,
    progress: &Progress,
) -> Result<Artifacts> {
    let cache = root.join("cache");
    tokio::fs::create_dir_all(&cache).await?;
    let binaries = async {
        if let Some(directory) = development {
            progress.note(
                "development binaries selected explicitly; this run does not qualify a published release",
            );
            let step = progress.step(Stage::Download, "use development binaries");
            let result = async {
                let bun = tokio::fs::canonicalize(directory.join("bun")).await?;
                let relish = tokio::fs::canonicalize(directory.join("relish")).await?;
                Ok::<_, anyhow::Error>((bun, relish))
            }
            .await;
            return step.record(result);
        }
        let bun = progress.step(Stage::Download, "download bun");
        let relish = progress.step(Stage::Download, "download relish");
        tokio::try_join!(
            async { bun.record(artifacts::binary(&cache, version, "bun", downloader, &bun).await) },
            async {
                relish
                    .record(artifacts::binary(&cache, version, "relish", downloader, &relish).await)
            }
        )
    };
    let image = async {
        let step = progress.step(Stage::Download, "download guest image");
        let result = if development.is_some() {
            async {
                let image = artifacts::guest_image(std::env::consts::ARCH)?;
                let path = cache.join(&image.asset);
                downloader
                    .fetch(
                        &image.url,
                        &image.sha256,
                        &path,
                        2 * 1024 * 1024 * 1024,
                        Some(&step),
                    )
                    .await?;
                Ok::<_, anyhow::Error>(path)
            }
            .await
        } else {
            artifacts::image(&cache, version, downloader, &step).await
        };
        step.record(result)
    };
    let tooling = async {
        let step = progress.step(Stage::Download, "install Lima 2.1.0");
        step.record(artifacts::tooling(root, downloader, &step).await)
    };
    let (lima, image, (bun, relish)) = tokio::try_join!(tooling, image, binaries)?;
    Ok(Artifacts {
        lima,
        image,
        bun,
        relish,
    })
}

/// Boot, configure and verify the cluster from verified local inputs.
async fn build_cluster(
    root: &Path,
    operation: &mut Operation,
    bootstrap: &Bootstrap,
    artifacts: &Artifacts,
    progress: &Progress,
) -> Result<()> {
    let spec = operation.state.spec.clone();
    let Artifacts {
        lima,
        image,
        bun,
        relish,
    } = artifacts;
    let mut statuses = Vec::new();
    for node in &operation.state.nodes {
        statuses.push(lima.status(&node.name).await?);
    }
    let to_start = statuses
        .iter()
        .filter(|status| status.as_deref() != Some("Running"))
        .count();
    let to_create = statuses.iter().filter(|status| status.is_none()).count();
    super::preflight::resources(to_start, to_create, root).await?;
    // Lima checks for its shared SSH key before taking a lock, so concurrent
    // first starts can overwrite each other's key. Create it ourselves first.
    lima.ensure_user_key().await?;
    let mut boots = FuturesUnordered::new();
    let nodes = operation.state.nodes.clone();
    for (index, node) in nodes.into_iter().enumerate() {
        let config_path = operation.directory.join(format!("{}.yaml", node.name));
        let yaml = provision::vm_config(
            image.to_str().context("non-UTF-8 image cache path")?,
            std::env::consts::ARCH,
            spec.api_port + index as u16,
            (index == 0).then_some(spec.ingress_port),
            (index == 0).then_some(spec.registry_port),
        )?;
        tokio::fs::write(&config_path, yaml).await?;
        let mut ports = vec![spec.api_port + index as u16];
        if index == 0 {
            ports.extend([spec.ingress_port, spec.registry_port]);
        }
        let step = progress.step(Stage::Boot, format!("boot VM {}", index + 1));
        boots.push(boot_vm(
            lima.clone(),
            node,
            index,
            statuses[index].clone(),
            config_path,
            ports,
            step,
        ));
        // The first start also launches Lima's shared network daemon, which
        // Lima does not guard against a concurrent second launch. Peers start
        // as soon as it runs, while the first VM is still booting.
        if index == 0 && to_start > 1 {
            wait_for_shared_network(lima, &mut boots, operation).await?;
        }
    }
    while let Some(result) = boots.next().await {
        record_boot(operation, result?).await?;
    }
    let peers: Vec<_> = operation
        .state
        .nodes
        .iter()
        .filter_map(|node| node.address)
        .collect();
    let first_address = operation.state.nodes[0]
        .address
        .context("bootstrap VM has no address")?;
    let endpoint = format!("https://127.0.0.1:{}", spec.api_port);
    let client = bootstrap.client(&endpoint)?;
    let service_path = operation.directory.join("reliaburger.service");
    tokio::fs::write(&service_path, provision::SERVICE).await?;
    let setup = NodeSetup {
        lima,
        progress,
        spec: &spec,
        bootstrap,
        client: &client,
        directory: operation.directory.clone(),
        sources: NodeSources {
            bun,
            relish,
            service: &service_path,
            security: &bootstrap.directory,
        },
        peers: &peers,
        first_address,
    };
    {
        // Peers enrol through the first node, so it must be ready first. The
        // rest configure concurrently; each writes only its own checkpoint.
        let checkpoints = tokio::sync::Mutex::new(&mut *operation);
        configure_node(&setup, 0, &checkpoints).await?;
        let mut peers: FuturesUnordered<_> = (1..spec.nodes)
            .map(|index| configure_node(&setup, index, &checkpoints))
            .collect();
        while let Some(result) = peers.next().await {
            result?;
        }
    }
    let names: Vec<_> = operation
        .state
        .nodes
        .iter()
        .map(|node| node.name.clone())
        .collect();
    let quorum = progress.step(Stage::Verify, "form council quorum");
    quorum.record(wait_for_quorum(&client, &names).await)?;
    let demo_step = progress.step(Stage::Verify, "run hello through ingress");
    let result = async {
        let demo = demo_config();
        tokio::fs::write(operation.directory.join("hello.toml"), &demo).await?;
        client.apply(&crate::config::Config::parse(&demo)?).await?;
        probe_demo(spec.ingress_port).await
    }
    .await;
    demo_step.record(result)?;
    let context = LocalContext {
        schema: 1,
        owner: operation.state.id.clone(),
        endpoint,
        token: tokio::fs::read_to_string(bootstrap.directory.join("admin.token")).await?,
        ca_cert: bootstrap.directory.join("identity/root-ca.crt"),
        service_endpoints: crate::bun::capabilities::ServiceEndpoints {
            registry: Some(format!("https://127.0.0.1:{}", spec.registry_port)),
            ingress_http: Some(format!("http://127.0.0.1:{}", spec.ingress_port)),
            ingress_https: None,
        },
    };
    let path = root.join("context.json");
    tokio::task::spawn_blocking(move || context.save(&path)).await??;
    Ok(())
}

/// Everything shared by the per-node configuration steps.
struct NodeSetup<'a> {
    lima: &'a Lima,
    progress: &'a Progress,
    spec: &'a ClusterSpec,
    bootstrap: &'a Bootstrap,
    client: &'a BunClient,
    directory: PathBuf,
    sources: NodeSources<'a>,
    peers: &'a [std::net::Ipv4Addr],
    first_address: std::net::Ipv4Addr,
}

/// Host files installed on every node.
struct NodeSources<'a> {
    bun: &'a Path,
    relish: &'a Path,
    service: &'a Path,
    /// The private bootstrap directory: master key and first identity.
    security: &'a Path,
}

/// Save one node's checkpoint while other nodes configure concurrently.
async fn checkpoint(
    operation: &tokio::sync::Mutex<&mut Operation>,
    index: usize,
    phase: NodePhase,
) -> Result<()> {
    let mut operation = operation.lock().await;
    operation.state.nodes[index].phase = phase;
    operation.save_async().await?;
    Ok(())
}

/// The files a node needs, in install order. The first node also gets the
/// cluster's bootstrap identity, with its commit marker last, just as the
/// local identity store writes it.
fn node_files(sources: &NodeSources<'_>, index: usize, config: PathBuf) -> Vec<GuestFile> {
    let security = sources.security;
    let mut files = vec![
        GuestFile::executable(sources.bun.to_owned(), "/usr/local/bin/bun"),
        GuestFile::executable(sources.relish.to_owned(), "/usr/local/bin/relish"),
        GuestFile::private(config, "/etc/reliaburger/node.toml"),
        GuestFile::private(security.join("master.key"), "/etc/reliaburger/master.key"),
        GuestFile {
            mode: 0o644,
            ..GuestFile::private(
                sources.service.to_owned(),
                "/etc/systemd/system/reliaburger.service",
            )
        },
    ];
    if index == 0 {
        files.push(GuestFile::private(
            security.join("security-bootstrap.json"),
            "/etc/reliaburger/security-bootstrap.json",
        ));
        for file in [
            "node.crt",
            "node.key",
            "node-ca.crt",
            "root-ca.crt",
            "meta.json",
            "node.bundle.json",
            "bundle.committed",
        ] {
            files.push(GuestFile::private(
                security.join("identity").join(file),
                &format!("/etc/reliaburger/identity/{file}"),
            ));
        }
    }
    files
}

/// Install, enrol and start one node, then wait until it's ready.
async fn configure_node(
    setup: &NodeSetup<'_>,
    index: usize,
    operation: &tokio::sync::Mutex<&mut Operation>,
) -> Result<()> {
    let node = operation.lock().await.state.nodes[index].clone();
    let lima = setup.lima;
    let number = index + 1;
    if node.phase != NodePhase::Started {
        let step = setup
            .progress
            .step(Stage::Configure, format!("install files on node {number}"));
        let result = async {
            let config = provision::node_config(
                &setup.spec.name,
                &node.name,
                node.address.context("VM has no address")?,
                (index > 0).then_some(setup.first_address),
                setup.peers,
            )?;
            let config_path = setup.directory.join(format!("{}.toml", node.name));
            tokio::fs::write(&config_path, config).await?;
            lima.install_files(&node.name, node_files(&setup.sources, index, config_path))
                .await
        }
        .await;
        step.record(result)?;
        checkpoint(operation, index, NodePhase::Configured).await?;
        if index > 0 {
            let step = setup
                .progress
                .step(Stage::Configure, format!("enrol node {number}"));
            step.record(enrol(setup, &node.name, &step).await)?;
        }
        checkpoint(operation, index, NodePhase::Enrolled).await?;
    }
    let step = setup
        .progress
        .step(Stage::Configure, format!("start node {number}"));
    let result = async {
        if node.phase != NodePhase::Started {
            lima.command(&[
                "shell",
                &node.name,
                "sudo",
                "sh",
                "-c",
                "systemctl daemon-reload && systemctl enable --now reliaburger.service",
            ])
            .await?;
            checkpoint(operation, index, NodePhase::Started).await?;
        } else {
            lima.command(&[
                "shell",
                &node.name,
                "sudo",
                "systemctl",
                "start",
                "reliaburger.service",
            ])
            .await?;
        }
        let node_client = setup.bootstrap.client(&format!(
            "https://127.0.0.1:{}",
            setup.spec.api_port + index as u16
        ))?;
        crate::relish::readiness::wait_for_node(&node_client, Duration::from_secs(45)).await?;
        let version: BinaryVersion = node_client.node_version().await?.parse()?;
        if version != setup.spec.version {
            bail!(
                "VM {} is running {version}, expected {}",
                node.name,
                setup.spec.version
            );
        }
        Ok(())
    }
    .await;
    step.record(result)
}

/// Guest script that stores a join token from standard input only for as
/// long as `relish join` needs it. Arguments: node name, CA fingerprint, URL.
const JOIN_SCRIPT: &str = "set -eu\numask 077\ntoken=/etc/reliaburger/join.token\n\
trap 'rm -f -- \"$token\"' EXIT\ncat > \"$token\"\n\
/usr/local/bin/relish join --token-file \"$token\" --node-id \"$1\" \
--identity-dir /etc/reliaburger/identity --ca-fingerprint \"$2\" \"$3\"\n";

/// Enrol a peer through the first node, unless it already has an identity.
async fn enrol(setup: &NodeSetup<'_>, name: &str, step: &Step) -> Result<()> {
    let lima = setup.lima;
    let enrolled = lima
        .command(&[
            "shell",
            name,
            "sudo",
            "test",
            "-f",
            "/etc/reliaburger/identity/bundle.committed",
        ])
        .await
        .is_ok();
    if enrolled {
        step.note("already enrolled");
        return Ok(());
    }
    let token = setup.client.join_token_create(name, 300).await?;
    // One file per node, because peers enrol concurrently.
    let token_path = setup.directory.join(format!("{name}.join-token"));
    let result = async {
        let path = token_path.clone();
        tokio::task::spawn_blocking(move || {
            crate::sesame::identity::atomic_write_mode(&path, token.as_bytes(), Some(0o600))
        })
        .await??;
        let input = tokio::fs::File::open(&token_path).await?.into_std().await;
        lima.command_with_input(
            &[
                "shell",
                name,
                "sudo",
                "sh",
                "-c",
                JOIN_SCRIPT,
                "sh",
                name,
                &setup.bootstrap.root_fingerprint,
                &format!("https://{}:9117", setup.first_address),
            ],
            input,
        )
        .await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let _ = tokio::fs::remove_file(&token_path).await;
    result
}

/// Create, restart or adopt one owned VM and return its shared address.
async fn boot_vm(
    lima: Lima,
    node: NodeState,
    index: usize,
    status: Option<String>,
    config_path: PathBuf,
    ports: Vec<u16>,
    step: Step,
) -> Result<(usize, std::net::Ipv4Addr)> {
    let result = async {
        start_vm(&lima, &node, status.as_deref(), &config_path, &ports, &step).await?;
        lima.wait_for_guest(&node.name).await?;
        Ok((index, lima.address(&node.name).await?))
    }
    .await;
    step.record(result)
}

async fn start_vm(
    lima: &Lima,
    node: &NodeState,
    status: Option<&str>,
    config_path: &Path,
    ports: &[u16],
    step: &Step,
) -> Result<()> {
    if status != Some("Running") {
        super::preflight::ports(ports).await?;
    }
    let name = node.name.as_str();
    match status {
        None if node.phase != NodePhase::Planned => bail!(
            "owned VM {name} disappeared; refusing to create a replacement cluster implicitly"
        ),
        None => {
            let config = config_path.to_str().context("invalid VM config path")?;
            let create = ["start", "--tty=false", &format!("--name={name}"), config];
            start_watched(lima, name, &create, BOOT_SILENCE, step).await?;
        }
        Some("Running") if !lima.console_started(name).await => {
            // A previous run left a VM that never booted; see BOOT_SILENCE.
            step.note("restarted a VM that never booted");
            lima.command(&["stop", "--force", name]).await?;
            lima.command(&["start", "--tty=false", name]).await?;
        }
        Some("Running") => step.note("already running"),
        Some("Stopped") => {
            step.note("restarted");
            let start = ["start", "--tty=false", name];
            start_watched(lima, name, &start, BOOT_SILENCE, step).await?;
        }
        Some(status) => bail!("VM {name} is in unexpected state {status}",),
    }
    Ok(())
}

/// How long a starting VM may print nothing at all on its console.
///
/// Measuring showed Apple's Virtualization.framework occasionally starts a VM
/// that never runs its firmware: Lima reports it running, the console stays
/// empty and SSH never answers, even with plain `limactl start` and no
/// Reliaburger involved. A healthy guest prints its login prompt within
/// seconds of starting, and disk preparation before that takes about ten.
const BOOT_SILENCE: Duration = Duration::from_secs(60);

/// Run a Lima start command, and if the VM's console is still silent after
/// `silence`, force it off and start it once more without the watchdog.
async fn start_watched(
    lima: &Lima,
    name: &str,
    args: &[&str],
    silence: Duration,
    step: &Step,
) -> Result<()> {
    let start = lima.command(args);
    let watchdog = async {
        tokio::time::sleep(silence).await;
        if lima.console_started(name).await {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        result = start => return result.map(drop),
        () = watchdog => {}
    }
    // Dropping the start command above killed limactl; the VM itself is
    // still registered with Lima, so stop it by name and start it again.
    step.note("restarted a VM that never booted");
    lima.command(&["stop", "--force", name]).await?;
    lima.command(&["start", "--tty=false", name]).await?;
    Ok(())
}

/// Wait until Lima's shared network runs, or the first boot finishes.
async fn wait_for_shared_network<F>(
    lima: &Lima,
    boots: &mut FuturesUnordered<F>,
    operation: &mut Operation,
) -> Result<()>
where
    F: std::future::Future<Output = Result<(usize, std::net::Ipv4Addr)>>,
{
    let network = async {
        while !lima.shared_network_running().await {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };
    tokio::select! {
        Some(result) = boots.next() => record_boot(operation, result?).await,
        () = network => Ok(()),
    }
}

async fn record_boot(
    operation: &mut Operation,
    (index, address): (usize, std::net::Ipv4Addr),
) -> Result<()> {
    let node = &mut operation.state.nodes[index];
    if node.address.is_some_and(|previous| previous != address) && node.phase == NodePhase::Started
    {
        bail!(
            "VM {} changed its shared address; refusing to resume with stale peer configuration",
            node.name
        );
    }
    node.address = Some(address);
    if node.phase == NodePhase::Planned {
        node.phase = NodePhase::Created;
    }
    operation.save_async().await?;
    Ok(())
}

fn quorum_ready(names: &[String], council: &CouncilStatus) -> bool {
    council
        .leader
        .as_ref()
        .is_some_and(|leader| names.contains(leader))
        && council.members.len() == names.len()
        && names
            .iter()
            .all(|name| council.members.iter().any(|member| &member.name == name))
}

pub(super) async fn wait_for_quorum(client: &BunClient, names: &[String]) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            if let (Ok(council), Ok(nodes)) = (client.council().await, client.nodes().await)
                && quorum_ready(names, &council)
                && names.iter().all(|name| {
                    nodes
                        .iter()
                        .any(|node| &node.node_id == name && node.state == "alive")
                })
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .context("owned nodes did not form a healthy council quorum")?;
    Ok(())
}

fn demo_config() -> String {
    format!(
        r#"[app.hello]
image = "{}"
command = ["/bin/sh", "-c", "echo hello-started; mkdir -p /tmp/www; printf 'Reliaburger is running\\n' > /tmp/www/index.html; exec httpd -f -p 8080 -h /tmp/www"]
port = 8080
replicas = 1
[app.hello.health]
path = "/"
interval = 1
threshold_healthy = 1
[app.hello.ingress]
host = "localhost"
[app.hello.env]
PATH = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
"#,
        crate::testkit::PINNED_TEST_WORKLOAD_IMAGE.replacen(
            "docker.io/library/",
            "public.ecr.aws/docker/library/",
            1
        )
    )
}

async fn probe_demo(port: u16) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?;
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            if let Ok(response) = client
                .get(format!("http://127.0.0.1:{port}/"))
                .header("Host", "localhost")
                .send()
                .await
                && response.status().is_success()
                && response
                    .text()
                    .await
                    .is_ok_and(|body| body.trim() == "Reliaburger is running")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .context("hello container did not answer through the host ingress port")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::agent::{CouncilMemberInfo, CouncilStatus};

    #[test]
    fn readiness_requires_every_owned_voter_and_an_owned_leader() {
        let names = vec!["one".into(), "two".into(), "three".into()];
        let mut council = CouncilStatus {
            leader: Some("one".into()),
            ..Default::default()
        };
        assert!(!quorum_ready(&names, &council));
        council.members = names
            .iter()
            .enumerate()
            .map(|(index, name)| CouncilMemberInfo {
                raft_id: index as u64,
                name: name.clone(),
                address: "192.168.104.1:9444".into(),
            })
            .collect();
        assert!(quorum_ready(&names, &council));
        council.leader = Some("foreign".into());
        assert!(!quorum_ready(&names, &council));
    }

    #[test]
    fn only_the_first_node_receives_the_bootstrap_identity_with_its_marker_last() {
        let sources = NodeSources {
            bun: Path::new("/host/bun"),
            relish: Path::new("/host/relish"),
            service: Path::new("/host/reliaburger.service"),
            security: Path::new("/host/security"),
        };
        let first = node_files(&sources, 0, "/host/one.toml".into());
        assert_eq!(
            first.last().unwrap().destination,
            "/etc/reliaburger/identity/bundle.committed"
        );
        assert!(
            first
                .iter()
                .any(|file| file.destination.ends_with("security-bootstrap.json"))
        );
        let peer = node_files(&sources, 1, "/host/two.toml".into());
        assert!(
            peer.iter()
                .all(|file| !file.destination.contains("identity")
                    && !file.destination.contains("security-bootstrap"))
        );
        for files in [&first, &peer] {
            for file in files.iter() {
                let expected = match file.destination.as_str() {
                    "/usr/local/bin/bun" | "/usr/local/bin/relish" => 0o755,
                    "/etc/systemd/system/reliaburger.service" => 0o644,
                    _ => 0o600,
                };
                assert_eq!(file.mode, expected, "{}", file.destination);
            }
        }
    }

    /// A stand-in `limactl` that logs its arguments; a create never returns,
    /// and writes a console line first only when `boots` is true.
    fn fake_lima(home: &Path, boots: bool) -> Lima {
        let script = home.join("limactl");
        let console = if boots {
            "mkdir -p \"$LIMA_HOME/vm\"; echo login > \"$LIMA_HOME/vm/serialv.log\"; exit 0"
        } else {
            "sleep 30"
        };
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$*\" >> \"$LIMA_HOME/calls\"\n\
                 case \"$3\" in --name=*) {console};; esac\n"
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        Lima::new(script, Duration::from_secs(5)).with_home(home.to_owned())
    }

    #[tokio::test]
    async fn a_vm_that_never_prints_to_its_console_is_restarted_once() {
        let home = tempfile::tempdir().unwrap();
        let lima = fake_lima(home.path(), false);
        let step = super::super::progress::tests_support::detached_step();
        let create = ["start", "--tty=false", "--name=vm", "vm.yaml"];
        start_watched(&lima, "vm", &create, Duration::from_millis(200), &step)
            .await
            .unwrap();
        let calls = std::fs::read_to_string(home.path().join("calls")).unwrap();
        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            [
                "start --tty=false --name=vm vm.yaml",
                "stop --force vm",
                "start --tty=false vm"
            ]
        );
    }

    #[tokio::test]
    async fn a_vm_that_boots_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let lima = fake_lima(home.path(), true);
        let step = super::super::progress::tests_support::detached_step();
        let create = ["start", "--tty=false", "--name=vm", "vm.yaml"];
        start_watched(&lima, "vm", &create, Duration::from_millis(200), &step)
            .await
            .unwrap();
        assert!(lima.console_started("vm").await);
        let calls = std::fs::read_to_string(home.path().join("calls")).unwrap();
        assert_eq!(calls.lines().count(), 1, "{calls}");
    }

    #[tokio::test]
    async fn timings_file_records_the_run_and_every_step() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("timings.json");
        let timings = Timings {
            total_seconds: 12.5,
            steps: Vec::new(),
        };
        let report = TimingsReport {
            schema: 1,
            version: "0.1.0",
            os: "macos",
            arch: "aarch64",
            nodes: 3,
            development_binaries: true,
            succeeded: false,
            timings: &timings,
        };
        save_timings(&path, &report).await.unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["schema"], 1);
        assert_eq!(saved["succeeded"], false);
        assert_eq!(saved["total_seconds"], 12.5);
        assert!(saved["steps"].as_array().unwrap().is_empty());
    }

    #[test]
    fn demo_is_a_pinned_container_with_health_and_browser_ingress() {
        let config = crate::config::Config::parse(&demo_config()).unwrap();
        let app = &config.app["hello"];
        assert!(
            app.image
                .as_ref()
                .unwrap()
                .starts_with("public.ecr.aws/docker/library/busybox@sha256:")
        );
        assert_eq!(app.command[0], "/bin/sh");
        assert!(app.env.contains_key("PATH"));
        assert_eq!(app.ingress.as_ref().unwrap().host, "localhost");
        assert_eq!(app.health.as_ref().unwrap().path, "/");
    }
}
