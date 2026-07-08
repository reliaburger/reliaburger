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
use reliaburger::ketchup::store::KetchupStore;
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

    Ok(reliaburger::cluster::runtime::ClusterParams {
        node_name,
        gossip_addr,
        raft_port: config.cluster.raft_port,
        reporting_port: config.cluster.reporting_port,
        reporting_config: config.reporting_tree.clone(),
        seeds,
        wrapping_ikm,
        bootstrap_security_state,
        data_dir: config.storage.data.clone(),
        // The MayoStore doesn't exist yet when params are built; the
        // caller sets it before starting the runtime.
        mayo: None,
        rollup_interval: std::time::Duration::from_secs(config.metrics.rollup_interval_secs),
    })
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    println!("bun: reliaburger node agent v{}", env!("CARGO_PKG_VERSION"));

    // Load node config
    let config = if let Some(ref path) = cli.config {
        NodeConfig::from_file(path).map_err(|e| anyhow::anyhow!("failed to load config: {e}"))?
    } else {
        NodeConfig::default()
    };

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

    // Select runtime
    let runtime = select_runtime(&cli.runtime, container_nameserver).await?;

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
    let _cluster_runtime;
    // Cloned out of the ClusterHandle before it's moved into the agent, so the
    // API router can expose council-backed endpoints (JWKS, tokens, secrets).
    let mut api_council: Option<Arc<reliaburger::council::CouncilNode>> = None;
    // The leader-side rollup store, exposed at /v1/metrics/cluster.
    let mut api_rollup_store = None;
    // Gossip membership for the pickle replication loop (cluster only).
    let mut replication_membership = None;
    let mut agent = if cli.cluster {
        let mut params = cluster_params_from_config(&config)?;
        params.mayo = Some(Arc::clone(&mayo_store));
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
        _cluster_runtime = Some(cluster_runtime);
        api_council = handle.council.clone();
        // Cloned before the handle moves into the agent: the pickle
        // replication loop derives its peer list from gossip.
        replication_membership = Some(handle.membership_rx.clone());
        BunAgent::with_cluster(runtime, port_allocator, cmd_rx, agent_shutdown, handle)
    } else {
        _cluster_runtime = None;
        BunAgent::new(runtime, port_allocator, cmd_rx, agent_shutdown)
    };

    // Report real schedulable capacity to the cluster (L6: StateReports
    // used to carry zeroes).
    let (capacity_cpu, capacity_memory) = node_capacity(&config);
    agent.set_node_capacity(capacity_cpu, capacity_memory);

    // Derive the internal service token from the shared master key, so bun's own
    // cross-node fan-out calls authenticate as the system principal on peers.
    let service_token = api_council
        .as_ref()
        .and_then(|c| c.wrapping_ikm())
        .map(reliaburger::sesame::token::derive_service_token)
        .transpose()?;

    // Seed the auth token store from the council's SecurityState and keep it
    // refreshed. The middleware reads this store; without the refresh, a token
    // created after startup would never engage enforcement (the ≤5 s lag is
    // deliberate).
    let api_token_store = if let Some(council) = &api_council {
        let store = reliaburger::sesame::auth::new_token_store();
        refresh_token_store(&store, council).await;

        let refresh_store = Arc::clone(&store);
        let refresh_council = Arc::clone(council);
        let refresh_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tokio::select! {
                    _ = refresh_shutdown.cancelled() => break,
                    _ = ticker.tick() => refresh_token_store(&refresh_store, &refresh_council).await,
                }
            }
        });
        Some(store)
    } else {
        None
    };
    agent.set_log_sink(log_tx);
    agent.set_trust_policy(config.images.trust_policy.clone());
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
        let wrapper_config = config.ingress.to_wrapper_config();
        let ingress_shutdown = shutdown.clone();
        let bound = reliaburger::wrapper::proxy::bind_proxy(
            wrapper_config,
            routing_table,
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
    let _ketchup_store = Arc::new(RwLock::new(KetchupStore::new(&logs_dir)));

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
    tokio::spawn(async move {
        let mut collector = SystemCollector::new();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(collection_interval));
        let mut flush_counter = 0u64;
        loop {
            tokio::select! {
                _ = collection_shutdown.cancelled() => break,
                _ = tick.tick() => {
                    collector.refresh();
                    let metrics = collector.collect_node_metrics();
                    let mut store = collection_mayo.write().await;
                    for m in &metrics {
                        store.insert_now(&m.key, m.value);
                    }
                    flush_counter += 1;
                    // Flush to Parquet every 6 ticks (~60s at 10s interval)
                    if flush_counter.is_multiple_of(6)
                        && let Err(e) = store.flush().await
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
            use reliaburger::ketchup::export::{ExportCheckpoint, export_logs};
            let mut tick = tokio::time::interval(export_interval);
            // Skip first tick (fires immediately)
            tick.tick().await;
            let store_guard = export_store.read().await;
            let checkpoint_path = store_guard.data_dir().join("_export_checkpoint.json");
            let mut checkpoint = ExportCheckpoint::load(&checkpoint_path);
            drop(store_guard);
            loop {
                tokio::select! {
                    _ = export_shutdown.cancelled() => break,
                    _ = tick.tick() => {
                        let store = export_store.read().await;
                        match export_logs(store.data_dir(), &export_dest, &node_id, &mut checkpoint) {
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
        tokio::spawn(async move {
            use reliaburger::bun::disk_pressure::check_and_relieve;
            use reliaburger::ketchup::export::ExportCheckpoint;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
            tick.tick().await; // skip first immediate tick

            let log_store_guard = dp_log_store.read().await;
            let log_checkpoint_path = log_store_guard.data_dir().join("_export_checkpoint.json");
            let log_data_dir = log_store_guard.data_dir().to_path_buf();
            drop(log_store_guard);
            let mut log_checkpoint = ExportCheckpoint::load(&log_checkpoint_path);

            let mayo_store_guard = dp_mayo_store.read().await;
            let mayo_checkpoint_path = mayo_store_guard.data_dir().join("_export_checkpoint.json");
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
                        );
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
                        );
                        if metrics_result.files_pruned > 0 {
                            println!(
                                "bun: disk pressure — pruned {} metrics file(s), reclaimed {} bytes",
                                metrics_result.files_pruned, metrics_result.bytes_reclaimed
                            );
                            mayo_checkpoint.save(&mayo_checkpoint_path).ok();
                        }
                    }
                }
            }
        });
    }

    // Start the API server
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

    let app = api::router(
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
    );
    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                server_shutdown.cancelled().await;
            })
            .await
            .ok();
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
    let node_name = config
        .node
        .name
        .clone()
        .unwrap_or_else(|| format!("node-{}", config.cluster.gossip_port));
    let node_raft_id = reliaburger::cluster::identity::raft_id_from_name(&node_name);

    let blob_store = Arc::new(BlobStore::new(&pickle_dir));
    let pickle_state = PickleState {
        store: Arc::clone(&blob_store),
        catalog: Arc::clone(&pickle_catalog),
        node_raft_id,
        council: api_council.clone(),
        persist_path: Some(catalog_path.clone()),
    };
    let pickle_app = reliaburger::pickle::api::router(pickle_state);
    let pickle_listener = tokio::net::TcpListener::bind(&registry_addr).await?;
    println!("bun: Pickle registry listening on {registry_addr}");

    let pickle_shutdown = shutdown.clone();
    let pickle_handle = tokio::spawn(async move {
        axum::serve(pickle_listener, pickle_app)
            .with_graceful_shutdown(async move {
                pickle_shutdown.cancelled().await;
            })
            .await
            .ok();
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

    // Pickle replication (L10): leader-only loop that keeps every
    // manifest's layers on at least `[images] redundancy` nodes.
    if let (Some(council), Some(membership_rx)) = (api_council.clone(), replication_membership) {
        use reliaburger::pickle::replication::{
            Peer, ReplicationConfig, replicate_manifest, select_peers,
        };

        let repl_store = Arc::clone(&blob_store);
        let repl_shutdown = shutdown.clone();
        let redundancy = config.images.redundancy.max(1);
        let registry_port = config.images.registry_port;

        tokio::spawn(async move {
            let client = reqwest::Client::new();
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
                let peers: Vec<Peer> = membership_rx
                    .borrow()
                    .iter()
                    .filter(|m| m.state == reliaburger::mustard::state::NodeState::Alive)
                    .map(|m| Peer {
                        node_id: reliaburger::cluster::identity::raft_id_from_name(&m.node_id.0),
                        base_url: format!("http://{}:{registry_port}", m.address.ip()),
                    })
                    .collect();
                if peers.len() < 2 {
                    continue; // nobody to replicate to
                }

                for (_, manifest) in &catalog.manifests {
                    // Nodes that hold EVERY layer of this manifest.
                    let mut full_holders: Option<std::collections::BTreeSet<u64>> = None;
                    for digest in manifest.all_digests() {
                        let holders = catalog.layer_holders(digest.as_str());
                        full_holders = Some(match full_holders {
                            Some(acc) => acc.intersection(&holders).copied().collect(),
                            None => holders,
                        });
                    }
                    let full_holders = full_holders.unwrap_or_default();
                    if full_holders.len() >= redundancy as usize {
                        continue;
                    }

                    // The leader replicates from its own blobs; skip
                    // manifests it doesn't hold (their holder pushes
                    // will be reconciled once forwarding lands in W6).
                    if !manifest
                        .all_digests()
                        .iter()
                        .all(|d| repl_store.has_blob(d))
                    {
                        continue;
                    }

                    let needed = redundancy as usize - full_holders.len();
                    let targets = select_peers(&peers, 0, &full_holders, needed);
                    if targets.is_empty() {
                        continue;
                    }

                    let config = ReplicationConfig {
                        redundancy,
                        peer_timeout: std::time::Duration::from_secs(30),
                    };
                    match replicate_manifest(manifest, &repl_store, &targets, &config, &client)
                        .await
                    {
                        Ok(result) => {
                            let updates = manifest
                                .all_digests()
                                .iter()
                                .map(|d| {
                                    let mut holders = catalog.layer_holders(d.as_str());
                                    holders.extend(result.successful_nodes.iter().copied());
                                    ((*d).clone(), holders)
                                })
                                .collect();
                            let update =
                                reliaburger::pickle::types::UpdateLayerLocations { updates };
                            if let Err(e) = council
                                .write(
                                    reliaburger::council::types::RaftRequest::UpdateLayerLocations(
                                        update,
                                    ),
                                )
                                .await
                            {
                                eprintln!("bun: replication holder update failed: {e}");
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "bun: replication of {}:{} failed: {e}",
                                manifest.repository,
                                manifest
                                    .tags
                                    .iter()
                                    .next()
                                    .map(String::as_str)
                                    .unwrap_or("?")
                            );
                        }
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
    println!("bun: shutdown complete");

    Ok(())
}

async fn select_runtime(
    name: &str,
    dns_nameserver: Option<std::net::Ipv4Addr>,
) -> anyhow::Result<AnyGrill> {
    // Silence the unused warning on platforms without runc.
    let _ = dns_nameserver;
    match name {
        "auto" => {
            let runtime = detect_runtime().await;
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
            Ok(AnyGrill::Process(ProcessGrill::new()))
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
}
