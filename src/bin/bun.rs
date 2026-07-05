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
    })
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

    // Select runtime
    let runtime = select_runtime(&cli.runtime).await?;

    // Create command channel
    let (cmd_tx, cmd_rx) = mpsc::channel(256);

    // Create shutdown token
    let shutdown = CancellationToken::new();

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
    let mut agent = if cli.cluster {
        let params = cluster_params_from_config(&config)?;
        println!(
            "bun: cluster mode — gossip on {}, {} seed(s)",
            params.gossip_addr,
            params.seeds.len()
        );
        let (handle, cluster_runtime) =
            reliaburger::cluster::runtime::start(params, agent_shutdown.clone())
                .await
                .map_err(|e| anyhow::anyhow!("failed to start cluster runtime: {e}"))?;
        _cluster_runtime = Some(cluster_runtime);
        BunAgent::with_cluster(runtime, port_allocator, cmd_rx, agent_shutdown, handle)
    } else {
        _cluster_runtime = None;
        BunAgent::new(runtime, port_allocator, cmd_rx, agent_shutdown)
    };
    agent.set_log_sink(log_tx);
    let deploy_history = agent.deploy_history_handle();
    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    // Create observability stores
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

    let pickle_catalog: Arc<RwLock<ManifestCatalog>> =
        Arc::new(RwLock::new(ManifestCatalog::default()));

    // Create the alert evaluator (shared between the API and the
    // evaluation loop so /v1/alerts always reflects current state).
    let alerts: Option<Arc<RwLock<AlertEvaluator>>> = if config.metrics.alerts_enabled {
        Some(Arc::new(RwLock::new(AlertEvaluator::with_defaults())))
    } else {
        None
    };

    let app = api::router(
        cmd_tx,
        Some(Arc::clone(&mayo_store)),
        Some(Arc::clone(&log_store)),
        Some(deploy_history),
        Some(Arc::clone(&pickle_catalog)),
        alerts.clone(),
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
    let blob_store = BlobStore::new(&pickle_dir);
    let pickle_state = PickleState {
        store: Arc::new(blob_store),
        catalog: Arc::clone(&pickle_catalog),
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

async fn select_runtime(name: &str) -> anyhow::Result<AnyGrill> {
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

            Ok(AnyGrill::Runc(reliaburger::grill::runc::RuncGrill::new(
                bundle_base,
                image_store,
                is_rootless,
                state_dir,
            )))
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
