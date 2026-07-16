//! Bun — the Reliaburger node agent.
//!
//! Runs on every node in the cluster. Manages container lifecycle,
//! health checks, and reports state to the cluster leader.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use reliaburger::bun::agent::BunAgent;
use reliaburger::bun::api;
use reliaburger::config::node::NodeConfig;
use reliaburger::grill::port::PortAllocator;
use reliaburger::grill::{AnyGrill, ProcessGrill, detect_runtime};
use reliaburger::ketchup::log_store::LogStore;
use reliaburger::mayo::alert::AlertEvaluator;
use reliaburger::mayo::collector::SystemCollector;
use reliaburger::mayo::store::MayoStore;
use reliaburger::mayo::webhook::{WebhookDispatcher, gather_latest_values};
use reliaburger::pickle::api::PickleState;
use reliaburger::pickle::store::BlobStore;
use reliaburger::pickle::types::ManifestCatalog;

#[derive(Parser)]
#[command(name = "bun", version, about = "Reliaburger node agent")]
struct Cli {
    /// Path to node configuration file.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Listen address for the local API.
    #[arg(long, default_value = "127.0.0.1:9117")]
    listen: String,

    /// Runtime to use: auto, process, runc, apple.
    #[arg(long, default_value = "auto")]
    runtime: String,

    /// Join/form a cluster using the `[cluster]` config (gossip membership).
    /// Without this flag, bun runs as a single node, as before.
    #[arg(long)]
    cluster: bool,
}

/// Build cluster startup parameters from node config.
///
/// Gossip binds/advertises on `advertise_address` (falling back to
/// loopback for single-host testing) at `cluster.gossip_port`; seeds are the
/// parseable `cluster.join` addresses. An empty seed list means this is the
/// first/bootstrap node.
fn cluster_params_from_config(
    config: &NodeConfig,
) -> anyhow::Result<reliaburger::cluster::runtime::ClusterParams> {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use anyhow::Context;
    use reliaburger::sesame::bootstrap;

    let ip = config
        .network
        .advertise_address
        .as_deref()
        .and_then(|s| {
            s.parse::<IpAddr>()
                .ok()
                .or_else(|| s.parse::<SocketAddr>().ok().map(|sa| sa.ip()))
        })
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let gossip_addr = SocketAddr::new(ip, config.cluster.gossip_port);

    let seeds: Vec<SocketAddr> = config
        .cluster
        .join
        .iter()
        .filter_map(|s| s.parse::<SocketAddr>().ok())
        .collect();

    let node_name = config
        .node
        .name
        .clone()
        .unwrap_or_else(|| format!("node-{}", config.cluster.gossip_port));

    // Load security material if the config points at it. A node told to load
    // secrets that are missing, malformed, or world-readable fails loudly here
    // rather than silently booting without CA material.
    let wrapping_ikm = config
        .security
        .master_key_path
        .as_deref()
        .map(|path| {
            bootstrap::load_master_key(path)
                .with_context(|| format!("failed to load master key from {}", path.display()))
        })
        .transpose()?;
    let bootstrap_security_state = config
        .security
        .bootstrap_path
        .as_deref()
        .map(|path| {
            bootstrap::load_bootstrap_state(path)
                .map(Box::new)
                .with_context(|| {
                    format!("failed to load security bootstrap from {}", path.display())
                })
        })
        .transpose()?;

    // Only feed the identity into the runtime when mTLS is actually
    // requested. Otherwise a node that merely has an identity on disk would
    // silently run mTLS transports while the mode-matrix warning says it is
    // plaintext. The identity is still loaded separately for that warning.
    let identity = if config.security.require_mtls {
        load_node_identity(config)?
    } else {
        None
    };

    Ok(reliaburger::cluster::runtime::ClusterParams {
        node_name,
        gossip_addr,
        raft_port: config.cluster.raft_port,
        reporting_port: config.cluster.reporting_port,
        // The CLI can override the API listen port; main() re-sets this
        // after parsing `--listen`, before the runtime starts.
        api_port: 9117,
        reporting_config: config.reporting_tree.clone(),
        seeds,
        wrapping_ikm,
        bootstrap_security_state,
        data_dir: config.storage.data.clone(),
        // The MayoStore doesn't exist yet when params are built; the
        // caller sets it before starting the runtime.
        mayo: None,
        rollup_interval: std::time::Duration::from_secs(config.metrics.rollup_interval_secs),
        identity,
        backup: config.cluster.backup.clone(),
        labels: config.node.labels.clone(),
        // Wired in by the caller (run) once the disk-pressure channel exists.
        self_disk_pressured_rx: None,
    })
}

/// The directory this node reads/writes its identity from: the configured
/// `[security] identity_dir`, or `{storage.data}/identity` by default.
fn node_identity_dir(config: &NodeConfig) -> std::path::PathBuf {
    config
        .security
        .identity_dir
        .clone()
        .unwrap_or_else(|| reliaburger::sesame::identity_store::identity_dir(&config.storage.data))
}

/// Load this node's mTLS identity from disk, if one has been installed.
fn load_node_identity(
    config: &NodeConfig,
) -> anyhow::Result<Option<std::sync::Arc<reliaburger::sesame::identity_store::NodeIdentity>>> {
    use anyhow::Context;
    let dir = node_identity_dir(config);
    let identity = reliaburger::sesame::identity_store::load(&dir)
        .with_context(|| format!("failed to load node identity from {}", dir.display()))?;
    Ok(identity.map(std::sync::Arc::new))
}

/// Serve an axum router over TLS, handshaking each connection in its own task
/// (a slow handshaker never blocks the accept loop). Mirrors the wrapper's
/// ingress TLS loop; runs until `shutdown` is cancelled.
async fn serve_api_over_tls(
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    router: axum::Router,
    shutdown: tokio_util::sync::CancellationToken,
) {
    use tower::Service;
    let mut make_service = router.into_make_service();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            accepted = listener.accept() => {
                let Ok((tcp, _peer)) = accepted else { continue };
                let acceptor = acceptor.clone();
                let service = match make_service.call(()).await {
                    Ok(service) => service,
                    Err(infallible) => match infallible {},
                };
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else { return };
                    let hyper_service = hyper_util::service::TowerToHyperService::new(service);
                    let _ = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    )
                    .serve_connection_with_upgrades(
                        hyper_util::rt::TokioIo::new(tls),
                        hyper_service,
                    )
                    .await;
                });
            }
        }
    }
}

/// Enforce the `require_mtls` mode matrix before the cluster starts.
///
/// With `require_mtls` set and no identity on disk, the node cannot speak the
/// internal transports, so it refuses to start. The message differs by role:
/// a bootstrap node (no seeds) needs `relish init`; a joiner (seeds set) needs
/// `relish join` and a restart.
fn enforce_mtls_mode(
    config: &NodeConfig,
    params: &reliaburger::cluster::runtime::ClusterParams,
) -> anyhow::Result<()> {
    if !config.security.require_mtls || params.identity.is_some() {
        return Ok(());
    }
    let dir = node_identity_dir(config).display().to_string();
    if params.seeds.is_empty() {
        anyhow::bail!(
            "[security] require_mtls is set but no identity was found at {dir}: \
             run `relish init` on the bootstrap node first"
        );
    }
    anyhow::bail!(
        "[security] require_mtls is set but this node has no identity yet at {dir}: \
         run `relish join --token <token> --node-id {} <member-api-url>` to enrol, then restart bun",
        params.node_name
    );
}

/// Schedulable node capacity: system totals minus the `[resources]`
/// reservation. Read once at startup.
fn node_capacity(config: &NodeConfig) -> (u32, u32) {
    use reliaburger::config::types::parse_resource_value;

    let system = sysinfo::System::new_all();
    let total_cpu_millicores = (system.cpus().len() as u64) * 1000;
    let total_memory_mb = system.total_memory() / (1024 * 1024);

    let reserved_cpu = parse_resource_value(&config.resources.reserved_cpu).unwrap_or(0);
    let reserved_memory_mb =
        parse_resource_value(&config.resources.reserved_memory).unwrap_or(0) / (1024 * 1024);

    (
        total_cpu_millicores.saturating_sub(reserved_cpu) as u32,
        total_memory_mb.saturating_sub(reserved_memory_mb) as u32,
    )
}

/// Overwrite the auth token store with the current API tokens from Raft.
///
/// Called on startup and every few seconds after, so a token created via
/// `relish token create` starts being enforced without restarting the agent.
async fn refresh_token_store(
    store: &reliaburger::sesame::auth::TokenStore,
    council: &reliaburger::council::CouncilNode,
) {
    let tokens = council.security_state().await.api_tokens;
    *store.write().await = tokens;
}

/// Refuse to bind a token-less (wide-open) API to a routable address (AUTH3).
///
/// Returns an error when `listen` resolves to a non-loopback IP, so the caller
/// aborts startup. A loopback address, an unparseable one (a hostname we leave
/// to the resolver), or a `0.0.0.0`/`::`-free routable IP all take the safe
/// path: only a clearly non-loopback bind is refused. The message names the
/// two recovery routes so the operator isn't left guessing.
fn refuse_open_non_loopback_bind(listen: &str) -> anyhow::Result<()> {
    let Ok(address) = listen.parse::<std::net::SocketAddr>() else {
        // A hostname, not a literal address: we can't tell if it's loopback
        // here, so don't block. The operator opted into a name deliberately.
        return Ok(());
    };
    if address.ip().is_loopback() {
        return Ok(());
    }
    anyhow::bail!(
        "refusing to bind the API to non-loopback address {address} while no user tokens exist: \
         the API would be open to anyone who can reach it. bind a loopback address \
         (e.g. 127.0.0.1:9117) and run `relish token create` to mint the first admin token, \
         then restart with your intended --listen address"
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Resolve the running version from the real executable path (not argv[0]):
    // in debug builds a `.version` sidecar next to the binary can override it,
    // which is how self-upgrade integration tests fake old/new versions.
    let exe_path = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("failed to resolve current executable path: {e}"))?;
    let running_version = reliaburger::upgrade::resolve_running_version(&exe_path);
    println!("bun: reliaburger node agent {running_version}");

    // Load node config
    let config = if let Some(ref path) = cli.config {
        NodeConfig::from_file(path).map_err(|e| anyhow::anyhow!("failed to load config: {e}"))?
    } else {
        NodeConfig::default()
    };

    // Validate the reconstruction thresholds and backup settings before we
    // build anything on top of them (12b.2 D21/CP12): a nonsensical coverage
    // or a zero backup interval must fail loudly at startup, not silently
    // misbehave later.
    config
        .reconstruction
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid config: {e}"))?;
    config
        .cluster
        .backup
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid config: {e}"))?;
    // A zero alert interval would panic `tokio::time::interval` at startup
    // (OBS4); reject it here with a clear message.
    config
        .alerts
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid config: {e}"))?;

    // Create port allocator from config
    let port_allocator = PortAllocator::new(
        config.network.port_range.start,
        config.network.port_range.end,
    );

    // Containers can only be pointed at the DNS responder when it
    // listens on port 53 (resolv.conf has no port syntax) on an IPv4
    // address; otherwise host DNS applies and we say so.
    let container_nameserver = if config.dns.enabled {
        match config.dns.to_dns_config()? {
            c if c.listen_addr.port() == 53 => match c.listen_addr.ip() {
                std::net::IpAddr::V4(ip) => Some(ip),
                std::net::IpAddr::V6(_) => None,
            },
            c => {
                println!(
                    "bun: dns listen {} is not port 53 — containers keep host DNS",
                    c.listen_addr
                );
                None
            }
        }
    } else {
        None
    };

    // Writable base for node-local runtime state (instance records,
    // upgrade markers). Prefers the configured data dir, falls back to the
    // user data dir like the metrics/logs stores below.
    let data_base = if std::fs::create_dir_all(&config.storage.data).is_ok() {
        config.storage.data.clone()
    } else {
        let fallback = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp/reliaburger"))
            .join("reliaburger");
        std::fs::create_dir_all(&fallback)
            .map_err(|e| anyhow::anyhow!("failed to create data directory: {e}"))?;
        fallback
    };

    // Instance records + process log files ({data}/instances). Started
    // workloads are recorded here so a future bun process (crash restart or
    // self-upgrade exec) adopts them instead of restarting them.
    let instances_dir = data_base.join("instances");
    std::fs::create_dir_all(&instances_dir)
        .map_err(|e| anyhow::anyhow!("failed to create instances directory: {e}"))?;

    // Self-upgrade: build the manager and run startup recovery BEFORE any
    // subsystem starts. A crash-looping new version reverts here; a freshly
    // swapped-in version gets a verification marker to prove itself against.
    let original_argv: Vec<String> = std::env::args().collect();
    let upgrade_manager = match reliaburger::upgrade::manager::UpgradeManager::new(
        &config.upgrades,
        &data_base,
        &exe_path,
        running_version.clone(),
        original_argv,
    ) {
        Ok(manager) => Some(manager),
        Err(e) => {
            eprintln!("bun: warning: self-upgrade unavailable: {e}");
            None
        }
    };
    let mut upgrade_verify = None;
    if let Some(manager) = &upgrade_manager {
        use reliaburger::upgrade::manager::StartupAction;
        match manager.startup_action() {
            Ok(StartupAction::Continue { verify }) => upgrade_verify = verify,
            Ok(StartupAction::ExecPrevious) => {
                // Only returns on error; on success the process is replaced.
                let error = manager.exec_current_symlink();
                anyhow::bail!("failed to exec previous version during revert: {error}");
            }
            Err(e) => eprintln!("bun: warning: upgrade startup recovery failed: {e}"),
        }
    }

    // Debug-only test hook: a `{exe}.fail-boot` sidecar next to the resolved
    // binary makes this process exit now, simulating a broken release. It
    // runs AFTER startup recovery so each failed boot burns an attempt and
    // the crash-loop revert machinery gets exercised for real. Release
    // builds never contain this branch.
    if cfg!(debug_assertions)
        && let Ok(resolved) = std::fs::canonicalize(&exe_path)
        && {
            let mut name = resolved
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_default();
            name.push(".fail-boot");
            resolved.with_file_name(name).exists()
        }
    {
        eprintln!("bun: fail-boot sidecar present; exiting (test hook)");
        std::process::exit(101);
    }

    // Select runtime
    let runtime = select_runtime(&cli.runtime, container_nameserver, &instances_dir).await?;
    // Image-store handle for installing the cluster P2P image source
    // once the registry and catalog exist — the runtime is selected
    // long before them, so the source is injected late via a OnceLock
    // slot shared by ImageStore clones.
    let cluster_image_store = runtime.image_store();

    // Create command channel
    let (cmd_tx, cmd_rx) = mpsc::channel(256);

    // Create shutdown token
    let shutdown = CancellationToken::new();

    // Mayo store first: the cluster runtime's rollup worker reads it, so
    // it must exist before the runtime starts.
    let metrics_dir = if std::fs::create_dir_all(&config.storage.metrics).is_ok() {
        config.storage.metrics.clone()
    } else {
        let fallback = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp/reliaburger"))
            .join("reliaburger")
            .join("metrics");
        std::fs::create_dir_all(&fallback).expect("failed to create metrics directory");
        fallback
    };
    let mayo_store = Arc::new(RwLock::new(MayoStore::new(metrics_dir)));

    // Create the agent (extract deploy history handle before spawning).
    // In cluster mode, start the cluster runtime (gossip, …) and build the
    // agent with a real ClusterHandle. `_cluster_runtime` holds resources
    // that must outlive the agent; it drops at the end of main.
    let agent_shutdown = shutdown.clone();
    // Channel carrying container log lines from the agent's per-instance
    // forwarders into the LogStore (drained below, once the store exists).
    let (log_tx, mut log_rx) =
        tokio::sync::mpsc::channel::<reliaburger::ketchup::types::LogRecord>(1024);
    let node_name = config
        .node
        .name
        .clone()
        .unwrap_or_else(|| format!("node-{}", config.cluster.gossip_port));
    let api_port: u16 = cli
        .listen
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(9117);

    // Disk-pressure resignation signal (12b.2 T3): the disk-pressure loop below
    // publishes this node's own sustained-pressure verdict here; in cluster mode
    // the runtime advertises it over gossip so the leader resigns and replaces a
    // pressured voter. Created before the cluster block so the receiver can ride
    // into ClusterParams.
    let (disk_pressured_tx, disk_pressured_rx) = tokio::sync::watch::channel(false);

    let _cluster_runtime;
    // Cloned out of the ClusterHandle before it's moved into the agent, so the
    // API router can expose council-backed endpoints (JWKS, tokens, secrets).
    let mut api_council: Option<Arc<reliaburger::council::CouncilNode>> = None;
    // Shared CRL for the internal mTLS verifiers, refreshed from Raft state.
    let mut crl_refresh: Option<reliaburger::sesame::mtls::CrlHandle> = None;
    // This node's mTLS identity, when the cluster runs mTLS. Drives the API
    // listener TLS and the cluster HTTP client (peer calls over https).
    let mut api_identity: Option<
        std::sync::Arc<reliaburger::sesame::identity_store::NodeIdentity>,
    > = None;
    // The leader-side rollup store, exposed at /v1/metrics/cluster.
    let mut api_rollup_store = None;
    // Gossip membership for the pickle replication loop (cluster only).
    let mut replication_membership = None;
    // Peer API addresses for cross-node fan-out and apply forwarding.
    let mut api_membership: Option<Arc<RwLock<Vec<api::NodeMembershipInfo>>>> = None;
    // Handles the orchestration tasks need, captured before the
    // ClusterHandle moves into the agent (spawned further down, once
    // the service token exists).
    let mut orchestration = None;
    let mut agent = if cli.cluster {
        let mut params = cluster_params_from_config(&config)?;
        params.mayo = Some(Arc::clone(&mayo_store));
        params.self_disk_pressured_rx = Some(disk_pressured_rx.clone());
        // Advertised via the gossip directory (12b.2): peers reach this
        // node's API at the port it actually listens on, not a derived one.
        params.api_port = api_port;

        // Mode matrix: with require_mtls set, a node must have an identity on
        // disk before it can speak the internal transports. Refuse to start
        // otherwise, with the command that fixes it.
        enforce_mtls_mode(&config, &params)?;
        api_identity = params.identity.clone();
        if params.identity.is_some() {
            println!("bun: mTLS enabled on the Raft RPC, reporting and API transports");
        } else if config.security.require_mtls {
            unreachable!("enforce_mtls_mode rejects require_mtls without an identity");
        } else if reliaburger::sesame::identity_store::load(&node_identity_dir(&config))
            .ok()
            .flatten()
            .is_some()
        {
            eprintln!(
                "bun: warning — a node identity is present but [security] require_mtls is false; \
                 internal transports stay plaintext. Set require_mtls = true to enable mTLS."
            );
        }

        println!(
            "bun: cluster mode — gossip on {}, {} seed(s)",
            params.gossip_addr,
            params.seeds.len()
        );
        let (handle, cluster_runtime) =
            reliaburger::cluster::runtime::start(params, agent_shutdown.clone())
                .await
                .map_err(|e| anyhow::anyhow!("failed to start cluster runtime: {e}"))?;
        api_rollup_store = Some(Arc::clone(&cluster_runtime.rollup_store));
        api_council = handle.council.clone();
        crl_refresh = Some(handle.crl_handle.clone());
        // Cloned before the handle moves into the agent: the pickle
        // replication loop derives its peer list from gossip.
        replication_membership = Some(handle.membership_rx.clone());
        orchestration = Some((
            handle.membership_rx.clone(),
            handle.raft_metrics_rx.clone(),
            cluster_runtime.aggregated_rx.clone(),
            cluster_runtime.directory_rx.clone(),
        ));
        _cluster_runtime = Some(cluster_runtime);
        BunAgent::with_cluster(runtime, port_allocator, cmd_rx, agent_shutdown, handle)
    } else {
        _cluster_runtime = None;
        BunAgent::new(runtime, port_allocator, cmd_rx, agent_shutdown)
    };
    let event_store = Arc::new(RwLock::new(reliaburger::bun::events::EventStore::new()));
    agent.set_event_store(Arc::clone(&event_store));
    // How this node reaches peer agent APIs: https + CA trust under mTLS,
    // plain http otherwise. Shared by the API fan-out, batch/build dispatch,
    // placement reconciler and upgrade orchestrator.
    let cluster_http = match &api_identity {
        Some(identity) => reliaburger::cluster::ClusterHttp::secure(
            reliaburger::sesame::mtls::build_cluster_http_client(identity)
                .map_err(|e| anyhow::anyhow!("failed to build cluster HTTP client: {e}"))?,
        ),
        None => reliaburger::cluster::ClusterHttp::plaintext(),
    };

    // Batch scheduling (F1) reads capacities from the same aggregated
    // view the deploy scheduler uses; None standalone.
    let api_aggregated_rx = orchestration.as_ref().map(|(_, _, rx, _)| rx.clone());

    // Report real schedulable capacity to the cluster (L6: StateReports
    // used to carry zeroes).
    let (capacity_cpu, capacity_memory) = node_capacity(&config);
    agent.set_node_capacity(capacity_cpu, capacity_memory);
    // Wire [storage] volumes — the agent constructors default it, which
    // left the config key dead (review M21's second half).
    agent.set_volumes_dir(config.storage.volumes.clone());

    // Scheduled volume snapshots ([storage.snapshots], Phase 12 E3).
    if config.storage.snapshots.interval_secs > 0 {
        tokio::spawn(reliaburger::bun::snapshot_worker::run_snapshot_loop(
            config.storage.volumes.clone(),
            config.storage.snapshots.clone(),
            shutdown.clone(),
        ));
    }

    // L8: load and attach the eBPF data path (Onion connect rewrite,
    // Smoker network faults, Sesame egress). Linux + `ebpf` feature only.
    // A load failure is logged and the node continues without kernel
    // enforcement rather than refusing to start.
    agent.set_ebpf_sweep_interval(config.ebpf.sweep_interval_secs);
    if config.ebpf.enabled {
        match config.ebpf.resolve_program_dir() {
            Some(program_dir) => {
                #[cfg(feature = "ebpf")]
                match reliaburger::onion::ebpf::loader::OnionEbpf::load(
                    &program_dir,
                    &config.ebpf.cgroup_path,
                ) {
                    Ok(ebpf) => {
                        eprintln!(
                            "bun: eBPF data path loaded from {} (attached={})",
                            program_dir.display(),
                            ebpf.is_attached()
                        );
                        agent.set_onion_ebpf(Arc::new(tokio::sync::Mutex::new(ebpf)));
                    }
                    Err(e) => {
                        eprintln!(
                            "bun: failed to load eBPF data path: {e}; continuing without enforcement"
                        );
                    }
                }
                #[cfg(not(feature = "ebpf"))]
                {
                    let _ = program_dir;
                    eprintln!(
                        "bun: [ebpf] enabled but this binary was built without the `ebpf` feature; \
                         network faults and egress allowlists are NOT enforced"
                    );
                }
            }
            None => {
                eprintln!(
                    "bun: [ebpf] enabled but no program_dir set and no build-time objects; skipping"
                );
            }
        }
    }

    // Derive the internal service token from the shared master key, so bun's own
    // cross-node fan-out calls authenticate as the system principal on peers.
    let service_token = api_council
        .as_ref()
        .and_then(|c| c.wrapping_ikm())
        .map(reliaburger::sesame::token::derive_service_token)
        .transpose()?;

    // L1 orchestration: the leader schedules desired apps into
    // placements, every node keeps a fresh peer-API table, and every
    // node reconciles its instances against its assignments.
    if let Some((membership_rx, metrics_rx, aggregated_rx, directory_rx)) = orchestration {
        if let Some(council) = &api_council {
            reliaburger::cluster::orchestrate::spawn_leader_scheduler(
                Arc::clone(council),
                membership_rx.clone(),
                aggregated_rx,
                config.reconstruction.clone(),
                shutdown.clone(),
            );
            // L3: leader-only autoscale loop, feeding on the same rollup
            // store /v1/metrics/cluster serves.
            if let Some(rollup_store) = &api_rollup_store {
                reliaburger::cluster::orchestrate::spawn_autoscaler(
                    Arc::clone(council),
                    Arc::clone(rollup_store),
                    std::time::Duration::from_secs(30),
                    shutdown.clone(),
                );
            }
        }

        // Peer API ports are derived from gossip ports by fixed offset
        // (uniform ports in production; distinct blocks on single-host
        // clusters).
        let gossip_to_api_offset = api_port as i32 - config.cluster.gossip_port as i32;
        let membership_table: Arc<RwLock<Vec<api::NodeMembershipInfo>>> =
            Arc::new(RwLock::new(Vec::new()));
        api_membership = Some(Arc::clone(&membership_table));
        let mut refresher_rx = membership_rx;
        let refresher_shutdown = shutdown.clone();
        tokio::spawn(async move {
            loop {
                let snapshot: Vec<api::NodeMembershipInfo> = refresher_rx
                    .borrow()
                    .iter()
                    .filter(|m| m.state == reliaburger::mustard::state::NodeState::Alive)
                    .map(|m| api::NodeMembershipInfo {
                        node_id: m.node_id.clone(),
                        address: std::net::SocketAddr::new(
                            m.address.ip(),
                            (m.address.port() as i32 + gossip_to_api_offset) as u16,
                        ),
                    })
                    .collect();
                *membership_table.write().await = snapshot;
                tokio::select! {
                    _ = refresher_shutdown.cancelled() => break,
                    changed = refresher_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        if let Some(metrics_rx) = metrics_rx {
            reliaburger::cluster::orchestrate::spawn_placement_reconciler(
                node_name.clone(),
                metrics_rx,
                directory_rx,
                api_port as i32 - config.cluster.raft_port as i32,
                service_token.clone(),
                cmd_tx.clone(),
                shutdown.clone(),
                cluster_http.clone(),
                Some(data_base.clone()),
            );
        }
    }

    // Rolling-upgrade orchestrator: dormant unless this node is the Raft
    // leader with an active upgrade in DesiredState (Phase 14).
    if let Some(council) = api_council.clone() {
        let control = reliaburger::upgrade::orchestrator::HttpNodeControl::with_http(
            service_token.clone(),
            cluster_http.clone(),
        );
        let orchestrator_cancel = shutdown.clone();
        let orchestrator_node = node_name.clone();
        tokio::spawn(async move {
            reliaburger::upgrade::orchestrator::run_orchestrator(
                council,
                control,
                orchestrator_node,
                orchestrator_cancel,
            )
            .await;
        });
    }

    // Seed the auth token store from the council's SecurityState and keep it
    // refreshed. The middleware reads this store; without the refresh, a token
    // created after startup would never engage enforcement (the ≤5 s lag is
    // deliberate).
    let api_token_store = if let Some(council) = &api_council {
        let store = reliaburger::sesame::auth::new_token_store();
        refresh_token_store(&store, council).await;
        if let Some(crl) = &crl_refresh {
            crl.update(council.security_state().await.crl);
        }

        let refresh_store = Arc::clone(&store);
        let refresh_council = Arc::clone(council);
        let refresh_shutdown = shutdown.clone();
        let refresh_crl = crl_refresh.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tokio::select! {
                    _ = refresh_shutdown.cancelled() => break,
                    _ = ticker.tick() => {
                        refresh_token_store(&refresh_store, &refresh_council).await;
                        // Same tick refreshes the CRL so a revoked peer is
                        // refused on its next handshake (≤5 s lag).
                        if let Some(crl) = &refresh_crl {
                            crl.update(refresh_council.security_state().await.crl);
                        }
                    }
                }
            }
        });
        Some(store)
    } else {
        None
    };
    agent.set_log_sink(log_tx);
    agent.set_trust_policy(config.images.trust_policy.clone());
    agent.set_records_dir(instances_dir.clone());
    if let Some(manager) = upgrade_manager.clone() {
        agent.set_upgrade_manager(manager);
    }
    // Adopt workloads that survived a previous bun process (restart or
    // self-upgrade exec) BEFORE the agent loop starts reconciling.
    agent.adopt_recorded_instances().await;
    let deploy_history = agent.deploy_history_handle();

    // Onion DNS: start the .internal responder when [dns] enables it,
    // resolving from the agent's service-map snapshots.
    if config.dns.enabled {
        let dns_config = config.dns.to_dns_config()?;
        let service_map_rx = agent.service_map_watch();
        let dns_shutdown = shutdown.clone();
        println!("bun: dns responder on {}", dns_config.listen_addr);
        tokio::spawn(async move {
            if let Err(e) =
                reliaburger::onion::dns::run_dns_responder(dns_config, service_map_rx, dns_shutdown)
                    .await
            {
                eprintln!("bun: dns responder failed to bind: {e}");
            }
        });
    }

    // Wrapper ingress: bind the HTTP(S) listeners when [ingress] enables
    // them, sharing the routing table the agent rebuilds on deploys.
    if config.ingress.enabled {
        let routing_table = agent.routing_table_handle();
        // Share the agent's drain tracker with the proxy so retiring backends
        // drain in-flight traffic before the container is killed (DEP5).
        let drains = agent.drains_handle();
        let wrapper_config = config.ingress.to_wrapper_config();
        let ingress_shutdown = shutdown.clone();
        let bound = reliaburger::wrapper::proxy::bind_proxy_with_drains(
            wrapper_config,
            routing_table,
            Some(drains),
            ingress_shutdown,
        )
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind ingress listeners: {e}"))?;
        println!(
            "bun: ingress listening on http {} / https {}",
            bound.http_addr, bound.https_addr
        );
        tokio::spawn(async move {
            if let Err(e) = bound.serve().await {
                eprintln!("bun: ingress proxy exited with error: {e}");
            }
        });
    }

    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    // Create observability stores (the Mayo store was created above,
    // before the cluster runtime that its rollup worker feeds from)
    let logs_dir = if std::fs::create_dir_all(&config.storage.logs).is_ok() {
        config.storage.logs.clone()
    } else {
        let fallback = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp/reliaburger"))
            .join("reliaburger")
            .join("logs");
        std::fs::create_dir_all(&fallback).expect("failed to create logs directory");
        fallback
    };

    // Create Arrow/DataFusion log store (SQL queries over logs)
    let log_store_dir = logs_dir.join("parquet");
    std::fs::create_dir_all(&log_store_dir).ok();
    // Seed the log store with startup events so it's never empty
    let mut log_store_inner = LogStore::new(log_store_dir);
    log_store_inner.append(
        "bun",
        "system",
        reliaburger::ketchup::types::LogStream::Stdout,
        &format!(
            "reliaburger node agent v{} started",
            env!("CARGO_PKG_VERSION")
        ),
    );
    log_store_inner.append(
        "bun",
        "system",
        reliaburger::ketchup::types::LogStream::Stdout,
        &format!("runtime: {}", cli.runtime),
    );
    let log_store = Arc::new(RwLock::new(log_store_inner));

    // Drain container log lines from the agent into the LogStore.
    {
        let drain_store = Arc::clone(&log_store);
        tokio::spawn(async move {
            while let Some(rec) = log_rx.recv().await {
                drain_store
                    .write()
                    .await
                    .append(&rec.app, &rec.namespace, rec.stream, &rec.line);
            }
        });
    }

    println!("bun: observability enabled (metrics + logs + alerts)");

    // Spawn metrics collection task
    let collection_mayo = Arc::clone(&mayo_store);
    let collection_interval = config.metrics.collection_interval_secs;
    let collection_shutdown = shutdown.clone();
    let collection_cmd_tx = cmd_tx.clone();
    tokio::spawn(async move {
        let mut collector = SystemCollector::new();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(collection_interval));
        let mut flush_counter = 0u64;
        loop {
            tokio::select! {
                _ = collection_shutdown.cancelled() => break,
                _ = tick.tick() => {
                    collector.refresh();
                    let mut samples = collector.collect_node_metrics();

                    // Per-app metrics (OBS3): ask the agent for its running
                    // instances and collect per-process CPU/memory for each one
                    // with a PID, labelled `namespace/app`. Without this only
                    // node-level metrics existed, so the autoscaler and the
                    // per-app dashboards had no signal. The labelling itself
                    // lives in `collect_instance_metrics` so it's unit-tested.
                    let (status_tx, status_rx) = tokio::sync::oneshot::channel();
                    if collection_cmd_tx
                        .send(reliaburger::bun::agent::AgentCommand::Status { response: status_tx })
                        .await
                        .is_ok()
                        && let Ok(statuses) = status_rx.await
                    {
                        let instances: Vec<(Option<u32>, &str, &str)> = statuses
                            .iter()
                            .map(|s| (s.pid, s.namespace.as_str(), s.app_name.as_str()))
                            .collect();
                        samples.extend(collector.collect_instance_metrics(&instances));
                    }

                    {
                        let mut store = collection_mayo.write().await;
                        for m in &samples {
                            store.insert_now(&m.key, m.value);
                        }
                    }

                    flush_counter += 1;
                    // Flush to Parquet every 6 ticks (~60s at 10s interval).
                    // `flush_off_lock` drains under a brief lock then writes
                    // outside it, so concurrent queries don't starve (OBS5).
                    if flush_counter.is_multiple_of(6)
                        && let Err(e) =
                            reliaburger::mayo::store::flush_off_lock(&collection_mayo).await
                    {
                        eprintln!("bun: metrics flush error: {e}");
                    }
                }
            }
        }
    });

    // Spawn log store flush task (every 60s)
    let log_flush_store = Arc::clone(&log_store);
    let log_flush_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = log_flush_shutdown.cancelled() => break,
                _ = tick.tick() => {
                    let mut store = log_flush_store.write().await;
                    if let Err(e) = store.flush().await {
                        eprintln!("bun: log flush error: {e}");
                    }
                }
            }
        }
    });

    // Spawn log export task (if configured)
    if let Some(ref export_path) = config.logs.export_path {
        let export_store = Arc::clone(&log_store);
        let export_shutdown = shutdown.clone();
        let export_dest = export_path.clone();
        let export_interval = std::time::Duration::from_secs(config.logs.export_interval_secs);
        let node_id = config
            .node
            .name
            .clone()
            .unwrap_or_else(|| "local".to_string());
        println!(
            "bun: log export enabled → {export_dest} (every {}s)",
            config.logs.export_interval_secs
        );
        tokio::spawn(async move {
            use reliaburger::ketchup::export::{
                CHECKPOINT_FILENAME, ExportCheckpoint, export_logs,
            };
            let mut tick = tokio::time::interval(export_interval);
            // Skip first tick (fires immediately)
            tick.tick().await;
            let store_guard = export_store.read().await;
            let checkpoint_path = store_guard.data_dir().join(CHECKPOINT_FILENAME);
            let mut checkpoint = ExportCheckpoint::load(&checkpoint_path);
            drop(store_guard);
            loop {
                tokio::select! {
                    _ = export_shutdown.cancelled() => break,
                    _ = tick.tick() => {
                        let data_dir = export_store.read().await.data_dir().to_path_buf();
                        match export_logs(&data_dir, &export_dest, &node_id, &mut checkpoint).await {
                            Ok(result) if result.files_exported > 0 => {
                                println!("bun: exported {} log file(s) to {}", result.files_exported, export_dest);
                                checkpoint.save(&checkpoint_path).ok();
                            }
                            Err(e) => eprintln!("bun: log export error: {e}"),
                            _ => {}
                        }
                    }
                }
            }
        });
    }

    // Spawn disk pressure check task (every 5 minutes)
    // Exports un-exported files before pruning, so data is never lost.
    {
        let dp_log_store = Arc::clone(&log_store);
        let dp_mayo_store = Arc::clone(&mayo_store);
        let dp_shutdown = shutdown.clone();
        let log_export_path = config.logs.export_path.clone();
        let log_max_bytes = config.logs.max_storage_mb * 1024 * 1024;
        let log_retention_days = config.logs.retention_days;
        let metrics_export_path = config.metrics.export_path.clone();
        let metrics_max_bytes = config.metrics.max_storage_mb * 1024 * 1024;
        let metrics_retention_days = config.metrics.retention_days;
        let dp_node_id = config
            .node
            .name
            .clone()
            .unwrap_or_else(|| "local".to_string());
        let dp_pressured_tx = disk_pressured_tx.clone();
        tokio::spawn(async move {
            use reliaburger::bun::disk_pressure::{
                DiskPressureResignation, ResignationVerdict, check_and_relieve, dir_parquet_size,
            };
            use reliaburger::ketchup::export::{CHECKPOINT_FILENAME, ExportCheckpoint};
            let tick_period = std::time::Duration::from_secs(300);
            let mut tick = tokio::time::interval(tick_period);
            // Council resignation waits for two sustained ticks (~10 min) over
            // the threshold before advertising, so a transient spike between
            // export and prune doesn't churn the council (12b.2 T3).
            let mut resignation = DiskPressureResignation::new(tick_period * 2);
            tick.tick().await; // skip first immediate tick

            let log_store_guard = dp_log_store.read().await;
            let log_checkpoint_path = log_store_guard.data_dir().join(CHECKPOINT_FILENAME);
            let log_data_dir = log_store_guard.data_dir().to_path_buf();
            drop(log_store_guard);
            let mut log_checkpoint = ExportCheckpoint::load(&log_checkpoint_path);

            let mayo_store_guard = dp_mayo_store.read().await;
            let mayo_checkpoint_path = mayo_store_guard.data_dir().join(CHECKPOINT_FILENAME);
            let mayo_data_dir = mayo_store_guard.data_dir().to_path_buf();
            drop(mayo_store_guard);
            let mut mayo_checkpoint = ExportCheckpoint::load(&mayo_checkpoint_path);

            loop {
                tokio::select! {
                    _ = dp_shutdown.cancelled() => break,
                    _ = tick.tick() => {
                        // Check log disk pressure
                        let log_result = check_and_relieve(
                            &log_data_dir,
                            log_export_path.as_deref(),
                            &dp_node_id,
                            &mut log_checkpoint,
                            log_max_bytes,
                            log_retention_days,
                        )
                        .await;
                        if log_result.files_pruned > 0 {
                            println!(
                                "bun: disk pressure — pruned {} log file(s), reclaimed {} bytes",
                                log_result.files_pruned, log_result.bytes_reclaimed
                            );
                            log_checkpoint.save(&log_checkpoint_path).ok();
                        }

                        // Check metrics disk pressure
                        let metrics_result = check_and_relieve(
                            &mayo_data_dir,
                            metrics_export_path.as_deref(),
                            &dp_node_id,
                            &mut mayo_checkpoint,
                            metrics_max_bytes,
                            metrics_retention_days,
                        )
                        .await;
                        if metrics_result.files_pruned > 0 {
                            println!(
                                "bun: disk pressure — pruned {} metrics file(s), reclaimed {} bytes",
                                metrics_result.files_pruned, metrics_result.bytes_reclaimed
                            );
                            mayo_checkpoint.save(&mayo_checkpoint_path).ok();
                        }

                        // Council resignation (12b.2 T3): if either store stays
                        // over its threshold AFTER export-and-prune, the disk is
                        // genuinely full, not just holding stale data. Sustained
                        // long enough, advertise resignation so the leader
                        // replaces this voter.
                        let over_threshold = (log_max_bytes > 0
                            && dir_parquet_size(&log_data_dir) > log_max_bytes)
                            || (metrics_max_bytes > 0
                                && dir_parquet_size(&mayo_data_dir) > metrics_max_bytes);
                        let verdict = resignation.observe(over_threshold, std::time::Instant::now());
                        let should_resign = verdict == ResignationVerdict::Resign;
                        if *dp_pressured_tx.borrow() != should_resign {
                            if should_resign {
                                println!(
                                    "bun: sustained disk pressure — advertising council resignation"
                                );
                            }
                            let _ = dp_pressured_tx.send(should_resign);
                        }
                    }
                }
            }
        });
    }

    // Start the API server.
    //
    // AUTH3 fail-closed: a fresh cluster with no user tokens leaves the API
    // wide open (the middleware's bootstrap window). That's fine on loopback
    // — the operator creates the first token locally — but binding a wide-open
    // API to a routable address exposes the whole cluster to anyone who can
    // reach it. Refuse that combination and tell the operator how to recover.
    if let Some(store) = &api_token_store {
        let no_tokens = store.read().await.is_empty();
        if no_tokens {
            refuse_open_non_loopback_bind(&cli.listen)?;
        }
    }
    let listener = tokio::net::TcpListener::bind(&cli.listen).await?;
    println!("bun: API server listening on {}", cli.listen);

    // L10: the catalog used to be `default()` on every boot — image
    // metadata evaporated on restart. Load the persisted copy; a
    // corrupt file aborts startup rather than silently orphaning blobs.
    let _ = std::fs::create_dir_all(&config.storage.data);
    let catalog_path = config.storage.data.join("pickle-catalog.json");
    let loaded_catalog = ManifestCatalog::load_from(&catalog_path)
        .map_err(|e| anyhow::anyhow!("failed to load pickle catalog: {e}"))?;
    if !loaded_catalog.manifests.is_empty() {
        println!(
            "bun: pickle catalog loaded ({} manifests)",
            loaded_catalog.manifests.len()
        );
    }
    let pickle_catalog: Arc<RwLock<ManifestCatalog>> = Arc::new(RwLock::new(loaded_catalog));

    // Create the alert evaluator (shared between the API and the
    // evaluation loop so /v1/alerts always reflects current state).
    let alerts: Option<Arc<RwLock<AlertEvaluator>>> = if config.metrics.alerts_enabled {
        Some(Arc::new(RwLock::new(AlertEvaluator::with_defaults())))
    } else {
        None
    };

    // Cloned for the GC task (asks the agent for actively deployed images).
    let gc_cmd_tx = cmd_tx.clone();

    // GitOps (L13): if [gitops] is configured on a cluster node, spawn
    // the leader-only sync loop and hand the API a webhook sender that
    // nudges it. The webhook endpoint returns 503 without this.
    // The webhook validator authenticates public webhook POSTs by the
    // `[gitops] webhook_secret` HMAC (GIT3). Without a secret it stays
    // `None`, and the public route fails closed.
    let mut gitops_webhook_validator = None;
    let gitops_webhook_tx =
        if let (Some(gitops), Some(council)) = (config.gitops.clone(), api_council.clone()) {
            let (webhook_tx, webhook_rx) = mpsc::channel::<()>(16);
            if let Some(secret) = gitops.webhook_secret.as_deref() {
                gitops_webhook_validator = Some(std::sync::Arc::new(tokio::sync::Mutex::new(
                    reliaburger::lettuce::webhook::WebhookValidator::new(
                        secret,
                        gitops.webhook_rate_limit,
                    ),
                )));
            }
            reliaburger::lettuce::runner::spawn_gitops_sync(
                council,
                gitops,
                webhook_rx,
                config.storage.data.clone(),
                shutdown.clone(),
            );
            println!("bun: gitops sync loop started");
            Some(webhook_tx)
        } else {
            None
        };

    // A freshly swapped-in version must prove itself: after the boot grace
    // period, ask the agent to verify that every pre-upgrade workload
    // survived, then commit (or flag revert and exit).
    // TODO(Phase 14, orchestration step): in cluster mode, also require
    // gossip rejoin within upgrades.gossip_rejoin_secs before committing.
    if let Some(marker) = upgrade_verify.take() {
        let verify_tx = cmd_tx.clone();
        let grace_secs = config.upgrades.boot_grace_secs;
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(grace_secs)).await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            if verify_tx
                .send(reliaburger::bun::agent::AgentCommand::UpgradeVerify {
                    marker,
                    response: tx,
                })
                .await
                .is_ok()
            {
                let _ = rx.await;
            }
        });
    }

    let app = api::router_with_upgrade(
        cmd_tx,
        Some(Arc::clone(&mayo_store)),
        Some(Arc::clone(&log_store)),
        Some(deploy_history),
        Some(Arc::clone(&pickle_catalog)),
        alerts.clone(),
        api_council.clone(),
        api_token_store.clone(),
        service_token.clone(),
        api_rollup_store,
        api_membership.clone(),
        gitops_webhook_tx,
        gitops_webhook_validator,
        api_port,
        Some(event_store),
        upgrade_manager.clone().map(Arc::new),
        // Batch capacity (F1): the leader's aggregated worker reports.
        api_aggregated_rx.clone(),
        Some(node_name.clone()),
        config.images.build_timeout_secs,
        cluster_http.clone(),
        config.images.registry_port,
        config.images.max_context_bytes,
        // Signing becomes part of a build's terminal state under a
        // signature-requiring trust policy (12b.2 JOB7).
        config.images.trust_policy.require_signatures,
    );
    let server_shutdown = shutdown.clone();
    // Serve the API over TLS when this node has an mTLS identity; the listener
    // accepts client certs optionally, so relish and browsers connect with a
    // bearer token / cookie over TLS while node-to-node calls may present a
    // node cert.
    let api_acceptor = match &api_identity {
        Some(identity) => {
            let crl = crl_refresh.clone().unwrap_or_default();
            let cfg = reliaburger::sesame::mtls::build_api_server_config(identity, crl)
                .map_err(|e| anyhow::anyhow!("failed to build API TLS config: {e}"))?;
            Some(tokio_rustls::TlsAcceptor::from(cfg))
        }
        None => None,
    };
    let server_handle = tokio::spawn(async move {
        match api_acceptor {
            Some(acceptor) => serve_api_over_tls(listener, acceptor, app, server_shutdown).await,
            None => {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        server_shutdown.cancelled().await;
                    })
                    .await
                    .ok();
            }
        }
    });

    // Spawn alert evaluation + webhook dispatch task
    if let Some(ref alert_evaluator) = alerts {
        let eval_mayo = Arc::clone(&mayo_store);
        let eval_alerts = Arc::clone(alert_evaluator);
        let eval_shutdown = shutdown.clone();
        let eval_interval = config.alerts.evaluation_interval_secs;
        let cluster_name = config
            .node
            .name
            .clone()
            .unwrap_or_else(|| "local".to_string());

        let webhook_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        let dispatcher = WebhookDispatcher::new(
            webhook_client,
            config.alerts.destinations.clone(),
            cluster_name,
        );

        if !config.alerts.destinations.is_empty() {
            println!(
                "bun: alert webhooks enabled ({} destination(s), every {}s)",
                config.alerts.destinations.len(),
                eval_interval,
            );
        }

        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(eval_interval));
            loop {
                tokio::select! {
                    _ = eval_shutdown.cancelled() => break,
                    _ = tick.tick() => {
                        let store = eval_mayo.read().await;
                        let latest = gather_latest_values(&store).await;
                        drop(store);

                        let transitions = {
                            let mut eval = eval_alerts.write().await;
                            eval.evaluate(&latest)
                        };

                        for t in transitions {
                            let d = dispatcher.clone();
                            tokio::spawn(async move {
                                d.dispatch(&t).await;
                            });
                        }
                    }
                }
            }
        });
    }

    // Start the Pickle OCI registry server
    let registry_addr = format!(
        "{}:{}",
        config.images.registry_bind, config.images.registry_port
    );
    let pickle_dir = if std::fs::create_dir_all(&config.storage.images).is_ok() {
        config.storage.images.clone()
    } else {
        // Fall back to user-writable directory (e.g. on macOS without root)
        let fallback = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp/reliaburger"))
            .join("reliaburger")
            .join("images");
        std::fs::create_dir_all(&fallback).expect("failed to create pickle directory");
        eprintln!(
            "bun: using fallback image store at {} (cannot write to {})",
            fallback.display(),
            config.storage.images.display()
        );
        fallback
    };
    let node_raft_id = reliaburger::cluster::identity::raft_id_from_name(&node_name);

    let blob_store = Arc::new(BlobStore::new(&pickle_dir));
    // Registry writes reuse the cluster's existing auth material (REG4):
    // the same token store and service token that guard the agent API. In
    // single-node/tokenless mode this stays `None` and writes are open.
    let registry_auth = api_token_store.as_ref().map(|store| {
        reliaburger::sesame::auth::AuthState::new(Arc::clone(store), service_token.clone())
    });
    // Storage quotas (REG4): a per-repository ceiling derived from
    // `[images] max_storage` divided across repositories is more than an
    // operator asked for; we apply `max_storage` as the registry-wide cap
    // and leave per-repository unlimited unless configured.
    let registry_quota = reliaburger::pickle::registry_auth::QuotaConfig {
        per_repository_bytes: 0,
        total_bytes: reliaburger::config::types::parse_resource_value(&config.images.max_storage)
            .unwrap_or(0),
    };
    let upload_sessions = reliaburger::pickle::registry_auth::UploadSessions::new(
        reliaburger::pickle::registry_auth::DEFAULT_UPLOAD_TTL,
    );
    let pickle_state = PickleState {
        store: Arc::clone(&blob_store),
        catalog: Arc::clone(&pickle_catalog),
        node_raft_id,
        council: api_council.clone(),
        persist_path: Some(catalog_path.clone()),
        auth: registry_auth,
        quota: registry_quota,
        sessions: upload_sessions.clone(),
    };
    // The registry serves over TLS when this node has an mTLS identity
    // (REG4), so peers must be addressed as https and the replication /
    // P2P client must trust the cluster CA. Without an identity it's plain
    // http, and a default client. The scheme is derived once and threaded
    // through every peer-URL derivation so they stay consistent.
    let registry_over_tls = api_identity.is_some();
    let registry_scheme = if registry_over_tls { "https" } else { "http" };
    let registry_client = match &api_identity {
        Some(identity) => reliaburger::sesame::mtls::build_cluster_http_client(identity)
            .unwrap_or_else(|_| reqwest::Client::new()),
        None => reqwest::Client::new(),
    };

    // Cluster-first image pulls (Phase 12 C2): the grill consults the
    // Pickle catalog before any external registry, filling layers from
    // peers in parallel. Standalone nodes get local-catalog resolution
    // (no peers) — the only way locally-pushed images deploy at all.
    if let Some(image_store) = &cluster_image_store {
        // Upstream registry credentials come from the environment
        // (variable named by [images] external_registries
        // password_secret); unresolvable entries degrade to anonymous.
        let credentials = reliaburger::pickle::upstream::resolve_credentials(
            &config.images.external_registries,
            |name| std::env::var(name).ok(),
        );
        image_store.set_cluster_source(std::sync::Arc::new(
            reliaburger::pickle::p2p::ClusterSource {
                state: pickle_state.clone(),
                members: replication_membership.clone(),
                registry_port: config.images.registry_port,
                peer_scheme: registry_scheme.to_string(),
                concurrency: config.images.p2p_concurrency,
                client: registry_client.clone(),
                upstream: Some(std::sync::Arc::new(
                    reliaburger::pickle::upstream::OciUpstream::new(credentials),
                )),
                pull_through: config.images.pull_through,
                cache_recheck_secs: config.images.cache_recheck_secs,
                fill_lock: tokio::sync::Mutex::new(()),
            },
        ));
    }

    let pickle_app = reliaburger::pickle::api::router(pickle_state);
    let pickle_listener = tokio::net::TcpListener::bind(&registry_addr).await?;
    println!(
        "bun: Pickle registry listening on {registry_addr} ({})",
        if registry_over_tls {
            "TLS, authenticated writes"
        } else {
            "plaintext, unauthenticated"
        }
    );

    // In cluster mode a loopback-bound registry silently disables peer
    // replication, healing, and P2P image pulls — peers address us as
    // <scheme>://<gossip-ip>:<registry_port>. Warn loudly rather than
    // fail: single-node-with-council setups are legitimate.
    if api_council.is_some() && config.images.registry_bind == "127.0.0.1" {
        eprintln!(
            "bun: WARNING: [images] registry_bind is 127.0.0.1 in cluster mode — \
             image replication and P2P pulls between nodes will not work; \
             set registry_bind to a peer-reachable address"
        );
    }

    // Sweep expired upload sessions and their temp files (REG8) so an
    // abandoned push doesn't leak a temp forever.
    {
        let sweep_sessions = upload_sessions.clone();
        let sweep_store = Arc::clone(&blob_store);
        let sweep_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(300));
            loop {
                tokio::select! {
                    _ = sweep_shutdown.cancelled() => break,
                    _ = ticker.tick() => {
                        for id in sweep_sessions.sweep(std::time::SystemTime::now()).await {
                            sweep_store.cancel_upload(&id).await;
                        }
                    }
                }
            }
        });
    }

    // Serve the registry over TLS when this node has an mTLS identity,
    // reusing the same server config as the agent API (REG4). Client certs
    // are optional — node-to-node replication presents the service token
    // inside TLS, exactly like the API's node-to-node fan-out.
    let pickle_acceptor = match (&api_identity, registry_over_tls) {
        (Some(identity), true) => {
            let crl = crl_refresh.clone().unwrap_or_default();
            match reliaburger::sesame::mtls::build_api_server_config(identity, crl) {
                Ok(cfg) => Some(tokio_rustls::TlsAcceptor::from(cfg)),
                Err(e) => {
                    eprintln!("bun: failed to build registry TLS config, serving plaintext: {e}");
                    None
                }
            }
        }
        _ => None,
    };

    let pickle_shutdown = shutdown.clone();
    let pickle_handle = tokio::spawn(async move {
        match pickle_acceptor {
            Some(acceptor) => {
                serve_api_over_tls(pickle_listener, acceptor, pickle_app, pickle_shutdown).await
            }
            None => {
                axum::serve(pickle_listener, pickle_app)
                    .with_graceful_shutdown(async move {
                        pickle_shutdown.cancelled().await;
                    })
                    .await
                    .ok();
            }
        }
    });

    // Scheduled image GC (L10/M2): two-phase — nominate candidates,
    // let the arbiter (Raft in cluster mode, the local catalog's same
    // rule otherwise) approve, then delete only what was approved.
    {
        use reliaburger::council::types::{CouncilResponse, RaftRequest};
        use reliaburger::pickle::gc::{GcConfig, delete_approved, gc_candidates};

        let gc_store = Arc::clone(&blob_store);
        let gc_catalog = Arc::clone(&pickle_catalog);
        let gc_council = api_council.clone();
        let gc_persist = catalog_path.clone();
        let gc_shutdown = shutdown.clone();
        let gc_config = GcConfig {
            retain_days: config.images.gc_retain_days,
            node_id: node_raft_id,
            orphan_grace: std::time::Duration::from_secs(3600),
        };
        let gc_interval = std::time::Duration::from_secs(
            u64::from(config.images.gc_interval_hours.max(1)) * 3600,
        );

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(gc_interval);
            ticker.tick().await; // skip the immediate first tick
            loop {
                tokio::select! {
                    _ = gc_shutdown.cancelled() => break,
                    _ = ticker.tick() => {}
                }

                // Actively deployed images are never collected.
                let (tx, rx) = tokio::sync::oneshot::channel();
                let _ = gc_cmd_tx
                    .send(reliaburger::bun::agent::AgentCommand::ActiveImages { response: tx })
                    .await;
                let active = rx.await.unwrap_or_default();

                // Phase 1: nominate (fs walk off the runtime).
                let catalog_snapshot = gc_catalog.read().await.clone();
                let store = Arc::clone(&gc_store);
                let gc_cfg = gc_config.clone();
                let nominated = tokio::task::spawn_blocking(move || {
                    gc_candidates(&store, &catalog_snapshot, &active, &gc_cfg)
                })
                .await;
                let Ok(Ok(nominated)) = nominated else {
                    continue;
                };
                let Some(report) = nominated.report(gc_config.node_id) else {
                    continue;
                };

                // Arbitration: Raft serialises cluster-wide; single-node
                // applies the identical rule to the local catalog.
                let approved = match &gc_council {
                    Some(council) => {
                        match council.write(RaftRequest::GcReport(report.clone())).await {
                            Ok(CouncilResponse::GcApproved { approved }) => {
                                // Mirror the holder removal locally too.
                                let mirror = reliaburger::pickle::types::GcReport {
                                    node_id: report.node_id,
                                    deleted_layers: approved.clone(),
                                };
                                let _ = gc_catalog.write().await.apply_gc_report(&mirror);
                                approved
                            }
                            Ok(_) => Vec::new(),
                            Err(e) => {
                                eprintln!(
                                    "bun: gc arbitration unavailable ({e}); deleting nothing"
                                );
                                Vec::new()
                            }
                        }
                    }
                    None => gc_catalog.write().await.apply_gc_report(&report),
                };

                if approved.is_empty() {
                    continue;
                }

                // Persist the catalog change, then phase 2: delete.
                let snapshot = gc_catalog.read().await.clone();
                let persist = gc_persist.clone();
                let store = Arc::clone(&gc_store);
                let deleted = tokio::task::spawn_blocking(move || {
                    if let Err(e) = snapshot.persist_to(&persist) {
                        eprintln!("bun: gc failed to persist catalog: {e}");
                    }
                    delete_approved(&store, &approved)
                })
                .await
                .unwrap_or_default();
                if !deleted.is_empty() {
                    println!("bun: gc removed {} blob(s)", deleted.len());
                }
            }
        });
    }

    // Pickle heal loop (L10 + Phase 12 B5): leader-only loop that keeps
    // every manifest's layers on at least `[images] redundancy` nodes.
    // The tick body lives in `pickle::replication::heal_tick` — rarest
    // first, capped per tick, pulling layers the leader lacks before
    // replicating onward — so it is testable without a running binary.
    if let (Some(council), Some(membership_rx)) = (api_council.clone(), replication_membership) {
        let repl_store = Arc::clone(&blob_store);
        let repl_shutdown = shutdown.clone();
        let redundancy = config.images.redundancy.max(1);
        let registry_port = config.images.registry_port;
        let self_node = node_raft_id;
        // Peers and the client must match the registry's scheme/TLS (REG4).
        let heal_scheme = registry_scheme.to_string();
        let heal_client = registry_client.clone();

        tokio::spawn(async move {
            let client = heal_client;
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = repl_shutdown.cancelled() => break,
                    _ = ticker.tick() => {}
                }
                if !council.is_leader().await {
                    continue;
                }

                let catalog = council.manifest_catalog().await;
                let peers = {
                    let members = membership_rx.borrow();
                    reliaburger::cluster::identity::pickle_peers_scheme(
                        &members,
                        registry_port,
                        &heal_scheme,
                    )
                };
                if peers.len() < 2 {
                    continue; // nobody to replicate to
                }

                let outcome = reliaburger::pickle::replication::heal_tick(
                    &catalog,
                    &repl_store,
                    self_node,
                    &peers,
                    redundancy,
                    10,
                    &client,
                )
                .await;

                for error in &outcome.errors {
                    eprintln!("bun: pickle heal: {error}");
                }
                for update in outcome.updates {
                    if let Err(e) = council
                        .write(
                            reliaburger::council::types::RaftRequest::UpdateLayerLocations(update),
                        )
                        .await
                    {
                        eprintln!("bun: replication holder update failed: {e}");
                    }
                }
            }
        });
    }

    // Wait for SIGINT or SIGTERM. Handling SIGTERM matters under systemd/docker
    // stop — without it the agent was killed before shutdown_all ran, orphaning
    // every workload process.
    let signal_shutdown = shutdown.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigterm = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("bun: failed to install SIGTERM handler: {e}");
                    tokio::signal::ctrl_c().await.ok();
                    signal_shutdown.cancel();
                    return;
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await.ok();
        }
        println!("\nbun: received shutdown signal");
        signal_shutdown.cancel();
    });

    // Wait for everything to finish
    let _ = tokio::join!(agent_handle, server_handle, pickle_handle);

    // OBS7: the flush loops break on cancellation and drop whatever they'd
    // buffered since their last tick. Force one final flush of both stores so
    // the last minute of metrics and logs is durable, not lost on a clean
    // stop. The tasks that fed these buffers are joined above, so nothing is
    // still appending.
    if let Err(e) = reliaburger::mayo::store::flush_off_lock(&mayo_store).await {
        eprintln!("bun: final metrics flush error: {e}");
    }
    if let Err(e) = reliaburger::ketchup::log_store::flush_shared(&log_store).await {
        eprintln!("bun: final log flush error: {e}");
    }
    println!("bun: shutdown complete");

    Ok(())
}

async fn select_runtime(
    name: &str,
    dns_nameserver: Option<std::net::Ipv4Addr>,
    instances_dir: &std::path::Path,
) -> anyhow::Result<AnyGrill> {
    // Silence the unused warning on platforms without runc.
    let _ = dns_nameserver;
    match name {
        "auto" => {
            let runtime = detect_runtime().await;
            // Rebuild the process fallback in file-backed mode: workload
            // output must go to files (not pipes) to survive a self-upgrade
            // exec and support adoption.
            let runtime = match runtime {
                AnyGrill::Process(_) => {
                    AnyGrill::Process(ProcessGrill::with_log_dir(instances_dir.to_path_buf()))
                }
                other => other,
            };
            let kind = match &runtime {
                AnyGrill::Process(_) => "process",
                #[cfg(target_os = "linux")]
                AnyGrill::Runc(_) => "runc",
                #[cfg(target_os = "macos")]
                AnyGrill::Apple(_) => "apple-container",
            };
            println!("bun: auto-detected runtime: {kind}");
            Ok(runtime)
        }
        "process" => {
            println!("bun: using process runtime");
            Ok(AnyGrill::Process(ProcessGrill::with_log_dir(
                instances_dir.to_path_buf(),
            )))
        }
        #[cfg(target_os = "linux")]
        "runc" => {
            let is_rootless = reliaburger::grill::rootless::is_rootless();
            let mode = if is_rootless { "rootless" } else { "root" };
            println!("bun: using runc runtime ({mode})");

            let (bundle_base, image_store, state_dir) = if is_rootless {
                let base = dirs::data_local_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("/tmp/reliaburger"))
                    .join("reliaburger");
                (
                    base.join("bundles"),
                    reliaburger::grill::ImageStore::new(base.join("images")),
                    reliaburger::grill::rootless::rootless_state_dir(),
                )
            } else {
                let base = std::path::PathBuf::from("/var/lib/reliaburger");
                (
                    base.join("bundles"),
                    reliaburger::grill::ImageStore::new(base.join("images")),
                    std::path::PathBuf::from("/run/reliaburger/runc"),
                )
            };

            let mut grill = reliaburger::grill::runc::RuncGrill::new(
                bundle_base,
                image_store,
                is_rootless,
                state_dir,
            );
            // Containers resolve .internal via the node's DNS responder;
            // resolv.conf has no port syntax, so this only applies to
            // port-53 listeners (checked by the caller).
            if let Some(nameserver) = dns_nameserver {
                grill = grill.with_dns_nameserver(nameserver);
            }
            Ok(AnyGrill::Runc(grill))
        }
        #[cfg(target_os = "macos")]
        "apple" => {
            println!("bun: using Apple Container runtime");
            Ok(AnyGrill::Apple(
                reliaburger::grill::apple::AppleContainerGrill::new(),
            ))
        }
        other => anyhow::bail!("unknown runtime: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use reliaburger::council::CouncilNode;
    use reliaburger::council::log_store::MemLogStore;
    use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
    use reliaburger::council::state_machine::CouncilStateMachine;
    use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo, RaftRequest};
    use reliaburger::sesame::token::create_token;
    use reliaburger::sesame::types::{ApiRole, TokenScope};

    #[test]
    fn bind_check_refuses_open_non_loopback() {
        // A wide-open API on a routable address must be refused (AUTH3).
        let err = refuse_open_non_loopback_bind("10.0.0.5:9117").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("non-loopback"), "message was: {msg}");
        assert!(msg.contains("relish token create"), "message was: {msg}");
    }

    #[test]
    fn bind_check_allows_loopback() {
        // Loopback is the bootstrap path: the operator mints the first token
        // locally, so a token-less loopback bind is fine.
        refuse_open_non_loopback_bind("127.0.0.1:9117").unwrap();
        refuse_open_non_loopback_bind("[::1]:9117").unwrap();
    }

    #[test]
    fn bind_check_allows_hostnames() {
        // A hostname isn't a literal address; we can't classify it here, so
        // we don't block — the resolver decides.
        refuse_open_non_loopback_bind("localhost:9117").unwrap();
    }

    #[tokio::test]
    async fn refresh_token_store_overwrites_with_current_raft_tokens() {
        // A single-node in-memory council, initialised as leader.
        let raft_router = InMemoryRaftRouter::new();
        let network = InMemoryRaftNetworkFactory::new(1, raft_router.clone());
        let council = CouncilNode::new(
            1,
            CouncilConfig::default(),
            network,
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap();
        raft_router.register(1, council.raft().clone()).await;
        let mut members = BTreeMap::new();
        members.insert(
            1,
            CouncilNodeInfo {
                addr: "127.0.0.1:9444".parse().unwrap(),
                name: "n1".into(),
            },
        );
        council.initialize(members).await.unwrap();

        let store = reliaburger::sesame::auth::new_token_store();
        // Empty to start.
        refresh_token_store(&store, &council).await;
        assert!(store.read().await.is_empty());

        // Create a token in Raft, then refresh: the store picks it up.
        let created = create_token("ci", ApiRole::Deployer, TokenScope::default(), None).unwrap();
        council
            .write(RaftRequest::CreateApiToken(created.token))
            .await
            .unwrap();
        refresh_token_store(&store, &council).await;

        let tokens = store.read().await;
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].name, "ci");
    }

    /// A config with require_mtls but no identity on disk.
    fn mtls_config_without_identity() -> (NodeConfig, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = NodeConfig::default();
        config.security.require_mtls = true;
        // Point the data dir at an empty temp dir so no identity is found.
        config.storage.data = dir.path().to_path_buf();
        (config, dir)
    }

    #[test]
    fn require_mtls_without_identity_refuses_to_bootstrap() {
        let (config, _dir) = mtls_config_without_identity();
        let params = cluster_params_from_config(&config).unwrap();
        assert!(params.identity.is_none());
        assert!(params.seeds.is_empty(), "bootstrap node has no seeds");

        let err = enforce_mtls_mode(&config, &params).unwrap_err();
        assert!(
            err.to_string().contains("relish init"),
            "bootstrap error should point at `relish init`, got: {err}"
        );
    }

    #[test]
    fn require_mtls_without_identity_tells_a_joiner_to_enrol() {
        let (mut config, _dir) = mtls_config_without_identity();
        config.cluster.join = vec!["127.0.0.1:9443".to_string()];
        let params = cluster_params_from_config(&config).unwrap();
        assert!(!params.seeds.is_empty(), "joiner has a seed");

        let err = enforce_mtls_mode(&config, &params).unwrap_err();
        assert!(
            err.to_string().contains("relish join"),
            "joiner error should point at `relish join`, got: {err}"
        );
    }

    #[test]
    fn mtls_mode_accepts_a_bootstrap_node_without_mtls_required() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = NodeConfig::default();
        config.storage.data = dir.path().to_path_buf();
        // require_mtls defaults to false — plaintext is allowed.
        let params = cluster_params_from_config(&config).unwrap();
        assert!(enforce_mtls_mode(&config, &params).is_ok());
    }
}
