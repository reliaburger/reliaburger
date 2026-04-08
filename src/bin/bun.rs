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
use reliaburger::mayo::collector::SystemCollector;
use reliaburger::mayo::store::MayoStore;
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

    // Create the agent (extract deploy history handle before spawning)
    let agent_shutdown = shutdown.clone();
    let mut agent = BunAgent::new(runtime, port_allocator, cmd_rx, agent_shutdown);
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

    // Start the API server
    let listener = tokio::net::TcpListener::bind(&cli.listen).await?;
    println!("bun: API server listening on {}", cli.listen);

    let app = api::router(
        cmd_tx,
        Some(Arc::clone(&mayo_store)),
        Some(Arc::clone(&log_store)),
        Some(deploy_history),
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

    // Start the Pickle OCI registry server
    let registry_addr = format!("0.0.0.0:{}", config.images.registry_port);
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
        catalog: Arc::new(RwLock::new(ManifestCatalog::default())),
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

    // Wait for SIGINT or SIGTERM
    let signal_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        ctrl_c.await.ok();
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
